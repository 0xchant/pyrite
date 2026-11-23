use alloy::primitives::Address;
use std::collections::{HashMap, HashSet};

use crate::amm::{human_to_u256, PoolState, ResidualPool};
use crate::discovery::MarketState;
use crate::router::{
    estimate_flows_gas, render_mermaid, DirectedFlow, RouteCandidate, SolverPruningConfig,
    MAX_DAG_STEPS,
};

const MAX_DEEP_HOPS: usize = MAX_DAG_STEPS;
const MAX_BEAM_WIDTH: usize = 12;

#[derive(Clone)]
struct DagPoolEdge {
    pool_idx: usize,
    to: usize,
    zero_for_one: bool,
    token_in: Address,
    token_out: Address,
}

/// Entry point returning standalone DAG + deep CSR cycle candidates PLUS sub-threshold
/// gross-positive `5..16`-hop routes for base-gas amortization in the coalescer.
#[allow(dead_code)]
pub fn solve_dag_and_deep(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    solve_dag_and_deep_with_config(
        market,
        routable_pools,
        dirty_pools,
        min_profit_usd,
        &SolverPruningConfig::default(),
    )
}

pub fn solve_dag_and_deep_with_config(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
    pruning: &SolverPruningConfig,
) -> Vec<RouteCandidate> {
    let mut all = solve_dag(market, routable_pools, dirty_pools, min_profit_usd);
    all.extend(solve_deep_cycles_with_config(
        market,
        routable_pools,
        dirty_pools,
        min_profit_usd,
        pruning,
    ));
    all
}

pub struct PrecomputedPoolRates {
    pub t0_id: u32,
    pub t1_id: u32,
    pub r01: f64,
    pub r10: f64,
    pub is_official: bool,
}

/// Layered Acyclic Network Flow Router (DAG Successive Marginal Allocation):
/// 1) Extracts active liquid subgraph anchored at `base_token`
/// 2) Assigns strict topological ordering to intermediate tokens (`dist_from_start - dist_to_end`)
/// 3) Pushes descending USD tranches along the highest marginal-return path in the residual DAG
/// 4) Aggregates net flows across parallel splits, intermediate cross-links, and merges.
pub fn solve_dag(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    if dirty_pools.is_some_and(|dp| dp.is_empty()) || routable_pools.len() < 2 {
        return Vec::new();
    }

    let dirty_toks_storage: Option<HashSet<Address>> = dirty_pools.map(|dp| {
        let mut s = HashSet::new();
        for addr in dp {
            if let Some(p) = market.pools.get(addr) {
                s.insert(p.token0);
                s.insert(p.token1);
            }
        }
        s
    });
    let dirty_toks = dirty_toks_storage.as_ref();
    let active_pools = routable_pools;

    // Precompute dense token IDs and marginal rates once per block across all routable pools
    let mut tok_id: HashMap<Address, u32> = HashMap::with_capacity(active_pools.len());
    let mut id_to_tok: Vec<Address> = Vec::with_capacity(active_pools.len());
    let mut get_tid = |addr: Address| -> u32 {
        if let Some(&id) = tok_id.get(&addr) {
            id
        } else {
            let id = id_to_tok.len() as u32;
            tok_id.insert(addr, id);
            id_to_tok.push(addr);
            id
        }
    };
    let anchors: Vec<Address> = market.base_anchors().into_iter().take(3).collect();
    for &b in &anchors {
        get_tid(b);
    }
    let mut pool_rates = Vec::with_capacity(active_pools.len());
    for &p in active_pools {
        let t0_id = get_tid(p.token0);
        let t1_id = get_tid(p.token1);
        let mut r01 = p.marginal_rate_human(p.token0);
        let mut r10 = p.marginal_rate_human(p.token1);
        let u0 = market.token_usd(p.token0);
        let u1 = market.token_usd(p.token1);
        let uncal = (!market.is_hub_token(p.token0)
            && !market.token_tax_ppm.contains_key(&p.token0))
            || (!market.is_hub_token(p.token1) && !market.token_tax_ppm.contains_key(&p.token1));
        if uncal && u0 > 0.0 && u1 > 0.0 {
            if r01 * (u1 / u0) > 1.35 {
                r01 = 0.0;
            }
            if r10 * (u0 / u1) > 1.35 {
                r10 = 0.0;
            }
        }
        pool_rates.push(PrecomputedPoolRates {
            t0_id,
            t1_id,
            r01,
            r10,
            is_official: !p.factory_name.starts_with("Generic") || p.cached_depth_usd >= 150.0,
        });
    }

    let mut results = Vec::new();
    for base in anchors {
        let base_usd = market.token_usd(base);
        if base_usd <= 0.0 {
            continue;
        }
        if let Some(cand) = solve_dag_for_base(
            market,
            &active_pools,
            &tok_id,
            &id_to_tok,
            &pool_rates,
            base,
            base_usd,
            dirty_pools,
            dirty_toks,
            min_profit_usd,
        ) {
            results.push(cand);
        }
    }
    results
}

