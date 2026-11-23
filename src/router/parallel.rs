use alloy::primitives::Address;
use std::collections::{HashMap, HashSet};

use crate::amm::{human_to_u256, PoolState, ResidualPool};
use crate::discovery::MarketState;
use crate::router::{render_mermaid, DirectedFlow, RouteCandidate};

/// Water-filling marginal rate equalizer across parallel pools on `(base_token, mid_token)`.
pub fn solve_parallel(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    if dirty_pools.is_some_and(|dp| dp.is_empty()) {
        return Vec::new();
    }
    let mut results = Vec::new();
    let base_anchors = market.base_anchors();

    // Group active pools by `(base_anchor, other_token)`
    let mut groups: HashMap<(Address, Address), Vec<&PoolState>> = HashMap::new();
    for &pool in routable_pools {
        for &anchor in &base_anchors {
            if let Some((other, _)) = pool.other_token(anchor) {
                groups.entry((anchor, other)).or_default().push(pool);
            }
        }
    }

    for ((base, mid), active) in groups {
        if active.len() < 2 {
            continue;
        }
        if let Some(dp) = dirty_pools {
            if !active.iter().any(|p| dp.contains(&p.address)) {
                continue;
            }
        }
        let base_usd = market.token_usd(base);
        if base_usd <= 0.0 {
            continue;
        }
        let mid_usd = market.token_usd(mid);

        // Check quick upper bound on spot spread
        let mut best_buy = 0.0_f64;
        let mut best_sell = 0.0_f64;
        for &p in &active {
            best_buy = best_buy.max(p.marginal_rate_human(base));
            best_sell = best_sell.max(p.marginal_rate_human(mid));
        }
        if best_buy * best_sell <= 1.0002 {
            continue;
        }

        if active.iter().any(|p| p.factory_name.starts_with("Generic")) {
            let official: Vec<&PoolState> = active
                .iter()
                .copied()
                .filter(|p| !p.factory_name.starts_with("Generic"))
                .collect();
            if official.len() >= 2 {
                if let Some(cand_off) =
                    water_fill_pair(market, base, mid, base_usd, mid_usd, &official, min_profit_usd)
                {
                    results.push(cand_off);
                }
            }
        }
        if let Some(cand) =
            water_fill_pair(market, base, mid, base_usd, mid_usd, &active, min_profit_usd)
        {
            results.push(cand);
        }
    }

    results
}

