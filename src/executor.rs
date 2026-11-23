use alloy::consensus::{SignableTransaction, TxEip1559, TxLegacy};
use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{keccak256, Address, Bytes, TxKind, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::signers::SignerSync;
use alloy::sol_types::{SolCall, SolValue};
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::abi::{IDagArbExecutor, IMulticall3};
use crate::config::{BiddingMode, SubmissionMode, TxType};
use crate::discovery::MarketState;
use crate::rpc::{RpcPool, TxReceipt};
use crate::simulator::{CompiledContract, SimulationResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BscTxSpec {
    pub exec_gp: u128,
    pub min_profit_raw: U256,
    pub bribe_bps: u16,
    pub msg_value_wei: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PnlTracker {
    pub chain: String,
    pub total_trades: u64,
    pub winning_trades: u64,
    pub reverted_trades: u64,
    pub cumulative_gross_usd: f64,
    pub cumulative_gas_usd: f64,
    pub cumulative_net_usd: f64,
    pub trades: Vec<ExecutedTradeLog>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutedTradeLog {
    pub timestamp: String,
    pub tx_hash: String,
    pub block_number: u64,
    pub strategy: String,
    pub status: String,
    pub gross_usd: f64,
    pub gas_usd: f64,
    pub net_usd: f64,
}

impl PnlTracker {
    pub fn load(path: &str, chain: &str) -> Self {
        if Path::new(path).exists() {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(p) = serde_json::from_str::<Self>(&text) {
                    if p.chain == chain || p.chain.is_empty() {
                        return p;
                    }
                }
            }
        }
        Self {
            chain: chain.to_string(),
            ..Default::default()
        }
    }

    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(self)?;
        std::fs::write(path, body)?;
        Ok(())
    }
}

pub fn load_or_create_wallet(env_path: &str, create_if_missing: bool) -> Result<(PrivateKeySigner, bool)> {
    let existing = std::env::var("PRIVATE_KEY").ok().filter(|s| !s.trim().is_empty());
    if let Some(pk) = existing {
        let signer: PrivateKeySigner = pk.trim().parse()?;
        return Ok((signer, false));
    }

    if !create_if_missing {
        return Err(eyre!(
            "PRIVATE_KEY not configured in .env; run `cargo run --release -- init-wallet`"
        ));
    }

    let signer = PrivateKeySigner::random();
    let pk_hex = format!("0x{}", hex::encode(signer.to_bytes()));
    upsert_env_var(env_path, "PRIVATE_KEY", &pk_hex)?;
    Ok((signer, true))
}

#[derive(Clone)]
pub struct EoaSlot {
    pub signer: PrivateKeySigner,
    pub address: Address,
    pub nonce: Arc<AtomicU64>,
    pub balance_wei: Arc<Mutex<U256>>,
    pub last_submitted_block: Arc<AtomicU64>,
    pub busy_until_block: Arc<AtomicU64>,
}

impl EoaSlot {
    pub fn new(signer: PrivateKeySigner) -> Self {
        let address = signer.address();
        Self {
            signer,
            address,
            nonce: Arc::new(AtomicU64::new(0)),
            balance_wei: Arc::new(Mutex::new(U256::ZERO)),
            last_submitted_block: Arc::new(AtomicU64::new(0)),
            busy_until_block: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn balance(&self) -> U256 {
        *self.balance_wei.lock().unwrap()
    }

    pub fn set_balance(&self, bal: U256) {
        *self.balance_wei.lock().unwrap() = bal;
    }
}

#[derive(Clone)]
pub struct EoaPool {
    pub slots: Vec<EoaSlot>,
    pub rr_cursor: Arc<AtomicUsize>,
}

impl EoaPool {
    pub fn new(primary: PrivateKeySigner, extras: Vec<PrivateKeySigner>) -> Self {
        let mut slots = vec![EoaSlot::new(primary)];
        let primary_addr = slots[0].address;
        for s in extras {
            if s.address() != primary_addr && !slots.iter().any(|x| x.address == s.address()) {
                slots.push(EoaSlot::new(s));
            }
        }
        Self {
            slots,
            rr_cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn extra_addresses(&self) -> Vec<Address> {
        self.slots.iter().skip(1).map(|s| s.address).collect()
    }

    pub fn max_balance_wei(&self) -> U256 {
        self.slots
            .iter()
            .map(|s| s.balance())
            .max()
            .unwrap_or(U256::ZERO)
    }

    /// Pick an EOA for `block` while tracking multi-block bundle horizons (`claim_until_block`).
    /// Prefers idle worker EOAs (1..N in round-robin) when `prefer_primary == false`, falling back
    /// to any funded EOA not yet used on `block` so non-overlapping bundles stay in flight together.
    pub fn pick_for_window(
        &self,
        block: u64,
        claim_until_block: u64,
        min_balance_wei: U256,
        prefer_primary: bool,
    ) -> Option<(usize, PrivateKeySigner, u64, U256)> {
        let n = self.slots.len();
        if n == 0 {
            return None;
        }
        let mut order = Vec::with_capacity(n);
        if prefer_primary || n == 1 {
            order.extend(0..n);
        } else {
            let workers = n - 1;
            let start = self.rr_cursor.fetch_add(1, Ordering::Relaxed) % workers;
            for k in 0..workers {
                order.push(1 + ((start + k) % workers));
            }
            order.push(0);
        }

        // Pass 1: idle EOA whose prior bundle horizon has already expired or cleared.
        for &idx in &order {
            let slot = &self.slots[idx];
            let bal = slot.balance();
            if bal < min_balance_wei {
                continue;
            }
            if slot.busy_until_block.load(Ordering::Acquire) >= block {
                continue;
            }
            let prev = slot.last_submitted_block.load(Ordering::Acquire);
            if prev < block
                && slot
                    .last_submitted_block
                    .compare_exchange(prev, block, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                slot.busy_until_block
                    .store(claim_until_block, Ordering::Release);
                let nonce = slot.nonce.load(Ordering::Acquire);
                return Some((idx, slot.signer.clone(), nonce, bal));
            }
        }

        // Pass 2: fallback if all funded EOAs hold an in-flight bundle from an earlier block.
        for &idx in &order {
            let slot = &self.slots[idx];
            let bal = slot.balance();
            if bal < min_balance_wei {
                continue;
            }
            let prev = slot.last_submitted_block.load(Ordering::Acquire);
            if prev < block
                && slot
                    .last_submitted_block
                    .compare_exchange(prev, block, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                slot.busy_until_block
                    .store(claim_until_block, Ordering::Release);
                let nonce = slot.nonce.load(Ordering::Acquire);
                return Some((idx, slot.signer.clone(), nonce, bal));
            }
        }
        None
    }

    /// Return a funded EOA and its current nonce without claiming/locking `last_submitted_block`
    /// or `busy_until_block`, allowing concurrent MEV-Share bundles on the same `(EOA, nonce)`.
    pub fn peek_for_flashbots_backrun(
        &self,
        min_balance_wei: U256,
        prefer_primary: bool,
    ) -> Option<(usize, PrivateKeySigner, u64, U256)> {
        let n = self.slots.len();
        if n == 0 {
            return None;
        }
        let mut order = Vec::with_capacity(n);
        if prefer_primary || n == 1 {
            order.extend(0..n);
        } else {
            let workers = n - 1;
            let start = self.rr_cursor.fetch_add(1, Ordering::Relaxed) % workers;
            for k in 0..workers {
                order.push(1 + ((start + k) % workers));
            }
            order.push(0);
        }
        for &idx in &order {
            let slot = &self.slots[idx];
            let bal = slot.balance();
            if bal >= min_balance_wei {
                let nonce = slot.nonce.load(Ordering::Acquire);
                return Some((idx, slot.signer.clone(), nonce, bal));
            }
        }
        None
    }

    #[allow(dead_code)]
    pub fn pick_for_block(
        &self,
        block: u64,
        min_balance_wei: U256,
        prefer_primary: bool,
    ) -> Option<(usize, PrivateKeySigner, u64, U256)> {
        self.pick_for_window(block, block, min_balance_wei, prefer_primary)
    }

    pub fn clear_busy(&self, addr: Address) {
        if let Some(slot) = self.slots.iter().find(|s| s.address == addr) {
            slot.busy_until_block.store(0, Ordering::Release);
        }
    }

    pub fn advance_nonce_and_clear_busy(&self, addr: Address, min_next_nonce: u64) {
        if let Some(slot) = self.slots.iter().find(|s| s.address == addr) {
            slot.nonce.fetch_max(min_next_nonce, Ordering::AcqRel);
            slot.busy_until_block.store(0, Ordering::Release);
        }
    }

    pub fn release_block_claim(&self, addr: Address, block: u64) {
        if let Some(slot) = self.slots.iter().find(|s| s.address == addr) {
            if slot
                .last_submitted_block
                .compare_exchange(
                    block,
                    block.saturating_sub(1),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                slot.busy_until_block.store(0, Ordering::Release);
            }
        }
    }

    pub async fn refresh_all(&self, rpc: &RpcPool, multicall3: Address) {
        let mut calls = Vec::with_capacity(self.slots.len());
        for s in &self.slots {
            calls.push(IMulticall3::Call3 {
                target: multicall3,
                allowFailure: true,
                callData: IMulticall3::getEthBalanceCall { addr: s.address }
                    .abi_encode()
                    .into(),
            });
        }
        if let Ok(res) = rpc.multicall(calls).await {
            for (s, r) in self.slots.iter().zip(res.iter()) {
                if r.success && r.returnData.len() >= 32 {
                    if let Ok(bal) = IMulticall3::getEthBalanceCall::abi_decode_returns(&r.returnData) {
                        s.set_balance(bal);
                    }
                }
            }
        }
        let futs = self.slots.iter().map(|s| async move {
            if let Ok(n) = rpc.get_transaction_count(s.address).await {
                s.nonce.store(n, Ordering::Release);
            }
        });
        futures_util::future::join_all(futs).await;
    }

    pub async fn refresh_single(&self, rpc: &RpcPool, addr: Address, fallback_nonce: Option<u64>) {
        if let Some(slot) = self.slots.iter().find(|s| s.address == addr) {
            let prev_nonce = slot.nonce.load(Ordering::Acquire);
            match rpc.get_transaction_count(addr).await {
                Ok(n) => {
                    slot.nonce.store(n, Ordering::Release);
                    if n > prev_nonce {
                        slot.busy_until_block.store(0, Ordering::Release);
                    }
                }
                Err(_) => {
                    if let Some(n) = fallback_nonce {
                        slot.nonce.store(n, Ordering::Release);
                        if n > prev_nonce {
                            slot.busy_until_block.store(0, Ordering::Release);
                        }
                    }
                }
            }
            if let Ok(b) = rpc.get_balance(addr).await {
                slot.set_balance(b);
            }
        }
    }
}

pub fn load_flashbots_auth_signer(fallback: &PrivateKeySigner) -> PrivateKeySigner {
    std::env::var("FLASHBOTS_AUTH_KEY")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<PrivateKeySigner>().ok())
        .unwrap_or_else(|| fallback.clone())
}

pub fn load_extra_signers_from_env() -> Vec<PrivateKeySigner> {
    let raw = std::env::var("EXTRA_PRIVATE_KEYS").unwrap_or_default();
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<PrivateKeySigner>().ok())
        .collect()
}

pub async fn sync_executor_callers(
    rpc: &RpcPool,
    market: &MarketState,
    owner: &PrivateKeySigner,
    exec_addr: Address,
    extra: &[Address],
) -> Result<()> {
    if extra.is_empty() {
        return Ok(());
    }
    let calls: Vec<IMulticall3::Call3> = extra
        .iter()
        .map(|&addr| IMulticall3::Call3 {
            target: exec_addr,
            allowFailure: true,
            callData: IDagArbExecutor::callersCall { addr }.abi_encode().into(),
        })
        .collect();
    let res = rpc.multicall(calls).await?;
    let mut missing = Vec::new();
    for (&addr, r) in extra.iter().zip(res.iter()) {
        let allowed = r.success
            && r.returnData.len() >= 32
            && IDagArbExecutor::callersCall::abi_decode_returns(&r.returnData).unwrap_or(false);
        if !allowed {
            missing.push(addr);
        }
    }
    if missing.is_empty() {
        tracing::info!(count = extra.len(), "all worker EOAs already authorized on executor");
        return Ok(());
    }

    let nonce = rpc.get_transaction_count(owner.address()).await?;
    let data: Bytes = IDagArbExecutor::setCallersCall {
        addrs: missing.clone(),
        allowed: true,
    }
    .abi_encode()
    .into();
    let gas_limit = 60_000u64 + (missing.len() as u64) * 28_000;
    let raw_hex = sign_tx(
        market,
        owner,
        nonce,
        TxKind::Call(exec_addr),
        U256::ZERO,
        data,
        gas_limit,
        None,
    )?;
    let tx_hash = rpc.broadcast_raw_tx(&raw_hex, None).await?;
    tracing::info!(%tx_hash, count = missing.len(), "authorizing worker EOAs on DagArbExecutor");
    let _ = rpc.wait_receipt(tx_hash, 15_000).await?;
    Ok(())
}

pub fn upsert_env_var(env_path: &str, key: &str, value: &str) -> Result<()> {
    let mut lines: Vec<String> = if Path::new(env_path).exists() {
        std::fs::read_to_string(env_path)?
            .lines()
            .map(|l| l.to_string())
            .collect()
    } else if Path::new(".env.example").exists() {
        std::fs::read_to_string(".env.example")?
            .lines()
            .map(|l| l.to_string())
            .collect()
    } else {
        Vec::new()
    };

    let prefix = format!("{key}=");
    let mut replaced = false;
    for line in &mut lines {
        if line.starts_with(&prefix) {
            *line = format!("{key}={value}");
            replaced = true;
            break;
        }
    }
    if !replaced {
        lines.push(format!("{key}={value}"));
    }

    let content = lines.join("\n") + "\n";
    std::fs::write(env_path, content)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(env_path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub async fn deploy_executor(
    rpc: &RpcPool,
    market: &MarketState,
    contract: &CompiledContract,
    signer: &PrivateKeySigner,
) -> Result<Address> {
    let owner = signer.address();
    let mut init_code = contract.creation_bytecode.clone();
    let encoded_arg = owner.abi_encode();
    init_code.extend_from_slice(&encoded_arg);

    let nonce = rpc.get_transaction_count(owner).await?;
    let gas_limit = rpc
        .estimate_gas(owner, None, &init_code)
        .await
        .map(|g| (g * 12) / 10)
        .unwrap_or(4_200_000);

    let raw_hex = sign_tx(
        market,
        signer,
        nonce,
        TxKind::Create,
        U256::ZERO,
        Bytes::from(init_code),
        gas_limit,
        None,
    )?;

    let tx_hash = rpc.broadcast_raw_tx(&raw_hex, None).await?;
    tracing::info!(%tx_hash, "deploying DagArbExecutor...");

    let deploy_wait_ms = if market.chain.block_time_ms >= 4_000 {
        90_000
    } else {
        45_000
    };
    let receipt = rpc
        .wait_receipt(tx_hash, deploy_wait_ms)
        .await?
        .ok_or_else(|| eyre!("deployment receipt timeout for {tx_hash}"))?;

    if receipt.status.as_deref() != Some("0x1") {
        return Err(eyre!("deployment tx reverted: {tx_hash}"));
    }

    let deployed = owner.create(nonce);
    // Verify owner() on-chain
    let check_call = IDagArbExecutor::ownerCall {}.abi_encode();
    let raw_owner = rpc
        .eth_call(None, deployed, &check_call, "latest", None)
        .await?;
    let onchain_owner = IDagArbExecutor::ownerCall::abi_decode_returns(&raw_owner)?;
    if onchain_owner != owner {
        return Err(eyre!("deployed owner mismatch: {onchain_owner} != {owner}"));
    }

    Ok(deployed)
}

#[allow(clippy::too_many_arguments)]
fn build_bep322_bundle_txs(
    rpc: &RpcPool,
    market: &MarketState,
    signer: &PrivateKeySigner,
    executor: Address,
    sim: &SimulationResult,
    _zero_gas_spec: Option<BscTxSpec>,
    paid_gas_spec: Option<BscTxSpec>,
    nonce: u64,
    prefix_tx: Option<&str>,
) -> Result<(B256, Vec<B256>, Vec<(String, Vec<String>)>)> {
    let arb_gas_limit = (((sim.gas_used as f64) * 1.35).ceil() as u64 + 35_000).max(280_000);
    let urls = rpc.bundle_urls();
    let mut signed_cache: HashMap<(Address, BscTxSpec), (String, B256)> = HashMap::new();
    let mut per_builder = Vec::with_capacity(urls.len());
    let mut candidate_hashes = Vec::new();
    let mut primary_hash: Option<B256> = None;

    for url in urls {
        let Some(builder) = market
            .chain
            .submission
            .builders
            .iter()
            .find(|b| b.url == *url)
        else {
            continue;
        };
        let Some(recipient) = builder.bribe_recipient else {
            continue;
        };
        let Some(spec) = paid_gas_spec else {
            continue;
        };
        let (raw_hex, tx_hash) = if let Some(cached) = signed_cache.get(&(recipient, spec)) {
            cached.clone()
        } else {
            let calldata = sim.calldata_for_builder(spec.min_profit_raw, recipient, spec.bribe_bps);
            let raw_hex = sign_tx(
                market,
                signer,
                nonce,
                TxKind::Call(executor),
                U256::from(spec.msg_value_wei),
                calldata,
                arb_gas_limit,
                Some(spec.exec_gp),
            )?;
            let raw_bytes = hex::decode(raw_hex.strip_prefix("0x").unwrap_or(&raw_hex))?;
            let h = keccak256(&raw_bytes);
            signed_cache.insert((recipient, spec), (raw_hex.clone(), h));
            (raw_hex, h)
        };
        if primary_hash.is_none() {
            primary_hash = Some(tx_hash);
        }
        if !candidate_hashes.contains(&tx_hash) {
            candidate_hashes.push(tx_hash);
        }
        let bundle_txs = if let Some(pre) = prefix_tx {
            vec![pre.to_string(), raw_hex]
        } else {
            vec![raw_hex]
        };
        per_builder.push((url.clone(), bundle_txs));
    }

    let p_hash = primary_hash.ok_or_else(|| eyre!("no MEV bundle URLs accepted spec"))?;
    Ok((p_hash, candidate_hashes, per_builder))
}

#[allow(clippy::too_many_arguments)]
async fn submit_flashbots_bundle_txs(
    rpc: &RpcPool,
    market: &MarketState,
    signer: &PrivateKeySigner,
    executor: Address,
    sim: &SimulationResult,
    spec: BscTxSpec,
    nonce: u64,
    max_block_num: u64,
    prefix_tx: Option<&str>,
) -> Result<(B256, Vec<B256>)> {
    let arb_gas_limit = (((sim.gas_used as f64) * 1.35).ceil() as u64 + 35_000).max(280_000);
    let calldata = sim.calldata_for_builder(spec.min_profit_raw, Address::ZERO, spec.bribe_bps);
    let raw_hex = sign_tx(
        market,
        signer,
        nonce,
        TxKind::Call(executor),
        U256::from(spec.msg_value_wei),
        calldata,
        arb_gas_limit,
        Some(spec.exec_gp),
    )?;
    let raw_bytes = hex::decode(raw_hex.strip_prefix("0x").unwrap_or(&raw_hex))?;
    let primary_hash = keccak256(&raw_bytes);
    let bundle_txs = if let Some(pre) = prefix_tx {
        vec![pre.to_string(), raw_hex]
    } else {
        vec![raw_hex]
    };
    let next_bn = market.last_block.saturating_add(1);
    let end_bn = max_block_num.max(next_bn);
    let max_targets = market.chain.timing.bundle_span_blocks.max(1) as usize;
    let target_blocks: Vec<u64> = (next_bn..=end_bn).take(max_targets).collect();
    let auth_signer = load_flashbots_auth_signer(signer);
    rpc.broadcast_eth_bundle(&bundle_txs, &target_blocks, &auth_signer, primary_hash)
        .await?;
    Ok((primary_hash, vec![primary_hash]))
}

#[allow(clippy::too_many_arguments)]
pub async fn submit_arb_bundle_nonblocking(
    rpc: &RpcPool,
    market: &MarketState,
    signer: &PrivateKeySigner,
    executor: Address,
    sim: &SimulationResult,
    zero_gas_spec: Option<BscTxSpec>,
    paid_gas_spec: Option<BscTxSpec>,
    nonce: u64,
    max_block_num: u64,
) -> Result<(B256, Vec<B256>)> {
    match market.chain.submission.mode {
        SubmissionMode::Bep322Builders => {
            let (primary_hash, candidate_hashes, per_builder) = build_bep322_bundle_txs(
                rpc,
                market,
                signer,
                executor,
                sim,
                zero_gas_spec,
                paid_gas_spec,
                nonce,
                None,
            )?;
            rpc.broadcast_bundle_per_builder(&per_builder, max_block_num, true, primary_hash)
                .await?;
            Ok((primary_hash, candidate_hashes))
        }
        SubmissionMode::FlashbotsRelay => {
            let spec = paid_gas_spec
                .or(zero_gas_spec)
                .ok_or_else(|| eyre!("missing tx spec for Flashbots bundle"))?;
            submit_flashbots_bundle_txs(
                rpc,
                market,
                signer,
                executor,
                sim,
                spec,
                nonce,
                max_block_num,
                None,
            )
            .await
        }
        SubmissionMode::PublicRpc => Err(eyre!(
            "MEV bundle submission not enabled on {}",
            market.chain.as_str()
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn submit_backrun_bundle_nonblocking(
    rpc: &RpcPool,
    market: &MarketState,
    signer: &PrivateKeySigner,
    executor: Address,
    sim: &SimulationResult,
    zero_gas_spec: Option<BscTxSpec>,
    paid_gas_spec: Option<BscTxSpec>,
    nonce: u64,
    raw_victim_tx: &str,
) -> Result<(B256, Vec<B256>)> {
    let max_blk = market
        .last_block
        .saturating_add(market.chain.timing.backrun_span_blocks);
    match market.chain.submission.mode {
        SubmissionMode::Bep322Builders => {
            let (primary_hash, candidate_hashes, per_builder) = build_bep322_bundle_txs(
                rpc,
                market,
                signer,
                executor,
                sim,
                zero_gas_spec,
                paid_gas_spec,
                nonce,
                Some(raw_victim_tx),
            )?;
            rpc.broadcast_bundle_per_builder(&per_builder, max_blk, false, primary_hash)
                .await?;
            Ok((primary_hash, candidate_hashes))
        }
        SubmissionMode::FlashbotsRelay => {
            let spec = paid_gas_spec
                .or(zero_gas_spec)
                .ok_or_else(|| eyre!("missing tx spec for Flashbots backrun bundle"))?;
            submit_flashbots_bundle_txs(
                rpc,
                market,
                signer,
                executor,
                sim,
                spec,
                nonce,
                max_blk,
                Some(raw_victim_tx),
            )
            .await
        }
        SubmissionMode::PublicRpc => Err(eyre!(
            "MEV backrun bundles not enabled on {}",
            market.chain.as_str()
        )),
    }
}

/// Sign and submit an atomic `mev_sendBundle` backrun referencing a MEV-Share `victim_hash`.
#[allow(clippy::too_many_arguments)]
pub async fn submit_mev_share_backrun_nonblocking(
    rpc: &RpcPool,
    market: &MarketState,
    signer: &PrivateKeySigner,
    executor: Address,
    sim: &SimulationResult,
    spec: BscTxSpec,
    nonce: u64,
    victim_hash: B256,
    target_block: u64,
    max_block: u64,
) -> Result<(B256, Vec<B256>, String)> {
    let arb_gas_limit = (((sim.gas_used as f64) * 1.35).ceil() as u64 + 35_000).max(280_000);
    let calldata = sim.calldata_for_builder(spec.min_profit_raw, Address::ZERO, spec.bribe_bps);
    let raw_hex = sign_tx(
        market,
        signer,
        nonce,
        TxKind::Call(executor),
        U256::from(spec.msg_value_wei),
        calldata,
        arb_gas_limit,
        Some(spec.exec_gp),
    )?;
    let raw_bytes = hex::decode(raw_hex.strip_prefix("0x").unwrap_or(&raw_hex))?;
    let primary_hash = keccak256(&raw_bytes);
    let auth_signer = load_flashbots_auth_signer(signer);
    let bundle_hash = rpc
        .send_mev_share_bundle(
            &market.chain.mev_share.relay_url,
            victim_hash,
            &raw_hex,
            target_block,
            max_block,
            &market.chain.mev_share.builders,
            &auth_signer,
            primary_hash,
        )
        .await?;
    Ok((primary_hash, vec![primary_hash], bundle_hash))
}

#[allow(clippy::too_many_arguments, dead_code)]
pub async fn send_arb_tx(
    rpc: &RpcPool,
    market: &MarketState,
    signer: &PrivateKeySigner,
    executor: Address,
    sim: &SimulationResult,
    min_profit_raw: U256,
    nonce: u64,
    bid_gas_price_wei: Option<u128>,
    bribe_bps: u16,
    msg_value_wei: u128,
) -> Result<(B256, Option<TxReceipt>)> {
    let span = market.chain.timing.bundle_span_blocks;
    let wait_ms = market.chain.timing.wait_receipt_ms;
    match market.chain.submission.mode {
        SubmissionMode::Bep322Builders => {
            let exec_gp = bid_gas_price_wei
                .unwrap_or((market.gas_price_wei * 105) / 100)
                .max(market.chain.min_gas_price_wei());
            let spec = BscTxSpec {
                exec_gp,
                min_profit_raw,
                bribe_bps,
                msg_value_wei,
            };
            let (primary_hash, _candidate_hashes, per_builder) = build_bep322_bundle_txs(
                rpc,
                market,
                signer,
                executor,
                sim,
                None,
                Some(spec),
                nonce,
                None,
            )?;
            let tx_hash = rpc
                .broadcast_bundle_per_builder(
                    &per_builder,
                    market.last_block.saturating_add(span),
                    false,
                    primary_hash,
                )
                .await?;
            let receipt = rpc.wait_receipt(tx_hash, wait_ms).await?;
            Ok((tx_hash, receipt))
        }
        SubmissionMode::FlashbotsRelay => {
            let exec_gp = bid_gas_price_wei
                .unwrap_or((market.gas_price_wei * 105) / 100)
                .max(market.chain.min_gas_price_wei());
            let spec = BscTxSpec {
                exec_gp,
                min_profit_raw,
                bribe_bps,
                msg_value_wei,
            };
            let max_blk = market.last_block.saturating_add(span);
            let (tx_hash, _) = submit_flashbots_bundle_txs(
                rpc, market, signer, executor, sim, spec, nonce, max_blk, None,
            )
            .await?;
            let receipt = rpc.wait_receipt(tx_hash, wait_ms).await?;
            Ok((tx_hash, receipt))
        }
        SubmissionMode::PublicRpc => {
            let gas_limit = (((sim.gas_used as f64) * 1.35).ceil() as u64).max(250_000);
            let calldata = sim.calldata_with_min_profit(min_profit_raw);
            let raw_hex = sign_tx(
                market,
                signer,
                nonce,
                TxKind::Call(executor),
                U256::from(msg_value_wei),
                calldata,
                gas_limit,
                bid_gas_price_wei,
            )?;
            let tx_hash = rpc.broadcast_raw_tx(&raw_hex, None).await?;
            let receipt = rpc.wait_receipt(tx_hash, wait_ms).await?;
            Ok((tx_hash, receipt))
        }
    }
}

fn sign_tx(
    market: &MarketState,
    signer: &PrivateKeySigner,
    nonce: u64,
    to: TxKind,
    value: U256,
    input: Bytes,
    gas_limit: u64,
    bid_gas_price_wei: Option<u128>,
) -> Result<String> {
    let chain_id = market.chain.chain_id;
    let gas_price = market.gas_price_wei.max(market.chain.min_gas_price_wei());

    let mut encoded = Vec::new();
    match market.chain.gas.tx_type {
        TxType::Legacy => {
            let bid_gp = match bid_gas_price_wei {
                Some(explicit) => explicit,
                None => ((gas_price * 110) / 100).max(market.chain.min_gas_price_wei()),
            };
            let tx = TxLegacy {
                chain_id: Some(chain_id),
                nonce,
                gas_price: bid_gp,
                gas_limit,
                to,
                value,
                input,
            };
            let sig = signer.sign_hash_sync(&tx.signature_hash())?;
            tx.into_signed(sig).encode_2718(&mut encoded);
        }
        TxType::Eip1559 => {
            let (max_fee, priority) = match bid_gas_price_wei {
                Some(gp) => match market.chain.bidding.mode {
                    BiddingMode::CoinbaseBribe => {
                        let prio = market.chain.gas.bundle_priority_fee_wei;
                        let mult = market.chain.gas.max_fee_multiplier_bps;
                        let mf = ((gp * mult) / 10_000).max(gp).saturating_add(prio);
                        (mf, prio)
                    }
                    BiddingMode::PriorityFee => {
                        let prio = gp;
                        let mult = market.chain.gas.max_fee_multiplier_bps;
                        let mf = ((gas_price * mult) / 10_000).saturating_add(prio);
                        (mf, prio)
                    }
                },
                None => {
                    let prio = (gas_price / 5).max(market.chain.min_gas_price_wei());
                    let mf = ((gas_price * 140) / 100).saturating_add(prio);
                    (mf, prio)
                }
            };
            let tx = TxEip1559 {
                chain_id,
                nonce,
                gas_limit,
                max_fee_per_gas: max_fee,
                max_priority_fee_per_gas: priority,
                to,
                value,
                access_list: Default::default(),
                input,
            };
            let sig = signer.sign_hash_sync(&tx.signature_hash())?;
            tx.into_signed(sig).encode_2718(&mut encoded);
        }
    }

    Ok(format!("0x{}", hex::encode(encoded)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_chain_config;

    #[test]
    fn test_bsc_bundle_payment_eoas_and_nonce_consistency() {
        let cfg = load_chain_config("bsc").unwrap();
        let market = MarketState::new(cfg.clone());
        let rpc = RpcPool::from_config(&cfg).unwrap();
        let signer = PrivateKeySigner::random();
        let executor = Address::repeat_byte(0x11);
        let wbnb = cfg.wrapped_native;
        let params = crate::abi::ArbParams {
            flashMode: 1,
            flashSource: Address::repeat_byte(0x22),
            baseToken: wbnb,
            borrowAmount: U256::from(100_000_000_000_000_000u128),
            minNetProfit: U256::from(10_000_000_000_000u128),
            payoutToOwner: true,
            wrappedNative: wbnb,
            bribeRecipient: Address::ZERO,
            bribeBps: 0,
            steps: Vec::new(),
        };
        let sim = SimulationResult {
            executor_addr: executor,
            calldata: Bytes::from_static(&[0xaa, 0xbb]),
            params,
            net_profit_raw: U256::from(1_000_000_000_000_000u128),
            net_profit_human: 0.001,
            gross_profit_usd: 0.60,
            gas_used: 320_000,
            l1_fee_wei: 0,
            gas_cost_usd: 0.01,
            net_profit_usd: 0.59,
        };

        let paid_spec = BscTxSpec {
            exec_gp: 60_000_000,
            min_profit_raw: U256::from(20_000_000_000_000u128),
            bribe_bps: 9600,
            msg_value_wei: 0,
        };
        let (_, hashes, bundles) = build_bep322_bundle_txs(
            &rpc,
            &market,
            &signer,
            executor,
            &sim,
            None,
            Some(paid_spec),
            7,
            None,
        )
        .unwrap();

        assert_eq!(bundles.len(), 4);
        assert_eq!(hashes.len(), 3);
        let mut br_paid_raws = Vec::new();
        let mut c48_seen = false;
        let mut nr_seen = false;
        for (url, txs) in &bundles {
            assert_eq!(txs.len(), 1, "expected 1-tx bundle for {url}");
            let raw = txs[0].to_ascii_lowercase();
            assert!(
                raw.contains("07"),
                "signed tx should embed consistent nonce 7"
            );
            if url.contains("48.club") {
                c48_seen = true;
                assert!(raw.contains("4848489f0b2bedd788c696e2d79b6b69d7484848"));
            } else if url.contains("nodereal.io") {
                nr_seen = true;
                assert!(raw.contains("fffffffffffffffffffffffffffffffffffffffe"));
            } else {
                assert!(raw.contains("1266c6be60392a8ff346e8d5eccd3e69dd9c5f20"));
                br_paid_raws.push(txs[0].as_str());
            }
        }
        assert!(c48_seen);
        assert!(nr_seen);
        assert_eq!(br_paid_raws.len(), 2);
        assert_eq!(br_paid_raws[0], br_paid_raws[1]);
        let br_bytes = hex::decode(br_paid_raws[0].strip_prefix("0x").unwrap()).unwrap();
        let decoded_br =
            <alloy::consensus::Signed<TxLegacy> as alloy::eips::eip2718::Decodable2718>::decode_2718(
                &mut br_bytes.as_slice(),
            )
            .unwrap();
        assert_eq!(decoded_br.tx().gas_price, 60_000_000);
    }

    #[test]
    fn test_eth_eip1559_bundle_signing_invariants() {
        let cfg = load_chain_config("eth").unwrap();
        let market = MarketState::new(cfg.clone());
        let signer = PrivateKeySigner::random();
        let executor = Address::repeat_byte(0x33);
        let raw_hex = sign_tx(
            &market,
            &signer,
            3,
            TxKind::Call(executor),
            U256::ZERO,
            Bytes::from_static(&[0x12, 0x34]),
            300_000,
            Some(120_000_000),
        )
        .unwrap();
        let raw_bytes = hex::decode(raw_hex.strip_prefix("0x").unwrap()).unwrap();
        let decoded =
            <alloy::consensus::Signed<TxEip1559> as alloy::eips::eip2718::Decodable2718>::decode_2718(
                &mut raw_bytes.as_slice(),
            )
            .unwrap();
        assert_eq!(decoded.tx().chain_id, 1);
        assert_eq!(decoded.tx().max_priority_fee_per_gas, 1_000_000);
        assert_eq!(decoded.tx().max_fee_per_gas, 163_000_000);
    }

    #[test]
    fn test_eoa_pool_window_rotation_avoids_busy_workers() {
        let primary = PrivateKeySigner::random();
        let w1 = PrivateKeySigner::random();
        let w2 = PrivateKeySigner::random();
        let w3 = PrivateKeySigner::random();
        let pool = EoaPool::new(primary, vec![w1, w2, w3]);
        let bal = U256::from(2_500_000_000_000_000u128);
        for s in &pool.slots {
            s.set_balance(bal);
        }
        let min_need = U256::from(50_000_000_000_000u128);

        let p1 = pool.pick_for_window(100, 110, min_need, false).unwrap();
        let p2 = pool.pick_for_window(101, 111, min_need, false).unwrap();
        let p3 = pool.pick_for_window(102, 112, min_need, false).unwrap();
        assert_ne!(p1.0, p2.0);
        assert_ne!(p2.0, p3.0);
        assert_ne!(p1.0, p3.0);
        assert!(p1.0 >= 1 && p2.0 >= 1 && p3.0 >= 1);

        // Once all workers are busy through block 110+, Pass 1 still picks Primary (idle) cleanly.
        let p4 = pool.pick_for_window(103, 113, min_need, false).unwrap();
        assert_eq!(p4.0, 0);

        // Clearing busy releases that worker cleanly on subsequent blocks.
        pool.clear_busy(p1.1.address());
        let p5 = pool.pick_for_window(104, 114, min_need, false).unwrap();
        assert_eq!(p5.0, p1.0);
    }
}
