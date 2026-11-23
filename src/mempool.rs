use alloy::eips::eip2718::Encodable2718;
use alloy::primitives::{Address, B256, U256};
use alloy::rpc::types::eth::Transaction as RpcTx;
use alloy::sol;
use alloy::sol_types::{SolCall, SolValue};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use crate::amm::u256_to_human;
use crate::config::{DexKind, RouterPoolFamily, WatchedRouterSpec};
use crate::discovery::MarketState;

const POOL_COOLDOWN: Duration = Duration::from_secs(2);

sol! {
    function swapExactETHForTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline);
    function swapExactETHForTokensSupportingFeeOnTransferTokens(uint256 amountOutMin, address[] path, address to, uint256 deadline);
    function swapExactTokensForETH(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
    function swapExactTokensForETHSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
    function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);
    function swapExactTokensForTokensSupportingFeeOnTransferTokens(uint256 amountIn, uint256 amountOutMin, address[] path, address to, uint256 deadline);

    struct ExactInputSingleParamsV3 {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function exactInputSingle(ExactInputSingleParamsV3 params);

    struct ExactInputSingleParamsSmart {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    #[allow(non_snake_case)]
    function exactInputSingleSmart(ExactInputSingleParamsSmart params);

    struct ExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24 fee;
        address recipient;
        uint256 amountOut;
        uint256 amountInMaximum;
        uint160 sqrtPriceLimitX96;
    }
    function exactOutputSingle(ExactOutputSingleParams params);
}

// SmartRouter exactInputSingle selector = 0x04e45aaf (same name, 7-field struct without deadline)
const SEL_EXACT_INPUT_SINGLE_SMART: [u8; 4] = [0x04, 0xe4, 0x5a, 0xaf];
// SmartRouter exactInput((bytes path, address recipient, uint256 amountIn, uint256 amountOutMinimum)) = 0xb858183f
const SEL_EXACT_INPUT_SMART: [u8; 4] = [0xb8, 0x58, 0x18, 0x3f];
// SmartRouter V2 swapExactTokensForTokens(uint256 amountIn,uint256 amountOutMin,address[] path,address to) = 0x472b43f3
const SEL_SMART_V2_SWAP: [u8; 4] = [0x47, 0x2b, 0x43, 0xf3];
// UniversalRouter execute(bytes commands, bytes[] inputs) = 0x24856bc3
const SEL_UNIVERSAL_EXEC_2: [u8; 4] = [0x24, 0x85, 0x6b, 0xc3];
// UniversalRouter execute(bytes commands, bytes[] inputs, uint256 deadline) = 0x3593564c
const SEL_UNIVERSAL_EXEC_3: [u8; 4] = [0x35, 0x93, 0x56, 0x4c];

sol! {
    struct ExactInputParamsSmart {
        bytes path;
        address recipient;
        uint256 amountIn;
        uint256 amountOutMinimum;
    }
    function exactInputSmart(ExactInputParamsSmart params);
    function swapExactTokensForTokens4(uint256 amountIn, uint256 amountOutMin, address[] path, address to);
    function execute(bytes commands, bytes[] inputs);
    function executeWithDeadline(bytes commands, bytes[] inputs, uint256 deadline);
}

#[derive(Debug, Clone)]
pub struct PendingRouterTx {
    pub hash: B256,
    pub to: Address,
    pub value: U256,
    pub input: Vec<u8>,
    pub raw_tx_hex: String,
}

#[derive(Debug, Clone, Copy)]
enum SwapAmount {
    ExactIn(U256),
    ChainedFromPrev,
    ExactOut {
        output: U256,
        max_input: U256,
        price_limit: U256,
    },
}

struct DecodedSwapHop {
    token_in: Address,
    token_out: Address,
    amount: SwapAmount,
    fee_ppm: Option<u32>,
}