fn water_fill_pair(
    market: &MarketState,
    base: Address,
    mid: Address,
    base_usd: f64,
    _mid_usd: f64,
    pools: &[&PoolState],
    min_profit_usd: f64,
) -> Option<RouteCandidate> {
    let mut res_pools: Vec<ResidualPool> = pools.iter().map(|p| p.to_residual()).collect();
    let buy_zfo: Vec<bool> = pools.iter().map(|p| p.token0 == base).collect();

    // Adaptive tranches from $400 down to $0.80 in base_token units
    let tranches_usd = [400.0, 160.0, 75.0, 32.0, 12.0, 3.5, 0.8];
    let per_edge_gas_usd = market.estimate_gas_cost_usd(90_000, 0);
    let mut buy_flows: HashMap<usize, (f64, f64)> = HashMap::new(); // pool_idx -> (base_in, mid_out)
    let mut sell_flows: HashMap<usize, (f64, f64)> = HashMap::new(); // pool_idx -> (mid_in, base_out)
    let mut total_in_h = 0.0_f64;
    let mut total_out_h = 0.0_f64;

    // Pick flash source excluding pools we might touch? First water-fill, then pick non-overlapping flash source
    let flash_margin = 1.0002; // conservative 2 bps hurdle to cover 1 bps flash fee

    for &t_usd in &tranches_usd {
        let dx = t_usd / base_usd;
        if dx <= 0.0 {
            continue;
        }
        for _ in 0..16 {
            let mut best_pair: Option<(usize, usize, f64, f64)> = None;
            for i in 0..res_pools.len() {
                if sell_flows.contains_key(&i) {
                    continue; // pool cannot be both buy and sell
                }
                let mid_out = res_pools[i].quote_human(buy_zfo[i], dx);
                if mid_out <= 0.0 {
                    continue;
                }
                for j in 0..res_pools.len() {
                    if i == j || buy_flows.contains_key(&j) {
                        continue;
                    }
                    let new_edges = usize::from(!buy_flows.contains_key(&i))
                        + usize::from(!sell_flows.contains_key(&j));
                    if buy_flows.len() + sell_flows.len() + new_edges > 5 {
                        continue;
                    }
                    let base_back = res_pools[j].quote_human(!buy_zfo[j], mid_out);
                    if base_back > dx * flash_margin {
                        let gain = base_back - dx;
                        if new_edges > 0
                            && (base_back - dx * flash_margin) * base_usd
                                < (new_edges as f64) * per_edge_gas_usd
                        {
                            continue;
                        }
                        if best_pair.map_or(true, |b| gain > b.3) {
                            best_pair = Some((i, j, mid_out, gain));
                        }
                    }
                }
            }
            let Some((bi, sj, _, _)) = best_pair else {
                break;
            };
            let actual_mid = res_pools[bi].apply_swap_human(buy_zfo[bi], dx);
            let actual_back = res_pools[sj].apply_swap_human(!buy_zfo[sj], actual_mid);
            if actual_back <= dx * flash_margin {
                break;
            }
            total_in_h += dx;
            total_out_h += actual_back;
            let e_b = buy_flows.entry(bi).or_insert((0.0, 0.0));
            e_b.0 += dx;
            e_b.1 += actual_mid;
            let e_s = sell_flows.entry(sj).or_insert((0.0, 0.0));
            e_s.0 += actual_mid;
            e_s.1 += actual_back;
        }
    }

    if total_in_h <= 0.0 || total_out_h <= total_in_h {
        return None;
    }

    let mut used_pools: Vec<Address> = Vec::new();
    let mut flows: Vec<DirectedFlow> = Vec::new();
    for (&i, &(fin, fout)) in &buy_flows {
        let p = pools[i];
        used_pools.push(p.address);
        flows.push(DirectedFlow {
            pool: p.address,
            label: format!("{}:{}->{}", p.factory_name, market.token_symbol(base), market.token_symbol(mid)),
            token_in: base,
            token_out: mid,
            dex_kind: p.dex_kind,
            fee_num: p.fee_num,
            flow_in_human: fin,
            flow_out_human: fout,
        });
    }
    for (&j, &(fin, fout)) in &sell_flows {
        let p = pools[j];
        used_pools.push(p.address);
        flows.push(DirectedFlow {
            pool: p.address,
            label: format!("{}:{}->{}", p.factory_name, market.token_symbol(mid), market.token_symbol(base)),
            token_in: mid,
            token_out: base,
            dex_kind: p.dex_kind,
            fee_num: p.fee_num,
            flow_in_human: fin,
            flow_out_human: fout,
        });
    }

    let dec = market.token_decimals(base);
    let borrow_raw = human_to_u256(total_in_h, dec);
    let flash = market.select_flash_source(base, borrow_raw, &used_pools)?;
    let flash_fee_h = total_in_h * (flash.fee_bps / 10_000.0);
    let gross_profit_h = total_out_h - total_in_h - flash_fee_h;
    if gross_profit_h <= 0.0 {
        return None;
    }
    let gross_usd = gross_profit_h * base_usd;
    let est_gas = 150_000 + (flows.len() as u64) * 95_000;
    let gas_usd = market.estimate_gas_cost_usd(est_gas, 0);
    let net_usd = gross_usd - gas_usd;
    if net_usd < min_profit_usd {
        return None;
    }

    let mermaid = render_mermaid(market, base, &flows);
    Some(RouteCandidate {
        strategy: format!("ParallelSplit({}b/{}s)", buy_flows.len(), sell_flows.len()),
        base_token: base,
        borrow_human: total_in_h,
        est_gross_profit_human: gross_profit_h,
        est_gross_profit_usd: gross_usd,
        est_net_profit_usd: net_usd,
        flash_mode: flash.flash_mode,
        flash_source: flash.pool,
        flash_fee_bps: flash.fee_bps,
        flows,
        mermaid,
    })
}