fn solve_dag_for_base(
    market: &MarketState,
    active_pools: &[&PoolState],
    tok_id: &HashMap<Address, u32>,
    id_to_tok: &[Address],
    pool_rates: &[PrecomputedPoolRates],
    base: Address,
    base_usd: f64,
    dirty_pools: Option<&HashSet<Address>>,
    dirty_tokens: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Option<RouteCandidate> {
    if active_pools.len() < 2 {
        return None;
    }
    let n_toks = id_to_tok.len();
    let base_id = *tok_id.get(&base)? as usize;

    // Flat-array 6-pass Bellman-Ford propagation (zero allocations & hashing inside iterations)
    let mut best_f = vec![0.0_f64; n_toks];
    let mut best_t = vec![0.0_f64; n_toks];
    let mut official_non_hub = vec![false; n_toks];

    for pr in pool_rates {
        let u0 = pr.t0_id as usize;
        let u1 = pr.t1_id as usize;
        if pr.is_official {
            if !market.is_hub_token(id_to_tok[u0]) {
                official_non_hub[u0] = true;
            }
            if !market.is_hub_token(id_to_tok[u1]) {
                official_non_hub[u1] = true;
            }
        }
        if u0 == base_id {
            if pr.r01 > best_f[u1] {
                best_f[u1] = pr.r01;
            }
            if pr.r10 > best_t[u1] {
                best_t[u1] = pr.r10;
            }
        } else if u1 == base_id {
            if pr.r10 > best_f[u0] {
                best_f[u0] = pr.r10;
            }
            if pr.r01 > best_t[u0] {
                best_t[u0] = pr.r01;
            }
        }
    }

    let mut snap_f = vec![0.0_f64; n_toks];
    let mut snap_t = vec![0.0_f64; n_toks];
    for _ in 0..8 {
        let mut improved = false;
        snap_f.copy_from_slice(&best_f);
        snap_t.copy_from_slice(&best_t);
        for pr in pool_rates {
            let u0 = pr.t0_id as usize;
            let u1 = pr.t1_id as usize;
            if u0 == base_id || u1 == base_id {
                continue;
            }
            if pr.r01 > 0.0 {
                let f0 = snap_f[u0];
                if f0 > 0.0 {
                    let cand = f0 * pr.r01;
                    if cand > best_f[u1] * (1.0 + 1e-12) {
                        best_f[u1] = cand;
                        improved = true;
                    }
                }
                let t1 = snap_t[u1];
                if t1 > 0.0 {
                    let cand = pr.r01 * t1;
                    if cand > best_t[u0] * (1.0 + 1e-12) {
                        best_t[u0] = cand;
                        improved = true;
                    }
                }
            }
            if pr.r10 > 0.0 {
                let f1 = snap_f[u1];
                if f1 > 0.0 {
                    let cand = f1 * pr.r10;
                    if cand > best_f[u0] * (1.0 + 1e-12) {
                        best_f[u0] = cand;
                        improved = true;
                    }
                }
                let t0 = snap_t[u0];
                if t0 > 0.0 {
                    let cand = pr.r10 * t0;
                    if cand > best_t[u1] * (1.0 + 1e-12) {
                        best_t[u1] = cand;
                        improved = true;
                    }
                }
            }
        }
        if !improved {
            break;
        }
    }

    let mut hub_cands: Vec<(Address, f64, f64)> = Vec::new();
    let mut non_hub_cands: Vec<(bool, Address, f64, f64, f64)> = Vec::new();
    for tid in 0..n_toks {
        if tid == base_id || best_f[tid] <= 0.0 || best_t[tid] <= 0.0 {
            continue;
        }
        let tk = id_to_tok[tid];
        if market.is_hub_token(tk) {
            hub_cands.push((tk, best_f[tid], best_t[tid]));
        } else {
            let dirty_boost = if dirty_tokens.is_some_and(|d| d.contains(&tk)) {
                0.02
            } else {
                0.0
            };
            let spread = best_f[tid] * best_t[tid] + dirty_boost;
            non_hub_cands.push((official_non_hub[tid], tk, spread, best_f[tid], best_t[tid]));
        }
    }
    hub_cands.sort_by(|a, b| a.0.cmp(&b.0));
    non_hub_cands.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.1.cmp(&b.1))
    });

    let mut inter_addrs: Vec<Address> = Vec::with_capacity(40);
    let mut inter_f: Vec<f64> = Vec::with_capacity(40);
    let mut inter_t: Vec<f64> = Vec::with_capacity(40);
    for (tk, fr, tr) in hub_cands {
        if inter_addrs.len() >= 40 {
            break;
        }
        inter_addrs.push(tk);
        inter_f.push(fr);
        inter_t.push(tr);
    }
    for (_, tk, _, fr, tr) in non_hub_cands.into_iter().take(30) {
        if inter_addrs.len() >= 40 {
            break;
        }
        inter_addrs.push(tk);
        inter_f.push(fr);
        inter_t.push(tr);
    }
    if inter_addrs.is_empty() {
        return None;
    }

    let mut idx_of_tid = vec![-1i16; n_toks];
    for (i, &tk) in inter_addrs.iter().enumerate() {
        idx_of_tid[tok_id[&tk] as usize] = i as i16;
    }
    let m = inter_addrs.len();

    // Scale-invariant Feedback Arc Set orientation:
    // For each unordered intermediate token pair (a, b), compute dimensionless profit ratio
    // `p_ab = inter_f[a] * max_rate(a->b) * inter_t[b]` vs reverse `p_ba`.
    // Greedily lock the stronger direction into an acyclic tournament / topological order!
    let mut best_pair_rate: HashMap<(usize, usize), f64> = HashMap::new();
    for pr in pool_rates {
        let u0 = pr.t0_id as usize;
        let u1 = pr.t1_id as usize;
        if u0 == base_id || u1 == base_id {
            continue;
        }
        let ia = idx_of_tid[u0];
        let ib = idx_of_tid[u1];
        if ia >= 0 && ib >= 0 {
            let (ia, ib) = (ia as usize, ib as usize);
            best_pair_rate
                .entry((ia, ib))
                .and_modify(|v| *v = v.max(pr.r01))
                .or_insert(pr.r01);
            best_pair_rate
                .entry((ib, ia))
                .and_modify(|v| *v = v.max(pr.r10))
                .or_insert(pr.r10);
        }
    }

    let mut directed_prefs: Vec<(usize, usize, f64)> = Vec::new();
    for (&(ia, ib), &rab) in &best_pair_rate {
        if ia >= ib {
            continue;
        }
        let rba = best_pair_rate.get(&(ib, ia)).copied().unwrap_or(0.0);
        let fa = inter_f[ia];
        let ta = inter_t[ia];
        let fb = inter_f[ib];
        let tb = inter_t[ib];
        let p_ab = fa * rab * tb;
        let p_ba = fb * rba * ta;
        if p_ab >= p_ba && p_ab > 0.0 {
            directed_prefs.push((ia, ib, p_ab - p_ba * 0.5));
        } else if p_ba > 0.0 {
            directed_prefs.push((ib, ia, p_ba - p_ab * 0.5));
        }
    }
    directed_prefs.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    // Add edges u -> v greedily if v cannot already reach u via stack [u64; 64] bitmask (m <= 40 < 64)
    let mut reach = [0u64; 64];
    let mut pref_adj = vec![Vec::new(); m];
    let mut pref_indeg = vec![0usize; m];
    for (u, v, _) in directed_prefs {
        if u == v || ((reach[v] >> u) & 1) != 0 {
            continue;
        }
        if ((reach[u] >> v) & 1) == 0 {
            pref_adj[u].push(v);
            pref_indeg[v] += 1;
            let v_reach = reach[v] | (1u64 << v);
            reach[u] |= v_reach;
            for x in 0..m {
                if ((reach[x] >> u) & 1) != 0 {
                    reach[x] |= v_reach;
                }
            }
        }
    }

    // Topological sort of intermediates
    let mut q: Vec<usize> = (0..m).filter(|&i| pref_indeg[i] == 0).collect();
    let mut topo_inter: Vec<usize> = Vec::with_capacity(m);
    let mut qi = 0;
    while qi < q.len() {
        let u = q[qi];
        qi += 1;
        topo_inter.push(u);
        for &v in &pref_adj[u] {
            pref_indeg[v] -= 1;
            if pref_indeg[v] == 0 {
                q.push(v);
            }
        }
    }

    let k = topo_inter.len();
    let start_node = 0usize;
    let end_node = k + 1;
    let mut node_token: Vec<Address> = Vec::with_capacity(k + 2);
    node_token.push(base);
    let mut token_node: HashMap<Address, usize> = HashMap::new();
    for (pos, &orig_idx) in topo_inter.iter().enumerate() {
        let tk = inter_addrs[orig_idx];
        node_token.push(tk);
        token_node.insert(tk, pos + 1);
    }
    node_token.push(base);

    // Filter subgraph pools to only edges touching `{base} U token_node` and deduplicate to top 4 per pair
    let mut pair_groups: HashMap<(Address, Address), Vec<&PoolState>> = HashMap::new();
    for &p in active_pools {
        if (p.token0 == base || token_node.contains_key(&p.token0))
            && (p.token1 == base || token_node.contains_key(&p.token1))
        {
            let key = if p.token0 < p.token1 {
                (p.token0, p.token1)
            } else {
                (p.token1, p.token0)
            };
            pair_groups.entry(key).or_default().push(p);
        }
    }
    let mut sub_pools: Vec<&PoolState> = Vec::new();
    for ((t_min, t_max), list) in pair_groups {
        if list.len() <= 4 {
            sub_pools.extend(list);
            continue;
        }
        let mut chosen_idx = Vec::with_capacity(4);
        if let Some((bi, _)) = list
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.marginal_rate_human(t_min)
                    .partial_cmp(&b.marginal_rate_human(t_min))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            chosen_idx.push(bi);
        }
        if let Some((si, _)) = list
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.marginal_rate_human(t_max)
                    .partial_cmp(&b.marginal_rate_human(t_max))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        {
            if !chosen_idx.contains(&si) {
                chosen_idx.push(si);
            }
        }
        let mut by_depth: Vec<(usize, f64)> = list
            .iter()
            .enumerate()
            .map(|(i, p)| (i, p.cached_depth_usd))
            .collect();
        by_depth.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (i, _) in by_depth {
            if chosen_idx.len() >= 4 {
                break;
            }
            if !chosen_idx.contains(&i) {
                chosen_idx.push(i);
            }
        }
        for i in chosen_idx {
            sub_pools.push(list[i]);
        }
    }
    if sub_pools.len() < 2 {
        return None;
    }
    if dirty_pools.is_some_and(|dp| !sub_pools.iter().any(|p| dp.contains(&p.address))) {
        return None;
    }

    let mut adj: Vec<Vec<DagPoolEdge>> = vec![Vec::new(); k + 2];
    for (pool_idx, p) in sub_pools.iter().enumerate() {
        if p.token0 == base {
            if let Some(&v) = token_node.get(&p.token1) {
                adj[start_node].push(DagPoolEdge {
                    pool_idx,
                    to: v,
                    zero_for_one: true,
                    token_in: base,
                    token_out: p.token1,
                });
                adj[v].push(DagPoolEdge {
                    pool_idx,
                    to: end_node,
                    zero_for_one: false,
                    token_in: p.token1,
                    token_out: base,
                });
            }
        } else if p.token1 == base {
            if let Some(&v) = token_node.get(&p.token0) {
                adj[start_node].push(DagPoolEdge {
                    pool_idx,
                    to: v,
                    zero_for_one: false,
                    token_in: base,
                    token_out: p.token0,
                });
                adj[v].push(DagPoolEdge {
                    pool_idx,
                    to: end_node,
                    zero_for_one: true,
                    token_in: p.token0,
                    token_out: base,
                });
            }
        } else if let (Some(&u0), Some(&u1)) = (token_node.get(&p.token0), token_node.get(&p.token1)) {
            if u0 < u1 {
                adj[u0].push(DagPoolEdge {
                    pool_idx,
                    to: u1,
                    zero_for_one: true,
                    token_in: p.token0,
                    token_out: p.token1,
                });
            } else if u1 < u0 {
                adj[u1].push(DagPoolEdge {
                    pool_idx,
                    to: u0,
                    zero_for_one: false,
                    token_in: p.token1,
                    token_out: p.token0,
                });
            }
        }
    }

    // Reserve non-overlapping flash source upfront: check best flash pool for `base`
    let min_flash_probe = human_to_u256(5.0 / base_usd, market.token_decimals(base));
    let flash_probe = market.select_flash_source(base, min_flash_probe, &[])?;
    let forbidden_flash_pool = if flash_probe.flash_mode >= 2 {
        Some(flash_probe.pool)
    } else {
        None
    };
    let flash_hurdle = 1.0 + (flash_probe.fee_bps + 0.5) / 10_000.0;

    // 1-pass topological spot-rate check BEFORE allocating `ResidualPool`s
    let mut best_spot = vec![0.0_f64; k + 2];
    best_spot[start_node] = 1.0;
    for u in 0..=k {
        let s_u = best_spot[u];
        if s_u <= 0.0 {
            continue;
        }
        for edge in &adj[u] {
            let p = sub_pools[edge.pool_idx];
            if forbidden_flash_pool == Some(p.address) {
                continue;
            }
            let r = p.marginal_rate_human(edge.token_in);
            if s_u * r > best_spot[edge.to] {
                best_spot[edge.to] = s_u * r;
            }
        }
    }
    if best_spot[end_node] <= flash_hurdle + 0.0003 {
        return None;
    }

    let mut res_pools: Vec<ResidualPool> = sub_pools.iter().map(|p| p.to_residual()).collect();

    // Successive marginal flow pushing across descending tranche sizes
    let tranches_usd = [400.0, 160.0, 75.0, 32.0, 14.0, 5.0, 1.5, 0.4];
    let per_edge_gas_usd = market.estimate_gas_cost_usd(95_000, 0);
    let mut edge_flows: HashMap<(usize, Address, Address), (f64, f64)> = HashMap::new();
    let mut total_in_h = 0.0_f64;
    let mut total_out_h = 0.0_f64;

    for &t_usd in &tranches_usd {
        let dx = t_usd / base_usd;
        if dx <= 0.0 {
            continue;
        }
        for _ in 0..5 {
            // Longest-output DP on topological order 0..=k+1
            let mut best_amt = vec![0.0_f64; k + 2];
            let mut prev: Vec<Option<(usize, usize)>> = vec![None; k + 2]; // (prev_node, edge_vec_idx)
            best_amt[start_node] = dx;

            for u in 0..=k {
                let amt_u = best_amt[u];
                if amt_u <= 0.0 {
                    continue;
                }
                for (ei, edge) in adj[u].iter().enumerate() {
                    if forbidden_flash_pool == Some(sub_pools[edge.pool_idx].address) {
                        continue;
                    }
                    if edge_flows.len() >= 5
                        && sub_pools[edge.pool_idx].factory_name.starts_with("Generic")
                        && sub_pools[edge.pool_idx].cached_depth_usd < 150.0
                    {
                        continue;
                    }
                    // Guard: never send flow through the same pool in opposite directions
                    let opp_key = (edge.pool_idx, edge.token_out, edge.token_in);
                    if edge_flows.contains_key(&opp_key) {
                        continue;
                    }
                    let out = res_pools[edge.pool_idx].quote_human(edge.zero_for_one, amt_u);
                    if out > best_amt[edge.to] {
                        best_amt[edge.to] = out;
                        prev[edge.to] = Some((u, ei));
                    }
                }
            }

            if best_amt[end_node] <= dx * flash_hurdle {
                break;
            }

            // Reconstruct path & commit swap updates on residual pools
            let mut chosen: Vec<(usize, usize)> = Vec::new();
            let mut cur = end_node;
            while let Some((pn, ei)) = prev[cur] {
                chosen.push((pn, ei));
                cur = pn;
            }
            if cur != start_node || chosen.is_empty() {
                break;
            }
            chosen.reverse();

            // Verify pool uniqueness along this single path
            let mut step_pools = HashSet::new();
            let mut valid = true;
            for &(pn, ei) in &chosen {
                if !step_pools.insert(adj[pn][ei].pool_idx) {
                    valid = false;
                    break;
                }
            }
            if !valid {
                break;
            }
            let new_edges = chosen
                .iter()
                .filter(|&&(pn, ei)| {
                    let e = &adj[pn][ei];
                    !edge_flows.contains_key(&(e.pool_idx, e.token_in, e.token_out))
                })
                .count();
            if edge_flows.len() + new_edges > MAX_DAG_STEPS {
                break;
            }
            if edge_flows.len() + new_edges > 5
                && chosen.iter().any(|&(pn, ei)| {
                    let e = &adj[pn][ei];
                    !edge_flows.contains_key(&(e.pool_idx, e.token_in, e.token_out))
                        && sub_pools[e.pool_idx].factory_name.starts_with("Generic")
                        && sub_pools[e.pool_idx].cached_depth_usd < 150.0
                })
            {
                break;
            }
            let mut non_hub_set: HashSet<Address> = HashSet::new();
            for &(_, t_in, t_out) in edge_flows.keys() {
                if !market.is_hub_token(t_in) {
                    non_hub_set.insert(t_in);
                }
                if !market.is_hub_token(t_out) {
                    non_hub_set.insert(t_out);
                }
            }
            for &(pn, ei) in &chosen {
                let e = &adj[pn][ei];
                if !market.is_hub_token(e.token_in) {
                    non_hub_set.insert(e.token_in);
                }
                if !market.is_hub_token(e.token_out) {
                    non_hub_set.insert(e.token_out);
                }
            }
            if non_hub_set.len() > 24 {
                break;
            }
            let tranche_gain_usd = (best_amt[end_node] - dx * flash_hurdle) * base_usd;
            if new_edges > 0 && tranche_gain_usd < (new_edges as f64) * per_edge_gas_usd * 0.25 {
                break;
            }

            let mut flow_amt = dx;
            for (pn, ei) in chosen {
                let edge = &adj[pn][ei];
                let out = res_pools[edge.pool_idx].apply_swap_human(edge.zero_for_one, flow_amt);
                let e = edge_flows
                    .entry((edge.pool_idx, edge.token_in, edge.token_out))
                    .or_insert((0.0, 0.0));
                e.0 += flow_amt;
                e.1 += out;
                flow_amt = out;
            }
            total_in_h += dx;
            total_out_h += flow_amt;
        }
    }

    // Prune gas-negative parasite intermediate components while keeping profitable branches intact
    if !edge_flows.is_empty() {
        let mut inter_toks: HashMap<Address, usize> = HashMap::new();
        for &(_, t_in, t_out) in edge_flows.keys() {
            if t_in != base {
                let next = inter_toks.len();
                inter_toks.entry(t_in).or_insert(next);
            }
            if t_out != base {
                let next = inter_toks.len();
                inter_toks.entry(t_out).or_insert(next);
            }
        }
        let mut adj_comp: Vec<Vec<usize>> = vec![Vec::new(); inter_toks.len()];
        for &(_, t_in, t_out) in edge_flows.keys() {
            if t_in != base && t_out != base {
                let u = inter_toks[&t_in];
                let v = inter_toks[&t_out];
                adj_comp[u].push(v);
                adj_comp[v].push(u);
            }
        }
        let mut comp_of = vec![usize::MAX; adj_comp.len()];
        let mut n_comps = 0usize;
        let mut stack = Vec::new();
        for start in 0..adj_comp.len() {
            if comp_of[start] != usize::MAX {
                continue;
            }
            comp_of[start] = n_comps;
            stack.push(start);
            while let Some(u) = stack.pop() {
                for &v in &adj_comp[u] {
                    if comp_of[v] == usize::MAX {
                        comp_of[v] = n_comps;
                        stack.push(v);
                    }
                }
            }
            n_comps += 1;
        }
        if n_comps >= 2 {
            let mut comp_in_h = vec![0.0_f64; n_comps];
            let mut comp_out_h = vec![0.0_f64; n_comps];
            let mut comp_gas = vec![0u64; n_comps];
            for (&(pool_idx, t_in, t_out), &(fin, fout)) in &edge_flows {
                let tk_c = if t_in != base { t_in } else { t_out };
                let Some(&tid) = inter_toks.get(&tk_c) else {
                    continue;
                };
                let cid = comp_of[tid];
                if t_in == base {
                    comp_in_h[cid] += fin;
                }
                if t_out == base {
                    comp_out_h[cid] += fout;
                }
                comp_gas[cid] += match sub_pools[pool_idx].dex_kind {
                    crate::config::DexKind::UniV2 => 60_000,
                    crate::config::DexKind::Solidly => 95_000,
                    crate::config::DexKind::UniV3 | crate::config::DexKind::PancakeV3 => 115_000,
                    crate::config::DexKind::InfinityCl | crate::config::DexKind::UniV4 => 145_000,
                    crate::config::DexKind::MetricOmm => 135_000,
                    crate::config::DexKind::BalancerV2 => 115_000,
                };
            }
            let comp_fee = flash_probe.fee_bps.max(1.0) / 10_000.0;
            let keep: Vec<bool> = (0..n_comps)
                .map(|c| {
                    let net_usd = (comp_out_h[c] - comp_in_h[c] * (1.0 + comp_fee)) * base_usd
                        - market.estimate_gas_cost_usd(comp_gas[c], 0);
                    net_usd > 0.0005
                })
                .collect();
            if keep.iter().any(|&k| k) && keep.iter().any(|&k| !k) {
                edge_flows.retain(|&(_, t_in, t_out), _| {
                    let tk_c = if t_in != base { t_in } else { t_out };
                    inter_toks
                        .get(&tk_c)
                        .is_some_and(|&tid| keep[comp_of[tid]])
                });
                total_in_h = 0.0;
                total_out_h = 0.0;
                for (&(_, t_in, t_out), &(fin, fout)) in &edge_flows {
                    if t_in == base {
                        total_in_h += fin;
                    }
                    if t_out == base {
                        total_out_h += fout;
                    }
                }
            }
        }
    }

    if edge_flows.is_empty() || total_out_h <= total_in_h {
        return None;
    }

    let mut used_pools = Vec::new();
    let mut flows = Vec::new();
    for ((pool_idx, t_in, t_out), (fin, fout)) in edge_flows {
        let p = sub_pools[pool_idx];
        used_pools.push(p.address);
        flows.push(DirectedFlow {
            pool: p.address,
            label: format!(
                "{}:{}->{}",
                p.factory_name,
                market.token_symbol(t_in),
                market.token_symbol(t_out)
            ),
            token_in: t_in,
            token_out: t_out,
            dex_kind: p.dex_kind,
            fee_num: p.fee_num,
            flow_in_human: fin,
            flow_out_human: fout,
        });
    }

    if let Some(dp) = dirty_pools {
        if !used_pools.iter().any(|p| dp.contains(p)) {
            return None;
        }
    }

    let borrow_raw = human_to_u256(total_in_h, market.token_decimals(base));
    let flash = market.select_flash_source(base, borrow_raw, &used_pools)?;
    let flash_fee_h = total_in_h * (flash.fee_bps / 10_000.0);
    let gross_h = total_out_h - total_in_h - flash_fee_h;
    if gross_h <= 0.0 {
        return None;
    }

    let gross_usd = gross_h * base_usd;
    let est_gas = estimate_flows_gas(&flows);
    let gas_usd = market.estimate_gas_cost_usd(est_gas, 0);
    let step_gas_usd = market.estimate_gas_cost_usd(est_gas.saturating_sub(65_000), 0);
    let net_usd = gross_usd - gas_usd;
    if net_usd < min_profit_usd && gross_usd <= step_gas_usd + 0.0008 {
        return None;
    }

    let mermaid = render_mermaid(market, base, &flows);
    Some(RouteCandidate {
        strategy: format!("DAG({}edges)", flows.len()),
        base_token: base,
        borrow_human: total_in_h,
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

#[derive(Clone, Copy)]
struct CsrEdge {
    to: u16,
    pool_id: u16,
    routable_idx: u16,
    zfo: bool,
    is_dirty: bool,
    is_non_hub_out: bool,
    spot_rate: f64,
    usd_mult: f64,
    max_in_usd: f64,
}

#[derive(Clone, Copy)]
struct BeamPath {
    rate: f64,
    usd_val: f64,
    min_depth: f64,
    len: u8,
    non_hub_cnt: u8,
    dirty: bool,
    last_node: u16,
    nodes: [u16; MAX_DEEP_HOPS],
    pool_ids: [u16; MAX_DEEP_HOPS],
    routable_idxs: [u16; MAX_DEEP_HOPS],
    zfo_bits: u32,
}

impl Default for BeamPath {
    fn default() -> Self {
        Self {
            rate: 0.0,
            usd_val: 0.0,
            min_depth: 0.0,
            len: 0,
            non_hub_cnt: 0,
            dirty: false,
            last_node: 0,
            nodes: [0; MAX_DEEP_HOPS],
            pool_ids: [0; MAX_DEEP_HOPS],
            routable_idxs: [0; MAX_DEEP_HOPS],
            zfo_bits: 0,
        }
    }
}

#[inline]
fn insert_beam_slot(
    slots: &mut [BeamPath; MAX_BEAM_WIDTH],
    cnt: &mut u8,
    beam_width: usize,
    cand: BeamPath,
) -> bool {
    let cap = beam_width.clamp(1, MAX_BEAM_WIDTH);
    let was_empty = *cnt == 0;
    let c = *cnt as usize;
    if c < cap {
        slots[c] = cand;
        *cnt += 1;
        let mut i = c;
        while i > 0 && slots[i].rate > slots[i - 1].rate {
            slots.swap(i, i - 1);
            i -= 1;
        }
    } else if cand.rate > slots[cap - 1].rate {
        slots[cap - 1] = cand;
        let mut i = cap - 1;
        while i > 0 && slots[i].rate > slots[i - 1].rate {
            slots.swap(i, i - 1);
            i -= 1;
        }
    }
    was_empty
}

/// Fast CSR layered pruned beam search over `5..=16`-hop simple cycles.
/// Uses flat Copy paths and dense u16 indexing to scan all active edges with zero inner allocations.
#[cfg_attr(not(test), allow(dead_code))]
pub fn solve_deep_cycles(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    solve_deep_cycles_with_config(
        market,
        routable_pools,
        dirty_pools,
        min_profit_usd,
        &SolverPruningConfig::default(),
    )
}

pub fn solve_deep_cycles_with_config(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
    pruning: &SolverPruningConfig,
) -> Vec<RouteCandidate> {
    if dirty_pools.is_some_and(|dp| dp.is_empty()) || routable_pools.len() < 5 {
        return Vec::new();
    }

    struct TempEdge {
        pool_id: u16,
        routable_idx: u16,
        zfo: bool,
        is_dirty: bool,
        is_non_hub_out: bool,
        spot_rate: f64,
        usd_mult: f64,
        depth_usd: f64,
        max_in_usd: f64,
    }

    let mut id_of: HashMap<Address, u16> = HashMap::new();
    let mut nodes_vec: Vec<Address> = Vec::new();
    let mut assign_id = |addr: Address| -> Option<u16> {
        if let Some(&id) = id_of.get(&addr) {
            Some(id)
        } else {
            let id = u16::try_from(nodes_vec.len()).ok()?;
            id_of.insert(addr, id);
            nodes_vec.push(addr);
            Some(id)
        }
    };

    let mut pool_ids: HashMap<Address, u16> = HashMap::new();
    let mut pair_buckets: HashMap<(Address, Address), Vec<TempEdge>> = HashMap::new();

    for (r_idx, &pool) in routable_pools.iter().enumerate() {
        if !pool.is_active()
            || pool.cached_depth_usd < 30.0
            || (pool.factory_name.starts_with("Generic") && pool.cached_depth_usd < 150.0)
        {
            continue;
        }
        let Ok(routable_idx) = u16::try_from(r_idx) else {
            continue;
        };
        let pid = if let Some(&id) = pool_ids.get(&pool.address) {
            id
        } else {
            let Ok(id) = u16::try_from(pool_ids.len()) else {
                continue;
            };
            pool_ids.insert(pool.address, id);
            id
        };
        let is_dirty = dirty_pools.map_or(true, |dp| dp.contains(&pool.address));
        let u0 = market.token_usd(pool.token0);
        let u1 = market.token_usd(pool.token1);

        let mut push_dir = |tin: Address, tout: Address, zfo: bool, u_in: f64, u_out: f64| {
            let spot = pool.marginal_rate_human(tin);
            if spot <= 0.0 || !spot.is_finite() {
                return;
            }
            let hub_in = market.is_hub_token(tin);
            let hub_out = market.is_hub_token(tout);
            let usd_mult = if u_in > 0.0 && u_out > 0.0 {
                let nr = spot * (u_out / u_in);
                let uncalibrated_mirage = (!hub_in && market.token_tax_ppm.get(&tin).is_none())
                    || (!hub_out && market.token_tax_ppm.get(&tout).is_none());
                if nr > 1.35 && uncalibrated_mirage {
                    return;
                }
                nr
            } else if hub_in || hub_out {
                1.0
            } else if pool.cached_depth_usd >= 100.0 {
                0.997
            } else {
                return;
            };
            pair_buckets.entry((tin, tout)).or_default().push(TempEdge {
                pool_id: pid,
                routable_idx,
                zfo,
                is_dirty,
                is_non_hub_out: !hub_out,
                spot_rate: spot,
                usd_mult,
                depth_usd: pool.cached_depth_usd,
                max_in_usd: pool.max_input_usd(tin, u_in),
            });
        };

        push_dir(pool.token0, pool.token1, true, u0, u1);
        push_dir(pool.token1, pool.token0, false, u1, u0);
    }

    if pair_buckets.is_empty() {
        return Vec::new();
    }

    for &(tin, tout) in pair_buckets.keys() {
        let _ = assign_id(tin);
        let _ = assign_id(tout);
    }
    let anchors = market.base_anchors();
    for &b in anchors.iter().take(3) {
        let _ = assign_id(b);
    }
    let n_nodes = nodes_vec.len();
    if n_nodes < 5 {
        return Vec::new();
    }

    let mut per_node: Vec<Vec<CsrEdge>> = vec![Vec::new(); n_nodes];
    for ((tin, tout), mut bucket) in pair_buckets {
        let (Some(&u_id), Some(&v_id)) = (id_of.get(&tin), id_of.get(&tout)) else {
            continue;
        };
        bucket.sort_by(|a, b| {
            b.spot_rate
                .partial_cmp(&a.spot_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let best_depth_idx = bucket
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                a.depth_usd
                    .partial_cmp(&b.depth_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
            .unwrap_or(0);

        let mut pushed = 0usize;
        for (i, te) in bucket.iter().enumerate() {
            if i < 2 || i == best_depth_idx {
                per_node[u_id as usize].push(CsrEdge {
                    to: v_id,
                    pool_id: te.pool_id,
                    routable_idx: te.routable_idx,
                    zfo: te.zfo,
                    is_dirty: te.is_dirty,
                    is_non_hub_out: te.is_non_hub_out,
                    spot_rate: te.spot_rate,
                    usd_mult: te.usd_mult,
                    max_in_usd: te.max_in_usd,
                });
                pushed += 1;
                if pushed >= 3 {
                    break;
                }
            }
        }
    }

    let total_edges: usize = per_node.iter().map(|v| v.len()).sum();
    let mut csr_offsets: Vec<u32> = Vec::with_capacity(n_nodes + 1);
    let mut csr_edges: Vec<CsrEdge> = Vec::with_capacity(total_edges);
    let non_hub_edge_cap = if dirty_pools.is_some() { 18 } else { 28 };
    let mut cur_off = 0u32;
    for list in &mut per_node {
        list.sort_by(|a, b| {
            b.is_dirty
                .cmp(&a.is_dirty)
                .then_with(|| {
                    b.usd_mult
                        .partial_cmp(&a.usd_mult)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        csr_offsets.push(cur_off);
        let mut non_hub_pushed = 0usize;
        for &e in list.iter() {
            if !e.is_non_hub_out || non_hub_pushed < non_hub_edge_cap {
                csr_edges.push(e);
                cur_off += 1;
                if e.is_non_hub_out {
                    non_hub_pushed += 1;
                }
            }
        }
    }
    csr_offsets.push(cur_off);

    let beam_width = pruning.deep_beam_width.clamp(1, MAX_BEAM_WIDTH);
    let sweep_hub_drift = market.last_block % 16 == 0;
    let mut slot_counts = vec![0u8; n_nodes];
    let mut slot_paths = vec![[BeamPath::default(); MAX_BEAM_WIDTH]; n_nodes];
    let mut cur_states: Vec<BeamPath> = Vec::with_capacity(256);
    let mut next_frontier: Vec<u16> = Vec::with_capacity(256);
    let mut out = Vec::new();
    let anchor_limit = if dirty_pools.is_some() { 2 } else { 3 };
    let tested_cap = if dirty_pools.is_some() {
        pruning.deep_tested_cap
    } else {
        16
    };

    for base in anchors.into_iter().take(anchor_limit) {
        let base_usd = market.token_usd(base);
        if base_usd <= 0.0 {
            continue;
        }
        let Some(&base_id) = id_of.get(&base) else {
            continue;
        };
        let b_s = csr_offsets[base_id as usize] as usize;
        let b_e = csr_offsets[base_id as usize + 1] as usize;
        if b_s == b_e {
            continue;
        }

        cur_states.clear();
        cur_states.push(BeamPath {
            rate: 1.0,
            usd_val: 1.0,
            min_depth: f64::MAX,
            len: 0,
            non_hub_cnt: 0,
            dirty: false,
            last_node: base_id,
            nodes: [base_id; MAX_DEEP_HOPS],
            pool_ids: [0; MAX_DEEP_HOPS],
            routable_idxs: [0; MAX_DEEP_HOPS],
            zfo_bits: 0,
        });

        let mut discovered: Vec<(f64, BeamPath)> = Vec::new();
        for k in 1..=MAX_DEEP_HOPS {
            next_frontier.clear();
            let min_usd = if k <= 6 { 0.980 } else { 0.985 };
            for p_idx in 0..cur_states.len() {
                let path = &cur_states[p_idx];
                let len = path.len as usize;
                let u = path.last_node as usize;
                let es = csr_offsets[u] as usize;
                let ee = csr_offsets[u + 1] as usize;
                for edge in &csr_edges[es..ee] {
                    let new_rate = path.rate * edge.spot_rate;
                    if !new_rate.is_finite() || new_rate <= 0.0 {
                        continue;
                    }
                    let new_dirty = path.dirty || edge.is_dirty;
                    if edge.to == base_id {
                        if k >= 5 && new_rate > 1.0004 && (new_dirty || sweep_hub_drift) {
                            if path.pool_ids[..len].contains(&edge.pool_id) {
                                continue;
                            }
                            let new_depth = path.min_depth.min(edge.max_in_usd);
                            let mut fin = *path;
                            fin.rate = new_rate;
                            fin.usd_val = new_rate;
                            fin.min_depth = new_depth;
                            fin.dirty = new_dirty;
                            fin.pool_ids[len] = edge.pool_id;
                            fin.routable_idxs[len] = edge.routable_idx;
                            if edge.zfo {
                                fin.zfo_bits |= 1u32 << len;
                            } else {
                                fin.zfo_bits &= !(1u32 << len);
                            }
                            fin.len = k as u8;
                            let score = new_depth.min(5_000.0) * (new_rate - 1.0);
                            discovered.push((score, fin));
                        }
                        continue;
                    }
                    if k == MAX_DEEP_HOPS {
                        continue;
                    }
                    let nh_next = path.non_hub_cnt + u8::from(edge.is_non_hub_out);
                    if nh_next > 24 {
                        continue;
                    }
                    let new_usd = path.usd_val * edge.usd_mult;
                    if new_usd < min_usd {
                        continue;
                    }
                    let dst = edge.to as usize;
                    let cnt_d = slot_counts[dst] as usize;
                    if cnt_d >= beam_width
                        && new_rate <= slot_paths[dst][beam_width - 1].rate
                    {
                        continue;
                    }
                    if path.nodes[..len].contains(&edge.to)
                        || path.pool_ids[..len].contains(&edge.pool_id)
                    {
                        continue;
                    }

                    let mut nxt = *path;
                    nxt.rate = new_rate;
                    nxt.usd_val = new_usd;
                    nxt.min_depth = path.min_depth.min(edge.max_in_usd);
                    nxt.len = k as u8;
                    nxt.non_hub_cnt = nh_next;
                    nxt.dirty = new_dirty;
                    nxt.last_node = edge.to;
                    nxt.nodes[len] = edge.to;
                    nxt.pool_ids[len] = edge.pool_id;
                    nxt.routable_idxs[len] = edge.routable_idx;
                    if edge.zfo {
                        nxt.zfo_bits |= 1u32 << len;
                    } else {
                        nxt.zfo_bits &= !(1u32 << len);
                    }

                    if insert_beam_slot(&mut slot_paths[dst], &mut slot_counts[dst], beam_width, nxt) {
                        next_frontier.push(edge.to);
                    }
                }
            }

            cur_states.clear();
            for &nid in &next_frontier {
                let d = nid as usize;
                let cnt = slot_counts[d] as usize;
                cur_states.extend_from_slice(&slot_paths[d][..cnt]);
                slot_counts[d] = 0;
            }
            if cur_states.is_empty() {
                break;
            }
            let beam_cap = if dirty_pools.is_some() {
                if k <= 8 {
                    pruning.deep_beam_cap_dirty
                } else {
                    (pruning.deep_beam_cap_dirty / 2).max(16)
                }
            } else {
                128
            };
            if cur_states.len() > beam_cap {
                cur_states.sort_by(|a, b| {
                    b.usd_val
                        .partial_cmp(&a.usd_val)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                cur_states.truncate(beam_cap);
            }
        }

        if discovered.is_empty() {
            continue;
        }
        discovered.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut seen_sets: HashSet<Vec<u16>> = HashSet::new();
        let mut tested = 0usize;
        for (_, bp) in discovered {
            let len = bp.len as usize;
            let mut key = bp.pool_ids[..len].to_vec();
            key.sort_unstable();
            if !seen_sets.insert(key) {
                continue;
            }
            tested += 1;
            if let Some(cand) =
                optimize_deep_cycle_amount(market, routable_pools, base, base_usd, &bp, min_profit_usd)
            {
                out.push(cand);
            }
            if tested >= tested_cap {
                break;
            }
        }
    }

    out
}

fn optimize_deep_cycle_amount(
    market: &MarketState,
    routable_pools: &[&PoolState],
    base: Address,
    base_usd: f64,
    bp: &BeamPath,
    min_profit_usd: f64,
) -> Option<RouteCandidate> {
    let len = bp.len as usize;
    if len < 5 {
        return None;
    }

    let mut used_addrs = Vec::with_capacity(len);
    let mut res_chain: Vec<(&PoolState, ResidualPool, bool)> = Vec::with_capacity(len);
    for i in 0..len {
        let p = routable_pools[bp.routable_idxs[i] as usize];
        let zfo = (bp.zfo_bits & (1u32 << i)) != 0;
        used_addrs.push(p.address);
        res_chain.push((p, p.to_residual(), zfo));
    }

    let max_in_usd = bp.min_depth.clamp(2.0, 25_000.0);
    let min_in_h = 0.25 / base_usd;
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
        for (_, rp, zfo) in &res_chain {
            let out = rp.quote_human(*zfo, cur);
            step_io.push((cur, out));
            if out <= 0.0 {
                return (-1e12, step_io);
            }
            cur = out;
        }
        (cur - amt_in * (1.0 + flash_fee_frac), step_io)
    };

    let (f_min, _) = eval(min_in_h);
    if f_min <= -1e11 {
        return None;
    }

    let mut a = min_in_h;
    let mut b = max_in_h;
    if eval(b).0 <= -1e11 {
        let mut lo = a;
        let mut hi = b;
        for _ in 0..10 {
            let mid = 0.5 * (lo + hi);
            if eval(mid).0 > -1e11 {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        b = lo;
    }

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
        for _ in 0..14 {
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

    let flows: Vec<DirectedFlow> = res_chain
        .iter()
        .zip(step_io.iter())
        .map(|((p, _, zfo), &(fin, fout))| {
            let (t_in, t_out) = if *zfo {
                (p.token0, p.token1)
            } else {
                (p.token1, p.token0)
            };
            DirectedFlow {
                pool: p.address,
                label: format!(
                    "{}:{}->{}",
                    p.factory_name,
                    market.token_symbol(t_in),
                    market.token_symbol(t_out)
                ),
                token_in: t_in,
                token_out: t_out,
                dex_kind: p.dex_kind,
                fee_num: p.fee_num,
                flow_in_human: fin,
                flow_out_human: fout,
            }
        })
        .collect();

    let gross_usd = gross_h * base_usd;
    let est_gas = estimate_flows_gas(&flows);
    let gas_usd = market.estimate_gas_cost_usd(est_gas, 0);
    let step_gas_usd = market.estimate_gas_cost_usd(est_gas.saturating_sub(65_000), 0);
    let net_usd = gross_usd - gas_usd;

    // Emit standalone net-profitable routes OR marginally gross-positive sub-routes for DAG coalescing
    if net_usd < min_profit_usd && gross_usd <= step_gas_usd * 0.65 {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::TokenMeta;
    use crate::config::{load_chain_config, DexKind};
    use crate::router::{compile_dag_steps, cyclic};
    use alloy::primitives::U256;
    use std::time::Instant;

    fn make_v2_pool(
        addr: u8,
        dex: &str,
        t0: Address,
        t1: Address,
        r0_h: u128,
        r1_h: u128,
        fee_num: u16,
    ) -> PoolState {
        let (a, b, ra, rb) = if t0 < t1 {
            (t0, t1, r0_h, r1_h)
        } else {
            (t1, t0, r1_h, r0_h)
        };
        let s = U256::from(10u128.pow(18));
        PoolState {
            address: Address::repeat_byte(addr),
            factory_name: dex.into(),
            dex_kind: DexKind::UniV2,
            token0: a,
            token1: b,
            dec0: 18,
            dec1: 18,
            fee_num,
            fee_ppm: ((10_000u32 - fee_num as u32) * 100),
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::from(ra) * s,
            r1_raw: U256::from(rb) * s,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: false,
            is_hub_1: false,
            cached_depth_usd: (ra + rb) as f64,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        }
    }

    #[test]
    fn test_dag_beats_single_cycle_on_fragmented_dex_network() {
        let cfg = load_chain_config("bsc").unwrap();
        let usdt = cfg.usd_anchor;
        let tok_a = Address::repeat_byte(0xA1);
        let tok_b = Address::repeat_byte(0xB2);
        let exec = Address::repeat_byte(0xEE);

        let mut market = MarketState::new(cfg);
        market.gas_price_wei = 50_000_000;
        market.tokens.insert(
            tok_a,
            TokenMeta { address: tok_a, symbol: "TOKA".into(), decimals: 18, approx_usd: 1.0 },
        );
        market.tokens.insert(
            tok_b,
            TokenMeta { address: tok_b, symbol: "TOKB".into(), decimals: 18, approx_usd: 1.0 },
        );
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        // Fragmented multi-DEX topology:
        // Two cheap buy pools USDT -> TOKA (split entry)
        market.insert_pool(make_v2_pool(0x11, "PancakeV2", usdt, tok_a, 10_000, 12_000, 9975));
        market.insert_pool(make_v2_pool(0x12, "BiSwapV2", usdt, tok_a, 10_000, 11_800, 9980));
        // Middle bridge TOKA -> TOKB
        market.insert_pool(make_v2_pool(0x13, "ThenaV2", tok_a, tok_b, 20_000, 22_000, 9980));
        // Two rich exit pools TOKB -> USDT (split exit)
        market.insert_pool(make_v2_pool(0x14, "ApeSwap", tok_b, usdt, 10_000, 11_600, 9980));
        market.insert_pool(make_v2_pool(0x15, "SushiV2", tok_b, usdt, 10_000, 11_500, 9970));

        let t0 = Instant::now();
        let routable: Vec<&PoolState> = market.pools.values().collect();
        let dag_res = solve_dag(&market, &routable, None, 0.01);
        let dag_elapsed = t0.elapsed();
        let cyc_res = cyclic::solve_cyclic(&market, &routable, None, 0.01);

        assert!(!dag_res.is_empty(), "DAG must find fragmented arbitrage");
        assert!(!cyc_res.is_empty(), "Cyclic must find single path");

        let best_dag = &dag_res[0];
        let best_cyc = cyc_res
            .iter()
            .max_by(|a, b| a.est_net_profit_usd.partial_cmp(&b.est_net_profit_usd).unwrap())
            .unwrap();

        println!(
            "DAG profit=${:.2} ({} edges, solved in {:?}) vs SingleCycle=${:.2} ({} edges)",
            best_dag.est_net_profit_usd,
            best_dag.flows.len(),
            dag_elapsed,
            best_cyc.est_net_profit_usd,
            best_cyc.flows.len()
        );
        assert!(
            best_dag.est_net_profit_usd > best_cyc.est_net_profit_usd,
            "DAG (${:.2}) must capture more profit than single cycle (${:.2})",
            best_dag.est_net_profit_usd,
            best_cyc.est_net_profit_usd
        );
        assert!(dag_elapsed.as_millis() < 20, "DAG solve took {:?}", dag_elapsed);

        let compiled = compile_dag_steps(&best_dag.flows, usdt, exec).expect("valid DAG compilation");
        assert!(compiled.len() >= 4, "Expected split/merge edges in compiled DAG");
        // Verify last outgoing edge for each tokenIn has shareBps == 10000
        let mut last_share: HashMap<Address, u16> = HashMap::new();
        for s in &compiled {
            last_share.insert(s.tokenIn, s.shareBps);
        }
        for (tk, sb) in last_share {
            assert_eq!(sb, 10_000, "Last edge for token {tk} must sweep 10000 bps");
        }
        assert!(best_dag.mermaid.starts_with("flowchart LR"));
    }

    #[test]
    fn test_deep_5hop_cycle_when_short_hops_unprofitable() {
        let cfg = load_chain_config("bsc").unwrap();
        let usdt = cfg.hub_tokens[0].address;
        let wbnb = cfg.hub_tokens[1].address;
        let usdc = cfg.hub_tokens[2].address;
        let eth = cfg.hub_tokens[6].address;
        let cake = cfg.hub_tokens[7].address;

        let mut market = MarketState::new(cfg);
        market.last_block = 16; // triggers sweep_hub_drift cleanly even without dirty filter
        market.gas_price_wei = 50_000_000;
        market.native_usd = 600.0;
        for tk in [usdt, wbnb, usdc, eth, cake] {
            if let Some(m) = market.tokens.get_mut(&tk) {
                m.approx_usd = 1.0;
            }
        }
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        // Serial dislocation across 5 hubs:
        // Bridges USDT<->WBNB, WBNB<->CAKE, USDC<->ETH are par at 1 bps (9999),
        // while PancakeV1 CAKE->USDC (+45 bps net of 20 bps fee) & SushiV2 ETH->USDT (+45 bps net)
        // are individually blocked on 2..4 hops by high 40 bps (9960) direct pair fees.
        market.insert_pool(make_v2_pool(0x21, "UniV3Bsc", usdt, wbnb, 500_000, 500_000, 9999));
        market.insert_pool(make_v2_pool(0x22, "PancakeV3", wbnb, cake, 500_000, 500_000, 9999));
        market.insert_pool(make_v2_pool(0x23, "PancakeV1", cake, usdc, 4_000, 4_028, 9980));
        market.insert_pool(make_v2_pool(0x24, "PancakeV3", usdc, eth, 500_000, 500_000, 9999));
        market.insert_pool(make_v2_pool(0x25, "SushiV2", eth, usdt, 4_000, 4_028, 9980));
        // High-fee direct closing pairs prevent 2..4 hop cycles from breaking even
        market.insert_pool(make_v2_pool(0x26, "BiSwapV2", usdc, usdt, 500_000, 500_000, 9960));
        market.insert_pool(make_v2_pool(0x27, "BiSwapV2", usdc, wbnb, 500_000, 500_000, 9960));
        market.insert_pool(make_v2_pool(0x28, "BiSwapV2", eth, wbnb, 500_000, 500_000, 9960));
        market.insert_pool(make_v2_pool(0x29, "BiSwapV2", usdt, usdc, 500_000, 500_000, 9960));

        let routable: Vec<&PoolState> = market.pools.values().collect();
        let min_p = 0.015;
        let short_res = cyclic::solve_cyclic(&market, &routable, None, min_p);
        assert!(
            short_res.is_empty(),
            "Short <=4 hop solver must find 0 profitable routes above ${min_p}, found {}",
            short_res.len()
        );

        let t0 = Instant::now();
        let deep_res = solve_deep_cycles(&market, &routable, None, min_p);
        let deep_elapsed = t0.elapsed();
        let five_hop = deep_res
            .iter()
            .find(|c| c.strategy == "Cyclic(5hop)" && c.est_net_profit_usd >= min_p)
            .expect("Expected profitable Cyclic(5hop) candidate");

        let refined = crate::router::verify_and_refine_candidate(&market, five_hop.clone(), min_p)
            .expect("Cyclic(5hop) must pass exact U256 replay");
        println!(
            "5-hop CSR solved in {:?}: borrow=${:.2} gross=${:.4} net=${:.4}",
            deep_elapsed, refined.borrow_human, refined.est_gross_profit_usd, refined.est_net_profit_usd
        );
        assert!(deep_elapsed.as_millis() < 5, "solve_deep_cycles took {:?}", deep_elapsed);
        assert!(refined.est_net_profit_usd >= min_p);
    }

    #[test]
    fn test_coalesced_dag_up_to_32_steps() {
        let cfg = load_chain_config("bsc").unwrap();
        let usdt = cfg.hub_tokens[0].address;
        let wbnb = cfg.hub_tokens[1].address;
        let usdc = cfg.hub_tokens[2].address;
        let fdusd = cfg.hub_tokens[3].address;
        let busd = cfg.hub_tokens[4].address;
        let btcb = cfg.hub_tokens[5].address;
        let eth = cfg.hub_tokens[6].address;
        let cake = cfg.hub_tokens[7].address;
        let dai = cfg.hub_tokens[8].address;
        let exec = Address::repeat_byte(0xEE);

        let mut market = MarketState::new(cfg);
        market.last_block = 32;
        market.gas_price_wei = 50_000_000;
        market.native_usd = 600.0;
        for tk in [usdt, wbnb, usdc, fdusd, busd, btcb, eth, cake, dai] {
            if let Some(m) = market.tokens.get_mut(&tk) {
                m.approx_usd = 1.0;
            }
        }
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        // 5 disjoint 3-hop branches anchored on USDT using hub intermediates (15 steps total <= 32):
        // Each branch yields ~$0.011 gross vs ~$0.010 gas -> individually below min_profit=$0.012.
        // Coalesced into 1 DAG (15 steps), shared 135k base gas unlocks strong net profit > $0.012.
        let branches = [
            (0x31u8, wbnb, usdc),
            (0x34u8, fdusd, busd),
            (0x37u8, btcb, eth),
            (0x3au8, cake, dai),
            (0x3du8, eth, fdusd),
        ];
        for (base_id, a, b) in branches {
            market.insert_pool(make_v2_pool(base_id, "PancakeV1", usdt, a, 2_000, 2_009, 9985));
            market.insert_pool(make_v2_pool(base_id + 1, "SushiV2", a, b, 2_000, 2_009, 9985));
            market.insert_pool(make_v2_pool(base_id + 2, "BiSwapV2", b, usdt, 2_000, 2_009, 9985));
        }

        let min_p = 0.012;
        let routable: Vec<&PoolState> = market.pools.values().collect();
        let single_cyc = cyclic::solve_cyclic(&market, &routable, None, min_p);
        assert!(
            single_cyc.is_empty(),
            "Individual 3-hop branches must fall below min_profit=${min_p}"
        );

        let t0 = Instant::now();
        let all = crate::router::solve_all(&market, None, min_p);
        let total_elapsed = t0.elapsed();

        let big_dag = all
            .iter()
            .find(|c| c.flows.len() >= 12 && c.flows.len() <= crate::router::MAX_DAG_STEPS)
            .expect("Expected coalesced 12..32 step DAG");

        let steps = compile_dag_steps(&big_dag.flows, big_dag.base_token, exec)
            .expect("Coalesced DAG must compile cleanly");
        assert!(steps.len() >= 12 && steps.len() <= crate::router::MAX_DAG_STEPS);
        assert!(
            steps.iter().any(|s| s.inputInPool),
            "Expected V2 Token-Hopping (inputInPool=true) in multi-branch DAG"
        );
        println!(
            "Coalesced {} ({} steps) solved + U256-verified in {:?}: borrow=${:.2} gross=${:.4} net=${:.4}",
            big_dag.strategy,
            steps.len(),
            total_elapsed,
            big_dag.borrow_human,
            big_dag.est_gross_profit_usd,
            big_dag.est_net_profit_usd
        );
        assert!(total_elapsed.as_millis() < 25, "solve_all took {:?}", total_elapsed);
        assert!(big_dag.est_net_profit_usd >= min_p);
    }

    #[test]
    fn test_deep_20hop_and_32step_dag_scaling() {
        let cfg = load_chain_config("bsc").unwrap();
        let usdt = cfg.hub_tokens[0].address;
        let hubs: [Address; 8] = [
            cfg.hub_tokens[1].address,
            cfg.hub_tokens[2].address,
            cfg.hub_tokens[3].address,
            cfg.hub_tokens[4].address,
            cfg.hub_tokens[5].address,
            cfg.hub_tokens[6].address,
            cfg.hub_tokens[7].address,
            cfg.hub_tokens[8].address,
        ];
        let exec = Address::repeat_byte(0xEE);

        // Part A: 20-hop serial cycle across 8 hubs + 11 non-hubs (0 <=4-hop opportunities)
        let mut m_deep = MarketState::new(cfg.clone());
        m_deep.last_block = 16;
        m_deep.gas_price_wei = 50_000_000;
        m_deep.native_usd = 600.0;
        for fp in &mut m_deep.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }
        for &h in &hubs {
            if let Some(m) = m_deep.tokens.get_mut(&h) {
                m.approx_usd = 1.0;
            }
        }
        let mut nh_a = Vec::with_capacity(11);
        for i in 0..11u8 {
            let mut bytes = [0u8; 20];
            bytes[0] = 0xD0;
            bytes[19] = i + 1;
            let addr = Address::from(bytes);
            m_deep.tokens.insert(
                addr,
                TokenMeta {
                    address: addr,
                    symbol: format!("NHA{i}"),
                    decimals: 18,
                    approx_usd: 1.0,
                },
            );
            nh_a.push(addr);
        }
        let mut ring = vec![usdt];
        ring.extend_from_slice(&hubs[..4]);
        ring.extend_from_slice(&nh_a[..6]);
        ring.extend_from_slice(&hubs[4..]);
        ring.extend_from_slice(&nh_a[6..]);
        assert_eq!(ring.len(), 20);
        for i in 0..20usize {
            let t_in = ring[i];
            let t_out = ring[(i + 1) % 20];
            let p_id = 0x50u8 + (i as u8);
            if i == 9 || i == 19 {
                m_deep.insert_pool(make_v2_pool(p_id, "PancakeV1", t_in, t_out, 4_000, 4_040, 9985));
            } else {
                let dex = match i % 4 {
                    0 => "UniV3Bsc",
                    1 => "PancakeV3",
                    2 => "ThenaV2",
                    _ => "SushiV2",
                };
                m_deep.insert_pool(make_v2_pool(p_id, dex, t_in, t_out, 500_000, 500_000, 9999));
            }
        }

        let routable_deep: Vec<&PoolState> = m_deep.pools.values().collect();
        let min_p_deep = 0.015;
        let short_a = cyclic::solve_cyclic(&m_deep, &routable_deep, None, min_p_deep);
        assert!(
            short_a.is_empty(),
            "Short <=4 hop solver must find 0 routes on 20-hop ring, found {}",
            short_a.len()
        );
        let t0_a = Instant::now();
        let deep_res = solve_deep_cycles(&m_deep, &routable_deep, None, min_p_deep);
        let deep_elapsed = t0_a.elapsed();
        let hop20 = deep_res
            .iter()
            .find(|c| c.strategy == "Cyclic(20hop)" && c.est_net_profit_usd >= min_p_deep)
            .expect("Expected profitable Cyclic(20hop) candidate");
        let refined_a =
            crate::router::verify_and_refine_candidate(&m_deep, hop20.clone(), min_p_deep)
                .expect("Cyclic(20hop) must pass exact U256 replay");
        let steps_a = crate::router::compile_dag_steps(&refined_a.flows, usdt, exec)
            .expect("Cyclic(20hop) must compile cleanly");
        assert_eq!(steps_a.len(), 20);
        println!(
            "20-hop CSR solved in {:?}: borrow=${:.2} gross=${:.4} net=${:.4}",
            deep_elapsed, refined_a.borrow_human, refined_a.est_gross_profit_usd, refined_a.est_net_profit_usd
        );
        assert!(deep_elapsed.as_millis() < 15, "20-hop solve_deep_cycles took {:?}", deep_elapsed);

        // Part B: 8 disjoint 4-hop non-hub branches coalesced to exact 32-step DAG (MAX_DAG_STEPS)
        let mut m_coal = MarketState::new(cfg);
        m_coal.last_block = 32;
        m_coal.gas_price_wei = 50_000_000;
        m_coal.native_usd = 600.0;
        for fp in &mut m_coal.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }
        for &h in &hubs {
            if let Some(m) = m_coal.tokens.get_mut(&h) {
                m.approx_usd = 1.0;
            }
        }
        for b in 0..8usize {
            let mut make_nh = |idx: u8, tag: &str| -> Address {
                let mut bytes = [0u8; 20];
                bytes[0] = 0xE0;
                bytes[19] = idx;
                let addr = Address::from(bytes);
                m_coal.tokens.insert(
                    addr,
                    TokenMeta {
                        address: addr,
                        symbol: format!("NHB{tag}"),
                        decimals: 18,
                        approx_usd: 1.0,
                    },
                );
                addr
            };
            let nh1 = make_nh((b as u8) * 2 + 1, &format!("{b}A"));
            let nh2 = make_nh((b as u8) * 2 + 2, &format!("{b}B"));
            let hub = hubs[b];
            let pid = 0x70u8 + (b as u8) * 4;
            m_coal.insert_pool(make_v2_pool(pid, "PancakeV1", usdt, hub, 3_600, 3_613, 9990));
            m_coal.insert_pool(make_v2_pool(pid + 1, "SushiV2", hub, nh1, 3_600, 3_613, 9990));
            m_coal.insert_pool(make_v2_pool(pid + 2, "BiSwapV2", nh1, nh2, 3_600, 3_613, 9990));
            m_coal.insert_pool(make_v2_pool(pid + 3, "ThenaV2", nh2, usdt, 3_600, 3_613, 9990));
        }

        let min_p_b = 0.022;
        let routable_b: Vec<&PoolState> = m_coal.pools.values().collect();
        let short_b = cyclic::solve_cyclic(&m_coal, &routable_b, None, min_p_b);
        assert!(
            short_b.is_empty(),
            "Individual 4-hop branches must fall below min_profit=${min_p_b}"
        );
        let t0_b = Instant::now();
        let all_b = crate::router::solve_all(&m_coal, None, min_p_b);
        let elapsed_b = t0_b.elapsed();
        let dag32 = all_b
            .iter()
            .find(|c| c.flows.len() == crate::router::MAX_DAG_STEPS && c.est_net_profit_usd >= min_p_b)
            .expect("Expected coalesced 32-step DAG (MAX_DAG_STEPS)");
        let steps_b = crate::router::compile_dag_steps(&dag32.flows, dag32.base_token, exec)
            .expect("32-step DAG must compile cleanly");
        assert_eq!(steps_b.len(), 32);
        println!(
            "32-step {} solved + U256-verified in {:?}: borrow=${:.2} gross=${:.4} net=${:.4}",
            dag32.strategy,
            elapsed_b,
            dag32.borrow_human,
            dag32.est_gross_profit_usd,
            dag32.est_net_profit_usd
        );
        assert!(elapsed_b.as_millis() < 20, "32-step solve_all took {:?}", elapsed_b);
    }

    #[test]
    fn test_v3_directional_sizing_matches_high_spread_pair() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.hub_tokens[1].address;
        let forest: Address = "0x11cf6bf6d87cb0eb9c294fd6cbfec91ee3a1a7d0".parse().unwrap();

        let mut market = MarketState::new(cfg);
        market.gas_price_wei = 50_000_000;
        market.native_usd = 591.0;
        market.tokens.insert(
            forest,
            TokenMeta {
                address: forest,
                symbol: "FOREST".into(),
                decimals: 18,
                approx_usd: 0.04,
            },
        );
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(10_000u128) * U256::from(10u128.pow(18));
        }

        // High-spread PancakeV3 FOREST/WBNB snapshot:
        // sqrtPriceX96 = 651293776019297102050627049, liquidity = 12762387809963670007153, tick = -96028, ts = 1, fee = 100ppm
        let mut v3_pool = PoolState {
            address: "0xf0601427e3bfc416bc7838fb9e15c6f0f56f4aa0".parse().unwrap(),
            factory_name: "PancakeV3".into(),
            dex_kind: DexKind::PancakeV3,
            token0: forest,
            token1: wbnb,
            dec0: 18,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 100,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::ZERO,
            r1_raw: U256::ZERO,
            sqrt_price_x96: U256::from(651_293_776_019_297_102_050_627_049u128),
            liquidity: 12_762_387_809_963_670_007_153u128,
            tick: -96028,
            tick_spacing: 1,
            allow_multi_tick: false, // insert_pool enables allow_multi_tick for official V3
            is_hub_0: false,
            is_hub_1: true,
            cached_depth_usd: 0.0,
            last_updated_block: 1_000_000,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        v3_pool.refresh_depth(0.04, 591.0);

        // GenericV2 FOREST/WBNB reserves snapshot: r0 ~ 309911.4 FOREST, r1 ~ 21.4588 WBNB, fee=9970
        let v2_pool = PoolState {
            address: "0x2871e5ce3e86cccd46f4a2104ba83937b7b15fbb".parse().unwrap(),
            factory_name: "GenericV2".into(),
            dex_kind: DexKind::UniV2,
            token0: forest,
            token1: wbnb,
            dec0: 18,
            dec1: 18,
            fee_num: 9970,
            fee_ppm: 3000,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: human_to_u256(309_911.4, 18),
            r1_raw: human_to_u256(21.4588, 18),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: false,
            is_hub_1: true,
            cached_depth_usd: 2.0 * 21.4588 * 591.0,
            last_updated_block: 1_000_000,
            v4_key: None,
            metric: None,
            balancer: None,
        };

        market.insert_pool(v3_pool);
        market.insert_pool(v2_pool);

        let routable: Vec<&PoolState> = market.pools.values().collect();
        let cyc = cyclic::solve_cyclic(&market, &routable, None, 0.008);
        let best = cyc
            .iter()
            .filter(|c| c.base_token == wbnb)
            .max_by(|a, b| a.est_net_profit_usd.partial_cmp(&b.est_net_profit_usd).unwrap())
            .expect("Expected profitable WBNB->FOREST->WBNB 2-hop cycle");

        println!(
            "High-spread FOREST/WBNB sized: borrow={:.6} WBNB gross=${:.4} net=${:.4}",
            best.borrow_human, best.est_gross_profit_usd, best.est_net_profit_usd
        );
        // Expected borrow ~0.1868 WBNB for ~$1.18 gross (vs conservative cap ~0.065 WBNB / ~$0.52)
        assert!(best.borrow_human >= 0.16 && best.borrow_human <= 0.22, "borrow={}", best.borrow_human);
        assert!(best.est_gross_profit_usd >= 1.10, "gross={}", best.est_gross_profit_usd);
    }

    #[test]
    fn replay_checks_tick_capacity_and_funds_the_refined_size() {
        use crate::config::FlashPoolCandidate;
        use crate::router::{build_arb_params, replay_dag_steps, verify_and_refine_candidate};

        let cfg = load_chain_config("bsc").unwrap();
        let base = cfg.usd_anchor;
        let mid = cfg.hub_tokens[2].address; // USDC
        let mut market = MarketState::new(cfg);
        let root = PoolState {
            dex_kind: DexKind::PancakeV3,
            fee_ppm: 2500,
            sqrt_price_x96: (U256::from(1025) << 96_usize) / U256::from(1024),
            tick: 19,
            tick_spacing: 50,
            liquidity: 10u128.pow(24),
            ..make_v2_pool(0xa1, "PancakeV3", base, mid, 0, 0, 0)
        };
        let root_addr = root.address;
        market.insert_pool(root);
        market.insert_pool(make_v2_pool(
            0xa2,
            "PancakeV2",
            base,
            mid,
            1_100_000,
            1_000_000,
            9975,
        ));
        for (_, balance) in &mut market.flash_pools {
            *balance = human_to_u256(10_000.0, 18);
        }
        let pools = market.pools.values().collect::<Vec<_>>();
        let mut cand = cyclic::solve_cyclic(&market, &pools, None, 0.01)
            .into_iter()
            .find(|c| c.base_token == base)
            .unwrap();
        let exec = Address::repeat_byte(0xee);
        let steps = compile_dag_steps(&cand.flows, base, exec).unwrap();
        assert_eq!(steps[0].pool, root_addr);
        // The search quote extrapolates through tick 0 at this size.
        assert!(market.pools[&root_addr].to_residual().quote_human(true, 2000.0) > 0.0);
        market.pools.get_mut(&root_addr).unwrap().allow_multi_tick = false;
        assert!(replay_dag_steps(&market, base, &steps, human_to_u256(2000.0, 18), 0.0, exec).is_none());
        market.pools.get_mut(&root_addr).unwrap().allow_multi_tick = true;

        cand.borrow_human = 500.0;
        market.flash_pools.clear();
        let inline = verify_and_refine_candidate(&market, cand.clone(), 0.01).unwrap();
        assert_eq!(
            (inline.flash_mode, inline.flash_source, inline.flash_fee_bps),
            (4, root_addr, 0.0)
        );

        // A split root needs an external loan. The cheapest lender only covers the initial size.
        let second_root = make_v2_pool(0xa3, "PancakeV2", base, mid, 1_000_000, 1_000_000, 9975);
        cand.flows.push(DirectedFlow {
            pool: second_root.address,
            dex_kind: second_root.dex_kind,
            fee_num: second_root.fee_num,
            ..cand.flows[0].clone()
        });
        market.insert_pool(second_root);
        let small = FlashPoolCandidate {
            flash_mode: 1,
            pool: cand.flash_source,
            base_token: base,
            fee_bps: 0.0,
        };
        let large = FlashPoolCandidate {
            flash_mode: 3,
            pool: Address::repeat_byte(0xfc),
            base_token: base,
            fee_bps: 5.0,
        };
        market.flash_pools = vec![
            (small, human_to_u256(500.0, 18)),
            (large.clone(), human_to_u256(2000.0, 18)),
        ];
        let split = verify_and_refine_candidate(&market, cand, 0.01).unwrap();
        assert!(split.borrow_human > 500.0);
        assert_eq!(split.flash_fee_bps, large.fee_bps);
        let params = build_arb_params(&split, &market, exec, U256::ONE, true).unwrap();
        assert_eq!(
            (params.flashMode, params.flashSource),
            (large.flash_mode, large.pool)
        );
    }

    #[test]
    fn test_dag_prunes_gas_negative_parasite_branch() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.hub_tokens[1].address;
        let usdc = cfg.hub_tokens[2].address;
        let busd = cfg.hub_tokens[4].address;
        let tok_hot = Address::repeat_byte(0xAA);

        let mut market = MarketState::new(cfg);
        market.last_block = 32;
        market.gas_price_wei = 50_000_000;
        market.native_usd = 600.0;
        for tk in [wbnb, usdc, busd] {
            if let Some(m) = market.tokens.get_mut(&tk) {
                m.approx_usd = if tk == wbnb { 600.0 } else { 1.0 };
            }
        }
        market.tokens.insert(
            tok_hot,
            TokenMeta { address: tok_hot, symbol: "HOT".into(), decimals: 18, approx_usd: 1.0 },
        );
        for fp in &mut market.flash_pools {
            fp.1 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        }

        // Profitable branch: WBNB -> HOT -> WBNB (scaled in $1 units by dec=18 with approx_usd=1: wait wbnb is $600 -> 1 WBNB = 600 HOT/BUSD)
        // Use 600:1 scale for WBNB pools so solve_dag's spot checks & tranches align smoothly:
        let s = U256::from(10u128.pow(18));
        let mk = |addr: u8, dex: &str, dk: DexKind, a: Address, b: Address, ra: u128, rb: u128, fee: u16, fee_ppm: u32, depth: f64| {
            let (t0, t1, r0, r1) = if a < b { (a, b, ra, rb) } else { (b, a, rb, ra) };
            PoolState {
                address: Address::repeat_byte(addr),
                factory_name: dex.into(),
                dex_kind: dk,
                token0: t0,
                token1: t1,
                dec0: 18,
                dec1: 18,
                fee_num: fee,
                fee_ppm,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: U256::from(r0) * s,
                r1_raw: U256::from(r1) * s,
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 0,
                allow_multi_tick: false,
                is_hub_0: t0 != tok_hot,
                is_hub_1: t1 != tok_hot,
                cached_depth_usd: depth,
                last_updated_block: 32,
                v4_key: None,
                metric: None,
                balancer: None,
            }
        };

        // Branch 1 (HOT): strong +1.5% spread (~$0.35 net)
        market.insert_pool(mk(0x61, "GenericV2", DexKind::UniV2, wbnb, tok_hot, 100, 60_000, 9980, 2000, 120_000.0));
        market.insert_pool(mk(0x62, "PancakeV2", DexKind::UniV2, tok_hot, wbnb, 60_000, 102, 9980, 2000, 120_000.0));

        // Branch 2 (USDC -> BUSD -> WBNB): shallow parasite loop that only yields ~$0.001 gross on $32 vs ~$0.01 step gas
        market.insert_pool(mk(0x63, "PancakeV2", DexKind::UniV2, wbnb, usdc, 10, 6_000, 9995, 500, 12_000.0));
        market.insert_pool(mk(0x64, "ThenaV2", DexKind::UniV2, usdc, busd, 100, 102, 9995, 500, 200.0));
        market.insert_pool(mk(0x65, "PancakeV2", DexKind::UniV2, busd, wbnb, 6_000, 10, 9995, 500, 12_000.0));

        let routable: Vec<&PoolState> = market.pools.values().collect();
        let dags = solve_dag(&market, &routable, None, 0.008);
        let wbnb_dag = dags.into_iter().find(|c| c.base_token == wbnb).expect("Expected WBNB DAG");
        assert!(
            wbnb_dag.flows.iter().all(|f| f.token_in == tok_hot || f.token_out == tok_hot),
            "Parasite USDC/BUSD branch must be pruned; got flows: {:?}",
            wbnb_dag.flows.iter().map(|f| &f.label).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_build_arb_params_selects_flash_v3_swap_or_moolah() {
        let cfg = load_chain_config("bsc").unwrap();
        let usdt = cfg.hub_tokens[0].address;
        let wbnb = cfg.hub_tokens[1].address;
        let moolah = alloy::primitives::address!("8F73b65B4caAf64FBA2aF91cC5D4a2A1318E5D8C");
        let v3_pool = Address::repeat_byte(0xA1);
        let v2_pool = Address::repeat_byte(0xA2);
        let v2_split = Address::repeat_byte(0xA3);
        let exec = Address::repeat_byte(0xEE);

        let market = MarketState::new(cfg);

        // 1. Eligible root V3 leg (shareBps == 10000, not repeated later) upgrades Moolah -> FLASH_V3_SWAP (4)
        let cand_v3 = crate::router::RouteCandidate {
            strategy: "Cyclic(2hop)".into(),
            base_token: usdt,
            borrow_human: 500.0,
            est_gross_profit_human: 2.0,
            est_gross_profit_usd: 2.0,
            est_net_profit_usd: 1.9,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: v3_pool,
                    label: "PancakeV3:USDT->WBNB".into(),
                    token_in: usdt,
                    token_out: wbnb,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 500.0,
                    flow_out_human: 0.85,
                },
                DirectedFlow {
                    pool: v2_pool,
                    label: "PancakeV2:WBNB->USDT".into(),
                    token_in: wbnb,
                    token_out: usdt,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 0.85,
                    flow_out_human: 502.0,
                },
            ],
            mermaid: String::new(),
        };
        let params_v3 = crate::router::build_arb_params(&cand_v3, &market, exec, U256::from(1u64), true)
            .expect("v3 root params must compile");
        assert_eq!(params_v3.flashMode, 4);
        assert_eq!(params_v3.flashSource, v3_pool);

        // 2. UniV2 root leg keeps Moolah (flashMode = 1)
        let cand_v2 = crate::router::RouteCandidate {
            strategy: "Cyclic(2hop)".into(),
            base_token: usdt,
            borrow_human: 500.0,
            est_gross_profit_human: 2.0,
            est_gross_profit_usd: 2.0,
            est_net_profit_usd: 1.9,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: v2_pool,
                    label: "PancakeV2:USDT->WBNB".into(),
                    token_in: usdt,
                    token_out: wbnb,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 500.0,
                    flow_out_human: 0.85,
                },
                DirectedFlow {
                    pool: v3_pool,
                    label: "PancakeV3:WBNB->USDT".into(),
                    token_in: wbnb,
                    token_out: usdt,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 0.85,
                    flow_out_human: 502.0,
                },
            ],
            mermaid: String::new(),
        };
        let params_v2 = crate::router::build_arb_params(&cand_v2, &market, exec, U256::from(1u64), true)
            .expect("v2 root params must compile");
        assert_eq!(params_v2.flashMode, 5);
        assert_eq!(params_v2.flashSource, v2_pool);

        // 3. Split root (shareBps < 10000 even if steps[0] is V3) preserves Moolah (flashMode = 1)
        let cand_split = crate::router::RouteCandidate {
            strategy: "ParallelSplit(2)".into(),
            base_token: usdt,
            borrow_human: 500.0,
            est_gross_profit_human: 2.0,
            est_gross_profit_usd: 2.0,
            est_net_profit_usd: 1.9,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                DirectedFlow {
                    pool: v3_pool,
                    label: "PancakeV3:USDT->WBNB".into(),
                    token_in: usdt,
                    token_out: wbnb,
                    dex_kind: DexKind::PancakeV3,
                    fee_num: 9999,
                    flow_in_human: 200.0,
                    flow_out_human: 0.34,
                },
                DirectedFlow {
                    pool: v2_split,
                    label: "BiSwapV2:USDT->WBNB".into(),
                    token_in: usdt,
                    token_out: wbnb,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9980,
                    flow_in_human: 300.0,
                    flow_out_human: 0.51,
                },
                DirectedFlow {
                    pool: v2_pool,
                    label: "PancakeV2:WBNB->USDT".into(),
                    token_in: wbnb,
                    token_out: usdt,
                    dex_kind: DexKind::UniV2,
                    fee_num: 9975,
                    flow_in_human: 0.85,
                    flow_out_human: 502.0,
                },
            ],
            mermaid: String::new(),
        };
        let params_split =
            crate::router::build_arb_params(&cand_split, &market, exec, U256::from(1u64), true)
                .expect("split params must compile");
        assert_eq!(params_split.flashMode, 1);
        assert_eq!(params_split.flashSource, moolah);
    }

    #[test]
    fn test_hub_v3_1bps_multi_tick_not_dropped_by_verify() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let usdt = cfg.usd_anchor;
        let mut market = MarketState::new(cfg);

        // 1 bps hub-to-hub V3 pool (`tick_spacing = 1`) where the optimal arb swap crosses >1 tick.
        // At $600/BNB (1 WBNB = 600 USDT), token0=USDT < token1=WBNB => sqrt_p = 1/sqrt(600), tick ~ -63974.
        let s_v3 = 1.0_f64 / 600.0_f64.sqrt();
        let tick = -63974i32;
        let v3_sqrt_x96 =
            U256::from((s_v3 * 79228162514264337593543950336.0_f64) as u128);
        let v3_pool = PoolState {
            dex_kind: DexKind::PancakeV3,
            fee_ppm: 100,
            fee_num: 0,
            sqrt_price_x96: v3_sqrt_x96,
            liquidity: 20_000_000_000_000_000_000_000u128, // ~$24.5 inside 1 tick, ~$3,000+ across multi-tick band
            tick,
            tick_spacing: 1,
            allow_multi_tick: true,
            ..make_v2_pool(0xb1, "PancakeV3", usdt, wbnb, 0, 0, 0)
        };
        let v3_addr = v3_pool.address;
        market.insert_pool(v3_pool);
        // V2 pool at ~2% discount (1 WBNB = 612 USDT): profitable multi-tick swap through v3_pool
        market.insert_pool(make_v2_pool(
            0xb2,
            "PancakeV2",
            usdt,
            wbnb,
            61_200,
            100,
            9975,
        ));
        for (_, bal) in &mut market.flash_pools {
            *bal = human_to_u256(500.0, 18);
        }

        // Confirm 1-tick bound rejects this swap, while allow_multi_tick=true allows it through verify_and_refine_candidate
        let p_ref = &market.pools[&v3_addr];
        assert_eq!(p_ref.residual_with_multi_tick(false).quote_human(true, 300.0), 0.0);
        assert!(p_ref.to_residual().quote_human(true, 300.0) > 0.0);

        let pools: Vec<&PoolState> = market.pools.values().collect();
        let cand = cyclic::solve_cyclic(&market, &pools, None, 0.01)
            .into_iter()
            .find(|c| c.flows.iter().any(|f| f.pool == v3_addr))
            .expect("Expected 1bps hub V3 cycle candidate");
        let refined = crate::router::verify_and_refine_candidate(&market, cand, 0.001)
            .expect("Hub 1bps multi-tick V3 candidate must NOT be dropped before eth_call");
        assert!(refined.est_net_profit_usd >= 0.001);
    }
}