/// Subscribe to `newPendingTransactions` (full tx objects) across configured WSS endpoints
/// and forward deduplicated swap router txs into a bounded channel.
pub fn spawn_ws_pending_watcher(
    ws_urls: &[String],
    watched_routers: Vec<Address>,
) -> mpsc::Receiver<PendingRouterTx> {
    let (tx, rx) = mpsc::channel::<PendingRouterTx>(128);
    if watched_routers.is_empty() {
        return rx;
    }
    let seen = Arc::new(Mutex::new((HashSet::<B256>::new(), VecDeque::<B256>::new())));

    for url in ws_urls.iter().cloned() {
        let tx_c = tx.clone();
        let seen_c = Arc::clone(&seen);
        let routers_c = watched_routers.clone();
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
                        let sub_msg = r#"{"jsonrpc":"2.0","id":2,"method":"eth_subscribe","params":["newPendingTransactions",true]}"#;
                        if write.send(Message::Text(sub_msg.into())).await.is_ok() {
                            while let Some(msg) = read.next().await {
                                match msg {
                                    Ok(Message::Text(txt)) => {
                                        if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                                            if v.get("id").and_then(|i| i.as_u64()) == Some(2) {
                                                if let Some(sub_id) =
                                                    v.get("result").and_then(|r| r.as_str())
                                                {
                                                    tracing::info!(
                                                        ws_url = %url,
                                                        sub_id,
                                                        "WS newPendingTransactions subscribed"
                                                    );
                                                }
                                                continue;
                                            }
                                            let Some(res) = v.pointer("/params/result") else {
                                                continue;
                                            };
                                            let Some(to_str) = res.get("to").and_then(|s| s.as_str()) else {
                                                continue;
                                            };
                                            let Ok(to_addr) = to_str.parse::<Address>() else {
                                                continue;
                                            };
                                            if !routers_c.contains(&to_addr) {
                                                continue;
                                            }
                                            let Ok(rpc_tx) = serde_json::from_value::<RpcTx>(res.clone()) else {
                                                continue;
                                            };
                                            let h = *rpc_tx.inner.tx_hash();
                                            {
                                                let mut guard = seen_c.lock().unwrap();
                                                if !guard.0.insert(h) {
                                                    continue;
                                                }
                                                guard.1.push_back(h);
                                                if guard.1.len() > 2048 {
                                                    if let Some(old) = guard.1.pop_front() {
                                                        guard.0.remove(&old);
                                                    }
                                                }
                                            }
                                            use alloy::consensus::Transaction as _;
                                            let val = rpc_tx.inner.value();
                                            let inp = rpc_tx.inner.input().to_vec();
                                            let raw_bytes = rpc_tx.inner.into_inner().encoded_2718();
                                            let raw_tx_hex = format!("0x{}", hex::encode(raw_bytes));
                                            let _ = tx_c.try_send(PendingRouterTx {
                                                hash: h,
                                                to: to_addr,
                                                value: val,
                                                input: inp,
                                                raw_tx_hex,
                                            });
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
                    _ => {}
                }
                tokio::time::sleep(Duration::from_millis(1500)).await;
            }
        });
    }

    rx
}

