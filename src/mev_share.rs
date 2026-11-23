use alloy::primitives::{Address, Bytes, B256};
use reqwest::Client;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::discovery::{self, MarketState};
use crate::rpc::ReceiptLog;

const DEFAULT_SSE_DEDUP_MS: u64 = 2_500;
const SSE_MAX_LINE_BYTES: usize = 1024 * 1024;
const SSE_CHUNK_TIMEOUT_SECS: u64 = 30;
const SSE_STATS_INTERVAL_SECS: u64 = 60;

#[derive(Debug, Clone, Deserialize)]
pub struct MevShareLog {
    pub address: Address,
    #[serde(default)]
    pub topics: Option<Vec<B256>>,
    #[serde(default)]
    pub data: Option<Bytes>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MevShareEvent {
    pub hash: B256,
    #[serde(default)]
    pub logs: Option<Vec<MevShareLog>>,
}

impl MevShareEvent {
    /// Convert optional MEV-Share hint logs into normalized `ReceiptLog` items.
    pub fn to_receipt_logs(&self) -> Vec<ReceiptLog> {
        let Some(logs) = &self.logs else {
            return Vec::new();
        };
        logs.iter()
            .map(|lg| ReceiptLog {
                address: lg.address,
                topics: lg.topics.clone().unwrap_or_default(),
                data: lg.data.clone().unwrap_or_default(),
            })
            .collect()
    }

    fn has_dex_swap_topic(&self) -> bool {
        let Some(logs) = &self.logs else {
            return false;
        };
        logs.iter().any(|lg| {
            lg.topics
                .as_ref()
                .and_then(|t| t.first())
                .copied()
                .is_some_and(discovery::is_mev_share_dex_topic)
        })
    }
}

/// Parse a single SSE `data:` line from Flashbots MEV-Share into a typed event.
pub fn parse_sse_data_line(line: &str) -> Option<MevShareEvent> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return None;
    }
    let payload = trimmed.strip_prefix("data:")?.trim();
    if payload.is_empty() || payload.starts_with(':') {
        return None;
    }
    serde_json::from_str::<MevShareEvent>(payload).ok()
}

