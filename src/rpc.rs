use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::signers::{local::PrivateKeySigner, SignerSync};
use alloy::sol_types::SolCall;
use eyre::{eyre, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;

use crate::abi::IMulticall3;
use crate::config::{BuilderSpec, ChainConfig, RawTimingOverrides, TimingConfig};

#[derive(Debug, Clone, Deserialize)]
pub struct ReceiptLog {
    pub address: Address,
    #[serde(default)]
    pub topics: Vec<B256>,
    pub data: Bytes,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TxReceipt {
    #[serde(rename = "transactionHash")]
    pub transaction_hash: Option<B256>,
    #[allow(dead_code)]
    #[serde(rename = "blockNumber")]
    pub block_number: Option<String>,
    pub status: Option<String>,
    #[serde(rename = "gasUsed")]
    pub gas_used: Option<String>,
    #[serde(rename = "effectiveGasPrice")]
    pub effective_gas_price: Option<String>,
    #[serde(default)]
    pub logs: Vec<ReceiptLog>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    #[serde(rename = "stateDiff", skip_serializing_if = "Option::is_none")]
    pub state_diff: Option<HashMap<B256, B256>>,
}

#[derive(Clone)]
pub struct RpcPool {
    client: Client,
    ws_urls: Vec<String>,
    read_urls: Vec<String>,
    broadcast_urls: Vec<String>,
    bundle_urls: Vec<String>,
    builders: Vec<BuilderSpec>,
    multicall3: Address,
    block_tag: String,
    timing: TimingConfig,
    rr_idx: Arc<AtomicUsize>,
    leader_idx: Arc<AtomicUsize>,
    ws_head: Arc<AtomicU64>,
    ws_next_base_fee_wei: Arc<AtomicU64>,
    ws_next_base_fee_block: Arc<AtomicU64>,
    ws_notify: Arc<Notify>,
}

impl RpcPool {
    pub fn from_config(cfg: &ChainConfig) -> Result<Self> {
        let mut pool = Self::new_with_ws_and_bundles(
            cfg.ws_rpcs.clone(),
            cfg.read_rpcs.clone(),
            cfg.broadcast_rpcs.clone(),
            cfg.bundle_rpcs.clone(),
            cfg.multicall3,
            &cfg.block_tag,
        )?;
        pool.timing = cfg.timing.clone();
        pool.builders = cfg.submission.builders.clone();
        Ok(pool)
    }

    #[allow(dead_code)]
    pub fn new_with_bundles(
        read_urls: Vec<String>,
        broadcast_urls: Vec<String>,
        bundle_urls: Vec<String>,
        multicall3: Address,
        block_tag: impl Into<String>,
    ) -> Result<Self> {
        Self::new_with_ws_and_bundles(
            Vec::new(),
            read_urls,
            broadcast_urls,
            bundle_urls,
            multicall3,
            block_tag,
        )
    }

    pub fn new_with_ws_and_bundles(
        ws_urls: Vec<String>,
        read_urls: Vec<String>,
        broadcast_urls: Vec<String>,
        bundle_urls: Vec<String>,
        multicall3: Address,
        block_tag: impl Into<String>,
    ) -> Result<Self> {
        let client = Client::builder()
            .connect_timeout(Duration::from_millis(1200))
            .timeout(Duration::from_millis(5000))
            .pool_max_idle_per_host(16)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            client,
            ws_urls,
            read_urls,
            broadcast_urls,
            bundle_urls,
            builders: Vec::new(),
            multicall3,
            block_tag: block_tag.into(),
            timing: TimingConfig::derive(450, RawTimingOverrides::default()),
            rr_idx: Arc::new(AtomicUsize::new(0)),
            leader_idx: Arc::new(AtomicUsize::new(0)),
            ws_head: Arc::new(AtomicU64::new(0)),
            ws_next_base_fee_wei: Arc::new(AtomicU64::new(0)),
            ws_next_base_fee_block: Arc::new(AtomicU64::new(0)),
            ws_notify: Arc::new(Notify::new()),
        })
    }

    #[allow(dead_code)]
    pub fn block_tag(&self) -> &str {
        &self.block_tag
    }

    pub fn with_block_tag(&self, tag: impl Into<String>) -> Self {
        let mut cloned = self.clone();
        cloned.block_tag = tag.into();
        cloned
    }

    /// Move archive-capable endpoints (e.g. publicnode) to the front for historical eth_call replay.
    pub fn prioritize_archive_rpcs(&mut self) {
        let mut archive = Vec::new();
        let mut others = Vec::new();
        for u in self.read_urls.drain(..) {
            if u.contains("publicnode.com") {
                archive.push(u);
            } else {
                others.push(u);
            }
        }
        self.read_urls.extend(archive);
        self.read_urls.extend(others);
    }

    async fn post_rpc_single(client: &Client, url: &str, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params
        });
        let resp = client.post(url).json(&body).send().await?;
        let text = resp.text().await?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|e| eyre!("RPC {} parse err ({}): {}", url, e, &text[..text.len().min(160)]))?;
        if let Some(err) = v.get("error") {
            if is_deterministic_evm_error(err) {
                return Err(eyre!("[EVM_REVERT] RPC {} error: {}", url, err));
            }
            return Err(eyre!("RPC {} error: {}", url, err));
        }
        v.get("result")
            .cloned()
            .ok_or_else(|| eyre!("RPC {} missing result", url))
    }

    async fn call_single(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        Self::post_rpc_single(&self.client, url, method, params).await
    }

    async fn call_single_bounded(&self, url: &str, method: &str, params: Value) -> Result<Value> {
        let timeout = if method == "eth_getLogs" {
            Duration::from_millis(self.timing.rpc_get_logs_timeout_ms)
        } else {
            Duration::from_millis(self.timing.rpc_bounded_timeout_ms)
        };
        match tokio::time::timeout(timeout, self.call_single(url, method, params)).await {
            Ok(res) => res,
            Err(_) => Err(eyre!("RPC {url} timeout on {method}")),
        }
    }

    pub async fn call_leader_first(
        &self,
        method: &str,
        params: Value,
        reject_null: bool,
    ) -> Result<Value> {
        let n = self.read_urls.len();
        if n == 0 {
            return Err(eyre!("no rpc"));
        }
        let primary = n.min(3);
        let leader = self.leader_idx.load(Ordering::Relaxed) % n;
        let mut order = Vec::with_capacity(n);
        order.push(leader);
        for i in 0..n {
            if i != leader {
                order.push(i);
            }
        }
        let mut last_err = eyre!("no rpc");
        for idx in order {
            let url = &self.read_urls[idx];
            match self.call_single_bounded(url, method, params.clone()).await {
                Ok(v) => {
                    if reject_null && v.is_null() {
                        last_err = eyre!("RPC {} returned null for {}", url, method);
                        continue;
                    }
                    if idx < primary {
                        self.leader_idx.store(idx, Ordering::Relaxed);
                    }
                    return Ok(v);
                }
                Err(e) => {
                    if e.to_string().starts_with("[EVM_REVERT]") {
                        return Err(e);
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    #[inline]
    fn has_dedicated_primary(&self) -> bool {
        self.read_urls.first().is_some_and(|u| u.contains("/v2/"))
    }

    pub async fn call_with_failover(&self, method: &str, params: Value) -> Result<Value> {
        if self.has_dedicated_primary() {
            return self.call_leader_first(method, params, false).await;
        }
        let n = self.read_urls.len();
        if n == 0 {
            return Err(eyre!("no rpc"));
        }
        let primary = n.min(3);
        let start = self.rr_idx.fetch_add(1, Ordering::Relaxed) % primary;
        let mut last_err = eyre!("no rpc");
        for offset in 0..primary {
            let url = &self.read_urls[(start + offset) % primary];
            match self.call_single_bounded(url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if e.to_string().starts_with("[EVM_REVERT]") {
                        return Err(e);
                    }
                    last_err = e;
                }
            }
        }
        for url in self.read_urls.iter().skip(primary) {
            match self.call_single_bounded(url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if e.to_string().starts_with("[EVM_REVERT]") {
                        return Err(e);
                    }
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    /// Maintain resilient WSS `newHeads` subscriptions across configured WS endpoints
    /// so new block arrival wakes the solver immediately without polling lag.
    pub fn spawn_ws_head_watcher(&self) {
        for url in self.ws_urls.clone() {
            let ws_head = Arc::clone(&self.ws_head);
            let ws_next_bf = Arc::clone(&self.ws_next_base_fee_wei);
            let ws_next_bf_blk = Arc::clone(&self.ws_next_base_fee_block);
            let ws_notify = Arc::clone(&self.ws_notify);
            tokio::spawn(async move {
                loop {
                    match tokio::time::timeout(
                        Duration::from_secs(4),
                        tokio_tungstenite::connect_async(&url),
                    )
                    .await
                    {
                        Ok(Ok((ws_stream, _))) => {
                            let (mut write, mut read) = ws_stream.split();
                            let sub_msg = r#"{"jsonrpc":"2.0","id":1,"method":"eth_subscribe","params":["newHeads"]}"#;
                            if write.send(Message::Text(sub_msg.into())).await.is_ok() {
                                while let Some(msg) = read.next().await {
                                    match msg {
                                        Ok(Message::Text(txt)) => {
                                            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                                if v.get("id").and_then(|i| i.as_u64()) == Some(1) {
                                                    if let Some(sub_id) =
                                                        v.get("result").and_then(|r| r.as_str())
                                                    {
                                                        tracing::info!(
                                                            ws_url = %url,
                                                            sub_id,
                                                            "WS newHeads subscribed"
                                                        );
                                                    }
                                                }
                                                if let Some(hex_str) = v
                                                    .pointer("/params/result/number")
                                                    .and_then(|n| n.as_str())
                                                {
                                                    if let Ok(bn) = parse_hex_u64(hex_str) {
                                                        if let Some(bf_hex) = v
                                                            .pointer("/params/result/baseFeePerGas")
                                                            .and_then(|x| x.as_str())
                                                        {
                                                            if let Ok(bf) = parse_hex_u128(bf_hex) {
                                                                if bf > 0 {
                                                                    let gas_used = v
                                                                        .pointer("/params/result/gasUsed")
                                                                        .and_then(|x| x.as_str())
                                                                        .and_then(|s| parse_hex_u64(s).ok())
                                                                        .unwrap_or(0);
                                                                    let gas_limit = v
                                                                        .pointer("/params/result/gasLimit")
                                                                        .and_then(|x| x.as_str())
                                                                        .and_then(|s| parse_hex_u64(s).ok())
                                                                        .unwrap_or(0);
                                                                    let next_bf = if gas_limit > 0 {
                                                                        calc_next_eip1559_base_fee(
                                                                            bf, gas_used, gas_limit,
                                                                        )
                                                                    } else {
                                                                        bf
                                                                    };
                                                                    let clamped =
                                                                        next_bf.min(u64::MAX as u128) as u64;
                                                                    let prev_bf_bn =
                                                                        ws_next_bf_blk.load(Ordering::Acquire);
                                                                    if bn >= prev_bf_bn {
                                                                        ws_next_bf.store(
                                                                            clamped,
                                                                            Ordering::Release,
                                                                        );
                                                                        ws_next_bf_blk.store(
                                                                            bn,
                                                                            Ordering::Release,
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        let prev = ws_head
                                                            .fetch_max(bn, Ordering::AcqRel);
                                                        if bn > prev {
                                                            ws_notify.notify_waiters();
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        Ok(Message::Ping(payload)) => {
                                            let _ = write.send(Message::Pong(payload)).await;
                                        }
                                        Ok(Message::Close(_)) | Err(_) => break,
                                        _ => {}
                                    }
                                }
                            }
                        }
                        Ok(Err(e)) => {
                            tracing::debug!(ws_url = %url, err = %e, "WS connect error");
                        }
                        Err(_) => {
                            tracing::debug!(ws_url = %url, "WS connect timeout");
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(1500)).await;
                }
            });
        }
    }

    pub async fn wait_next_block(&self, last_seen: u64, poll_interval_ms: u64) -> Result<u64> {
        let notified = self.ws_notify.notified();
        tokio::pin!(notified);
        let ws_bn = self.ws_head.load(Ordering::Acquire);
        if ws_bn > last_seen {
            return Ok(ws_bn);
        }
        tokio::select! {
            _ = &mut notified => {
                let bn = self.ws_head.load(Ordering::Acquire);
                if bn > last_seen {
                    return Ok(bn);
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(poll_interval_ms)) => {}
        }
        let ws_bn2 = self.ws_head.load(Ordering::Acquire);
        if ws_bn2 > last_seen {
            return Ok(ws_bn2);
        }
        tokio::select! {
            _ = &mut notified => {
                let bn = self.ws_head.load(Ordering::Acquire);
                if bn > last_seen {
                    return Ok(bn);
                }
                self.poll_new_block(last_seen).await
            }
            res = self.poll_new_block(last_seen) => res,
        }
    }

    /// Fire-and-forget TCP/TLS socket keep-warm across primary read nodes and MEV builder endpoints.
    pub fn spawn_keep_warm(&self) {
        let mut urls: Vec<String> = self.read_urls.iter().take(4).cloned().collect();
        for u in &self.bundle_urls {
            if !urls.contains(u) {
                urls.push(u.clone());
            }
        }
        let client = self.client.clone();
        tokio::spawn(async move {
            for url in urls {
                let c = client.clone();
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_blockNumber",
                    "params": []
                });
                tokio::spawn(async move {
                    let _ = c.post(&url).json(&body).send().await;
                });
            }
        });
    }

    pub async fn get_receipt(&self, tx_hash: B256) -> Result<Option<TxReceipt>> {
        let v = self
            .call_with_failover("eth_getTransactionReceipt", json!([format!("{tx_hash:#x}")]))
            .await?;
        if v.is_null() {
            return Ok(None);
        }
        let receipt: TxReceipt = serde_json::from_value(v)?;
        Ok(Some(receipt))
    }

    /// Race fastest regional nodes concurrently for `eth_blockNumber`, falling back to top 4.
    pub async fn poll_new_block(&self, last_seen: u64) -> Result<u64> {
        let fast_ms = self.timing.rpc_fast_ms;
        let fallback_ms = self.timing.rpc_fallback_ms;
        let fast_n = self.read_urls.len().min(4);
        if last_seen != u64::MAX && fast_n >= 2 {
            let mut set = JoinSet::new();
            for idx in 0..fast_n {
                let client = self.client.clone();
                let url = self.read_urls[idx].clone();
                set.spawn(async move {
                    let res = tokio::time::timeout(
                        Duration::from_millis(fast_ms),
                        Self::post_rpc_single(&client, &url, "eth_blockNumber", json!([])),
                    )
                    .await;
                    match res {
                        Ok(Ok(v)) => {
                            let hex_str = v.as_str().ok_or_else(|| eyre!("bad blockNumber"))?;
                            let bn = parse_hex_u64(hex_str)?;
                            Ok((idx, bn))
                        }
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(eyre!("fast blockNumber timeout on {url}")),
                    }
                });
            }
            let mut best: Option<(usize, u64)> = None;
            let mut slow_deadline: Option<tokio::time::Instant> = None;
            loop {
                let next_joined = if let Some(dl) = slow_deadline {
                    tokio::select! {
                        j = set.join_next() => j,
                        _ = tokio::time::sleep_until(dl) => break,
                    }
                } else {
                    set.join_next().await
                };
                let Some(joined) = next_joined else {
                    break;
                };
                if let Ok(Ok((idx, bn))) = joined {
                    if bn > last_seen {
                        self.leader_idx.store(idx, Ordering::Relaxed);
                        return Ok(bn);
                    }
                    if best.map_or(true, |b| bn > b.1) {
                        best = Some((idx, bn));
                    }
                    if slow_deadline.is_none() {
                        slow_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(16));
                    }
                }
            }
            if let Some((_, max_bn)) = best {
                return Ok(max_bn);
            }
        }
        let primary = self.read_urls.len().min(4);
        if primary > 0 {
            let mut set = JoinSet::new();
            for idx in 0..primary {
                let client = self.client.clone();
                let url = self.read_urls[idx].clone();
                set.spawn(async move {
                    let res = tokio::time::timeout(
                        Duration::from_millis(fallback_ms),
                        Self::post_rpc_single(&client, &url, "eth_blockNumber", json!([])),
                    )
                    .await;
                    match res {
                        Ok(Ok(v)) => {
                            let hex_str = v.as_str().ok_or_else(|| eyre!("bad blockNumber"))?;
                            let bn = parse_hex_u64(hex_str)?;
                            Ok((idx, bn))
                        }
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(eyre!("blockNumber timeout on {url}")),
                    }
                });
            }
            let mut best: Option<(usize, u64)> = None;
            while let Some(joined) = set.join_next().await {
                if let Ok(Ok((idx, bn))) = joined {
                    if bn > last_seen {
                        self.leader_idx.store(idx, Ordering::Relaxed);
                        return Ok(bn);
                    }
                    if best.map_or(true, |b| bn > b.1) {
                        best = Some((idx, bn));
                    }
                }
            }
            if let Some((best_idx, max_bn)) = best {
                self.leader_idx.store(best_idx, Ordering::Relaxed);
                return Ok(max_bn);
            }
        }
        let v = self.call_with_failover("eth_blockNumber", json!([])).await?;
        let hex_str = v.as_str().ok_or_else(|| eyre!("bad blockNumber"))?;
        parse_hex_u64(hex_str)
    }

    pub async fn block_number(&self) -> Result<u64> {
        self.poll_new_block(u64::MAX).await
    }

    pub fn latest_ws_next_base_fee(&self, block_num: u64) -> Option<u128> {
        let bf_bn = self.ws_next_base_fee_block.load(Ordering::Acquire);
        let bf = self.ws_next_base_fee_wei.load(Ordering::Acquire);
        if bf > 0 && bf_bn >= block_num.saturating_sub(1) {
            Some(bf as u128)
        } else {
            None
        }
    }

    pub async fn get_block_next_base_fee(&self, block_num: u64) -> Result<Option<u128>> {
        let v = self
            .call_leader_first(
                "eth_getBlockByNumber",
                json!([format!("0x{block_num:x}"), false]),
                true,
            )
            .await?;
        let Some(bf_hex) = v.get("baseFeePerGas").and_then(|x| x.as_str()) else {
            return Ok(None);
        };
        let bf = parse_hex_u128(bf_hex)?;
        if bf == 0 {
            return Ok(None);
        }
        let gas_used = v
            .get("gasUsed")
            .and_then(|x| x.as_str())
            .and_then(|s| parse_hex_u64(s).ok())
            .unwrap_or(0);
        let gas_limit = v
            .get("gasLimit")
            .and_then(|x| x.as_str())
            .and_then(|s| parse_hex_u64(s).ok())
            .unwrap_or(0);
        let next_bf = if gas_limit > 0 {
            calc_next_eip1559_base_fee(bf, gas_used, gas_limit)
        } else {
            bf
        };
        Ok(Some(next_bf))
    }

    pub async fn gas_price(&self) -> Result<u128> {
        let primary = self.read_urls.len().min(4);
        if primary > 0 {
            let gp_timeout_ms = self.timing.rpc_gas_price_timeout_ms;
            let mut handles = Vec::with_capacity(primary);
            for idx in 0..primary {
                let client = self.client.clone();
                let url = self.read_urls[idx].clone();
                handles.push(tokio::spawn(async move {
                    let res = tokio::time::timeout(
                        Duration::from_millis(gp_timeout_ms),
                        Self::post_rpc_single(&client, &url, "eth_gasPrice", json!([])),
                    )
                    .await;
                    match res {
                        Ok(Ok(v)) => {
                            let hex_str = v.as_str().ok_or_else(|| eyre!("bad gasPrice"))?;
                            parse_hex_u128(hex_str)
                        }
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(eyre!("gasPrice timeout on {url}")),
                    }
                }));
            }
            let mut min_gp: Option<u128> = None;
            for h in handles {
                if let Ok(Ok(gp)) = h.await {
                    if gp > 0 {
                        min_gp = Some(min_gp.map_or(gp, |m| m.min(gp)));
                    }
                }
            }
            if let Some(gp) = min_gp {
                return Ok(gp);
            }
        }
        let v = self.call_with_failover("eth_gasPrice", json!([])).await?;
        let hex_str = v.as_str().ok_or_else(|| eyre!("bad gasPrice"))?;
        parse_hex_u128(hex_str)
    }

    pub async fn get_balance(&self, addr: Address) -> Result<U256> {
        let v = self
            .call_with_failover("eth_getBalance", json!([format!("{addr:#x}"), "latest"]))
            .await?;
        let hex_str = v.as_str().ok_or_else(|| eyre!("bad balance"))?;
        parse_hex_u256(hex_str)
    }

    pub async fn get_transaction_count(&self, addr: Address) -> Result<u64> {
        let v = self
            .call_with_failover("eth_getTransactionCount", json!([format!("{addr:#x}"), "pending"]))
            .await?;
        let hex_str = v.as_str().ok_or_else(|| eyre!("bad nonce"))?;
        parse_hex_u64(hex_str)
    }

    pub async fn get_code(&self, addr: Address) -> Result<Bytes> {
        let v = self
            .call_with_failover("eth_getCode", json!([format!("{addr:#x}"), "latest"]))
            .await?;
        let hex_str = v.as_str().ok_or_else(|| eyre!("bad code"))?;
        let raw = hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?;
        Ok(Bytes::from(raw))
    }

    pub async fn get_block_receipts(&self, block_param: &str) -> Result<Vec<TxReceipt>> {
        let br_ms = self.timing.rpc_receipts_timeout_ms;
        let n = self.read_urls.len();
        let fast_n = n.min(4);
        if fast_n >= 2 {
            let leader = self.leader_idx.load(Ordering::Relaxed) % fast_n;
            let mut set = JoinSet::new();
            for offset in 0..fast_n {
                let idx = (leader + offset) % fast_n;
                let client = self.client.clone();
                let url = self.read_urls[idx].clone();
                let param = block_param.to_string();
                set.spawn(async move {
                    let res = tokio::time::timeout(
                        Duration::from_millis(br_ms),
                        Self::post_rpc_single(
                            &client,
                            &url,
                            "eth_getBlockReceipts",
                            json!([param]),
                        ),
                    )
                    .await;
                    match res {
                        Ok(Ok(v)) if !v.is_null() => {
                            let receipts: Vec<TxReceipt> = serde_json::from_value(v)?;
                            Ok((idx, receipts))
                        }
                        Ok(Ok(_)) => Err(eyre!("null blockReceipts")),
                        Ok(Err(e)) => Err(e),
                        Err(_) => Err(eyre!("getBlockReceipts timeout on {url}")),
                    }
                });
            }
            while let Some(joined) = set.join_next().await {
                if let Ok(Ok((idx, receipts))) = joined {
                    self.leader_idx.store(idx, Ordering::Relaxed);
                    return Ok(receipts);
                }
            }
        }
        let v = self
            .call_leader_first("eth_getBlockReceipts", json!([block_param]), true)
            .await?;
        let receipts: Vec<TxReceipt> = serde_json::from_value(v)?;
        Ok(receipts)
    }

    pub async fn get_block_receipts_by_number(&self, block_num: u64) -> Result<Vec<TxReceipt>> {
        let tag = format!("0x{block_num:x}");
        self.get_block_receipts(&tag).await
    }

    pub async fn eth_call(
        &self,
        from: Option<Address>,
        to: Address,
        data: &[u8],
        block_tag: &str,
        overrides: Option<&HashMap<Address, AccountOverride>>,
    ) -> Result<Bytes> {
        let mut tx = json!({
            "to": format!("{to:#x}"),
            "data": format!("0x{}", hex::encode(data)),
        });
        if to != self.multicall3 {
            // Cap non-multicall simulation gas at 5.0M to allow up to 32-step DAGs while halting gas-bomb tokens
            tx["gas"] = json!("0x4c4b40");
        }
        if let Some(f) = from {
            tx["from"] = json!(format!("{f:#x}"));
        }
        let params = if let Some(ov) = overrides {
            let mut ov_map = serde_json::Map::new();
            for (k, v) in ov {
                ov_map.insert(format!("{k:#x}"), serde_json::to_value(v)?);
            }
            json!([tx, block_tag, Value::Object(ov_map)])
        } else {
            json!([tx, block_tag])
        };

        let fast_call_ms = self.timing.rpc_call_timeout_ms;
        let v = if !self.has_dedicated_primary()
            && to != self.multicall3
            && block_tag == "latest"
            && self.read_urls.len() >= 2
        {
            let fast_idx = self.rr_idx.fetch_add(1, Ordering::Relaxed) % 2;
            let url = &self.read_urls[fast_idx];
            match tokio::time::timeout(
                Duration::from_millis(fast_call_ms),
                self.call_single(url, "eth_call", params.clone()),
            )
            .await
            {
                Ok(Ok(v)) if !v.is_null() => v,
                Ok(Err(e)) if e.to_string().starts_with("[EVM_REVERT]") => return Err(e),
                _ => self.call_leader_first("eth_call", params, false).await?,
            }
        } else {
            self.call_leader_first("eth_call", params, false).await?
        };
        let hex_str = v.as_str().ok_or_else(|| eyre!("bad eth_call result"))?;
        let raw = hex::decode(hex_str.strip_prefix("0x").unwrap_or(hex_str))?;
        Ok(Bytes::from(raw))
    }

    pub async fn estimate_gas(&self, from: Address, to: Option<Address>, data: &[u8]) -> Result<u64> {
        let mut tx = json!({
            "from": format!("{from:#x}"),
            "data": format!("0x{}", hex::encode(data)),
        });
        if let Some(t) = to {
            tx["to"] = json!(format!("{t:#x}"));
        }
        let v = self.call_with_failover("eth_estimateGas", json!([tx])).await?;
        let hex_str = v.as_str().ok_or_else(|| eyre!("bad estimateGas"))?;
        parse_hex_u64(hex_str)
    }

    pub async fn multicall(&self, calls: Vec<IMulticall3::Call3>) -> Result<Vec<IMulticall3::Result>> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }
        let chunks: Vec<Vec<IMulticall3::Call3>> = calls.chunks(140).map(|c| c.to_vec()).collect();
        if chunks.len() == 1 {
            let calldata = IMulticall3::aggregate3Call {
                calls: chunks.into_iter().next().unwrap(),
            }
            .abi_encode();
            let raw = self
                .eth_call(None, self.multicall3, &calldata, &self.block_tag, None)
                .await?;
            return Ok(IMulticall3::aggregate3Call::abi_decode_returns(&raw)?);
        }

        let mut out = Vec::with_capacity(calls.len());
        for batch in chunks.chunks(6) {
            let mut handles = Vec::with_capacity(batch.len());
            for chunk in batch {
                let rpc_c = self.clone();
                let calls_c = chunk.clone();
                handles.push(tokio::spawn(async move {
                    let calldata = IMulticall3::aggregate3Call { calls: calls_c }.abi_encode();
                    let raw = rpc_c
                        .eth_call(None, rpc_c.multicall3, &calldata, &rpc_c.block_tag, None)
                        .await?;
                    Ok::<Vec<IMulticall3::Result>, eyre::Error>(
                        IMulticall3::aggregate3Call::abi_decode_returns(&raw)?,
                    )
                }));
            }
            for h in handles {
                let decoded = h.await.map_err(|e| eyre!("multicall join err: {e}"))??;
                out.extend(decoded);
            }
        }
        Ok(out)
    }

    /// Broadcast raw signed transaction concurrently to all configured broadcast endpoints
    /// (plus `eth_sendBundle` to 48Club Puissant builder when `max_block_num` is provided).
    pub async fn broadcast_raw_tx(&self, raw_tx_hex: &str, max_block_num: Option<u64>) -> Result<B256> {
        let mut futs = Vec::new();
        for url in &self.broadcast_urls {
            let client = self.client.clone();
            let url_c = url.clone();
            let raw = raw_tx_hex.to_string();
            futs.push(tokio::spawn(async move {
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_sendRawTransaction",
                    "params": [raw]
                });
                let resp = client.post(&url_c).json(&body).send().await?;
                let v: Value = resp.json().await?;
                if let Some(err) = v.get("error") {
                    return Err(eyre!("{url_c}: {err}"));
                }
                let hash_str = v["result"].as_str().ok_or_else(|| eyre!("{url_c}: missing hash"))?;
                let hash: B256 = hash_str.parse()?;
                Ok::<(String, B256), eyre::Error>((url_c, hash))
            }));
        }

        let mut bundle_futs = Vec::new();
        if let Some(mb) = max_block_num {
            for url in &self.broadcast_urls {
                if url.contains("puissant-builder.48.club") {
                    let client = self.client.clone();
                    let url_c = url.clone();
                    let raw = raw_tx_hex.to_string();
                    bundle_futs.push(tokio::spawn(async move {
                        let body = json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "eth_sendBundle",
                            "params": [{
                                "txs": [raw],
                                "maxBlockNumber": mb
                            }]
                        });
                        let resp = client.post(&url_c).json(&body).send().await?;
                        let v: Value = resp.json().await?;
                        if let Some(err) = v.get("error") {
                            return Err(eyre!("{url_c} bundle: {err}"));
                        }
                        Ok::<String, eyre::Error>(v["result"].to_string())
                    }));
                }
            }
        }

        let mut first_hash = None;
        let mut bundle_accepted = false;
        let mut errors = Vec::new();
        for f in futs {
            match f.await {
                Ok(Ok((url, h))) => {
                    tracing::info!(endpoint = %url, tx_hash = %h, "broadcast accepted");
                    if first_hash.is_none() {
                        first_hash = Some(h);
                    }
                }
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(e.to_string()),
            }
        }
        for bf in bundle_futs {
            match bf.await {
                Ok(Ok(res)) => {
                    bundle_accepted = true;
                    tracing::info!(bundle_res = %res, "48Club eth_sendBundle accepted");
                }
                Ok(Err(e)) => errors.push(e.to_string()),
                Err(e) => errors.push(e.to_string()),
            }
        }
        if first_hash.is_none() && bundle_accepted {
            if let Ok(raw_bytes) = hex::decode(raw_tx_hex.strip_prefix("0x").unwrap_or(raw_tx_hex)) {
                first_hash = Some(keccak256(&raw_bytes));
            }
        }
        first_hash.ok_or_else(|| eyre!("all broadcasts failed: {}", errors.join(" | ")))
    }

    pub fn bundle_urls(&self) -> &[String] {
        &self.bundle_urls
    }

    /// Submit raw signed transaction bundle(s) per configured MEV builder as revert-protected
    /// `eth_sendBundle` requests (supporting 2-tx `[arb_tx, builder_eoa_tip_tx]` atomic bundles).
    pub async fn broadcast_bundle_per_builder(
        &self,
        builder_txs: &[(String, Vec<String>)],
        max_block_num: u64,
        position_first: bool,
        primary_tx_hash: B256,
    ) -> Result<B256> {
        if builder_txs.is_empty() {
            return Err(eyre!("all MEV builders rejected bundle: no bundle URLs configured"));
        }

        let grace_end =
            tokio::time::Instant::now() + Duration::from_millis(self.timing.bundle_grace_ms);
        let http_timeout_ms = self.timing.bundle_http_timeout_ms;
        let backend_map: HashMap<String, String> = self
            .builders
            .iter()
            .map(|b| (b.url.clone(), b.backend.clone()))
            .collect();
        let mut submissions = JoinSet::new();
        for (url, txs) in builder_txs {
            let client = self.client.clone();
            let url_c = url.clone();
            let txs_c = txs.clone();
            let supports_position_first = self
                .builders
                .iter()
                .find(|b| b.url == *url)
                .is_some_and(|b| b.position_first);
            submissions.spawn(async move {
                let mut bundle_obj = json!({
                    "txs": txs_c,
                    "maxBlockNumber": max_block_num,
                    "noMerge": false
                });
                if supports_position_first {
                    bundle_obj["positionFirst"] = json!(position_first);
                }
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "eth_sendBundle",
                    "params": [bundle_obj]
                });
                let reply = async {
                    let timeout = Duration::from_millis(http_timeout_ms);
                    let resp = tokio::time::timeout(timeout, client.post(&url_c).json(&body).send())
                        .await
                        .map_err(|_| eyre!("builder HTTP send timeout"))??;
                    let v: Value = tokio::time::timeout(timeout, resp.json())
                        .await
                        .map_err(|_| eyre!("builder HTTP json timeout"))??;
                    builder_acknowledgement(v)
                }
                .await;
                // Log in the worker so detached builders still report their outcomes.
                match reply {
                    Ok(Some(res)) => tracing::info!(builder = %url_c, bundle_res = %res,
                        tx_hash = %primary_tx_hash, "MEV bundle accepted"),
                    Ok(None) => {
                        tracing::debug!(builder = %url_c, tx_hash = %primary_tx_hash,
                            "MEV bundle deduplicated on shared builder backend");
                    }
                    Err(e) => {
                        tracing::info!(builder = %url_c, err = %e, tx_hash = %primary_tx_hash,
                            "MEV bundle rejected");
                        return Err(eyre!("{url_c}: {e}"));
                    }
                }
                Ok(url_c)
            });
        }

        wait_for_builders(submissions, grace_end, &backend_map).await?;
        Ok(primary_tx_hash)
    }

    /// Broadcast an Ethereum Mainnet `eth_sendBundle` across MEV-Boost builders (Flashbots,
    /// Titan, BeaverBuild, Quasar) signed with `X-Flashbots-Signature`.
    pub async fn broadcast_eth_bundle(
        &self,
        txs: &[String],
        target_blocks: &[u64],
        auth_signer: &PrivateKeySigner,
        primary_tx_hash: B256,
    ) -> Result<B256> {
        if self.bundle_urls.is_empty() || target_blocks.is_empty() || txs.is_empty() {
            return Err(eyre!(
                "all MEV builders rejected bundle: missing Ethereum bundle URLs, targets, or txs"
            ));
        }

        let grace_end =
            tokio::time::Instant::now() + Duration::from_millis(self.timing.bundle_grace_ms);
        let http_timeout_ms = self.timing.bundle_http_timeout_ms;
        let backend_map: HashMap<String, String> = self
            .builders
            .iter()
            .map(|b| (b.url.clone(), b.backend.clone()))
            .collect();
        let mut submissions = JoinSet::new();
        for &target_bn in target_blocks {
            let body = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "eth_sendBundle",
                "params": [{
                    "txs": txs,
                    "blockNumber": format!("0x{target_bn:x}")
                }]
            });
            let body_str = serde_json::to_string(&body)?;
            let sig_hdr = flashbots_signature_header(auth_signer, &body_str)?;
            for url in &self.bundle_urls {
                let client = self.client.clone();
                let url_c = url.clone();
                let body_c = body_str.clone();
                let sig_c = sig_hdr.clone();
                submissions.spawn(async move {
                    let timeout = Duration::from_millis(http_timeout_ms);
                    let reply = async {
                        let resp = tokio::time::timeout(
                            timeout,
                            client
                                .post(&url_c)
                                .header("Content-Type", "application/json")
                                .header("X-Flashbots-Signature", &sig_c)
                                .body(body_c)
                                .send(),
                        )
                        .await
                        .map_err(|_| eyre!("builder HTTP send timeout"))??;
                        let text = tokio::time::timeout(timeout, resp.text())
                            .await
                            .map_err(|_| eyre!("builder HTTP body timeout"))??;
                        let v: Value = serde_json::from_str(&text)
                            .map_err(|e| eyre!("builder JSON parse err ({}): {}", e, &text[..text.len().min(160)]))?;
                        eth_builder_acknowledgement(v)
                    }
                    .await;
                    match reply {
                        Ok(Some(res)) => tracing::info!(
                            builder = %url_c,
                            target_block = target_bn,
                            bundle_res = %res,
                            tx_hash = %primary_tx_hash,
                            "ETH MEV bundle accepted"
                        ),
                        Ok(None) => tracing::debug!(
                            builder = %url_c,
                            target_block = target_bn,
                            tx_hash = %primary_tx_hash,
                            "ETH MEV bundle accepted (dedup)"
                        ),
                        Err(e) => {
                            tracing::info!(
                                builder = %url_c,
                                target_block = target_bn,
                                err = %e,
                                tx_hash = %primary_tx_hash,
                                "ETH MEV bundle rejected"
                            );
                            return Err(eyre!("{url_c}@{target_bn}: {e}"));
                        }
                    }
                    Ok(url_c)
                });
            }
        }

        wait_for_builders(submissions, grace_end, &backend_map).await?;
        Ok(primary_tx_hash)
    }

    /// Submit a single `mev_sendBundle` backrun payload to Flashbots MEV-Share relay,
    /// falling back once without `privacy.builders` if the relay rejects an outdated builder name.
    #[allow(clippy::too_many_arguments)]
    pub async fn send_mev_share_bundle(
        &self,
        relay_url: &str,
        victim_hash: B256,
        signed_backrun_tx: &str,
        target_block: u64,
        max_block: u64,
        builders: &[String],
        auth_signer: &PrivateKeySigner,
        primary_tx_hash: B256,
    ) -> Result<String> {
        let timeout = Duration::from_millis(self.timing.bundle_http_timeout_ms);
        let mut try_builders = builders;
        let mut tried_fallback = builders.is_empty();

        loop {
            let body = build_mev_share_bundle_payload(
                victim_hash,
                signed_backrun_tx,
                target_block,
                max_block,
                try_builders,
            );
            let body_str = serde_json::to_string(&body)?;
            let sig_hdr = flashbots_signature_header(auth_signer, &body_str)?;
            let reply = async {
                let resp = tokio::time::timeout(
                    timeout,
                    self.client
                        .post(relay_url)
                        .header("Content-Type", "application/json")
                        .header("X-Flashbots-Signature", &sig_hdr)
                        .body(body_str)
                        .send(),
                )
                .await
                .map_err(|_| eyre!("MEV-Share HTTP send timeout"))??;
                let text = tokio::time::timeout(timeout, resp.text())
                    .await
                    .map_err(|_| eyre!("MEV-Share HTTP body timeout"))??;
                let v: Value = serde_json::from_str(&text).map_err(|e| {
                    eyre!(
                        "MEV-Share JSON parse err ({}): {}",
                        e,
                        &text[..text.len().min(160)]
                    )
                })?;
                eth_builder_acknowledgement(v)
            }
            .await;

            match reply {
                Ok(res_opt) => {
                    let bh = match res_opt.as_ref() {
                        Some(v) => v
                            .get("bundleHash")
                            .and_then(|x| x.as_str())
                            .map(str::to_string)
                            .or_else(|| v.as_str().map(str::to_string))
                            .unwrap_or_else(|| v.to_string()),
                        None => "dedup".to_string(),
                    };
                    tracing::info!(
                        relay = %relay_url,
                        %victim_hash,
                        tx_hash = %primary_tx_hash,
                        target_block,
                        max_block,
                        bundle_hash = %bh,
                        "MEV-Share bundle accepted"
                    );
                    return Ok(bh);
                }
                Err(e) => {
                    let msg_lower = e.to_string().to_ascii_lowercase();
                    if !tried_fallback && msg_lower.contains("builder") {
                        tried_fallback = true;
                        try_builders = &[];
                        tracing::warn!(
                            relay = %relay_url,
                            %victim_hash,
                            err = %e,
                            "MEV-Share relay rejected builder list; retrying with default routing"
                        );
                        continue;
                    }
                    tracing::warn!(
                        relay = %relay_url,
                        %victim_hash,
                        tx_hash = %primary_tx_hash,
                        target_block,
                        max_block,
                        err = %e,
                        "MEV-Share bundle rejected"
                    );
                    return Err(e);
                }
            }
        }
    }

    pub async fn wait_receipt(&self, tx_hash: B256, timeout_ms: u64) -> Result<Option<TxReceipt>> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        while tokio::time::Instant::now() < deadline {
            if let Ok(v) = self
                .call_with_failover("eth_getTransactionReceipt", json!([format!("{tx_hash:#x}")]))
                .await
            {
                if !v.is_null() {
                    let receipt: TxReceipt = serde_json::from_value(v)?;
                    return Ok(Some(receipt));
                }
            }
            tokio::time::sleep(Duration::from_millis(280)).await;
        }
        Ok(None)
    }
}

/// Build the Flashbots MEV-Share `mev_sendBundle` JSON-RPC request body.
pub fn build_mev_share_bundle_payload(
    victim_hash: B256,
    signed_backrun_tx: &str,
    target_block: u64,
    max_block: u64,
    builders: &[String],
) -> Value {
    let mut bundle_obj = json!({
        "version": "v0.1",
        "inclusion": {
            "block": format!("0x{target_block:x}"),
            "maxBlock": format!("0x{max_block:x}")
        },
        "body": [
            { "hash": format!("{victim_hash:#x}") },
            { "tx": signed_backrun_tx, "canRevert": false }
        ]
    });
    if !builders.is_empty() {
        if let Some(obj) = bundle_obj.as_object_mut() {
            obj.insert(
                "privacy".to_string(),
                json!({
                    "builders": builders
                }),
            );
        }
    }
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "mev_sendBundle",
        "params": [bundle_obj]
    })
}

/// Compute the Flashbots/MEV-Boost `X-Flashbots-Signature` HTTP header value (`address:signature`).
pub fn flashbots_signature_header(auth_signer: &PrivateKeySigner, body_str: &str) -> Result<String> {
    let body_hash = format!("0x{}", hex::encode(keccak256(body_str.as_bytes())));
    let sig = auth_signer.sign_message_sync(body_hash.as_bytes())?;
    Ok(format!("{:#x}:0x{}", auth_signer.address(), hex::encode(sig.as_bytes())))
}

/// Ethereum builders vary in `result`: Flashbots/Titan return `{"result":{"bundleHash":"..."}}`,
/// whereas BeaverBuild returns `{"jsonrpc":"2.0","id":1,"result":null}` without `error` on success.
fn eth_builder_acknowledgement(mut reply: Value) -> Result<Option<Value>> {
    if let Some(error) = reply.get("error").filter(|e| !e.is_null()) {
        let message = error.to_string();
        if message.to_ascii_lowercase().contains("already exist") {
            return Ok(None);
        }
        return Err(eyre!("{message}"));
    }
    let Some(res) = reply.get_mut("result") else {
        return Err(eyre!("missing result field in builder reply"));
    };
    if res.as_bool() == Some(false) {
        return Err(eyre!("builder returned result=false"));
    }
    Ok(Some(res.take()))
}

/// A shared backend can acknowledge an earlier submission without returning a new bundle hash.
fn builder_acknowledgement(mut reply: Value) -> Result<Option<Value>> {
    if let Some(error) = reply.get("error").filter(|e| !e.is_null()) {
        let message = error.to_string();
        if message.to_ascii_lowercase().contains("bundle already exist") {
            return Ok(None);
        }
        return Err(eyre!("{message}"));
    }
    let result = reply
        .get_mut("result")
        .filter(|r| !r.is_null() && r.as_bool() != Some(false))
        .ok_or_else(|| eyre!("missing bundle acknowledgement"))?;
    Ok(Some(result.take()))
}

async fn wait_for_builders(
    mut submissions: JoinSet<Result<String>>,
    grace_end: tokio::time::Instant,
    backend_map: &HashMap<String, String>,
) -> Result<()> {
    let mut accepted = HashSet::new();
    let mut errors = Vec::new();
    let grace = tokio::time::sleep_until(grace_end);
    tokio::pin!(grace);
    loop {
        let joined = tokio::select! {
            biased;
            _ = &mut grace, if accepted.len() >= 2 => {
                submissions.detach_all();
                break;
            }
            joined = submissions.join_next() => joined,
        };
        match joined {
            Some(Ok(Ok(url))) => {
                // Regional endpoints can acknowledge the same builder's bundle.
                accepted.insert(backend_map.get(&url).cloned().unwrap_or(url));
            }
            Some(Ok(Err(e))) => errors.push(e.to_string()),
            Some(Err(e)) => errors.push(e.to_string()),
            None => break,
        }
    }
    if accepted.is_empty() {
        return Err(eyre!("all MEV builders rejected bundle: {}", errors.join(" | ")));
    }
    Ok(())
}

pub fn is_deterministic_evm_error(err_val: &Value) -> bool {
    let code = err_val.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
    if code == 3 {
        return true;
    }
    let msg = err_val
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    msg.contains("execution reverted")
        || msg.contains("out of gas")
        || msg.contains("gas required exceeds")
        || msg.contains("invalid opcode")
        || msg.contains("stack underflow")
}

pub fn parse_hex_u64(s: &str) -> Result<u64> {
    let trimmed = s.strip_prefix("0x").unwrap_or(s);
    if trimmed.is_empty() {
        return Ok(0);
    }
    Ok(u64::from_str_radix(trimmed, 16)?)
}

/// Compute the deterministic EIP-1559 `baseFeePerGas` for block `parent + 1`.
pub fn calc_next_eip1559_base_fee(
    parent_base_fee: u128,
    parent_gas_used: u64,
    parent_gas_limit: u64,
) -> u128 {
    let gas_target = (parent_gas_limit / 2) as u128;
    if parent_base_fee == 0 || gas_target == 0 {
        return parent_base_fee;
    }
    let gas_used = parent_gas_used as u128;
    if gas_used == gas_target {
        return parent_base_fee;
    }
    if gas_used > gas_target {
        let gas_used_delta = gas_used - gas_target;
        let base_fee_delta = ((parent_base_fee * gas_used_delta) / gas_target / 8).max(1);
        parent_base_fee.saturating_add(base_fee_delta)
    } else {
        let gas_used_delta = gas_target - gas_used;
        let base_fee_delta = (parent_base_fee * gas_used_delta) / gas_target / 8;
        parent_base_fee.saturating_sub(base_fee_delta)
    }
}

pub fn parse_hex_u128(s: &str) -> Result<u128> {
    let trimmed = s.strip_prefix("0x").unwrap_or(s);
    if trimmed.is_empty() {
        return Ok(0);
    }
    Ok(u128::from_str_radix(trimmed, 16)?)
}

pub fn parse_hex_u256(s: &str) -> Result<U256> {
    let trimmed = s.strip_prefix("0x").unwrap_or(s);
    if trimmed.is_empty() {
        return Ok(U256::ZERO);
    }
    Ok(U256::from_str_radix(trimmed, 16)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;
    use tokio::time::{timeout, Instant};

    #[test]
    fn only_accepted_or_deduplicated_replies_count() {
        let result = json!({"bundleHash": "0xab"});
        assert_eq!(
            builder_acknowledgement(json!({"error": null, "result": result})).unwrap(),
            Some(result)
        );
        assert_eq!(
            builder_acknowledgement(json!({"error": {"message": "Bundle already exists"}})).unwrap(),
            None
        );
        for reply in [
            json!({}),
            json!({"result": null}),
            json!({"result": false}),
            json!({"error": "invalid bundle"}),
        ] {
            assert!(builder_acknowledgement(reply).is_err());
        }
    }

    fn test_backend_map() -> HashMap<String, String> {
        HashMap::from([
            ("frankfurt.bsc.blockrazor.xyz".into(), "blockrazor".into()),
            ("singapore.bsc.blockrazor.xyz".into(), "blockrazor".into()),
            ("puissant-builder.48.club".into(), "48club".into()),
            ("nodereal.io".into(), "nodereal".into()),
            ("48.club".into(), "48club".into()),
        ])
    }

    #[tokio::test]
    async fn builder_quorum_observes_grace_and_detaches_slow_first_submission() {
        let mut tasks = JoinSet::new();
        let backends = test_backend_map();
        let (release, slow) = oneshot::channel();
        let (finished, done) = oneshot::channel();
        tasks.spawn(async move {
            slow.await?;
            finished.send(()).unwrap();
            Ok("nodereal.io".into())
        });
        let start = Instant::now();
        for url in ["frankfurt.bsc.blockrazor.xyz", "puissant-builder.48.club"] {
            tasks.spawn(async move {
                tokio::time::sleep_until(start + Duration::from_millis(7)).await;
                Ok(url.into())
            });
        }
        let deadline = start + Duration::from_millis(18);
        timeout(
            Duration::from_secs(1),
            wait_for_builders(tasks, deadline, &backends),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(Instant::now() >= deadline);
        println!(
            "Builder quorum returned in {:?} with the slow builder still pending",
            start.elapsed()
        );
        release.send(()).unwrap();
        timeout(Duration::from_secs(1), done).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn regional_builder_acks_do_not_satisfy_quorum() {
        let mut tasks = JoinSet::new();
        let backends = test_backend_map();
        tasks.spawn(async { Ok("frankfurt.bsc.blockrazor.xyz".into()) });
        tasks.spawn(async { Ok("singapore.bsc.blockrazor.xyz".into()) });
        let (release, slow) = oneshot::channel();
        tasks.spawn(async move {
            slow.await?;
            Ok("puissant-builder.48.club".into())
        });
        let wait = wait_for_builders(tasks, Instant::now(), &backends);
        tokio::pin!(wait);
        assert!(timeout(Duration::from_millis(10), &mut wait).await.is_err());
        release.send(()).unwrap();
        timeout(Duration::from_secs(1), wait).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn all_builders_finished_returns_any_acceptance_or_all_errors() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let backends = test_backend_map();
        let mut tasks = JoinSet::new();
        tasks.spawn(async { Err(eyre!("first rejection")) });
        tasks.spawn(async { Ok("48.club".into()) });
        timeout(
            Duration::from_secs(1),
            wait_for_builders(tasks, deadline, &backends),
        )
        .await
        .unwrap()
        .unwrap();

        let mut tasks = JoinSet::new();
        tasks.spawn(async { Err(eyre!("first rejection")) });
        tasks.spawn(async { Err(eyre!("second rejection")) });
        let err = wait_for_builders(tasks, deadline, &backends)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("first rejection") && err.contains("second rejection"));
    }

    #[test]
    fn test_flashbots_signature_and_eth_builder_ack() {
        let signer = PrivateKeySigner::random();
        let body = r#"{"jsonrpc":"2.0","id":1,"method":"eth_sendBundle","params":[{"txs":["0x02"],"blockNumber":"0x1"}]}"#;
        let hdr = flashbots_signature_header(&signer, body).unwrap();
        let (addr_str, sig_str) = hdr.split_once(':').unwrap();
        assert_eq!(addr_str.parse::<Address>().unwrap(), signer.address());
        let sig_bytes = hex::decode(sig_str.strip_prefix("0x").unwrap()).unwrap();
        assert_eq!(sig_bytes.len(), 65);
        let sig = alloy::signers::Signature::from_raw(&sig_bytes).unwrap();
        let body_hash = format!("0x{}", hex::encode(keccak256(body.as_bytes())));
        assert_eq!(
            sig.recover_address_from_msg(body_hash.as_bytes()).unwrap(),
            signer.address()
        );

        assert!(eth_builder_acknowledgement(json!({"result": {"bundleHash": "0x1234"}})).is_ok());
        assert!(eth_builder_acknowledgement(json!({"jsonrpc": "2.0", "id": 1, "result": null})).is_ok());
        assert!(eth_builder_acknowledgement(json!({"error": {"message": "bundle already exists"}})).is_ok());
        assert!(eth_builder_acknowledgement(json!({"result": false})).is_err());
        assert!(eth_builder_acknowledgement(json!({"error": {"code": -32600, "message": "signature is required"}})).is_err());
        assert!(eth_builder_acknowledgement(json!({})).is_err());
    }

    #[test]
    fn test_calc_next_eip1559_base_fee() {
        let base = 1_000_000_000u128; // 1 gwei
        let gas_limit = 30_000_000u64;
        // At 50% gas target (15M / 30M): unchanged
        assert_eq!(calc_next_eip1559_base_fee(base, 15_000_000, gas_limit), base);
        // Full block (30M / 30M): +12.5%
        assert_eq!(
            calc_next_eip1559_base_fee(base, 30_000_000, gas_limit),
            1_125_000_000
        );
        // Empty block (0 / 30M): -12.5%
        assert_eq!(
            calc_next_eip1559_base_fee(base, 0, gas_limit),
            875_000_000
        );
        // 75% block (22.5M / 30M): +6.25%
        assert_eq!(
            calc_next_eip1559_base_fee(base, 22_500_000, gas_limit),
            1_062_500_000
        );
    }

    #[test]
    fn test_build_mev_share_bundle_payload() {
        let vh = B256::repeat_byte(0x42);
        let builders = vec!["flashbots".to_string(), "Titan".to_string()];
        let with_b = build_mev_share_bundle_payload(vh, "0x02deadbeef", 100, 102, &builders);
        assert_eq!(with_b["method"], "mev_sendBundle");
        let p = &with_b["params"][0];
        assert_eq!(p["version"], "v0.1");
        assert_eq!(p["inclusion"]["block"], "0x64");
        assert_eq!(p["inclusion"]["maxBlock"], "0x66");
        assert_eq!(p["body"][0]["hash"], format!("{vh:#x}"));
        assert_eq!(p["body"][1]["tx"], "0x02deadbeef");
        assert_eq!(p["body"][1]["canRevert"], false);
        assert_eq!(p["privacy"]["builders"], json!(["flashbots", "Titan"]));

        let no_b = build_mev_share_bundle_payload(vh, "0x02cafe", 100, 100, &[]);
        assert!(no_b["params"][0].get("privacy").is_none());
    }
}