/// Decode all supported swap hops in a watched router call in execution order.
fn decode_router_swaps(
    router: &WatchedRouterSpec,
    value: U256,
    input: &[u8],
    wbnb: Address,
) -> Vec<DecodedSwapHop> {
    if input.len() < 4 {
        return Vec::new();
    }
    let sel: [u8; 4] = [input[0], input[1], input[2], input[3]];
    let args = &input[4..];
    match sel {
        swapExactETHForTokensCall::SELECTOR
        | swapExactETHForTokensSupportingFeeOnTransferTokensCall::SELECTOR => {
            if let Ok(c) = swapExactETHForTokensCall::abi_decode_raw(args) {
                return v2_path_hops(&c.path, value, false);
            }
            return Vec::new();
        }
        swapExactTokensForETHCall::SELECTOR
        | swapExactTokensForETHSupportingFeeOnTransferTokensCall::SELECTOR
        | swapExactTokensForTokensCall::SELECTOR
        | swapExactTokensForTokensSupportingFeeOnTransferTokensCall::SELECTOR => {
            if let Ok(c) = swapExactTokensForTokensCall::abi_decode_raw(args) {
                return v2_path_hops(&c.path, c.amountIn, false);
            }
            return Vec::new();
        }
        _ => {}
    }
    if router.supports_exact_output_single && sel == exactOutputSingleCall::SELECTOR {
        if let Ok(decoded) = exactOutputSingleCall::abi_decode_validate(input) {
            let p = decoded.params;
            return vec![DecodedSwapHop {
                token_in: p.tokenIn,
                token_out: p.tokenOut,
                amount: SwapAmount::ExactOut {
                    output: p.amountOut,
                    max_input: p.amountInMaximum,
                    price_limit: U256::from(p.sqrtPriceLimitX96),
                },
                fee_ppm: Some(p.fee.to::<u32>()),
            }];
        }
        return Vec::new();
    }
    if sel == exactInputSingleCall::SELECTOR {
        if let Ok(c) = exactInputSingleCall::abi_decode(input) {
            let amt = if c.params.tokenIn == wbnb && !value.is_zero() && c.params.amountIn.is_zero() {
                value
            } else {
                c.params.amountIn
            };
            if !amt.is_zero() {
                return vec![DecodedSwapHop {
                    token_in: c.params.tokenIn,
                    token_out: c.params.tokenOut,
                    amount: SwapAmount::ExactIn(amt),
                    fee_ppm: Some(c.params.fee.to::<u32>()),
                }];
            }
        }
        return Vec::new();
    }
    if sel == SEL_EXACT_INPUT_SINGLE_SMART {
        if let Ok(c) = exactInputSingleSmartCall::abi_decode_raw(args) {
            let amt = if c.params.tokenIn == wbnb && !value.is_zero() && c.params.amountIn.is_zero() {
                value
            } else {
                c.params.amountIn
            };
            if !amt.is_zero() {
                return vec![DecodedSwapHop {
                    token_in: c.params.tokenIn,
                    token_out: c.params.tokenOut,
                    amount: SwapAmount::ExactIn(amt),
                    fee_ppm: Some(c.params.fee.to::<u32>()),
                }];
            }
        }
        return Vec::new();
    }
    if sel == SEL_SMART_V2_SWAP {
        if let Ok(c) = swapExactTokensForTokens4Call::abi_decode_raw(args) {
            let amt = if !value.is_zero() && c.amountIn.is_zero() {
                value
            } else {
                c.amountIn
            };
            return v2_path_hops(&c.path, amt, false);
        }
        return Vec::new();
    }
    if sel == SEL_EXACT_INPUT_SMART {
        if let Ok(c) = exactInputSmartCall::abi_decode_raw(args) {
            return v3_bytes_path_hops(&c.params.path, c.params.amountIn, false);
        }
        return Vec::new();
    }
    if sel == SEL_UNIVERSAL_EXEC_2 || sel == SEL_UNIVERSAL_EXEC_3 {
        let decoded = if sel == SEL_UNIVERSAL_EXEC_2 {
            executeCall::abi_decode(input).ok().map(|c| (c.commands, c.inputs))
        } else {
            executeWithDeadlineCall::abi_decode_raw(args)
                .ok()
                .map(|c| (c.commands, c.inputs))
        };
        if let Some((commands, inputs)) = decoded {
            let contract_balance_sentinel = U256::from(1u64) << 255;
            let mut hops = Vec::new();
            for (cmd, inp) in commands.iter().zip(inputs.iter()) {
                let op = cmd & 0x3f;
                // 0x00 = V3_SWAP_EXACT_IN: (address recipient, uint256 amountIn, uint256 amountOutMin, bytes path, bool payerIsUser)
                if op == 0x00 {
                    if let Ok((_rcp, amt_in, _min_out, path, _payer)) =
                        <(Address, U256, U256, alloy::primitives::Bytes, bool)>::abi_decode(inp)
                    {
                        let chained = amt_in == contract_balance_sentinel && !hops.is_empty();
                        if !amt_in.is_zero() && (chained || amt_in < contract_balance_sentinel) {
                            hops.extend(v3_bytes_path_hops(&path, amt_in, chained || !hops.is_empty()));
                        }
                    }
                }
                // 0x08 = V2_SWAP_EXACT_IN: (address recipient, uint256 amountIn, uint256 amountOutMin, address[] path, bool payerIsUser)
                if op == 0x08 {
                    if let Ok((_rcp, amt_in, _min_out, path, _payer)) =
                        <(Address, U256, U256, Vec<Address>, bool)>::abi_decode(inp)
                    {
                        let chained = amt_in == contract_balance_sentinel && !hops.is_empty();
                        if !amt_in.is_zero() && (chained || amt_in < contract_balance_sentinel) {
                            hops.extend(v2_path_hops(&path, amt_in, chained || !hops.is_empty()));
                        }
                    }
                }
            }
            return hops;
        }
    }
    Vec::new()
}

