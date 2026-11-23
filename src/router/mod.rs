pub mod cyclic;
pub mod dag;
pub mod gpu;
pub mod parallel;

use alloy::primitives::{aliases::{I24, U24}, Address, Bytes, U256};
use std::collections::{HashMap, HashSet};

use crate::abi::{ArbParams, DagStep};
use crate::amm::{human_to_u256, u256_to_human, univ2_amount_out_u256, PoolState, ResidualPool};
use crate::config::DexKind;
use crate::discovery::MarketState;

pub const MAX_DAG_STEPS: usize = 32;

/// Estimate execution gas for logical directed flows prior to DagStep compilation.
pub fn estimate_flows_gas(flows: &[DirectedFlow]) -> u64 {
    let mut gas = 65_000u64;
    for f in flows {
        gas += match f.dex_kind {
            DexKind::UniV2 => 52_000,
            DexKind::Solidly => 82_000,
            DexKind::UniV3 | DexKind::PancakeV3 => 92_000,
            DexKind::InfinityCl | DexKind::UniV4 => 125_000,
            DexKind::MetricOmm => 135_000,
            DexKind::BalancerV2 => 115_000,
        };
    }
    gas
}

/// Estimate execution gas for compiled DagStep[] accounting for V2 Token-Hopping savings.
pub fn estimate_steps_gas(steps: &[DagStep]) -> u64 {
    let mut gas = 65_000u64;
    for s in steps {
        if s.dexKind == DexKind::UniV2.as_u8() {
            gas += if s.inputInPool { 45_000 } else { 52_000 };
        } else if s.dexKind == DexKind::Solidly.as_u8() {
            gas += 82_000;
        } else if s.dexKind == DexKind::InfinityCl.as_u8() || s.dexKind == DexKind::UniV4.as_u8() {
            gas += 125_000;
        } else if s.dexKind == DexKind::MetricOmm.as_u8() {
            gas += 135_000;
        } else if s.dexKind == DexKind::BalancerV2.as_u8() {
            gas += 115_000;
        } else {
            gas += 92_000;
        }
    }
    gas
}

#[derive(Debug, Clone)]
pub struct DirectedFlow {
    pub pool: Address,
    pub label: String,
    pub token_in: Address,
    pub token_out: Address,
    pub dex_kind: DexKind,
    pub fee_num: u16,
    pub flow_in_human: f64,
    pub flow_out_human: f64,
}

#[derive(Debug, Clone)]
pub struct RouteCandidate {
    pub strategy: String,
    pub base_token: Address,
    pub borrow_human: f64,
    #[allow(dead_code)]
    pub est_gross_profit_human: f64,
    #[allow(dead_code)]
    pub est_gross_profit_usd: f64,
    pub est_net_profit_usd: f64,
    pub flash_mode: u8,
    pub flash_source: Address,
    #[allow(dead_code)]
    pub flash_fee_bps: f64,
    pub flows: Vec<DirectedFlow>,
    pub mermaid: String,
}