/// Spawn a resilient background task subscribing to Flashbots MEV-Share SSE hints
/// and forwarding DEX-relevant events onto a bounded channel.
pub fn spawn_mev_share_sse_watcher(sse_url: String) -> mpsc::Receiver<MevShareEvent> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        // Long-lived SSE stream intentionally omits global request `.timeout(...)`;
        // stalled reads are caught per chunk via `tokio::time::timeout`.
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .tcp_keepalive(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| Client::new());

        let mut backoff_ms = 500u64;
        let mut recent_hashes: HashMap<B256, Instant> = HashMap::new();
        let mut stats_at = Instant::now();
        let mut stats_events = 0u64;
        let mut stats_dex_events = 0u64;
        let mut stats_forwarded = 0u64;
        let mut stats_parse_errors = 0u64;
        let mut line_buf: Vec<u8> = Vec::with_capacity(16 * 1024);

        loop {
            if tx.is_closed() {
                break;
            }
            line_buf.clear();

            match client
                .get(&sse_url)
                .header("Accept", "text/event-stream")
                .header("User-Agent", "pyrite/0.1")
                .send()
                .await
            {
                Ok(mut resp) if resp.status().is_success() => {
                    tracing::info!(sse_url = %sse_url, status = %resp.status(), "MEV-Share SSE connected");
                    backoff_ms = 500;
                    let dedup_window = Duration::from_millis(DEFAULT_SSE_DEDUP_MS);
                    let chunk_timeout = Duration::from_secs(SSE_CHUNK_TIMEOUT_SECS);

                    loop {
                        if tx.is_closed() {
                            return;
                        }
                        let next_res = tokio::time::timeout(chunk_timeout, resp.chunk()).await;
                        let chunk = match next_res {
                            Ok(Ok(Some(bytes))) => bytes,
                            Ok(Err(e)) => {
                                tracing::warn!(sse_url = %sse_url, err = %e, "MEV-Share SSE stream read error; reconnecting");
                                break;
                            }
                            Ok(Ok(None)) => {
                                tracing::warn!(sse_url = %sse_url, "MEV-Share SSE stream EOF; reconnecting");
                                break;
                            }
                            Err(_) => {
                                tracing::warn!(sse_url = %sse_url, timeout_secs = SSE_CHUNK_TIMEOUT_SECS, "MEV-Share SSE chunk timeout; reconnecting");
                                break;
                            }
                        };

                        if line_buf.len() + chunk.len() > SSE_MAX_LINE_BYTES {
                            line_buf.clear();
                        }
                        line_buf.extend_from_slice(&chunk);

                        while let Some(pos) = line_buf.iter().position(|&b| b == b'\n') {
                            let raw_line: Vec<u8> = line_buf.drain(..=pos).collect();
                            let line_bytes = raw_line
                                .strip_suffix(b"\n")
                                .unwrap_or(&raw_line);
                            let line_bytes = line_bytes
                                .strip_suffix(b"\r")
                                .unwrap_or(line_bytes);
                            let line = std::str::from_utf8(line_bytes).unwrap_or("");
                            let trimmed = line.trim();
                            if trimmed.is_empty() || trimmed.starts_with(':') {
                                continue;
                            }
                            if trimmed.starts_with("data:") {
                                match parse_sse_data_line(trimmed) {
                                    Some(ev) => {
                                        stats_events += 1;
                                        if ev.has_dex_swap_topic() {
                                            stats_dex_events += 1;
                                            let now = Instant::now();
                                            if recent_hashes.len() > 2048 {
                                                recent_hashes
                                                    .retain(|_, seen_at| now.saturating_duration_since(*seen_at) < dedup_window);
                                            }
                                            let is_dup = recent_hashes
                                                .get(&ev.hash)
                                                .is_some_and(|seen_at| now.saturating_duration_since(*seen_at) < dedup_window);
                                            if !is_dup {
                                                recent_hashes.insert(ev.hash, now);
                                                if tx.try_send(ev).is_ok() {
                                                    stats_forwarded += 1;
                                                }
                                            }
                                        }
                                    }
                                    None => {
                                        let payload = trimmed.strip_prefix("data:").unwrap_or("").trim();
                                        if !payload.is_empty() && !payload.starts_with(':') {
                                            stats_parse_errors += 1;
                                        }
                                    }
                                }
                            }
                        }

                        if stats_at.elapsed() >= Duration::from_secs(SSE_STATS_INTERVAL_SECS) {
                            tracing::info!(
                                events = stats_events,
                                dex_events = stats_dex_events,
                                forwarded = stats_forwarded,
                                parse_errors = stats_parse_errors,
                                "MEV-Share SSE stream stats"
                            );
                            stats_at = Instant::now();
                            stats_events = 0;
                            stats_dex_events = 0;
                            stats_forwarded = 0;
                            stats_parse_errors = 0;
                        }
                    }
                }
                Ok(resp) => {
                    tracing::warn!(
                        sse_url = %sse_url,
                        status = %resp.status(),
                        backoff_ms,
                        "MEV-Share SSE unexpected HTTP status; retrying"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        sse_url = %sse_url,
                        err = %e,
                        backoff_ms,
                        "MEV-Share SSE connect failed; retrying"
                    );
                }
            }

            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(5_000);
        }
    });
    rx
}