#[cfg(test)]
fn decode_first_swap(
    router: &WatchedRouterSpec,
    value: U256,
    input: &[u8],
    wbnb: Address,
) -> Option<DecodedSwapHop> {
    decode_router_swaps(router, value, input, wbnb)
        .into_iter()
        .next()
}

fn v2_path_hops(path: &[Address], amount_in_raw: U256, chain_first: bool) -> Vec<DecodedSwapHop> {
    if path.len() < 2 || (!chain_first && amount_in_raw.is_zero()) {
        return Vec::new();
    }
    path.windows(2)
        .enumerate()
        .map(|(i, w)| DecodedSwapHop {
            token_in: w[0],
            token_out: w[1],
            amount: if i == 0 && !chain_first {
                SwapAmount::ExactIn(amount_in_raw)
            } else {
                SwapAmount::ChainedFromPrev
            },
            fee_ppm: None,
        })
        .collect()
}

fn v3_bytes_path_hops(path: &[u8], amount_in_raw: U256, chain_first: bool) -> Vec<DecodedSwapHop> {
    if path.len() < 43 || (!chain_first && amount_in_raw.is_zero()) {
        return Vec::new();
    }
    let mut hops = Vec::new();
    let mut off = 0usize;
    while off + 43 <= path.len() {
        let token_in = Address::from_slice(&path[off..off + 20]);
        let fee_u32 = u32::from_be_bytes([0, path[off + 20], path[off + 21], path[off + 22]]);
        let token_out = Address::from_slice(&path[off + 23..off + 43]);
        hops.push(DecodedSwapHop {
            token_in,
            token_out,
            amount: if hops.is_empty() && !chain_first {
                SwapAmount::ExactIn(amount_in_raw)
            } else {
                SwapAmount::ChainedFromPrev
            },
            fee_ppm: Some(fee_u32),
        });
        off += 23;
    }
    hops
}

fn match_hop_pool<'a>(
    market: &'a MarketState,
    hop: &DecodedSwapHop,
    prefer_uniswap: bool,
) -> Option<&'a crate::amm::PoolState> {
    market
        .token_pools
        .get(&hop.token_in)?
        .iter()
        .filter_map(|a| market.pools.get(a))
        .find(|p| {
            p.other_token(hop.token_in)
                .is_some_and(|(other, _)| other == hop.token_out)
                && match hop.fee_ppm {
                    Some(fee) if prefer_uniswap => {
                        p.dex_kind == DexKind::UniV3
                            && p.fee_ppm == fee
                            && (p.factory_name == "UniV3" || p.factory_name == "UniV3Bsc")
                    }
                    Some(fee) => {
                        p.dex_kind == DexKind::PancakeV3
                            && p.fee_ppm == fee
                            && p.factory_name == "PancakeV3"
                    }
                    None if prefer_uniswap => {
                        p.factory_name == "UniV2" && p.dex_kind == DexKind::UniV2
                    }
                    None => p.factory_name == "PancakeV2" && p.dex_kind == DexKind::UniV2,
                }
        })
}