/// Compile directed acyclic flows into topological `DagStep[]` with relative remaining `shareBps`
/// and safe Token-Hopping (`recipient = nextPool`, `inputInPool = true`).
pub fn compile_dag_steps(
    flows: &[DirectedFlow],
    base_token: Address,
    executor_addr: Address,
) -> Option<Vec<DagStep>> {
    if flows.is_empty() {
        return None;
    }

    // Aggregate duplicate (pool, token_in, token_out) edges if any
    let mut merged: Vec<DirectedFlow> = Vec::new();
    let mut idx_map: HashMap<(Address, Address, Address), usize> = HashMap::new();
    for f in flows {
        if f.flow_in_human <= 0.0 {
            continue;
        }
        let key = (f.pool, f.token_in, f.token_out);
        if let Some(&i) = idx_map.get(&key) {
            merged[i].flow_in_human += f.flow_in_human;
            merged[i].flow_out_human += f.flow_out_human;
        } else {
            idx_map.insert(key, merged.len());
            merged.push(f.clone());
        }
    }
    if merged.is_empty() {
        return None;
    }

    // Build topological order over intermediate tokens using virtual START (outgoing from base_token)
    // and END (incoming to base_token).
    let mut inter_tokens: HashSet<Address> = HashSet::new();
    for f in &merged {
        if f.token_in != base_token {
            inter_tokens.insert(f.token_in);
        }
        if f.token_out != base_token {
            inter_tokens.insert(f.token_out);
        }
    }

    let mut in_deg: HashMap<Address, usize> = inter_tokens.iter().map(|&t| (t, 0)).collect();
    let mut adj: HashMap<Address, Vec<Address>> = HashMap::new();
    for f in &merged {
        if f.token_in != base_token && f.token_out != base_token {
            adj.entry(f.token_in).or_default().push(f.token_out);
            *in_deg.entry(f.token_out).or_insert(0) += 1;
        }
    }

    let mut queue: Vec<Address> = in_deg
        .iter()
        .filter_map(|(&t, &d)| if d == 0 { Some(t) } else { None })
        .collect();
    // Deterministic ordering
    queue.sort();
    let mut topo_tokens: Vec<Address> = Vec::with_capacity(inter_tokens.len() + 1);
    topo_tokens.push(base_token);

    let mut visited = 0usize;
    let mut head = 0usize;
    while head < queue.len() {
        let u = queue[head];
        head += 1;
        visited += 1;
        topo_tokens.push(u);
        if let Some(nbrs) = adj.get(&u) {
            let mut next_ready = Vec::new();
            for &v in nbrs {
                if let Some(d) = in_deg.get_mut(&v) {
                    *d -= 1;
                    if *d == 0 {
                        next_ready.push(v);
                    }
                }
            }
            next_ready.sort();
            queue.extend(next_ready);
        }
    }

    // Reject if intermediate subgraph has a cycle
    if visited != inter_tokens.len() {
        return None;
    }

    // Order flows by token_in topological rank, and calculate relative remaining shareBps
    let mut ordered: Vec<(DirectedFlow, u16)> = Vec::with_capacity(merged.len());
    for tk in topo_tokens {
        let mut out_edges: Vec<DirectedFlow> = merged
            .iter()
            .filter(|f| f.token_in == tk)
            .cloned()
            .collect();
        if out_edges.is_empty() {
            continue;
        }
        // Sort largest flow last (or deterministic) so last edge is always 10000
        out_edges.sort_by(|a, b| {
            a.flow_in_human
                .partial_cmp(&b.flow_in_human)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut remaining: f64 = out_edges.iter().map(|e| e.flow_in_human).sum();
        let len = out_edges.len();
        for (idx, e) in out_edges.into_iter().enumerate() {
            let share_bps = if idx + 1 == len || remaining <= 0.0 {
                10_000u16
            } else {
                let frac = (e.flow_in_human / remaining) * 10_000.0;
                let bps = frac.round().clamp(1.0, 9_999.0) as u16;
                remaining = (remaining - e.flow_in_human).max(1e-18);
                bps
            };
            ordered.push((e, share_bps));
        }
    }

    let mut steps: Vec<DagStep> = ordered
        .iter()
        .map(|(f, share_bps)| DagStep {
            pool: f.pool,
            tokenIn: f.token_in,
            tokenOut: f.token_out,
            dexKind: f.dex_kind.as_u8(),
            inputInPool: false,
            feeNumerator: f.fee_num,
            shareBps: *share_bps,
            recipient: executor_addr,
            extraData: alloy::primitives::Bytes::new(),
        })
        .collect();
    if steps.len() > MAX_DAG_STEPS {
        return None;
    }

    // Token-Hopping pass: if intermediate token `u` has exactly 1 consumer step `next_idx`,
    // and that consumer is UniV2 (0) or Solidly (1), and its pool is NOT touched between any
    // producer step and `next_idx`, send all incoming `u` straight into `steps[next_idx].pool`!
    for tk in inter_tokens {
        let incoming: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter_map(|(i, s)| if s.tokenOut == tk { Some(i) } else { None })
            .collect();
        let outgoing: Vec<usize> = steps
            .iter()
            .enumerate()
            .filter_map(|(i, s)| if s.tokenIn == tk { Some(i) } else { None })
            .collect();

        if incoming.is_empty() || outgoing.len() != 1 {
            continue;
        }
        let next_idx = outgoing[0];
        let next_step = &steps[next_idx];
        if next_step.dexKind != DexKind::UniV2.as_u8()
            && next_step.dexKind != DexKind::Solidly.as_u8()
        {
            continue;
        }
        let target_pool = next_step.pool;
        let min_inc = *incoming.iter().min().unwrap();
        let safe = (min_inc..next_idx).all(|mid| steps[mid].pool != target_pool)
            && !incoming.iter().any(|&i| steps[i].pool == target_pool);
        if !safe {
            continue;
        }
        for inc_idx in incoming {
            steps[inc_idx].recipient = target_pool;
        }
        steps[next_idx].inputInPool = true;
    }

    Some(steps)
}

fn execution_flash_source(
    market: &MarketState,
    cand: &RouteCandidate,
    steps: &[DagStep],
    executor_addr: Address,
) -> (u8, Address) {
    if cand.flash_mode != 0
        && steps.len() >= 2
        && steps[0].tokenIn == cand.base_token
        && steps[0].shareBps == 10_000
        && !steps[1..].iter().any(|s| s.pool == steps[0].pool)
    {
        let root = &steps[0];
        let fac = market
            .pools
            .get(&root.pool)
            .map(|p| p.factory_name.as_str())
            .or_else(|| {
                cand.flows
                    .iter()
                    .find(|f| f.pool == root.pool)
                    .and_then(|f| f.label.split(':').next())
            })
            .unwrap_or("");
        if (root.dexKind == DexKind::UniV3.as_u8() || root.dexKind == DexKind::PancakeV3.as_u8())
            && market.chain.factory_supports_flash(fac)
        {
            return (4, root.pool);
        }
        if root.dexKind == DexKind::MetricOmm.as_u8() {
            return (6, root.pool);
        }
        // V2 Pair.swap invokes callback on `to` (`root.recipient`), so token-hopped steps
        // (`recipient == steps[1].pool`) and unsupported V2 forks (Mdex/BabySwap/Generic)
        // must use Moolah / external flash rather than FLASH_V2_SWAP.
        if root.dexKind == DexKind::UniV2.as_u8()
            && root.recipient == executor_addr
            && market.chain.factory_supports_flash(fac)
        {
            return (5, root.pool);
        }
    }
    if cand.flash_mode == 7
        && steps.iter().any(|s| {
            s.dexKind == DexKind::BalancerV2.as_u8()
                && market
                    .pools
                    .get(&s.pool)
                    .is_some_and(|p| p.balancer.as_ref().is_some_and(|b| b.vault == cand.flash_source))
        })
    {
        let used_pools: Vec<Address> = steps.iter().map(|s| s.pool).collect();
        let borrow_raw = human_to_u256(cand.borrow_human, market.token_decimals(cand.base_token));
        if let Some(alt) = market.select_flash_source(cand.base_token, borrow_raw, &used_pools) {
            return (alt.flash_mode, alt.pool);
        }
    }
    (cand.flash_mode, cand.flash_source)
}

pub fn populate_singleton_steps(steps: &mut [DagStep], market: &MarketState) {
    for step in steps {
        if step.dexKind == DexKind::InfinityCl.as_u8() || step.dexKind == DexKind::UniV4.as_u8() {
            if let Some(pool) = market.pools.get(&step.pool) {
                if let Some(ref k) = pool.v4_key {
                    step.pool = k.manager;
                    step.recipient = k.vault;
                    step.extraData = k.encode_extra_data(pool.dex_kind);
                }
            }
        } else if step.dexKind == DexKind::MetricOmm.as_u8() {
            if let Some(pool) = market.pools.get(&step.pool) {
                step.extraData = alloy::primitives::Bytes::from(vec![u8::from(step.tokenIn == pool.token0)]);
            }
        } else if step.dexKind == DexKind::BalancerV2.as_u8() {
            if let Some(pool) = market.pools.get(&step.pool) {
                if let Some(ref bal) = pool.balancer {
                    step.pool = bal.vault;
                    step.extraData = alloy::primitives::Bytes::from(bal.pool_id.as_slice().to_vec());
                }
            }
        }
    }
}

pub fn build_arb_params(
    cand: &RouteCandidate,
    market: &MarketState,
    executor_addr: Address,
    min_net_profit_raw: U256,
    payout_to_owner: bool,
) -> Option<ArbParams> {
    let mut steps = compile_dag_steps(&cand.flows, cand.base_token, executor_addr)?;
    let dec = market.token_decimals(cand.base_token);
    let borrow_raw = human_to_u256(cand.borrow_human, dec);
    if borrow_raw.is_zero() {
        return None;
    }
    let (flash_mode, flash_source) = execution_flash_source(market, cand, &steps, executor_addr);
    populate_singleton_steps(&mut steps, market);
    Some(ArbParams {
        flashMode: flash_mode,
        flashSource: flash_source,
        baseToken: cand.base_token,
        borrowAmount: borrow_raw,
        minNetProfit: min_net_profit_raw,
        payoutToOwner: payout_to_owner,
        wrappedNative: market.chain.wrapped_native,
        bribeRecipient: Address::ZERO,
        bribeBps: 0,
        steps,
    })
}

/// Encode `ArbParams` into V2 `DagArbExecutor` compact binary `fallback()` payload (`0x00` prefix).
pub fn encode_compact_arb(params: &ArbParams, executor_addr: Address) -> eyre::Result<Bytes> {
    use alloy::sol_types::SolValue;
    if params.steps.len() > MAX_DAG_STEPS {
        eyre::bail!("too many DAG steps: {}", params.steps.len());
    }

    let mut tokens: Vec<Address> = Vec::with_capacity(16);
    tokens.push(params.baseToken);
    let mut tok_idx = |t: Address| -> eyre::Result<u8> {
        if let Some(pos) = tokens.iter().position(|&x| x == t) {
            return Ok(pos as u8);
        }
        if tokens.len() >= 16 {
            eyre::bail!("compact arb exceeds 16 unique tokens");
        }
        tokens.push(t);
        Ok((tokens.len() - 1) as u8)
    };

    let mut step_indices = Vec::with_capacity(params.steps.len());
    for s in &params.steps {
        step_indices.push((tok_idx(s.tokenIn)?, tok_idx(s.tokenOut)?));
    }

    let borrow_u128: u128 = params
        .borrowAmount
        .try_into()
        .map_err(|_| eyre::eyre!("borrowAmount overflows uint128"))?;
    let min_profit_u128: u128 = params
        .minNetProfit
        .try_into()
        .map_err(|_| eyre::eyre!("minNetProfit overflows uint128"))?;

    let mut flags = 0u8;
    if params.payoutToOwner {
        flags |= 0x01;
    }
    if params.wrappedNative != Address::ZERO {
        flags |= 0x02;
    }
    if params.bribeRecipient != Address::ZERO {
        flags |= 0x04;
    }
    if params.flashSource != Address::ZERO {
        flags |= 0x08;
    }

    let mut out = Vec::with_capacity(128 + params.steps.len() * 32);
    out.push(0x00u8); // MAGIC_COMPACT_ENTRY
    out.push(params.flashMode);
    out.push(flags);
    out.extend_from_slice(&params.bribeBps.to_be_bytes());
    out.extend_from_slice(&borrow_u128.to_be_bytes());
    out.extend_from_slice(&min_profit_u128.to_be_bytes());
    if (flags & 0x02) != 0 {
        out.extend_from_slice(params.wrappedNative.as_slice());
    }
    if (flags & 0x04) != 0 {
        out.extend_from_slice(params.bribeRecipient.as_slice());
    }
    if (flags & 0x08) != 0 {
        out.extend_from_slice(params.flashSource.as_slice());
    }

    out.push(tokens.len() as u8);
    for t in &tokens {
        out.extend_from_slice(t.as_slice());
    }
    out.push(params.steps.len() as u8);

    for (i, s) in params.steps.iter().enumerate() {
        let dex_kind = s.dexKind & 0x0f;
        let rec_is_next = dex_kind != DexKind::InfinityCl.as_u8()
            && dex_kind != DexKind::UniV4.as_u8()
            && i + 1 < params.steps.len()
            && s.recipient != executor_addr
            && s.recipient == params.steps[i + 1].pool;
        let share_full = s.shareBps == 10_000;
        let metric_zfo =
            dex_kind == DexKind::MetricOmm.as_u8() && !s.extraData.is_empty() && s.extraData[0] != 0;

        let mut hdr = dex_kind;
        if s.inputInPool {
            hdr |= 0x10;
        }
        if rec_is_next {
            hdr |= 0x20;
        }
        if share_full {
            hdr |= 0x40;
        }
        if metric_zfo {
            hdr |= 0x80;
        }
        out.push(hdr);
        let (in_idx, out_idx) = step_indices[i];
        out.push((in_idx << 4) | (out_idx & 0x0f));
        out.extend_from_slice(s.pool.as_slice());
        if !share_full {
            out.extend_from_slice(&s.shareBps.to_be_bytes());
        }

        match dex_kind {
            x if x == DexKind::UniV2.as_u8() => {
                out.extend_from_slice(&s.feeNumerator.to_be_bytes());
            }
            x if x == DexKind::BalancerV2.as_u8() => {
                if s.extraData.len() < 32 {
                    eyre::bail!("BalancerV2 extraData < 32 bytes");
                }
                out.extend_from_slice(&s.extraData[..32]);
            }
            x if x == DexKind::UniV4.as_u8() => {
                let (c0, _c1, fee, tick_spacing, hooks) =
                    <(Address, Address, U24, I24, Address)>::abi_decode_params(
                        s.extraData.as_ref(),
                    )
                    .map_err(|e| eyre::eyre!("decode UniV4 extraData failed: {e}"))?;
                out.push(u8::from(c0 == Address::ZERO));
                let fee_u32: u32 = fee.to::<u32>();
                out.extend_from_slice(&fee_u32.to_be_bytes()[1..4]);
                let ts_bytes = tick_spacing.as_i32().to_be_bytes();
                out.extend_from_slice(&ts_bytes[1..4]);
                out.extend_from_slice(hooks.as_slice());
            }
            x if x == DexKind::InfinityCl.as_u8() => {
                let (c0, _c1, hooks, _mgr, fee, parameters) =
                    <(Address, Address, Address, Address, U24, alloy::primitives::B256)>::abi_decode_params(
                        s.extraData.as_ref(),
                    )
                    .map_err(|e| eyre::eyre!("decode InfinityCL extraData failed: {e}"))?;
                out.extend_from_slice(s.recipient.as_slice());
                out.push(u8::from(c0 == Address::ZERO));
                let fee_u32: u32 = fee.to::<u32>();
                out.extend_from_slice(&fee_u32.to_be_bytes()[1..4]);
                out.extend_from_slice(hooks.as_slice());
                out.extend_from_slice(parameters.as_slice());
            }
            _ => {}
        }
    }

    Ok(Bytes::from(out))
}

pub fn render_mermaid(market: &MarketState, base_token: Address, flows: &[DirectedFlow]) -> String {
    let mut lines = vec!["flowchart LR".to_string()];
    let base_sym = market.token_symbol(base_token);
    for (i, f) in flows.iter().enumerate() {
        let s_in = if f.token_in == base_token {
            format!("{base_sym}_START")
        } else {
            market.token_symbol(f.token_in)
        };
        let s_out = if f.token_out == base_token {
            format!("{base_sym}_END")
        } else {
            market.token_symbol(f.token_out)
        };
        lines.push(format!(
            "    {s_in} -->|\"#{i} {} ({:.4} -> {:.4})\"| {s_out}",
            f.label, f.flow_in_human, f.flow_out_human
        ));
    }
    lines.join("\n")
}

fn prefer_wrapped_native_cycle(market: &MarketState, mut cand: RouteCandidate) -> RouteCandidate {
    let wbnb = market.chain.wrapped_native;
    let n = cand.flows.len();
    if cand.base_token == wbnb || n < 2 {
        return cand;
    }
    let is_simple_cycle = cand.flows[0].token_in == cand.base_token
        && cand.flows.last().is_some_and(|f| f.token_out == cand.base_token)
        && cand.flows.windows(2).all(|w| w[0].token_out == w[1].token_in);

    let rotated_flows = if is_simple_cycle {
        let wbnb_positions: Vec<usize> = cand
            .flows
            .iter()
            .enumerate()
            .filter_map(|(i, f)| if f.token_in == wbnb { Some(i) } else { None })
            .collect();
        if wbnb_positions.len() != 1 {
            return cand;
        }
        let k = wbnb_positions[0];
        if k == 0 {
            return cand;
        }
        let mut flows = cand.flows.clone();
        flows.rotate_left(k);
        flows
    } else if n <= 8 {
        // Reorder unordered single-cycle emissions (e.g. from DAG path reconstruction) onto WBNB.
        let mut out_idx_by_token: HashMap<Address, usize> = HashMap::with_capacity(n);
        let mut in_seen: HashSet<Address> = HashSet::with_capacity(n);
        for (i, f) in cand.flows.iter().enumerate() {
            if f.flow_in_human <= 0.0
                || out_idx_by_token.insert(f.token_in, i).is_some()
                || !in_seen.insert(f.token_out)
            {
                return cand;
            }
        }
        if !out_idx_by_token.contains_key(&wbnb) {
            return cand;
        }
        let mut flows = Vec::with_capacity(n);
        let mut cur = wbnb;
        for step in 0..n {
            let Some(&idx) = out_idx_by_token.get(&cur) else {
                return cand;
            };
            let f = cand.flows[idx].clone();
            cur = f.token_out;
            flows.push(f);
            if step + 1 < n && cur == wbnb {
                return cand;
            }
        }
        if flows.len() != n || cur != wbnb {
            return cand;
        }
        flows
    } else {
        return cand;
    };

    let wbnb_usd = market.token_usd(wbnb);
    if wbnb_usd <= 0.0 || !wbnb_usd.is_finite() {
        return cand;
    }
    let borrow_h = rotated_flows[0].flow_in_human;
    let borrow_raw = human_to_u256(borrow_h, market.token_decimals(wbnb));
    if borrow_raw.is_zero() {
        return cand;
    }
    let used_pools: Vec<Address> = rotated_flows.iter().map(|f| f.pool).collect();
    let Some(flash) = market.select_flash_source(wbnb, borrow_raw, &used_pools) else {
        return cand;
    };
    cand.flows = rotated_flows;
    cand.base_token = wbnb;
    cand.borrow_human = borrow_h;
    cand.flash_mode = flash.flash_mode;
    cand.flash_source = flash.pool;
    cand.flash_fee_bps = flash.fee_bps;
    cand.est_gross_profit_human = cand.est_gross_profit_usd / wbnb_usd;
    cand.mermaid = render_mermaid(market, wbnb, &cand.flows);
    cand
}

/// Configurable solver pruning parameters for A/B backtesting and production tuning.
#[derive(Debug, Clone, Copy)]
pub struct SolverPruningConfig {
    pub name: &'static str,
    pub max_non_hub_bfs: usize,
    pub bfs_min_depth_usd: f64,
    pub max_routable_pools: usize,
    pub top_extra_per_token: usize,
    pub cyclic_shell_1: usize,
    pub cyclic_shell_2: usize,
    pub cyclic_raw_cap: usize,
    pub deep_beam_width: usize,
    pub deep_beam_cap_dirty: usize,
    pub deep_tested_cap: usize,
}

impl Default for SolverPruningConfig {
    fn default() -> Self {
        Self {
            name: "baseline(bw=4,pools=520)",
            max_non_hub_bfs: 84,
            bfs_min_depth_usd: 80.0,
            max_routable_pools: 520,
            top_extra_per_token: 6,
            cyclic_shell_1: 4,
            cyclic_shell_2: 3,
            cyclic_raw_cap: 36,
            deep_beam_width: 4,
            deep_beam_cap_dirty: 64,
            deep_tested_cap: 10,
        }
    }
}

pub fn pruning_ab_profiles() -> [SolverPruningConfig; 3] {
    [
        SolverPruningConfig::default(),
        SolverPruningConfig {
            name: "medium(bw=8,pools=750)",
            max_non_hub_bfs: 132,
            bfs_min_depth_usd: 60.0,
            max_routable_pools: 750,
            top_extra_per_token: 9,
            cyclic_shell_1: 6,
            cyclic_shell_2: 5,
            cyclic_raw_cap: 64,
            deep_beam_width: 8,
            deep_beam_cap_dirty: 112,
            deep_tested_cap: 16,
        },
        SolverPruningConfig {
            name: "wide(bw=12,pools=1000)",
            max_non_hub_bfs: 192,
            bfs_min_depth_usd: 40.0,
            max_routable_pools: 1000,
            top_extra_per_token: 12,
            cyclic_shell_1: 8,
            cyclic_shell_2: 8,
            cyclic_raw_cap: 96,
            deep_beam_width: 12,
            deep_beam_cap_dirty: 160,
            deep_tested_cap: 24,
        },
    ]
}

/// Run Parallel + Cyclic + DAG routers on active hub pairs + dirty 1-hop neighborhood and return sorted profitable candidates.
pub fn solve_all(
    market: &MarketState,
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    solve_all_with_config(market, dirty_pools, min_profit_usd, &SolverPruningConfig::default())
}

pub fn solve_all_with_config(
    market: &MarketState,
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
    pruning: &SolverPruningConfig,
) -> Vec<RouteCandidate> {
    let periodic_sweep = market.last_block > 0
        && market.last_block % 12 == 0
        && dirty_pools.is_some_and(|dp| !dp.is_empty());
    let full_sweep = dirty_pools.is_none() || periodic_sweep;
    let routable_pools: Vec<&PoolState> = if let Some(dp) = dirty_pools.filter(|_| periodic_sweep) {
        let mut seen: HashSet<Address> = HashSet::with_capacity(256);
        let mut list: Vec<&PoolState> = Vec::with_capacity(pruning.max_routable_pools.max(1_150));
        for &pa in dp {
            if let Some(p) = market.pools.get(&pa) {
                if market.is_pool_arbitrageable(p) && seen.insert(pa) {
                    list.push(p);
                }
            }
        }
        for &pa in &market.hub_pair_pools {
            if let Some(p) = market.pools.get(&pa) {
                if market.is_pool_arbitrageable(p) && seen.insert(pa) {
                    list.push(p);
                }
            }
        }
        let mut rest: Vec<(&PoolState, f64)> = market
            .pools
            .values()
            .filter(|p| {
                !seen.contains(&p.address)
                    && market.is_pool_arbitrageable(p)
                    && (!p.factory_name.starts_with("Generic")
                        || market.sim_verified_pools.contains(&p.address))
                    && p.cached_depth_usd >= 25.0
            })
            .map(|p| {
                let mut w = p.cached_depth_usd;
                if market.sim_verified_pools.contains(&p.address) {
                    w *= 4.0;
                }
                if market.last_block <= p.last_updated_block.saturating_add(256) {
                    w *= 2.5;
                } else if market.last_block <= p.last_updated_block.saturating_add(1_200) {
                    w *= 1.4;
                }
                (p, w)
            })
            .collect();
        rest.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let sweep_cap = pruning.max_routable_pools.max(1_150);
        for (p, _) in rest {
            if list.len() >= sweep_cap {
                break;
            }
            list.push(p);
        }
        list
    } else if full_sweep {
        market
            .pools
            .values()
            .filter(|p| market.is_pool_arbitrageable(p))
            .collect()
    } else {
        let dp = dirty_pools.unwrap();
        if dp.is_empty() {
            return Vec::new();
        }
        let mut dirty_non_hub: HashSet<Address> = HashSet::new();
        for &pa in dp {
            if let Some(p) = market.pools.get(&pa) {
                if !p.is_hub_0 {
                    dirty_non_hub.insert(p.token0);
                }
                if !p.is_hub_1 {
                    dirty_non_hub.insert(p.token1);
                }
            }
        }
        let mut relevant_non_hub = dirty_non_hub.clone();
        let mut frontier: Vec<Address> = dirty_non_hub.iter().copied().take(32).collect();
        for _layer in 0..3 {
            if frontier.is_empty() || relevant_non_hub.len() >= pruning.max_non_hub_bfs {
                break;
            }
            let mut next_frontier = Vec::new();
            'bfs: for tk in frontier {
                if let Some(plist) = market.token_pools.get(&tk) {
                    for &pa in plist {
                        if let Some(p) = market.pools.get(&pa) {
                            if market.is_pool_arbitrageable(p)
                                && p.cached_depth_usd >= pruning.bfs_min_depth_usd
                            {
                                for cand_tk in [p.token0, p.token1] {
                                    if !market.is_hub_token(cand_tk)
                                        && relevant_non_hub.insert(cand_tk)
                                    {
                                        next_frontier.push(cand_tk);
                                        if relevant_non_hub.len() >= pruning.max_non_hub_bfs {
                                            break 'bfs;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            frontier = next_frontier;
        }
        let mut seen: HashSet<Address> = HashSet::with_capacity(256);
        let mut list: Vec<&PoolState> = Vec::with_capacity(256);
        for &pa in &market.hub_pair_pools {
            if let Some(p) = market.pools.get(&pa) {
                if market.is_pool_arbitrageable(p) && seen.insert(pa) {
                    list.push(p);
                }
            }
        }
        for tk in relevant_non_hub {
            if let Some(plist) = market.token_pools.get(&tk) {
                if plist.len() <= pruning.top_extra_per_token + 2 {
                    for &pa in plist {
                        if let Some(p) = market.pools.get(&pa) {
                            if market.is_pool_arbitrageable(p) && seen.insert(pa) {
                                list.push(p);
                            }
                        }
                    }
                } else {
                    let mut top_extra: Vec<&PoolState> = Vec::with_capacity(plist.len());
                    for &pa in plist {
                        if let Some(p) = market.pools.get(&pa) {
                            if !market.is_pool_arbitrageable(p) || seen.contains(&pa) {
                                continue;
                            }
                            if dp.contains(&pa) {
                                if seen.insert(pa) {
                                    list.push(p);
                                }
                            } else {
                                top_extra.push(p);
                            }
                        }
                    }
                    if top_extra.len() > pruning.top_extra_per_token {
                        top_extra.sort_by(|a, b| {
                            b.cached_depth_usd
                                .partial_cmp(&a.cached_depth_usd)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        top_extra.truncate(pruning.top_extra_per_token);
                    }
                    for p in top_extra {
                        if seen.insert(p.address) {
                            list.push(p);
                        }
                    }
                }
            }
        }
        if list.len() > pruning.max_routable_pools {
            list.sort_by(|a, b| {
                let pa = dp.contains(&a.address) || (a.is_hub_0 && a.is_hub_1);
                let pb = dp.contains(&b.address) || (b.is_hub_0 && b.is_hub_1);
                pb.cmp(&pa).then_with(|| {
                    b.cached_depth_usd
                        .partial_cmp(&a.cached_depth_usd)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            });
            list.truncate(pruning.max_routable_pools);
        }
        list
    };
    if routable_pools.len() < 2 {
        return Vec::new();
    }

    let sub_hurdle = (min_profit_usd - 0.008).max(-0.006);
    let eff_dirty = if full_sweep { None } else { dirty_pools };
    let mut raw_all = Vec::new();
    let solver_mode = gpu::solver_mode();
    let run_cpu = solver_mode != gpu::SolverMode::Gpu || !gpu::is_available();
    if run_cpu {
        raw_all.extend(parallel::solve_parallel(
            market,
            &routable_pools,
            eff_dirty,
            sub_hurdle,
        ));
        raw_all.extend(cyclic::solve_cyclic_with_config(
            market,
            &routable_pools,
            eff_dirty,
            sub_hurdle,
            pruning,
        ));
        raw_all.extend(dag::solve_dag_and_deep_with_config(
            market,
            &routable_pools,
            eff_dirty,
            sub_hurdle,
            pruning,
        ));
    }
    if matches!(solver_mode, gpu::SolverMode::Gpu | gpu::SolverMode::Hybrid) {
        raw_all.extend(gpu::solve_gpu(
            market,
            &routable_pools,
            eff_dirty,
            sub_hurdle,
        ));
    }

    let raw_all: Vec<RouteCandidate> = raw_all
        .into_iter()
        .map(|c| prefer_wrapped_native_cycle(market, c))
        .collect();
    let coalesced = coalesce_multi_branch_dags(market, &raw_all, min_profit_usd);
    let mut all: Vec<RouteCandidate> = raw_all
        .into_iter()
        .filter(|c| c.est_net_profit_usd >= min_profit_usd)
        .collect();
    all.extend(coalesced);

    all.sort_by(|a, b| {
        b.est_net_profit_usd
            .partial_cmp(&a.est_net_profit_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Deduplicate routes with identical sorted pool sequence + base_token
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for c in all {
        let mut pools_sig: Vec<(Address, Address, Address)> = c
            .flows
            .iter()
            .map(|f| (f.pool, f.token_in, f.token_out))
            .collect();
        pools_sig.sort();
        let key = (c.base_token, pools_sig);
        if seen.insert(key) {
            deduped.push(c);
        }
    }

    let mut verified: Vec<RouteCandidate> = deduped
        .into_iter()
        .take(24)
        .filter_map(|c| verify_and_refine_candidate(market, c, min_profit_usd))
        .collect();
    verified.sort_by(|a, b| {
        b.est_net_profit_usd
            .partial_cmp(&a.est_net_profit_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    verified
}

/// Coalesce >= 2 independent/compatible positive-marginal branches anchored on the same base token
/// into a single `<= 32`-step atomic DAG so one shared flash loan + 135k base gas unlocks
/// multi-branch micro-dislocations that are unprofitable standalone.
fn coalesce_multi_branch_dags(
    market: &MarketState,
    raw_all: &[RouteCandidate],
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    let virtual_exec = Address::repeat_byte(0xEE);
    let mut by_base: HashMap<Address, Vec<(f64, &RouteCandidate)>> = HashMap::new();

    const MAX_COALESCED_STEPS: usize = 14;
    for c in raw_all {
        if c.flows.len() < 2 || c.flows.len() > MAX_COALESCED_STEPS {
            continue;
        }
        let branch_clean = c.flows.iter().all(|f| {
            let pool_cooled = market
                .pool_cooldown_until
                .get(&f.pool)
                .is_some_and(|&until| market.last_block < until);
            let tok_in_cooled = !market.is_hub_token(f.token_in)
                && market
                    .token_cooldown_until
                    .get(&f.token_in)
                    .is_some_and(|&until| market.last_block < until);
            let tok_out_cooled = !market.is_hub_token(f.token_out)
                && market
                    .token_cooldown_until
                    .get(&f.token_out)
                    .is_some_and(|&until| market.last_block < until);
            let unverified_generic = market.pools.get(&f.pool).is_some_and(|p| {
                p.factory_name.starts_with("Generic")
                    && !market.sim_verified_pools.contains(&f.pool)
            });
            !pool_cooled && !tok_in_cooled && !tok_out_cooled && !unverified_generic
        });
        if !branch_clean {
            continue;
        }
        let step_gas = estimate_flows_gas(&c.flows).saturating_sub(65_000);
        let marg_gas_usd = market.estimate_gas_cost_usd(step_gas, 0);
        let marg_net = c.est_gross_profit_usd - marg_gas_usd;
        if marg_net > 0.0005 {
            by_base.entry(c.base_token).or_default().push((marg_net, c));
        }
    }

    let mut out = Vec::new();
    for (base_token, mut group) in by_base {
        if group.len() < 2 {
            continue;
        }
        let base_usd = market.token_usd(base_token);
        if base_usd <= 0.0 {
            continue;
        }
        group.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        group.truncate(24);

        let mut used_branch_indices: HashSet<usize> = HashSet::new();
        for seed_idx in 0..group.len().min(4) {
            if used_branch_indices.contains(&seed_idx) {
                continue;
            }
            let mut packed_flows: Vec<DirectedFlow> = Vec::new();
            let mut merged_dir_usd: HashMap<(Address, Address, Address), f64> = HashMap::new();
            let mut non_hub_set: HashSet<Address> = HashSet::new();
            let mut sum_gross_usd = 0.0_f64;
            let mut sum_gross_h = 0.0_f64;
            let mut sum_borrow_h = 0.0_f64;
            let mut branch_count = 0usize;
            let mut local_used: Vec<usize> = Vec::new();

            for cand_idx in seed_idx..group.len() {
                if used_branch_indices.contains(&cand_idx) {
                    continue;
                }
                let (marg_net, c) = group[cand_idx];
                if branch_count > 0 && marg_net <= 0.0005 {
                    continue;
                }

                // Require strictly disjoint non-hub tokens/pools across coalesced branches;
                // allow sharing a directed hub-to-hub edge only when depth > 10x combined flow.
                let mut ok = true;
                let mut new_dir_usd = merged_dir_usd.clone();
                let mut cand_non_hub: HashSet<Address> = HashSet::new();

                for f in &c.flows {
                    if !market.is_hub_token(f.token_in) {
                        cand_non_hub.insert(f.token_in);
                    }
                    if !market.is_hub_token(f.token_out) {
                        cand_non_hub.insert(f.token_out);
                    }
                }
                if branch_count > 0 && !cand_non_hub.is_disjoint(&non_hub_set) {
                    continue;
                }

                for f in &c.flows {
                    // Reject opposite direction on same pool
                    if new_dir_usd
                        .keys()
                        .any(|&(p, ti, to)| p == f.pool && ti == f.token_out && to == f.token_in)
                    {
                        ok = false;
                        break;
                    }
                    let u_in = market.token_usd(f.token_in);
                    let f_usd = if u_in > 0.0 {
                        f.flow_in_human * u_in
                    } else {
                        c.borrow_human * base_usd
                    };
                    let key = (f.pool, f.token_in, f.token_out);
                    let prev_usd = new_dir_usd.get(&key).copied().unwrap_or(0.0);
                    let next_usd = prev_usd + f_usd;
                    if prev_usd > 0.0 {
                        let is_hub_edge =
                            market.is_hub_token(f.token_in) && market.is_hub_token(f.token_out);
                        let deep_enough = is_hub_edge
                            && market
                                .pools
                                .get(&f.pool)
                                .is_some_and(|p| p.cached_depth_usd > 10.0 * next_usd);
                        if !deep_enough {
                            ok = false;
                            break;
                        }
                    }
                    new_dir_usd.insert(key, next_usd);
                }
                let mut new_non_hub = non_hub_set.clone();
                new_non_hub.extend(cand_non_hub);
                if !ok || new_non_hub.len() > 24 || new_dir_usd.len() > MAX_COALESCED_STEPS {
                    continue;
                }

                let mut candidate_flows = packed_flows.clone();
                candidate_flows.extend(c.flows.iter().cloned());
                let Some(steps) = compile_dag_steps(&candidate_flows, base_token, virtual_exec)
                else {
                    continue;
                };
                if steps.len() > MAX_COALESCED_STEPS {
                    continue;
                }
                if steps.len() > 6
                    && candidate_flows.iter().any(|f| {
                        market.pools.get(&f.pool).is_some_and(|p| {
                            p.factory_name.starts_with("Generic") && p.cached_depth_usd < 150.0
                        })
                    })
                {
                    continue;
                }

                // Only add if marginal contribution covers added step gas
                let prev_steps_len = if packed_flows.is_empty() {
                    0
                } else {
                    compile_dag_steps(&packed_flows, base_token, virtual_exec)
                        .map_or(packed_flows.len(), |s| s.len())
                };
                let added_steps = steps.len().saturating_sub(prev_steps_len);
                let added_step_gas_usd = market.estimate_gas_cost_usd((added_steps as u64) * 95_000, 0);
                if branch_count > 0 && c.est_gross_profit_usd <= added_step_gas_usd + 0.0003 {
                    continue;
                }

                packed_flows = candidate_flows;
                merged_dir_usd = new_dir_usd;
                non_hub_set = new_non_hub;
                sum_gross_usd += c.est_gross_profit_usd;
                sum_gross_h += c.est_gross_profit_human;
                sum_borrow_h += c.borrow_human;
                branch_count += 1;
                local_used.push(cand_idx);
            }

            if branch_count < 2 {
                continue;
            }
            // Merge duplicate `(pool, token_in, token_out)` entries cleanly before emitting RouteCandidate
            let mut merged_flows: Vec<DirectedFlow> = Vec::new();
            let mut f_idx_map: HashMap<(Address, Address, Address), usize> = HashMap::new();
            for f in packed_flows {
                let key = (f.pool, f.token_in, f.token_out);
                if let Some(&i) = f_idx_map.get(&key) {
                    merged_flows[i].flow_in_human += f.flow_in_human;
                    merged_flows[i].flow_out_human += f.flow_out_human;
                } else {
                    f_idx_map.insert(key, merged_flows.len());
                    merged_flows.push(f);
                }
            }
            let Some(final_steps) = compile_dag_steps(&merged_flows, base_token, virtual_exec)
            else {
                continue;
            };
            let used_pools: Vec<Address> = merged_flows.iter().map(|f| f.pool).collect();
            let borrow_raw = human_to_u256(sum_borrow_h, market.token_decimals(base_token));
            let Some(flash) = market.select_flash_source(base_token, borrow_raw, &used_pools)
            else {
                continue;
            };
            let est_gas = estimate_steps_gas(&final_steps);
            let gas_usd = market.estimate_gas_cost_usd(est_gas, 0);
            let net_usd = sum_gross_usd - gas_usd;
            if net_usd < min_profit_usd {
                continue;
            }
            for idx in local_used {
                used_branch_indices.insert(idx);
            }
            let mermaid = render_mermaid(market, base_token, &merged_flows);
            out.push(RouteCandidate {
                strategy: format!("DAG({}steps)", final_steps.len()),
                base_token,
                borrow_human: sum_borrow_h,
                est_gross_profit_human: sum_gross_h,
                est_gross_profit_usd: sum_gross_usd,
                est_net_profit_usd: net_usd,
                flash_mode: flash.flash_mode,
                flash_source: flash.pool,
                flash_fee_bps: flash.fee_bps,
                flows: merged_flows,
                mermaid,
            });
        }
    }
    out
}

#[inline]
fn apply_tax_ppm_u256(amount: U256, tax_ppm: u32) -> U256 {
    if tax_ppm == 0 || amount.is_zero() {
        return amount;
    }
    let keep = 1_000_000u64.saturating_sub(tax_ppm.min(999_999) as u64);
    (amount * U256::from(keep)) / U256::from(1_000_000u64)
}

fn replay_dag_steps(
    market: &MarketState,
    base_token: Address,
    steps: &[DagStep],
    borrow_raw: U256,
    flash_fee_bps: f64,
    virtual_exec: Address,
) -> Option<(f64, f64, f64)> {
    let base_dec = market.token_decimals(base_token);
    let base_usd = market.token_usd(base_token);
    if base_usd <= 0.0 || borrow_raw.is_zero() {
        return None;
    }

    let mut v2_reserves: HashMap<Address, (U256, U256)> = HashMap::new();
    let mut res_pools: HashMap<Address, ResidualPool> = HashMap::new();
    let mut pool_only_tax: HashMap<Address, u32> = HashMap::new();
    for s in steps {
        if v2_reserves.contains_key(&s.pool) || res_pools.contains_key(&s.pool) {
            continue;
        }
        let p = market.pools.get(&s.pool)?;
        let tax0 = market.token_tax_ppm.get(&p.token0).copied().unwrap_or(0);
        let tax1 = market.token_tax_ppm.get(&p.token1).copied().unwrap_or(0);
        let p_only = p.extra_tax_ppm.saturating_sub(tax0.saturating_add(tax1));
        pool_only_tax.insert(s.pool, p_only);
        if p.dex_kind == DexKind::UniV2 {
            v2_reserves.insert(s.pool, (p.r0_raw, p.r1_raw));
        } else {
            let mut rp = p.to_residual();
            // Token taxes are applied explicitly on transfers around each step
            rp.extra_tax_ppm = 0;
            res_pools.insert(s.pool, rp);
        }
    }

    let mut exec_bal: HashMap<Address, U256> = HashMap::new();
    exec_bal.insert(base_token, borrow_raw);
    let mut hopped_in_pool: HashMap<(Address, Address), U256> = HashMap::new();

    for step in steps {
        let zero_for_one = res_pools
            .get(&step.pool)
            .map(|rp| step.tokenIn == rp.token0)
            .unwrap_or_else(|| step.tokenIn < step.tokenOut);
        let tax_in = market.token_tax_ppm.get(&step.tokenIn).copied().unwrap_or(0);
        let tax_out = market.token_tax_ppm.get(&step.tokenOut).copied().unwrap_or(0);
        let p_only = pool_only_tax.get(&step.pool).copied().unwrap_or(0);

        let actual_in_raw = if !step.inputInPool {
            let bal = exec_bal.get(&step.tokenIn).copied().unwrap_or(U256::ZERO);
            let amt_in = (bal * U256::from(step.shareBps)) / U256::from(10_000u64);
            if amt_in.is_zero() {
                return None;
            }
            exec_bal.insert(step.tokenIn, bal - amt_in);
            let delivered = apply_tax_ppm_u256(amt_in, tax_in);
            let waiting = hopped_in_pool.remove(&(step.pool, step.tokenIn)).unwrap_or(U256::ZERO);
            waiting + delivered
        } else {
            hopped_in_pool.remove(&(step.pool, step.tokenIn)).unwrap_or(U256::ZERO)
        };
        if actual_in_raw.is_zero() {
            return None;
        }

        let amt_out_raw = if step.dexKind == DexKind::UniV2.as_u8() {
            let (r0, r1) = v2_reserves.get_mut(&step.pool)?;
            let (r_in, r_out) = if zero_for_one { (*r0, *r1) } else { (*r1, *r0) };
            let eff_in = apply_tax_ppm_u256(actual_in_raw, p_only);
            let out = univ2_amount_out_u256(eff_in, r_in, r_out, step.feeNumerator);
            if out.is_zero() || out >= r_out {
                return None;
            }
            if zero_for_one {
                *r0 = *r0 + actual_in_raw;
                *r1 = *r1 - out;
            } else {
                *r1 = *r1 + actual_in_raw;
                *r0 = *r0 - out;
            }
            out
        } else {
            let rp = res_pools.get_mut(&step.pool)?;
            let dec_in = market.token_decimals(step.tokenIn);
            let dec_out = market.token_decimals(step.tokenOut);
            let actual_in_h = u256_to_human(actual_in_raw, dec_in);
            let out_h = rp.apply_swap_human(zero_for_one, actual_in_h);
            if out_h <= 0.0 || !out_h.is_finite() {
                return None;
            }
            let out_h_adj = out_h * 0.99999 * (1.0 - (p_only.min(999_999) as f64) / 1_000_000.0);
            let out_raw = human_to_u256(out_h_adj, dec_out);
            if out_raw.is_zero() {
                return None;
            }
            out_raw
        };

        let received_out = apply_tax_ppm_u256(amt_out_raw, tax_out);
        if received_out.is_zero() {
            return None;
        }
        if step.recipient == virtual_exec {
            *exec_bal.entry(step.tokenOut).or_default() += received_out;
        } else {
            *hopped_in_pool.entry((step.recipient, step.tokenOut)).or_default() += received_out;
        }
    }

    let final_raw = exec_bal.get(&base_token).copied().unwrap_or(U256::ZERO);
    let fee_ppm = (flash_fee_bps * 100.0).round().max(0.0) as u64;
    let flash_fee_raw = if fee_ppm == 0 {
        U256::ZERO
    } else {
        (borrow_raw * U256::from(fee_ppm) + U256::from(999_999u64)) / U256::from(1_000_000u64)
    };
    let repay_raw = borrow_raw + flash_fee_raw;
    if final_raw <= repay_raw {
        return None;
    }
    let gross_raw = final_raw - repay_raw;
    let gross_h = u256_to_human(gross_raw, base_dec);
    let gross_usd = gross_h * base_usd;
    let est_gas = estimate_steps_gas(steps);
    let gas_usd = market.estimate_gas_cost_usd(est_gas, 0);
    let net_usd = gross_usd - gas_usd;
    Some((gross_h, gross_usd, net_usd))
}

/// Exact local `DagStep[]` integer & fee-aware replay + optimal down-sizing pre-filter.
pub fn verify_and_refine_candidate(
    market: &MarketState,
    mut cand: RouteCandidate,
    min_profit_usd: f64,
) -> Option<RouteCandidate> {
    let virtual_exec = Address::repeat_byte(0xEE);
    let steps = compile_dag_steps(&cand.flows, cand.base_token, virtual_exec)?;
    for s in &steps {
        if (s.dexKind == DexKind::UniV3.as_u8()
            || s.dexKind == DexKind::PancakeV3.as_u8()
            || s.dexKind == DexKind::InfinityCl.as_u8()
            || s.dexKind == DexKind::UniV4.as_u8()
            || s.dexKind == DexKind::MetricOmm.as_u8()
            || s.dexKind == DexKind::BalancerV2.as_u8())
            && market.token_tax_ppm.get(&s.tokenIn).copied().unwrap_or(0) >= 500
        {
            // Uniswap/Pancake V3 swap callbacks revert `IIA` on fee-on-transfer tokenIn
            return None;
        }
    }

    let mut best: Option<(f64, f64, f64, f64)> = None;
    let used_pools: Vec<Address> = steps.iter().map(|s| s.pool).collect();
    let base_dec = market.token_decimals(cand.base_token);
    let (mode, source) = execution_flash_source(market, &cand, &steps, virtual_exec);
    for scale in [1.85_f64, 1.40, 1.18, 1.0, 0.85, 0.68, 0.50, 0.30, 0.15] {
        let borrow = human_to_u256(cand.borrow_human * scale, base_dec);
        let funding = if (1..=3).contains(&mode) || mode == 7 {
            let Some(flash) = market.select_flash_source(cand.base_token, borrow, &used_pools) else {
                continue;
            };
            (flash.flash_mode, flash.pool, flash.fee_bps)
        } else {
            // A root flash swap already pays the trading fee on the first leg.
            (mode, source, 0.0)
        };
        if let Some((gross_h, gross_usd, net_usd)) =
            replay_dag_steps(market, cand.base_token, &steps, borrow, funding.2, virtual_exec)
        {
            if net_usd >= min_profit_usd
                && best.map_or(true, |b| net_usd > b.3 + 1e-6)
            {
                best = Some((scale, gross_h, gross_usd, net_usd));
                (cand.flash_mode, cand.flash_source, cand.flash_fee_bps) = funding;
            }
        }
    }

    let (scale, gross_h, gross_usd, net_usd) = best?;
    if (scale - 1.0).abs() > 1e-9 {
        cand.borrow_human *= scale;
        for f in &mut cand.flows {
            f.flow_in_human *= scale;
            f.flow_out_human *= scale;
        }
    }
    cand.est_gross_profit_human = gross_h;
    cand.est_gross_profit_usd = gross_usd;
    cand.est_net_profit_usd = net_usd;
    Some(cand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_chain_config;

    #[test]
    fn test_prefer_wrapped_native_cycle_rotation() {
        let cfg = load_chain_config("bsc").unwrap();
        let usdt = cfg.hub_tokens[0].address;
        let wbnb = cfg.wrapped_native;
        let tok = Address::repeat_byte(0x77);
        let mut market = MarketState::new(cfg);
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        let cand = RouteCandidate {
            strategy: "Cyclic(3hop)".into(),
            base_token: usdt,
            borrow_human: 120.0,
            est_gross_profit_human: 1.5,
            est_gross_profit_usd: 1.5,
            est_net_profit_usd: 1.4,
            flash_mode: 1,
            flash_source: market.flash_pools[0].0.pool,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: Address::repeat_byte(0x01),
                    label: "PancakeV2:USDT->TOK".into(),
                    token_in: usdt,
                    token_out: tok,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 120.0,
                    flow_out_human: 500.0,
                },
                DirectedFlow {
                    pool: Address::repeat_byte(0x02),
                    label: "PancakeV2:TOK->WBNB".into(),
                    token_in: tok,
                    token_out: wbnb,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 500.0,
                    flow_out_human: 0.2,
                },
                DirectedFlow {
                    pool: Address::repeat_byte(0x03),
                    label: "PancakeV3:WBNB->USDT".into(),
                    token_in: wbnb,
                    token_out: usdt,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 0.2,
                    flow_out_human: 121.5,
                },
            ],
            mermaid: String::new(),
        };

        let rotated = prefer_wrapped_native_cycle(&market, cand);
        assert_eq!(rotated.base_token, wbnb);
        assert!((rotated.borrow_human - 0.2).abs() < 1e-12);
        assert_eq!(rotated.flows[0].token_in, wbnb);
        assert_eq!(rotated.flows[2].token_out, wbnb);
        assert_eq!(rotated.flash_mode, 1);
    }

    #[test]
    fn test_prefer_wrapped_native_rotates_unordered_dag_cycle() {
        let cfg = load_chain_config("bsc").unwrap();
        let usdt = cfg.hub_tokens[0].address;
        let wbnb = cfg.wrapped_native;
        let yes = Address::repeat_byte(0x88);
        let mut market = MarketState::new(cfg);
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        // Emitted out of topological/cycle order: [YES->USDT, WBNB->YES, USDT->WBNB]
        let cand = RouteCandidate {
            strategy: "DAG(3edges)".into(),
            base_token: usdt,
            borrow_human: 27.2,
            est_gross_profit_human: 0.23,
            est_gross_profit_usd: 0.23,
            est_net_profit_usd: 0.21,
            flash_mode: 1,
            flash_source: market.flash_pools[0].0.pool,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: Address::repeat_byte(0x11),
                    label: "PancakeV2:YES->USDT".into(),
                    token_in: yes,
                    token_out: usdt,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 100.0,
                    flow_out_human: 27.43,
                },
                DirectedFlow {
                    pool: Address::repeat_byte(0x12),
                    label: "PancakeV2:WBNB->YES".into(),
                    token_in: wbnb,
                    token_out: yes,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 0.04,
                    flow_out_human: 100.0,
                },
                DirectedFlow {
                    pool: Address::repeat_byte(0x13),
                    label: "PancakeV3:USDT->WBNB".into(),
                    token_in: usdt,
                    token_out: wbnb,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 27.43,
                    flow_out_human: 0.0404,
                },
            ],
            mermaid: String::new(),
        };

        let rotated = prefer_wrapped_native_cycle(&market, cand);
        assert_eq!(rotated.base_token, wbnb);
        assert!((rotated.borrow_human - 0.04).abs() < 1e-12);
        assert_eq!(rotated.flows.len(), 3);
        assert_eq!(rotated.flows[0].token_in, wbnb);
        assert_eq!(rotated.flows[0].token_out, yes);
        assert_eq!(rotated.flows[1].token_in, yes);
        assert_eq!(rotated.flows[1].token_out, usdt);
        assert_eq!(rotated.flows[2].token_in, usdt);
        assert_eq!(rotated.flows[2].token_out, wbnb);
    }

    #[test]
    fn test_inline_v2_flash_guards_token_hop_and_factory_and_generic_coalesce() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let usdt = cfg.usd_anchor;
        let moolah = alloy::primitives::address!("8F73b65B4caAf64FBA2aF91cC5D4a2A1318E5D8C");
        let exec = Address::repeat_byte(0xEE);

        let p_v2_a = Address::repeat_byte(0x21);
        let p_v2_b = Address::repeat_byte(0x22);
        let p_mdex = Address::repeat_byte(0x23);
        let p_v3 = Address::repeat_byte(0x24);
        let p_gen = Address::repeat_byte(0x25);
        let tok_a = Address::repeat_byte(0x51);
        let tok_b = Address::repeat_byte(0x52);
        let tok_c = Address::repeat_byte(0x53);

        let mut market = MarketState::new(cfg);
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }
        market.last_block = 1_000;
        let r0 = U256::from(100_000u128) * U256::from(10u128.pow(18));
        let r1 = U256::from(100_000u128) * U256::from(10u128.pow(18));
        let mk_v2 = |addr: Address, fac: &str, t0: Address, t1: Address, fee_num: u16| PoolState {
            address: addr,
            factory_name: fac.into(),
            dex_kind: DexKind::UniV2,
            token0: t0,
            token1: t1,
            dec0: 18,
            dec1: 18,
            fee_num,
            fee_ppm: 2500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: r0,
            r1_raw: r1,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: false,
            cached_depth_usd: 1000.0,
            last_updated_block: 1_000,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        market.insert_pool(mk_v2(p_v2_a, "PancakeV2", wbnb.min(tok_a), wbnb.max(tok_a), 9975));
        market.insert_pool(mk_v2(p_v2_b, "PancakeV1", wbnb.min(tok_a), wbnb.max(tok_a), 9980));
        market.insert_pool(mk_v2(p_mdex, "MdexV2", wbnb.min(tok_b), wbnb.max(tok_b), 9970));
        market.insert_pool(mk_v2(p_gen, "GenericV2", wbnb.min(tok_c), wbnb.max(tok_c), 9975));

        // 1) Token-hopped PancakeV2 -> PancakeV1 keeps steps[0].recipient == p_v2_b, inputInPool == true,
        // and MUST NOT select FLASH_V2_SWAP (flashMode == 5).
        let cand_hop = RouteCandidate {
            strategy: "Cyclic(2hop)".into(),
            base_token: wbnb,
            borrow_human: 0.1,
            est_gross_profit_human: 0.001,
            est_gross_profit_usd: 0.6,
            est_net_profit_usd: 0.59,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: p_v2_a,
                    label: "PancakeV2:WBNB->TOKA".into(),
                    token_in: wbnb,
                    token_out: tok_a,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 0.1,
                    flow_out_human: 100.0,
                },
                DirectedFlow {
                    pool: p_v2_b,
                    label: "PancakeV1:TOKA->WBNB".into(),
                    token_in: tok_a,
                    token_out: wbnb,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9980,
                    flow_in_human: 100.0,
                    flow_out_human: 0.101,
                },
            ],
            mermaid: String::new(),
        };
        let p_hop = build_arb_params(&cand_hop, &market, exec, U256::ONE, true).unwrap();
        assert_eq!(p_hop.flashMode, 1);
        assert_eq!(p_hop.steps[0].recipient, p_v2_b);
        assert!(p_hop.steps[1].inputInPool);

        // 2) MdexV2 -> PancakeV3 has unsupported Mdex swapV2Call callback -> must stay flashMode == 1.
        let cand_mdex = RouteCandidate {
            strategy: "Cyclic(2hop)".into(),
            base_token: wbnb,
            borrow_human: 0.1,
            est_gross_profit_human: 0.001,
            est_gross_profit_usd: 0.6,
            est_net_profit_usd: 0.59,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: p_mdex,
                    label: "MdexV2:WBNB->TOKB".into(),
                    token_in: wbnb,
                    token_out: tok_b,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9970,
                    flow_in_human: 0.1,
                    flow_out_human: 100.0,
                },
                DirectedFlow {
                    pool: p_v3,
                    label: "PancakeV3:TOKB->WBNB".into(),
                    token_in: tok_b,
                    token_out: wbnb,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 100.0,
                    flow_out_human: 0.101,
                },
            ],
            mermaid: String::new(),
        };
        let p_mdex_params = build_arb_params(&cand_mdex, &market, exec, U256::ONE, true).unwrap();
        assert_eq!(p_mdex_params.flashMode, 1);

        // 3) PancakeV2 -> PancakeV3 (recipient == exec) safely uses FLASH_V2_SWAP (5).
        let cand_safe_v2 = RouteCandidate {
            strategy: "Cyclic(2hop)".into(),
            base_token: wbnb,
            borrow_human: 0.1,
            est_gross_profit_human: 0.001,
            est_gross_profit_usd: 0.6,
            est_net_profit_usd: 0.59,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: p_v2_a,
                    label: "PancakeV2:WBNB->TOKA".into(),
                    token_in: wbnb,
                    token_out: tok_a,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 0.1,
                    flow_out_human: 100.0,
                },
                DirectedFlow {
                    pool: p_v3,
                    label: "PancakeV3:TOKA->WBNB".into(),
                    token_in: tok_a,
                    token_out: wbnb,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 100.0,
                    flow_out_human: 0.101,
                },
            ],
            mermaid: String::new(),
        };
        let p_safe = build_arb_params(&cand_safe_v2, &market, exec, U256::ONE, true).unwrap();
        assert_eq!(p_safe.flashMode, 5);
        assert_eq!(p_safe.steps[0].recipient, exec);

        // 4) coalesce_multi_branch_dags skips unverified GenericV2 branch until sim_verified_pools contains it.
        let p_v2_d = Address::repeat_byte(0x26);
        let mut cand_clean_2 = cand_safe_v2.clone();
        cand_clean_2.flows[0].pool = p_v2_d;
        cand_clean_2.flows[0].token_out = usdt;
        cand_clean_2.flows[1].pool = Address::repeat_byte(0x28);
        cand_clean_2.flows[1].token_in = usdt;
        let cand_gen = RouteCandidate {
            strategy: "Cyclic(2hop)".into(),
            base_token: wbnb,
            borrow_human: 0.1,
            est_gross_profit_human: 0.001,
            est_gross_profit_usd: 0.6,
            est_net_profit_usd: 0.59,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: p_gen,
                    label: "GenericV2:WBNB->TOKC".into(),
                    token_in: wbnb,
                    token_out: tok_c,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 0.1,
                    flow_out_human: 100.0,
                },
                DirectedFlow {
                    pool: Address::repeat_byte(0x27),
                    label: "PancakeV3:TOKC->WBNB".into(),
                    token_in: tok_c,
                    token_out: wbnb,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 100.0,
                    flow_out_human: 0.101,
                },
            ],
            mermaid: String::new(),
        };
        let cands = vec![cand_safe_v2.clone(), cand_gen.clone(), cand_clean_2.clone()];
        let coalesced_before = coalesce_multi_branch_dags(&market, &cands, 0.0008);
        assert_eq!(coalesced_before.len(), 1);
        assert!(coalesced_before[0].flows.iter().all(|f| f.pool != p_gen));

        market.record_sim_success(&[p_gen], &[tok_c]);
        let coalesced_after = coalesce_multi_branch_dags(&market, &[cand_safe_v2, cand_gen], 0.0008);
        assert_eq!(coalesced_after.len(), 1);
        assert!(coalesced_after[0].flows.iter().any(|f| f.pool == p_gen));
    }

    #[test]
    fn test_periodic_sweep_prioritizes_recent_active_pools_over_cold_cache_and_clears_dirty_filter() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let usdt = cfg.usd_anchor;
        let mut market = MarketState::new(cfg);
        market.last_block = 1_608; // 1608 % 12 == 0 -> periodic_sweep
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        let scale18 = U256::from(10u128.pow(18));
        // 800 cold high-depth cached pools (depth ~$50k, fair rate -> zero arb) at last_updated_block = 400
        for i in 1..=800u32 {
            let mut b_tok = [0u8; 20];
            b_tok[0..4].copy_from_slice(&i.to_be_bytes());
            b_tok[19] = 0xAA;
            let tok = Address::from(b_tok);

            let mut b_pool = [0u8; 20];
            b_pool[0..4].copy_from_slice(&i.to_be_bytes());
            b_pool[19] = 0xBB;
            let addr = Address::from(b_pool);

            let (t0, t1, r0, r1) = if usdt < tok {
                (usdt, tok, U256::from(25_000u128) * scale18, U256::from(25_000u128) * scale18)
            } else {
                (tok, usdt, U256::from(25_000u128) * scale18, U256::from(25_000u128) * scale18)
            };
            market.insert_pool(PoolState {
                address: addr,
                factory_name: "PancakeV2".into(),
                dex_kind: DexKind::UniV2,
                token0: t0,
                token1: t1,
                dec0: 18,
                dec1: 18,
                fee_num: 9975,
                fee_ppm: 2500,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: r0,
                r1_raw: r1,
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 0,
                allow_multi_tick: false,
                is_hub_0: false,
                is_hub_1: false,
                cached_depth_usd: 50_000.0,
                last_updated_block: 400,
                v4_key: None,
                metric: None,
                balancer: None,
            });
        }

        // 5 unrelated dummy dirty pools in block 1600 so periodic_sweep activates (dp.len() == 5)
        let mut dp: HashSet<Address> = HashSet::new();
        for j in 1..=5u8 {
            dp.insert(Address::repeat_byte(0xC0 + j));
        }

        // Two recently active (~$300 depth, last_updated_block = 1595) resting non-hub branches
        // outside `dp`, each with ~$0.0102 gross (< $0.006 standalone net) that coalesce into DAG(4steps) >= $0.006.
        for (idx, (t_b, p1_b, p2_b)) in [
            (0xD1u8, 0xE1u8, 0xF1u8),
            (0xD2u8, 0xE2u8, 0xF2u8),
        ]
        .into_iter()
        .enumerate()
        {
            let _ = idx;
            let tok = Address::repeat_byte(t_b);
            // Branch leg 1 (PancakeV2): 0.25 WBNB ($155) <-> 155 TOK -> $310 depth
            let p1 = Address::repeat_byte(p1_b);
            let (t0a, t1a, r0a, r1a) = if wbnb < tok {
                (
                    wbnb,
                    tok,
                    U256::from(250_000_000_000_000_000u128),
                    U256::from(155u128) * scale18,
                )
            } else {
                (
                    tok,
                    wbnb,
                    U256::from(155u128) * scale18,
                    U256::from(250_000_000_000_000_000u128),
                )
            };
            market.insert_pool(PoolState {
                address: p1,
                factory_name: "PancakeV2".into(),
                dex_kind: DexKind::UniV2,
                token0: t0a,
                token1: t1a,
                dec0: 18,
                dec1: 18,
                fee_num: 9975,
                fee_ppm: 2500,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: r0a,
                r1_raw: r1a,
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 0,
                allow_multi_tick: false,
                is_hub_0: false,
                is_hub_1: false,
                cached_depth_usd: 310.0,
                last_updated_block: 1_595,
                v4_key: None,
                metric: None,
                balancer: None,
            });
            // Branch leg 2 (BiSwapV2, skewed +12% WBNB return): 0.28 WBNB <-> 155 TOK -> ~$347 depth
            let p2 = Address::repeat_byte(p2_b);
            let (t0b, t1b, r0b, r1b) = if wbnb < tok {
                (
                    wbnb,
                    tok,
                    U256::from(280_000_000_000_000_000u128),
                    U256::from(155u128) * scale18,
                )
            } else {
                (
                    tok,
                    wbnb,
                    U256::from(155u128) * scale18,
                    U256::from(280_000_000_000_000_000u128),
                )
            };
            market.insert_pool(PoolState {
                address: p2,
                factory_name: "BiSwapV2".into(),
                dex_kind: DexKind::UniV2,
                token0: t0b,
                token1: t1b,
                dec0: 18,
                dec1: 18,
                fee_num: 9980,
                fee_ppm: 2000,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: r0b,
                r1_raw: r1b,
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 0,
                allow_multi_tick: false,
                is_hub_0: false,
                is_hub_1: false,
                cached_depth_usd: 347.0,
                last_updated_block: 1_595,
                v4_key: None,
                metric: None,
                balancer: None,
            });
        }
        market.refresh_token_usd_prices_passes(2);

        let cands = solve_all(&market, Some(&dp), 0.006);
        assert!(
            cands.iter().any(|c| c.strategy == "DAG(4steps)"
                || c.flows.len() >= 4),
            "expected periodic_sweep to retain recent $310 pools ahead of 800 cold $50k pools and coalesce resting branches, got: {:?}",
            cands.iter().map(|c| (&c.strategy, c.est_net_profit_usd)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_two_core_strips_deg1_and_retains_aged_quiet_pairs() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let usdt = cfg.usd_anchor;
        let mut market = MarketState::new(cfg);
        market.last_block = 4_800; // 4800 % 12 == 0 -> periodic_sweep
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        let scale18 = U256::from(10u128.pow(18));
        // Insert 3,700 deg==1 dead-end pools: half brand new ($50k depth at block 4800) so sweep_cap would
        // otherwise overflow, and half 3,000 blocks old (block 1800) to test prune_dead_pools.
        let mut fresh_deg1_addr = Address::ZERO;
        let mut stale_deg1_addr = Address::ZERO;
        for i in 1..=3_700u32 {
            let mut b_tok = [0u8; 20];
            b_tok[0..4].copy_from_slice(&i.to_be_bytes());
            b_tok[19] = 0xA1;
            let tok = Address::from(b_tok);

            let mut b_pool = [0u8; 20];
            b_pool[0..4].copy_from_slice(&i.to_be_bytes());
            b_pool[19] = 0xB1;
            let addr = Address::from(b_pool);

            let lub = if i <= 1_300 { 4_800 } else { 1_800 };
            if fresh_deg1_addr.is_zero() && lub == 4_800 {
                fresh_deg1_addr = addr;
            }
            if stale_deg1_addr.is_zero() && lub == 1_800 {
                stale_deg1_addr = addr;
            }
            let side_human = if lub == 4_800 { 25_000u128 } else { 1_000u128 };
            let (t0, t1, r0, r1) = if usdt < tok {
                (usdt, tok, U256::from(side_human) * scale18, U256::from(side_human) * scale18)
            } else {
                (tok, usdt, U256::from(side_human) * scale18, U256::from(side_human) * scale18)
            };
            market.insert_pool(PoolState {
                address: addr,
                factory_name: "PancakeV2".into(),
                dex_kind: DexKind::UniV2,
                token0: t0,
                token1: t1,
                dec0: 18,
                dec1: 18,
                fee_num: 9975,
                fee_ppm: 2500,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: r0,
                r1_raw: r1,
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 0,
                allow_multi_tick: false,
                is_hub_0: false,
                is_hub_1: false,
                cached_depth_usd: (side_human as f64) * 2.0,
                last_updated_block: lub,
                v4_key: None,
                metric: None,
                balancer: None,
            });
        }

        // Insert two 3,000-block-aged (last_updated_block = 1800) deg>=2 official arbitrageable pairs
        let mut quiet_addrs = Vec::new();
        for (t_b, p1_b, p2_b) in [(0xD3u8, 0xE3u8, 0xF3u8), (0xD4u8, 0xE4u8, 0xF4u8)] {
            let tok = Address::repeat_byte(t_b);
            let p1 = Address::repeat_byte(p1_b);
            let p2 = Address::repeat_byte(p2_b);
            quiet_addrs.push(p1);
            quiet_addrs.push(p2);

            for (addr, fac, wbnb_amt) in [
                (p1, "PancakeV2", 250_000_000_000_000_000u128),
                (p2, "BiSwapV2", 280_000_000_000_000_000u128),
            ] {
                let (t0, t1, r0, r1) = if wbnb < tok {
                    (wbnb, tok, U256::from(wbnb_amt), U256::from(155u128) * scale18)
                } else {
                    (tok, wbnb, U256::from(155u128) * scale18, U256::from(wbnb_amt))
                };
                market.insert_pool(PoolState {
                    address: addr,
                    factory_name: fac.into(),
                    dex_kind: DexKind::UniV2,
                    token0: t0,
                    token1: t1,
                    dec0: 18,
                    dec1: 18,
                    fee_num: if fac == "BiSwapV2" { 9980 } else { 9975 },
                    fee_ppm: if fac == "BiSwapV2" { 2000 } else { 2500 },
                    extra_tax_ppm: 0,
                    stable: false,
                    r0_raw: r0,
                    r1_raw: r1,
                    sqrt_price_x96: U256::ZERO,
                    liquidity: 0,
                    tick: 0,
                    tick_spacing: 0,
                    allow_multi_tick: false,
                    is_hub_0: false,
                    is_hub_1: false,
                    cached_depth_usd: 320.0,
                    last_updated_block: 1_800,
                    v4_key: None,
                    metric: None,
                    balancer: None,
                });
            }
        }
        market.refresh_token_usd_prices_passes(2);

        assert!(!market.is_pool_arbitrageable(&market.pools[&fresh_deg1_addr]));
        for &qa in &quiet_addrs {
            assert!(market.is_pool_arbitrageable(&market.pools[&qa]));
        }

        let mut dp = HashSet::new();
        dp.insert(fresh_deg1_addr);
        let cands = solve_all(&market, Some(&dp), 0.006);
        assert!(
            cands.iter().any(|c| c.strategy == "DAG(4steps)" || c.flows.len() >= 4),
            "expected periodic_sweep to strip fresh deg==1 $50k pools and surface aged quiet deg>=2 pairs, got: {:?}",
            cands.iter().map(|c| (&c.strategy, c.est_net_profit_usd)).collect::<Vec<_>>()
        );

        market.prune_dead_pools();
        assert!(!market.pools.contains_key(&stale_deg1_addr));
        assert!(market.pools.contains_key(&fresh_deg1_addr));
        for qa in quiet_addrs {
            assert!(
                market.pools.contains_key(&qa),
                "expected aged deg>=2 arbitrageable pool {qa} to survive prune_dead_pools"
            );
        }
    }

    #[test]
    fn test_metric_omm_unsorted_flash_mode_and_extradata() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native; // 0xbb4c... (> usdt!)
        let usdt = cfg.usd_anchor;     // 0x55d3...
        assert!(wbnb > usdt, "test assumes unsorted MetricOmm token0=WBNB > token1=USDT");

        let mut market = MarketState::new(cfg);
        market.last_block = 2_000;
        let metric_pool = Address::repeat_byte(0x91);
        let pcs_v2 = Address::repeat_byte(0x92);
        let unit = U256::from(10u128.pow(18));

        market.insert_pool(PoolState {
            address: metric_pool,
            factory_name: "Metric".into(),
            dex_kind: DexKind::MetricOmm,
            token0: wbnb, // unsorted: WBNB
            token1: usdt, // unsorted: USDT
            dec0: 18,
            dec1: 18,
            fee_num: 10_000,
            fee_ppm: 0,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::from(500u128) * unit,
            r1_raw: U256::from(300_000u128) * unit,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 1,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 600_000.0,
            last_updated_block: 2_000,
            v4_key: None,
            metric: Some(crate::amm::MetricPoolState {
                sell_price_x64: 550u128 << 64,
                buy_price_x64: 551u128 << 64,
                num_bins: 40,
                active_bin: 0,
                paused: false,
            }),
            balancer: None,
        });
        market.insert_pool(PoolState {
            address: pcs_v2,
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
            r0_raw: U256::from(520_000u128) * unit,
            r1_raw: U256::from(1_000u128) * unit,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 1_000_000.0,
            last_updated_block: 2_000,
            v4_key: None,
            metric: None,
            balancer: None,
        });

        let cand_fwd = RouteCandidate {
            strategy: "Cyclic(Metric->V2)".into(),
            base_token: wbnb,
            borrow_human: 1.0,
            est_gross_profit_human: 0.05,
            est_gross_profit_usd: 27.5,
            est_net_profit_usd: 27.4,
            flash_mode: 1,
            flash_source: Address::repeat_byte(0x11),
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: metric_pool,
                    label: "Metric:WBNB->USDT".into(),
                    token_in: wbnb,
                    token_out: usdt,
                    dex_kind: DexKind::MetricOmm,
                    fee_num: 10_000,
                    flow_in_human: 1.0,
                    flow_out_human: 550.0,
                },
                DirectedFlow {
                    pool: pcs_v2,
                    label: "PancakeV2:USDT->WBNB".into(),
                    token_in: usdt,
                    token_out: wbnb,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 550.0,
                    flow_out_human: 1.05,
                },
            ],
            mermaid: String::new(),
        };
        let params_fwd = build_arb_params(&cand_fwd, &market, Address::repeat_byte(0xaa), U256::from(1u64), false)
            .expect("compile forward Metric route");
        assert_eq!(params_fwd.flashMode, 6);
        assert_eq!(params_fwd.flashSource, metric_pool);
        assert_eq!(params_fwd.steps[0].dexKind, 6);
        assert_eq!(params_fwd.steps[0].extraData, alloy::primitives::Bytes::from(vec![1u8]));

        let cand_rev = RouteCandidate {
            strategy: "Cyclic(MetricRev->V2)".into(),
            base_token: usdt,
            borrow_human: 550.0,
            est_gross_profit_human: 10.0,
            est_gross_profit_usd: 10.0,
            est_net_profit_usd: 9.9,
            flash_mode: 1,
            flash_source: Address::repeat_byte(0x11),
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: metric_pool,
                    label: "Metric:USDT->WBNB".into(),
                    token_in: usdt,
                    token_out: wbnb,
                    dex_kind: DexKind::MetricOmm,
                    fee_num: 10_000,
                    flow_in_human: 550.0,
                    flow_out_human: 1.0,
                },
                DirectedFlow {
                    pool: pcs_v2,
                    label: "PancakeV2:WBNB->USDT".into(),
                    token_in: wbnb,
                    token_out: usdt,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 1.0,
                    flow_out_human: 560.0,
                },
            ],
            mermaid: String::new(),
        };
        let params_rev = build_arb_params(&cand_rev, &market, Address::repeat_byte(0xaa), U256::from(1u64), false)
            .expect("compile reverse Metric route");
        assert_eq!(params_rev.flashMode, 6);
        assert_eq!(params_rev.flashSource, metric_pool);
        assert_eq!(params_rev.steps[0].dexKind, 6);
        assert_eq!(params_rev.steps[0].extraData, alloy::primitives::Bytes::from(vec![0u8]));
    }

    #[test]
    fn test_balancer_v2_populate_singleton_steps_and_flash_guard() {
        let cfg = load_chain_config("eth").unwrap();
        let weth = cfg.wrapped_native;
        let usdc = cfg.usd_anchor;
        let bal_vault = cfg.balancer_v2_vault().expect("ETH config must define balancer_v2 vault");
        let pool_id = alloy::primitives::b256!(
            "06df3b2bbb68adc8b0e302443692037ed9f91b42000000000000000000000063"
        );
        let synth_pair = crate::discovery::balancer_pair_address(bal_vault, pool_id, weth, usdc);

        let mut market = MarketState::new(cfg);
        market.last_block = 26_000_000;
        let (t0, t1, d0, d1, r0, r1) = if usdc < weth {
            (
                usdc,
                weth,
                6u8,
                18u8,
                U256::from(3_000_000u128) * U256::from(10u128.pow(6)),
                U256::from(1_000u128) * U256::from(10u128.pow(18)),
            )
        } else {
            (
                weth,
                usdc,
                18u8,
                6u8,
                U256::from(1_000u128) * U256::from(10u128.pow(18)),
                U256::from(3_000_000u128) * U256::from(10u128.pow(6)),
            )
        };
        market.insert_pool(PoolState {
            address: synth_pair,
            factory_name: "BalancerV2".into(),
            dex_kind: DexKind::BalancerV2,
            token0: t0,
            token1: t1,
            dec0: d0,
            dec1: d1,
            fee_num: 9996,
            fee_ppm: 400,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: r0,
            r1_raw: r1,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 6_000_000.0,
            last_updated_block: 26_000_000,
            v4_key: None,
            metric: None,
            balancer: Some(crate::amm::BalancerPoolState {
                vault: bal_vault,
                pool_id,
                kind: crate::amm::BalancerPoolKind::Weighted { w0: 0.5, w1: 0.5 },
            }),
        });

        let exit_v2 = Address::repeat_byte(0x77);
        market.insert_pool(PoolState {
            address: exit_v2,
            factory_name: "UniV2".into(),
            dex_kind: DexKind::UniV2,
            token0: t0,
            token1: t1,
            dec0: d0,
            dec1: d1,
            fee_num: 9970,
            fee_ppm: 3000,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: r0,
            r1_raw: r1,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 6_000_000.0,
            last_updated_block: 26_000_000,
            v4_key: None,
            metric: None,
            balancer: None,
        });

        // Give only BalancerV2 flash source balance first; Morpho = 0 => selecting while using synth_pair must skip flash_mode=7!
        for (fp, bal) in &mut market.flash_pools {
            if fp.flash_mode == 7 && fp.base_token == weth {
                *bal = U256::from(100u128) * U256::from(10u128.pow(18));
            } else {
                *bal = U256::ZERO;
            }
        }
        assert!(market.select_flash_source(weth, U256::from(10u128.pow(18)), &[synth_pair]).is_none());
        let ok_flash = market
            .select_flash_source(weth, U256::from(10u128.pow(18)), &[exit_v2])
            .expect("non-Balancer route can use flash_mode 7");
        assert_eq!(ok_flash.flash_mode, 7);
        assert_eq!(ok_flash.pool, bal_vault);

        // Give Morpho liquidity and verify build_arb_params switches away from flash_mode 7 when BalancerV2 pool is in route,
        // and rewrites step.pool -> bal_vault + step.extraData -> 32-byte pool_id.
        for (fp, bal) in &mut market.flash_pools {
            if fp.flash_mode == 1 && fp.base_token == weth {
                *bal = U256::from(100u128) * U256::from(10u128.pow(18));
            }
        }
        let cand = RouteCandidate {
            strategy: "Cyclic(Bal->V2)".into(),
            base_token: weth,
            borrow_human: 1.0,
            est_gross_profit_human: 0.01,
            est_gross_profit_usd: 30.0,
            est_net_profit_usd: 29.5,
            flash_mode: 7,
            flash_source: bal_vault,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: synth_pair,
                    label: "BalancerV2:WETH->USDC".into(),
                    token_in: weth,
                    token_out: usdc,
                    dex_kind: DexKind::BalancerV2,
                    fee_num: 9996,
                    flow_in_human: 1.0,
                    flow_out_human: 3000.0,
                },
                DirectedFlow {
                    pool: exit_v2,
                    label: "UniV2:USDC->WETH".into(),
                    token_in: usdc,
                    token_out: weth,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9970,
                    flow_in_human: 3000.0,
                    flow_out_human: 1.02,
                },
            ],
            mermaid: String::new(),
        };
        let params = build_arb_params(&cand, &market, Address::repeat_byte(0xaa), U256::from(1u64), true)
            .expect("compile BalancerV2 route");
        assert_eq!(params.flashMode, 1);
        assert_eq!(params.steps[0].dexKind, 7);
        assert_eq!(params.steps[0].pool, bal_vault);
        assert_eq!(params.steps[0].extraData.as_ref(), pool_id.as_slice());
    }

    #[test]
    fn test_encode_compact_arb_layout_and_size() {
        use alloy::sol_types::SolCall;

        let exec = Address::repeat_byte(0xee);
        let weth = Address::repeat_byte(0x01);
        let usdc = Address::repeat_byte(0x02);
        let pool_v2 = Address::repeat_byte(0x10);
        let pool_v3 = Address::repeat_byte(0x20);
        let morpho = Address::repeat_byte(0x30);
        let v4_mgr = Address::repeat_byte(0x40);
        let hooks = Address::repeat_byte(0x55);

        let v4_key = crate::amm::V4PoolKey {
            currency0: Address::ZERO,
            currency1: usdc,
            manager: v4_mgr,
            vault: v4_mgr,
            pool_id: alloy::primitives::B256::ZERO,
            fee: 500,
            tick_spacing: -60,
            hooks,
            parameters: alloy::primitives::B256::ZERO,
        };

        let params = ArbParams {
            flashMode: 1,
            flashSource: morpho,
            baseToken: weth,
            borrowAmount: U256::from(1_000_000_000_000_000_000u128),
            minNetProfit: U256::from(1_000_000_000_000u128),
            payoutToOwner: false,
            wrappedNative: weth,
            bribeRecipient: Address::ZERO,
            bribeBps: 9500,
            steps: vec![
                DagStep {
                    pool: pool_v2,
                    tokenIn: weth,
                    tokenOut: usdc,
                    dexKind: DexKind::UniV2.as_u8(),
                    inputInPool: false,
                    feeNumerator: 9970,
                    shareBps: 10_000,
                    recipient: pool_v3,
                    extraData: Bytes::new(),
                },
                DagStep {
                    pool: pool_v3,
                    tokenIn: usdc,
                    tokenOut: weth,
                    dexKind: DexKind::UniV3.as_u8(),
                    inputInPool: true,
                    feeNumerator: 0,
                    shareBps: 5000,
                    recipient: exec,
                    extraData: Bytes::new(),
                },
                DagStep {
                    pool: v4_mgr,
                    tokenIn: weth,
                    tokenOut: usdc,
                    dexKind: DexKind::UniV4.as_u8(),
                    inputInPool: false,
                    feeNumerator: 0,
                    shareBps: 10_000,
                    recipient: v4_mgr,
                    extraData: v4_key.encode_extra_data(DexKind::UniV4),
                },
            ],
        };

        let compact = encode_compact_arb(&params, exec).expect("encode compact");
        let abi_bytes = crate::abi::IDagArbExecutor::executeArbCall { params }.abi_encode();
        assert_eq!(compact[0], 0x00, "magic byte must be 0x00");
        assert!(
            compact.len() < abi_bytes.len() / 3,
            "compact {} should be < 1/3 of ABI {}",
            compact.len(),
            abi_bytes.len()
        );
        // Header check: flashMode=1, flags=0x0a (wrappedNative + flashSource), bribeBps=9500
        assert_eq!(compact[1], 1);
        assert_eq!(compact[2], 0x0a);
        assert_eq!(u16::from_be_bytes([compact[3], compact[4]]), 9500);
        // After 37B fixed + 40B optional addrs = offset 77: numTokens=2, tokTable=40B, numSteps=3
        assert_eq!(compact[77], 2);
        assert_eq!(compact[118], 3);
        // Step 0 header: dex=0 | recIsNext=0x20 | shareFull=0x40 => 0x60
        assert_eq!(compact[119], 0x60);
        // Step 0 ends at 119 + 1(hdr) + 1(idx) + 20(pool) + 2(feeNum) = 143
        // Step 1 header: dex=2 | inputInPool=0x10 => 0x12, idx = (1<<4)|0 = 0x10
        assert_eq!(compact[143], 0x12);
        assert_eq!(compact[144], 0x10);
        // Step 2 (UniV4, starts at 143 + 1 + 1 + 20 + 2 = 167): c0IsNat=1, tickSpacing=-60 (0xffffc4)
        assert_eq!(compact[167], 0x45);
        assert_eq!(compact[189], 1);
        assert_eq!(&compact[193..196], &[0xff, 0xff, 0xc4]);
    }
}
