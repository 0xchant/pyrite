use alloy::primitives::Address;
use std::collections::{HashMap, HashSet};

use crate::amm::{human_to_u256, PoolState};
use crate::discovery::MarketState;
use crate::router::{render_mermaid, DirectedFlow, RouteCandidate, SolverPruningConfig};

#[derive(Clone, Copy)]
struct EdgeRef<'a> {
    pool: &'a PoolState,
    token_in: Address,
    token_out: Address,
    out_id: usize,
    zero_for_one: bool,
    spot_rate: f64,
    max_in_usd: f64,
    is_hub_pair: bool,
    is_dirty: bool,
}

/// Fast O(E + V) Meet-in-the-Middle cyclic arbitrage finder (2..4 hops)
/// with cached depth bounds and feasible-domain Golden-Section optimal sizing.
#[allow(dead_code)]
pub fn solve_cyclic(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    solve_cyclic_with_config(
        market,
        routable_pools,
        dirty_pools,
        min_profit_usd,
        &SolverPruningConfig::default(),
    )
}

pub fn solve_cyclic_with_config(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
    pruning: &SolverPruningConfig,
) -> Vec<RouteCandidate> {
    if dirty_pools.is_some_and(|dp| dp.is_empty()) {
        return Vec::new();
    }

    // 1) Assign compact node IDs and group active liquid pools per directed `(token_in, token_out)`
    let mut id_of: HashMap<Address, usize> = HashMap::new();
    let mut get_id = |addr: Address| -> usize {
        if let Some(&id) = id_of.get(&addr) {
            id
        } else {
            let id = id_of.len();
            id_of.insert(addr, id);
            id
        }
    };

    struct RawEdge<'a> {
        pool: &'a PoolState,
        token_in: Address,
        token_out: Address,
        zero_for_one: bool,
        spot_rate: f64,
        depth_usd: f64,
        max_in_usd: f64,
    }

    let mut pair_buckets: HashMap<(Address, Address), Vec<RawEdge<'_>>> = HashMap::new();
    for &pool in routable_pools {
        let depth = pool.cached_depth_usd;
        let r01 = pool.marginal_rate_human(pool.token0);
        let r10 = pool.marginal_rate_human(pool.token1);
        let u0 = market.token_usd(pool.token0);
        let u1 = market.token_usd(pool.token1);
        let uncal = (!market.is_hub_token(pool.token0)
            && !market.token_tax_ppm.contains_key(&pool.token0))
            || (!market.is_hub_token(pool.token1) && !market.token_tax_ppm.contains_key(&pool.token1));
        let ok01 = !(uncal && u0 > 0.0 && u1 > 0.0 && r01 * (u1 / u0) > 1.35);
        let ok10 = !(uncal && u0 > 0.0 && u1 > 0.0 && r10 * (u0 / u1) > 1.35);
        if ok01 && r01 > 0.0 && r01.is_finite() {
            pair_buckets
                .entry((pool.token0, pool.token1))
                .or_default()
                .push(RawEdge {
                    pool,
                    token_in: pool.token0,
                    token_out: pool.token1,
                    zero_for_one: true,
                    spot_rate: r01,
                    depth_usd: depth,
                    max_in_usd: pool.max_input_usd(pool.token0, u0),
                });
        }
        if ok10 && r10 > 0.0 && r10.is_finite() {
            pair_buckets
                .entry((pool.token1, pool.token0))
                .or_default()
                .push(RawEdge {
                    pool,
                    token_in: pool.token1,
                    token_out: pool.token0,
                    zero_for_one: false,
                    spot_rate: r10,
                    depth_usd: depth,
                    max_in_usd: pool.max_input_usd(pool.token1, u1),
                });
        }
    }

    for &(tin, tout) in pair_buckets.keys() {
        get_id(tin);
        get_id(tout);
    }
    let anchors = market.base_anchors();
    for &b in &anchors {
        get_id(b);
    }
    let n_nodes = id_of.len();
    if n_nodes == 0 {
        return Vec::new();
    }

    // Deduplicate parallel pools per directed `(token_in, token_out)` pair:
    // keep top 2 by spot_rate + top 2 by depth_usd if different (max 4 per pair).
    let mut adj: Vec<Vec<EdgeRef<'_>>> = vec![Vec::new(); n_nodes];
    for ((tin, tout), mut bucket) in pair_buckets {
        bucket.sort_by(|a, b| {
            b.spot_rate
                .partial_cmp(&a.spot_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut best_d1: Option<usize> = None;
        let mut best_d2: Option<usize> = None;
        for (i, re) in bucket.iter().enumerate() {
            if best_d1.map_or(true, |b1| re.depth_usd > bucket[b1].depth_usd) {
                best_d2 = best_d1;
                best_d1 = Some(i);
            } else if best_d2.map_or(true, |b2| re.depth_usd > bucket[b2].depth_usd) {
                best_d2 = Some(i);
            }
        }

        let in_id = id_of[&tin];
        let out_id = id_of[&tout];
        let is_hub_pair = market.is_hub_token(tin) && market.is_hub_token(tout);
        let mut pushed = 0usize;
        for (i, re) in bucket.iter().enumerate() {
            if i < 2 || Some(i) == best_d1 || Some(i) == best_d2 {
                let is_dirty = dirty_pools.map_or(true, |dp| dp.contains(&re.pool.address));
                adj[in_id].push(EdgeRef {
                    pool: re.pool,
                    token_in: re.token_in,
                    token_out: re.token_out,
                    out_id,
                    zero_for_one: re.zero_for_one,
                    spot_rate: re.spot_rate,
                    max_in_usd: re.max_in_usd,
                    is_hub_pair,
                    is_dirty,
                });
                pushed += 1;
                if pushed >= 4 {
                    break;
                }
            }
        }
    }

    let min_upper_usd = min_profit_usd + 0.008;
    let mut out = Vec::new();
    let sweep_hub_drift = market.last_block % 16 == 0;

    for (anchor_idx, base) in anchors.into_iter().take(5).enumerate() {
        let base_usd = market.token_usd(base);
        if base_usd <= 0.0 {
            continue;
        }
        let start_id = id_of[&base];
        if adj[start_id].is_empty() {
            continue;
        }
        if anchor_idx >= 2
            && market.last_block % 4 != 0
            && !dirty_pools.map_or(true, |dp| adj[start_id].iter().any(|e| dp.contains(&e.pool.address)))
        {
            continue;
        }

        // Flat fixed-capacity shells up to 8 edges/pairs per intermediate node — preserve deep hub pools
        let s1 = pruning.cyclic_shell_1.clamp(1, 8);
        let s2 = pruning.cyclic_shell_2.clamp(1, 8);
        let mut from_1: Vec<[Option<EdgeRef<'_>>; 8]> = vec![[None; 8]; n_nodes];
        for &e in &adj[start_id] {
            if e.out_id != start_id {
                insert_top_k(&mut from_1[e.out_id][..s1], e, |x| x.spot_rate);
            }
        }

        let mut to_1: Vec<[Option<EdgeRef<'_>>; 8]> = vec![[None; 8]; n_nodes];
        for (u, edges) in adj.iter().enumerate() {
            if u == start_id {
                continue;
            }
            for &e in edges {
                if e.out_id == start_id {
                    insert_top_k(&mut to_1[u][..s1], e, |x| x.spot_rate);
                }
            }
        }

        let mut from_2: Vec<[Option<(f64, EdgeRef<'_>, EdgeRef<'_>)>; 8]> =
            vec![[None; 8]; n_nodes];
        for (c, firsts) in from_1.iter().enumerate() {
            if c == start_id {
                continue;
            }
            for e1 in firsts.iter().flatten() {
                for &e2 in &adj[c] {
                    let u = e2.out_id;
                    if u != start_id && u != c && e1.pool.address != e2.pool.address {
                        insert_top_k(
                            &mut from_2[u][..s2],
                            (e1.spot_rate * e2.spot_rate, *e1, e2),
                            |x| x.0,
                        );
                    }
                }
            }
        }

        let mut to_2: Vec<[Option<(f64, EdgeRef<'_>, EdgeRef<'_>)>; 8]> =
            vec![[None; 8]; n_nodes];
        for (u, edges) in adj.iter().enumerate() {
            if u == start_id {
                continue;
            }
            for &e1 in edges {
                let d = e1.out_id;
                if d == start_id || d == u {
                    continue;
                }
                for e2 in to_1[d].iter().flatten() {
                    if e1.pool.address != e2.pool.address {
                        insert_top_k(
                            &mut to_2[u][..s2],
                            (e1.spot_rate * e2.spot_rate, e1, *e2),
                            |x| x.0,
                        );
                    }
                }
            }
        }

        // Meet-in-the-Middle join over midpoint `u`
        let mut raw_cycles: Vec<(f64, Vec<EdgeRef<'_>>)> = Vec::new();
        for u in 0..n_nodes {
            if u == start_id {
                continue;
            }
            // 2-hop: from_1[u] x to_1[u]
            for e1 in from_1[u].iter().flatten() {
                for e2 in to_1[u].iter().flatten() {
                    if e1.pool.address != e2.pool.address {
                        consider_raw_cycle(
                            &[*e1, *e2],
                            sweep_hub_drift,
                            min_upper_usd,
                            &mut raw_cycles,
                        );
                    }
                }
            }
            // 3-hop (a): from_1[u] x to_2[u]
            for e1 in from_1[u].iter().flatten() {
                for &(_, e2, e3) in to_2[u].iter().flatten() {
                    if e2.out_id != u
                        && e1.pool.address != e2.pool.address
                        && e1.pool.address != e3.pool.address
                    {
                        consider_raw_cycle(
                            &[*e1, e2, e3],
                            sweep_hub_drift,
                            min_upper_usd,
                            &mut raw_cycles,
                        );
                    }
                }
            }
            // 3-hop (b): from_2[u] x to_1[u]
            for &(_, e1, e2) in from_2[u].iter().flatten() {
                for e3 in to_1[u].iter().flatten() {
                    if e1.out_id != u
                        && e3.pool.address != e1.pool.address
                        && e3.pool.address != e2.pool.address
                    {
                        consider_raw_cycle(
                            &[e1, e2, *e3],
                            sweep_hub_drift,
                            min_upper_usd,
                            &mut raw_cycles,
                        );
                    }
                }
            }
            // 4-hop: from_2[u] x to_2[u]
            for &(_, e1, e2) in from_2[u].iter().flatten() {
                for &(_, e3, e4) in to_2[u].iter().flatten() {
                    let c = e1.out_id;
                    let d = e3.out_id;
                    if c != d
                        && c != u
                        && d != u
                        && e1.pool.address != e3.pool.address
                        && e1.pool.address != e4.pool.address
                        && e2.pool.address != e3.pool.address
                        && e2.pool.address != e4.pool.address
                    {
                        consider_raw_cycle(
                            &[e1, e2, e3, e4],
                            sweep_hub_drift,
                            min_upper_usd,
                            &mut raw_cycles,
                        );
                    }
                }
            }
        }

        raw_cycles.sort_by(|a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        raw_cycles.truncate(pruning.cyclic_raw_cap);

        for (_, cyc) in raw_cycles {
            if let Some(cand) =
                optimize_cycle_amount(market, base, base_usd, &cyc, min_profit_usd)
            {
                out.push(cand);
            }
        }
    }
    out
}

#[inline]
fn insert_top_k<T: Copy, F: Fn(&T) -> f64>(slot: &mut [Option<T>], item: T, score: F) {
    let s = score(&item);
    for i in 0..slot.len() {
        match slot[i] {
            None => {
                slot[i] = Some(item);
                return;
            }
            Some(cur) if s > score(&cur) => {
                for j in (i + 1..slot.len()).rev() {
                    slot[j] = slot[j - 1];
                }
                slot[i] = Some(item);
                return;
            }
            Some(_) => {}
        }
    }
}

fn consider_raw_cycle<'a>(
    cyc: &[EdgeRef<'a>],
    sweep_hub_drift: bool,
    min_upper_usd: f64,
    out: &mut Vec<(f64, Vec<EdgeRef<'a>>)>,
) {
    let has_non_hub_edge = cyc.iter().any(|e| !e.is_hub_pair);
    let dirty_ok = if has_non_hub_edge && !sweep_hub_drift {
        cyc.iter().any(|e| !e.is_hub_pair && e.is_dirty)
    } else {
        cyc.iter().any(|e| e.is_dirty)
    };
    if !dirty_ok {
        return;
    }
    let mut spot_prod = 1.0_f64;
    let mut min_cap = f64::MAX;
    for e in cyc {
        spot_prod *= e.spot_rate;
        min_cap = min_cap.min(e.max_in_usd);
    }
    if spot_prod <= 1.0004 || !spot_prod.is_finite() || min_cap <= 0.0 {
        return;
    }
    let upper = min_cap * (spot_prod - 1.0);
    if upper >= min_upper_usd {
        out.push((upper, cyc.to_vec()));
    }
}

fn optimize_cycle_amount(
    market: &MarketState,
    base: Address,
    base_usd: f64,
    path: &[EdgeRef<'_>],
    min_profit_usd: f64,
) -> Option<RouteCandidate> {
    let used_addrs: Vec<Address> = path.iter().map(|e| e.pool.address).collect();
    let res_chain: Vec<(crate::amm::ResidualPool, bool)> = path
        .iter()
        .map(|e| (e.pool.to_residual(), e.zero_for_one))
        .collect();

    // Determine conservative upper bound from minimum directional capacity along cycle
    let mut min_cap_usd = f64::MAX;
    for e in path {
        min_cap_usd = min_cap_usd.min(e.max_in_usd);
    }
    let max_in_usd = min_cap_usd.clamp(2.0, 25_000.0);
    let min_in_h = 0.30 / base_usd;
    let max_in_h = max_in_usd / base_usd;
    if max_in_h <= min_in_h {
        return None;
    }

    let flash_test_raw = human_to_u256(min_in_h * 2.0, market.token_decimals(base));
    let flash_meta = market.select_flash_source(base, flash_test_raw, &used_addrs)?;
    let flash_fee_frac = flash_meta.fee_bps / 10_000.0;

    let eval = |amt_in: f64| -> (f64, Vec<(f64, f64)>) {
        let mut cur = amt_in;
        let mut step_io = Vec::with_capacity(res_chain.len());
        for (rp, zfo) in &res_chain {
            let out = rp.quote_human(*zfo, cur);
            step_io.push((cur, out));
            if out <= 0.0 {
                return (-1e12, step_io);
            }
            cur = out;
        }
        let net_h = cur - amt_in * (1.0 + flash_fee_frac);
        (net_h, step_io)
    };

    let (f_min, _) = eval(min_in_h);
    if f_min <= -1e11 {
        return None;
    }

    // Clamp upper bound inside feasible V3 tick domain via binary search
    let mut a = min_in_h;
    let mut b = max_in_h;
    if eval(b).0 <= -1e11 {
        let mut lo = a;
        let mut hi = b;
        for _ in 0..12 {
            let mid = 0.5 * (lo + hi);
            if eval(mid).0 > -1e11 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        b = lo;
    }

    // Golden-section search on feasible interval [a, b]
    let phi = 0.6180339887498949_f64;
    let mut best_in = a;
    let mut best_f = f_min;
    let (fb, _) = eval(b);
    if fb > best_f {
        best_f = fb;
        best_in = b;
    }
    if b > a * 1.001 {
        let mut x1 = b - phi * (b - a);
        let mut x2 = a + phi * (b - a);
        let mut f1 = eval(x1).0;
        let mut f2 = eval(x2).0;

        for _ in 0..24 {
            if f2 <= -1e11 || f1 > f2 {
                b = x2;
                x2 = x1;
                f2 = f1;
                x1 = b - phi * (b - a);
                f1 = eval(x1).0;
            } else {
                a = x1;
                x1 = x2;
                f1 = f2;
                x2 = a + phi * (b - a);
                f2 = eval(x2).0;
            }
        }
        if f1 > best_f {
            best_f = f1;
            best_in = x1;
        }
        if f2 > best_f {
            best_in = x2;
        }
    }

    let (best_gross_h, step_io) = eval(best_in);
    if best_gross_h <= 0.0 {
        return None;
    }

    let borrow_raw = human_to_u256(best_in, market.token_decimals(base));
    let flash = market.select_flash_source(base, borrow_raw, &used_addrs)?;
    let gross_h = step_io.last()?.1 - best_in * (1.0 + flash.fee_bps / 10_000.0);
    if gross_h <= 0.0 {
        return None;
    }

    let flows: Vec<DirectedFlow> = path
        .iter()
        .zip(step_io.iter())
        .map(|(e, &(fin, fout))| DirectedFlow {
            pool: e.pool.address,
            label: format!(
                "{}:{}->{}",
                e.pool.factory_name,
                market.token_symbol(e.token_in),
                market.token_symbol(e.token_out)
            ),
            token_in: e.token_in,
            token_out: e.token_out,
            dex_kind: e.pool.dex_kind,
            fee_num: e.pool.fee_num,
            flow_in_human: fin,
            flow_out_human: fout,
        })
        .collect();

    let gross_usd = gross_h * base_usd;
    let est_gas = crate::router::estimate_flows_gas(&flows);
    let gas_usd = market.estimate_gas_cost_usd(est_gas, 0);
    let net_usd = gross_usd - gas_usd;
    if net_usd < min_profit_usd {
        return None;
    }

    let mermaid = render_mermaid(market, base, &flows);
    Some(RouteCandidate {
        strategy: format!("Cyclic({}hop)", flows.len()),
        base_token: base,
        borrow_human: best_in,
        est_gross_profit_human: gross_h,
        est_gross_profit_usd: gross_usd,
        est_net_profit_usd: net_usd,
        flash_mode: flash.flash_mode,
        flash_source: flash.pool,
        flash_fee_bps: flash.fee_bps,
        flows,
        mermaid,
    })
}