/// Project a decoded pending router swap onto a cloned `MarketState` if swap USD >= `min_swap_usd`.
/// Returns `(shadow_market, touched_pools, swap_usd)`.
pub fn evaluate_and_project_victim(
    market: &MarketState,
    tx: &PendingRouterTx,
    min_swap_usd: f64,
    cooldowns: &mut HashMap<Address, Instant>,
) -> Option<(MarketState, HashSet<Address>, f64)> {
    let router = market
        .chain
        .mempool
        .routers
        .iter()
        .find(|r| r.address == tx.to)?;
    let hops = decode_router_swaps(router, tx.value, &tx.input, market.chain.wrapped_native);
    let first = hops.first()?;
    let prefer_uniswap = router.family == RouterPoolFamily::Uniswap;
    let token_usd = market.token_usd(first.token_in);
    let decimals = market.token_decimals(first.token_in);
    let input_cap = match first.amount {
        SwapAmount::ExactIn(input) => input,
        SwapAmount::ExactOut { max_input, .. } => max_input,
        SwapAmount::ChainedFromPrev => return None,
    };
    let input_cap_h = u256_to_human(input_cap, decimals);
    let max_swap_usd = input_cap_h * token_usd;
    if token_usd <= 0.0 || !max_swap_usd.is_finite() || max_swap_usd < min_swap_usd {
        return None;
    }

    // Check tax suitability up front so clean 0-tax non-hub tokens (absent from `token_tax_ppm`)
    // pass freely while heavy/honeypot >=10% tokens and V3 input-taxed hops are skipped.
    for hop in &hops {
        let tax_in = market.token_tax_ppm.get(&hop.token_in).copied().unwrap_or(0);
        let tax_out = market.token_tax_ppm.get(&hop.token_out).copied().unwrap_or(0);
        if tax_in >= 100_000 || tax_out >= 100_000 || (hop.fee_ppm.is_some() && tax_in >= 500) {
            return None;
        }
    }

    let now = Instant::now();
    let first_pool = match_hop_pool(market, first, prefer_uniswap)?;
    if market.ignored_pools.contains(&first_pool.address) || !market.is_pool_routable(first_pool) {
        return None;
    }

    let mut shadow = market.clone();
    let mut dirty_pools = HashSet::new();
    let mut any_fresh_pool = false;
    let mut current_amount_h = input_cap_h;
    let mut first_swap_usd = max_swap_usd;

    for (hop_idx, hop) in hops.iter().enumerate() {
        let Some(pool) = match_hop_pool(&shadow, hop, prefer_uniswap) else {
            break;
        };
        if shadow.ignored_pools.contains(&pool.address) || !shadow.is_pool_routable(pool) {
            break;
        }
        let pool_addr = pool.address;
        let on_cooldown = cooldowns.get(&pool_addr).is_some_and(|&until| now < until);
        let (hop_swap_usd, next_price, exact_in_h) = match hop.amount {
            SwapAmount::ExactIn(amt_raw) => {
                let dec_in = shadow.token_decimals(hop.token_in);
                let h = u256_to_human(amt_raw, dec_in);
                let usd = h * shadow.token_usd(hop.token_in);
                (usd, None, h)
            }
            SwapAmount::ChainedFromPrev => {
                let usd = current_amount_h * shadow.token_usd(hop.token_in);
                (usd, None, current_amount_h)
            }
            SwapAmount::ExactOut {
                output,
                max_input,
                price_limit,
            } => {
                let (required, price) =
                    pool.quote_v3_exact_output(hop.token_in == pool.token0, output, price_limit)?;
                let dec_in = shadow.token_decimals(hop.token_in);
                let usd = u256_to_human(required, dec_in) * shadow.token_usd(hop.token_in);
                if required > max_input || usd < min_swap_usd {
                    return None;
                }
                (
                    usd,
                    Some(price),
                    u256_to_human(output, shadow.token_decimals(hop.token_out)),
                )
            }
        };
        if hop_idx == 0 {
            first_swap_usd = hop_swap_usd;
            if !first_swap_usd.is_finite() || first_swap_usd < min_swap_usd {
                return None;
            }
        }
        let p0 = shadow.token_usd(pool.token0);
        let p1 = shadow.token_usd(pool.token1);
        let mut projected = pool.clone();
        let next_amount_h = if let Some(price) = next_price {
            projected.sqrt_price_x96 = price;
            projected.refresh_depth(p0, p1);
            exact_in_h
        } else {
            let out_h = projected.apply_virtual_swap(hop.token_in, exact_in_h, p0, p1);
            if out_h <= 0.0 {
                if hop_idx == 0 {
                    return None;
                }
                break;
            }
            out_h
        };
        shadow.pools.insert(pool_addr, projected);
        if !on_cooldown {
            any_fresh_pool = true;
            cooldowns.insert(pool_addr, now + POOL_COOLDOWN);
            dirty_pools.insert(pool_addr);
        }
        current_amount_h = next_amount_h;
    }

    if !any_fresh_pool || dirty_pools.is_empty() {
        return None;
    }
    Some((shadow, dirty_pools, first_swap_usd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::PoolState;
    use crate::config::load_chain_config;
    use alloy::primitives::U160;

    fn router_by_name(cfg: &crate::config::ChainConfig, name: &str) -> WatchedRouterSpec {
        cfg.mempool
            .routers
            .iter()
            .find(|r| r.name == name)
            .unwrap()
            .clone()
    }

    fn pancake_market_and_v2_swap() -> (MarketState, PendingRouterTx) {
        let cfg = load_chain_config("bsc").unwrap();
        let pancake_v2_router = router_by_name(&cfg, "PancakeV2Router").address;
        let wbnb = cfg.wrapped_native;
        let usdt = cfg.usd_anchor;
        let mut market = MarketState::new(cfg);

        // Two WBNB/USDT pools at parity; whale buys USDT with 2.0 WBNB on PancakeV2 creating backrun vs UniV3
        let p_v2 = Address::repeat_byte(0x11);
        let p_v3 = Address::repeat_byte(0x22);
        market.insert_pool(PoolState {
            address: p_v2,
            factory_name: "PancakeV2".into(),
            dex_kind: DexKind::UniV2,
            token0: usdt,
            token1: wbnb,
            dec0: 18,
            dec1: 18,
            fee_num: 9975,
            fee_ppm: 2500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::from(60_000u128) * U256::from(10u128.pow(18)),
            r1_raw: U256::from(100u128) * U256::from(10u128.pow(18)),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 120_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        });
        let sqrt_p = U256::from(((1.0_f64 / 600.0).sqrt() * 79228162514264337593543950336.0) as u128);
        market.insert_pool(PoolState {
            address: p_v3,
            factory_name: "PancakeV3".into(),
            dex_kind: DexKind::PancakeV3,
            token0: usdt,
            token1: wbnb,
            dec0: 18,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::ZERO,
            r1_raw: U256::ZERO,
            sqrt_price_x96: sqrt_p,
            liquidity: 5_000_000_000_000_000_000_000_000u128,
            tick: -63974,
            tick_spacing: 10,
            allow_multi_tick: true,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 200_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        });
        for (_, bal) in &mut market.flash_pools {
            *bal = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        let calldata = swapExactETHForTokensCall {
            amountOutMin: U256::from(1u64),
            path: vec![wbnb, usdt],
            to: Address::repeat_byte(0x99),
            deadline: U256::from(9999999999u64),
        }
        .abi_encode();

        let ptx = PendingRouterTx {
            hash: B256::repeat_byte(0xaa),
            to: pancake_v2_router,
            value: U256::from(2u128) * U256::from(10u128.pow(18)),
            input: calldata,
            raw_tx_hex: "0xdeadbeef".into(),
        };

        (market, ptx)
    }

    #[test]
    fn test_decode_pancake_v2_and_project_backrun() {
        let (market, ptx) = pancake_market_and_v2_swap();
        let (shadow, dirty, swap_usd) =
            evaluate_and_project_victim(&market, &ptx, 100.0, &mut HashMap::new())
                .expect("must project whale swap");
        assert!(swap_usd > 1000.0);
        assert!(dirty.contains(&Address::repeat_byte(0x11)));

        let cands = crate::router::solve_all(&shadow, Some(&dirty), 0.01);
        assert!(
            !cands.is_empty() && cands[0].est_net_profit_usd > 1.0,
            "expected profitable backrun after 2 BNB whale swap"
        );
    }

    #[test]
    fn cooldown_applies_to_eligible_pools_after_projection() {
        let (mut market, mut tx) = pancake_market_and_v2_swap();
        let pool = Address::repeat_byte(0x11);
        let token = market.chain.usd_anchor;
        let mut recent = HashMap::new();
        market.hub_set.remove(&token);
        // Clean tokens not in `token_tax_ppm` are eligible; heavy-tax (>=10%) tokens are skipped.
        market.token_tax_ppm.insert(token, 150_000);
        assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_none());
        market.token_tax_ppm.remove(&token);
        market.ignored_pools.insert(pool);
        assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_none());
        market.ignored_pools.remove(&pool);
        market.token_cooldown_until.insert(token, market.last_block + 1);
        assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_none());
        market.token_cooldown_until.clear();
        assert!(evaluate_and_project_victim(&market, &tx, 10_000.0, &mut recent).is_none());
        assert!(recent.is_empty());

        let start = Instant::now();
        assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_some());
        let until = recent[&pool];
        assert!(until >= start + Duration::from_secs(2));
        for i in 1..14 {
            tx.hash = B256::repeat_byte(i);
            assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_none());
        }
        assert_eq!(recent[&pool], until);
        recent.insert(pool, Instant::now());
        assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_some());
    }

    #[test]
    fn smart_exact_output_single_projects_full_fill_with_rounded_input() {
        assert_eq!(exactOutputSingleCall::SELECTOR, [0x50, 0x23, 0xb4, 0xdf]);
        let (market, v2_tx) = pancake_market_and_v2_swap();
        let pool = &market.pools[&Address::repeat_byte(0x22)];
        // Single-interval Q96 swap vectors, with amounts in raw units.
        for (token_in, token_out, output, required, next) in [
            (
                pool.token0,
                pool.token1,
                10u128.pow(18),
                600_303_090_947_565_108_791u128,
                3_234_460_344_671_650_461_435_367_179u128,
            ),
            (
                pool.token1,
                pool.token0,
                600 * 10u128.pow(18),
                1_000_505_151_579_275_463u128,
                3_234_492_036_014_283_976_036_969_074u128,
            ),
        ] {
            let mut recent = HashMap::new();
            evaluate_and_project_victim(&market, &v2_tx, 35.0, &mut recent).unwrap();
            let zero_for_one = token_in == pool.token0;
            for unavailable in [U256::ZERO, U256::from(output * 100), U256::MAX] {
                assert!(pool
                    .quote_v3_exact_output(zero_for_one, unavailable, U256::ZERO)
                    .is_none());
            }
            let mut call = exactOutputSingleCall {
                params: ExactOutputSingleParams {
                    tokenIn: token_in,
                    tokenOut: token_out,
                    fee: pool.fee_ppm.try_into().unwrap(),
                    recipient: Address::repeat_byte(0x99),
                    amountOut: U256::from(output),
                    amountInMaximum: U256::from(required - 1),
                    sqrtPriceLimitX96: U160::ZERO,
                },
            };
            let smart_router = router_by_name(&market.chain, "PancakeSmartRouter");
            let v2_router = router_by_name(&market.chain, "PancakeV2Router");
            let mut tx = PendingRouterTx {
                to: smart_router.address,
                value: U256::ZERO,
                input: call.abi_encode(),
                ..v2_tx.clone()
            };
            assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_none());
            call.params.amountInMaximum = U256::MAX;
            call.params.sqrtPriceLimitX96 = U160::from(if zero_for_one { next + 1 } else { next - 1 });
            tx.input = call.abi_encode();
            assert!(evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).is_none());
            call.params.sqrtPriceLimitX96 = U160::ZERO;
            tx.input = call.abi_encode();
            assert!(evaluate_and_project_victim(&market, &tx, 10_000.0, &mut recent).is_none());
            assert!(decode_first_swap(
                &v2_router,
                tx.value,
                &tx.input,
                market.chain.wrapped_native
            )
            .is_none());
            assert!(
                decode_first_swap(&smart_router, tx.value, &tx.input[..100], market.chain.wrapped_native).is_none()
            );

            call.params.amountInMaximum = U256::from(required);
            call.params.sqrtPriceLimitX96 = U160::from(next);
            tx.input = call.abi_encode();
            let (shadow, dirty, usd) = evaluate_and_project_victim(&market, &tx, 35.0, &mut recent).unwrap();
            assert_eq!(dirty, HashSet::from([pool.address]));
            assert_eq!(shadow.pools[&pool.address].sqrt_price_x96, U256::from(next));
            assert_eq!(
                usd,
                u256_to_human(U256::from(required), 18) * market.token_usd(token_in)
            );
            assert_ne!(pool.sqrt_price_x96, U256::from(next));
        }
    }

    #[test]
    fn test_multihop_wbnb_usdt_nonhub_and_v3_cross_tick_backrun() {
        let (mut market, base_tx) = pancake_market_and_v2_swap();
        let wbnb = market.chain.wrapped_native;
        let usdt = market.chain.usd_anchor;
        let meme = Address::repeat_byte(0x77);
        market.tokens.insert(
            meme,
            crate::amm::TokenMeta {
                address: meme,
                symbol: "MEME".into(),
                decimals: 18,
                approx_usd: 0.0,
            },
        );
        // Clean 0-tax non-hub token NOT present in `token_tax_ppm`, traded via 2-hop WBNB -> USDT -> MEME
        let pool_meme_v2 = Address::repeat_byte(0x33);
        let pool_meme_alt = Address::repeat_byte(0x44);
        market.insert_pool(PoolState {
            address: pool_meme_v2,
            factory_name: "PancakeV2".into(),
            dex_kind: DexKind::UniV2,
            token0: usdt,
            token1: meme,
            dec0: 18,
            dec1: 18,
            fee_num: 9975,
            fee_ppm: 2500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::from(10_000u128) * U256::from(10u128.pow(18)),
            r1_raw: U256::from(10_000u128) * U256::from(10u128.pow(18)),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: false,
            cached_depth_usd: 20_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        });
        market.insert_pool(PoolState {
            address: pool_meme_alt,
            factory_name: "BiSwapV2".into(),
            dex_kind: DexKind::UniV2,
            token0: usdt,
            token1: meme,
            dec0: 18,
            dec1: 18,
            fee_num: 9980,
            fee_ppm: 2000,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::from(10_000u128) * U256::from(10u128.pow(18)),
            r1_raw: U256::from(10_000u128) * U256::from(10u128.pow(18)),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: false,
            cached_depth_usd: 20_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        });
        market.refresh_token_usd_prices_passes(3);
        assert!(!market.token_tax_ppm.contains_key(&meme));

        // 2-hop victim swap WBNB -> USDT -> MEME on PancakeV2
        let calldata_2hop = swapExactETHForTokensCall {
            amountOutMin: U256::from(1u64),
            path: vec![wbnb, usdt, meme],
            to: Address::repeat_byte(0x99),
            deadline: U256::from(9999999999u64),
        }
        .abi_encode();
        let tx_2hop = PendingRouterTx {
            input: calldata_2hop,
            ..base_tx.clone()
        };
        let mut cooldowns = HashMap::new();
        // Pre-warm WBNB/USDT pool on cooldown to confirm 2-hop still projects MEME pool!
        cooldowns.insert(Address::repeat_byte(0x11), Instant::now() + POOL_COOLDOWN);
        let (shadow_2hop, dirty_2hop, usd_2hop) =
            evaluate_and_project_victim(&market, &tx_2hop, 35.0, &mut cooldowns)
                .expect("2-hop WBNB->USDT->MEME clean token must project");
        assert!(usd_2hop > 1000.0);
        assert!(dirty_2hop.contains(&pool_meme_v2));
        assert!(
            shadow_2hop.pools[&pool_meme_v2].r0_raw > market.pools[&pool_meme_v2].r0_raw,
            "second hop USDT reserve must increase on shadow state"
        );

        // (b) Cross-tick PancakeV3 swap (~$1,200 on USDT -> WBNB) must also project (not return 0.0)
        let calldata_v3 = exactInputSingleCall {
            params: ExactInputSingleParamsV3 {
                tokenIn: usdt,
                tokenOut: wbnb,
                fee: 500u32.try_into().unwrap(),
                recipient: Address::repeat_byte(0x99),
                deadline: U256::from(9999999999u64),
                amountIn: U256::from(1_200u128) * U256::from(10u128.pow(18)),
                amountOutMinimum: U256::ZERO,
                sqrtPriceLimitX96: U160::ZERO,
            },
        }
        .abi_encode();
        let v3_swap_router = router_by_name(&market.chain, "PancakeV3SwapRouter").address;
        let tx_v3 = PendingRouterTx {
            to: v3_swap_router,
            value: U256::ZERO,
            input: calldata_v3,
            ..base_tx
        };
        let mut cd2 = HashMap::new();
        let (shadow_v3, dirty_v3, usd_v3) =
            evaluate_and_project_victim(&market, &tx_v3, 35.0, &mut cd2)
                .expect("cross-tick PancakeV3 victim swap must project");
        assert!(usd_v3 >= 1_199.0);
        assert_eq!(dirty_v3, HashSet::from([Address::repeat_byte(0x22)]));
        assert!(
            shadow_v3.pools[&Address::repeat_byte(0x22)].sqrt_price_x96
                != market.pools[&Address::repeat_byte(0x22)].sqrt_price_x96
        );
    }
}