/// Apply pre-simulated MEV-Share `logs` hints onto a cloned shadow `MarketState`
/// when at least one known routable pool is touched and swap volume meets `min_swap_usd`.
pub fn evaluate_and_project_mev_share(
    market: &MarketState,
    ev: &MevShareEvent,
    min_swap_usd: f64,
    target_block: u64,
) -> Option<(MarketState, HashSet<Address>, f64)> {
    let receipt_logs = ev.to_receipt_logs();
    if receipt_logs.is_empty() || !market.has_known_routable_pool_for_logs(&receipt_logs) {
        return None;
    }

    let mut shadow = market.clone();
    let delta = shadow.apply_projected_logs(&receipt_logs, target_block);
    let swap_usd = delta.max_swap_usd.max(delta.total_swap_usd);
    if delta.dirty_pools.is_empty() || !swap_usd.is_finite() || swap_usd < min_swap_usd {
        return None;
    }
    Some((shadow, delta.dirty_pools, swap_usd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::PoolState;
    use crate::config::{load_chain_config, DexKind};
    use crate::router;
    use alloy::primitives::{b256, I256, U256};

    #[test]
    fn test_parse_real_mev_share_sse_payload() {
        let ping = ":ping";
        assert!(parse_sse_data_line(ping).is_none());

        let minimal = r#"data: {"hash":"0x1111111111111111111111111111111111111111111111111111111111111111","logs":null,"txs":null}"#;
        let ev_min = parse_sse_data_line(minimal).expect("minimal event should parse");
        assert_eq!(ev_min.hash, B256::repeat_byte(0x11));
        assert!(ev_min.to_receipt_logs().is_empty());

        let captured = r#"data: {"hash":"0xa6a58a24b740437622be39699546c36a207d31544f3a3f6b0cf6c26a1d8d70d4","logs":[{"address":"0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2","topics":["0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"],"data":"0x"},{"address":"0xe0554a476a092703abdb3ef35c80e0d76d32939f","topics":["0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67","0x00000000000000000000000028267ea1f7f194c8e48e60d1438f37f23cb55a7d","0x000000000000000000000000b5f650e96ed10184e1be22390fddb0a0c0f024d0"],"data":"0xffffffffffffffffffffffffffffffffffffffffffffffffffffffff7c80411200000000000000000000000000000000000000000000000011e5bb8877b66ca30000000000000000000000000000000000005e736f3b895feea48c43d3752ae100000000000000000000000000000000000000000000000002f5f32e272ee7730000000000000000000000000000000000000000000000000000000000031493"}],"txs":[{"to":"0xb5f650e96ed10184e1be22390fddb0a0c0f024d0","functionSelector":"0x62402c0e","callData":"0x62402c0e"}]}"#;
        let ev = parse_sse_data_line(captured).expect("captured event should parse");
        assert!(ev.has_dex_swap_topic());
        let rlogs = ev.to_receipt_logs();
        assert_eq!(rlogs.len(), 2);
        assert_eq!(rlogs[1].data.len(), 160);
    }

    fn encode_i256_word(val: I256) -> [u8; 32] {
        val.to_be_bytes::<32>()
    }

    fn encode_u256_word(val: U256) -> [u8; 32] {
        val.to_be_bytes::<32>()
    }

    #[test]
    fn test_project_mev_share_v3_and_v2_logs_creates_backrun() {
        let mut cfg = load_chain_config("eth").unwrap();
        cfg.default_native_usd = 3000.0;
        let weth = cfg.wrapped_native;
        let usdc = cfg.usd_anchor; // 6 decimals, token0 (< WETH)
        let mut market = MarketState::new(cfg);
        market.native_usd = 3000.0;

        let p_v2 = Address::repeat_byte(0x11);
        let p_v3 = Address::repeat_byte(0x22);

        // Both pools initially at 1 WETH = 3000 USDC
        // token0 = USDC (6 dec), token1 = WETH (18 dec)
        market.insert_pool(PoolState {
            address: p_v2,
            factory_name: "UniV2".into(),
            dex_kind: DexKind::UniV2,
            token0: usdc,
            token1: weth,
            dec0: 6,
            dec1: 18,
            fee_num: 9970,
            fee_ppm: 3000,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::from(300_000u128) * U256::from(10u128.pow(6)),
            r1_raw: U256::from(100u128) * U256::from(10u128.pow(18)),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 600_000.0,
            last_updated_block: 100,
            v4_key: None,
            metric: None,
            balancer: None,
        });

        // sqrtPriceX96 = sqrt(1e18 / 3000e6) * 2^96
        let sqrt_parity =
            ((1e18_f64 / 3000e6_f64).sqrt() * 79228162514264337593543950336.0) as u128;
        let tick_parity = ((1e18_f64 / 3000e6_f64).ln() / 1.0001_f64.ln()).round() as i32;
        market.insert_pool(PoolState {
            address: p_v3,
            factory_name: "UniV3".into(),
            dex_kind: DexKind::UniV3,
            token0: usdc,
            token1: weth,
            dec0: 6,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::ZERO,
            r1_raw: U256::ZERO,
            sqrt_price_x96: U256::from(sqrt_parity),
            liquidity: 5_000_000_000_000_000_000u128,
            tick: tick_parity,
            tick_spacing: 10,
            allow_multi_tick: true,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 1_000_000.0,
            last_updated_block: 100,
            v4_key: None,
            metric: None,
            balancer: None,
        });
        for (_, bal) in &mut market.flash_pools {
            *bal = U256::from(10_000_000u128) * U256::from(10u128.pow(18));
        }

        // Pre-victim: pools at parity -> no arb
        let pre_cands = router::solve_all(&market, None, 0.0001);
        assert!(
            pre_cands.is_empty(),
            "expected no arb before MEV-Share victim swap, got {:?}",
            pre_cands.iter().map(|c| &c.strategy).collect::<Vec<_>>()
        );

        // Victim buys WETH on p_v3 (pays 30,000 USDC = amount0 > 0 -> WETH becomes ~2% more expensive on p_v3)
        // sqrtPrice shifts upward by ~1.015x (~3% price move, large enough to dominate V2+V3 fees)
        let shifted_sqrt = (sqrt_parity as f64 * 1.015) as u128;
        let shifted_tick = tick_parity + 298;
        let amt0 = I256::try_from(30_000_000_000i64).unwrap(); // +30,000 USDC
        let amt1 = I256::try_from(-9_850_000_000_000_000_000i128).unwrap(); // -9.85 WETH

        let mut data = Vec::with_capacity(160);
        data.extend_from_slice(&encode_i256_word(amt0));
        data.extend_from_slice(&encode_i256_word(amt1));
        data.extend_from_slice(&encode_u256_word(U256::from(shifted_sqrt)));
        data.extend_from_slice(&encode_u256_word(U256::from(5_000_000_000_000_000_000u128)));
        data.extend_from_slice(&encode_i256_word(I256::try_from(shifted_tick).unwrap()));

        let ev = MevShareEvent {
            hash: B256::repeat_byte(0xcd),
            logs: Some(vec![MevShareLog {
                address: p_v3,
                topics: Some(vec![
                    b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67"),
                ]),
                data: Some(Bytes::from(data)),
            }]),
        };

        let (shadow, dirty, swap_usd) =
            evaluate_and_project_mev_share(&market, &ev, 50.0, 101)
                .expect("expected valid MEV-Share projection");
        assert!(dirty.contains(&p_v3));
        assert!(
            (swap_usd - 30_000.0).abs() < 1_000.0,
            "unexpected projected swap_usd: {swap_usd}"
        );

        let post_cands = router::solve_all(&shadow, Some(&dirty), 0.0001);
        assert!(
            !post_cands.is_empty(),
            "expected profitable backrun candidate after MEV-Share V3 Swap log"
        );
        assert!(post_cands[0].est_net_profit_usd > 0.0001);

        // Also verify UniV2 Sync log path
        let new_r0 = U256::from(315_000u128) * U256::from(10u128.pow(6));
        let new_r1 = U256::from(95_250_000_000_000_000_000u128);
        let mut sync_data = Vec::with_capacity(64);
        sync_data.extend_from_slice(&encode_u256_word(new_r0));
        sync_data.extend_from_slice(&encode_u256_word(new_r1));
        let ev_v2 = MevShareEvent {
            hash: B256::repeat_byte(0xef),
            logs: Some(vec![MevShareLog {
                address: p_v2,
                topics: Some(vec![
                    b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1"),
                ]),
                data: Some(Bytes::from(sync_data)),
            }]),
        };
        let (shadow_v2, dirty_v2, swap_v2_usd) =
            evaluate_and_project_mev_share(&market, &ev_v2, 50.0, 101)
                .expect("expected valid V2 Sync MEV-Share projection");
        assert!(dirty_v2.contains(&p_v2));
        assert!(swap_v2_usd > 14_000.0);
        let post_v2 = router::solve_all(&shadow_v2, Some(&dirty_v2), 0.0001);
        assert!(
            !post_v2.is_empty(),
            "expected profitable backrun candidate after MEV-Share V2 Sync log"
        );
    }
}
