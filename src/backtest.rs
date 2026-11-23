use alloy::primitives::{Address, U256};
use eyre::Result;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::time::Instant;

use crate::config::load_chain_config;
use crate::discovery::MarketState;
use crate::router::{self, RouteCandidate, SolverPruningConfig};
use crate::rpc::{RpcPool, TxReceipt};
use crate::simulator::{
    decode_sim_error, diagnose_simulation, execute_simulation, load_compiled_executor,
    prepare_simulation, CompiledContract,
};

const BACKTEST_CALLER_ADDR: Address =
    alloy::primitives::address!("1111111111111111111111111111111111111111");

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct OppLogRow {
    pub block: u64,
    pub strategy: String,
    pub base_token: String,
    pub borrow_human: f64,
    pub est_net_usd: f64,
    pub sim_ok: bool,
    pub sim_net_usd: Option<f64>,
    pub sim_gas: Option<u64>,
    pub sim_err_cat: Option<String>,
    pub edges: usize,
}

#[derive(Default)]
struct BucketStats {
    total_rows: usize,
    sim_ok_rows: usize,
    sum_sim_net_usd: f64,
    max_sim_net_usd: f64,
    sum_gas: u128,
}

#[derive(Clone, Copy)]
struct BidSweepProfile {
    name: &'static str,
    surplus_floor_usd: f64,
    share_4_5: f64,
    share_6_plus: f64,
}

fn hop_bucket(edges: usize) -> &'static str {
    match edges {
        0..=3 => "02-03 hops",
        4..=5 => "04-05 hops",
        6..=10 => "06-10 hops",
        11..=15 => "11-15 hops",
        _ => "16-32 hops",
    }
}

fn sweep_bid_profile(
    profile: BidSweepProfile,
    is_wbnb: bool,
    edges: usize,
    gas_used: u64,
    gross_usd: f64,
    native_usd: f64,
    min_profit_usd: f64,
) -> Option<(f64, f64)> {
    let eff_gas = gas_used;
    let exec_gp = 51_000_000u128;
    let base_gas_wei = (eff_gas as u128) * exec_gp;
    let gross_wei = ((gross_usd / native_usd) * 1e18) as u128;
    let surplus_wei = gross_wei.saturating_sub(base_gas_wei);
    let surplus_usd = (surplus_wei as f64 / 1e18) * native_usd;
    let min_keep_usd = min_profit_usd * 1.0005;
    if surplus_usd <= min_keep_usd {
        return None;
    }
    let tip_share = if edges >= 6 && surplus_usd >= profile.surplus_floor_usd {
        profile.share_6_plus
    } else if (4..=5).contains(&edges) && surplus_usd >= profile.surplus_floor_usd {
        profile.share_4_5
    } else if surplus_usd >= 0.20 {
        0.9965
    } else if surplus_usd >= 0.04 {
        0.994
    } else {
        0.985
    };
    let tip_usd = (surplus_usd * tip_share).min(surplus_usd - min_keep_usd);
    let net_usd = surplus_usd - tip_usd;
    let _ = is_wbnb;
    if net_usd + 1e-9 < min_profit_usd {
        return None;
    }
    Some((tip_usd, net_usd))
}

pub fn run_opps_backtest(path: &str, min_profit_usd: f64) -> Result<()> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut buckets: BTreeMap<&'static str, BucketStats> = BTreeMap::new();
    let mut err_cats: HashMap<String, usize> = HashMap::new();
    let mut verified_best_per_block: BTreeMap<u64, OppLogRow> = BTreeMap::new();

    for line in reader.lines() {
        let Ok(line) = line else {
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_str::<OppLogRow>(&line) else {
            continue;
        };
        let b = buckets.entry(hop_bucket(row.edges)).or_default();
        b.total_rows += 1;
        if row.sim_ok {
            b.sim_ok_rows += 1;
            let net = row.sim_net_usd.unwrap_or(0.0);
            b.sum_sim_net_usd += net;
            if net > b.max_sim_net_usd {
                b.max_sim_net_usd = net;
            }
            b.sum_gas += row.sim_gas.unwrap_or(0) as u128;

            let replace = verified_best_per_block
                .get(&row.block)
                .is_none_or(|prev| net > prev.sim_net_usd.unwrap_or(0.0));
            if replace {
                verified_best_per_block.insert(row.block, row);
            }
        } else if let Some(cat) = &row.sim_err_cat {
            *err_cats.entry(cat.clone()).or_default() += 1;
        }
    }

    println!("=== OFFLINE OPP LOG BACKTEST ({path}) ===");
    println!(
        "{:<12} | {:>8} | {:>8} | {:>7} | {:>10} | {:>10} | {:>9}",
        "HopBucket", "Total", "SimOK", "Pass%", "AvgNet$", "MaxNet$", "AvgGas"
    );
    for (name, st) in &buckets {
        let pass_pct = if st.total_rows > 0 {
            (st.sim_ok_rows as f64 / st.total_rows as f64) * 100.0
        } else {
            0.0
        };
        let avg_net = if st.sim_ok_rows > 0 {
            st.sum_sim_net_usd / st.sim_ok_rows as f64
        } else {
            0.0
        };
        let avg_gas = if st.sim_ok_rows > 0 {
            st.sum_gas / st.sim_ok_rows as u128
        } else {
            0
        };
        println!(
            "{:<12} | {:>8} | {:>8} | {:>6.1}% | {:>10.4} | {:>10.4} | {:>9}",
            name, st.total_rows, st.sim_ok_rows, pass_pct, avg_net, st.max_sim_net_usd, avg_gas
        );
    }

    let mut cats: Vec<(String, usize)> = err_cats.into_iter().collect();
    cats.sort_by(|a, b| b.1.cmp(&a.1));
    println!("\nTop Revert Categories:");
    for (cat, cnt) in cats.into_iter().take(6) {
        println!("  {cat:<24} : {cnt}");
    }

    let profiles = [
        BidSweepProfile {
            name: "old_flat_99.5%",
            surplus_floor_usd: 1e9,
            share_4_5: 0.9965,
            share_6_plus: 0.9965,
        },
        BidSweepProfile {
            name: "live_tier_floor0.05(90/82)",
            surplus_floor_usd: 0.05,
            share_4_5: 0.90,
            share_6_plus: 0.82,
        },
        BidSweepProfile {
            name: "tier_floor0.02(90/82)",
            surplus_floor_usd: 0.02,
            share_4_5: 0.90,
            share_6_plus: 0.82,
        },
        BidSweepProfile {
            name: "tier_floor0.02(88/76)",
            surplus_floor_usd: 0.02,
            share_4_5: 0.88,
            share_6_plus: 0.76,
        },
    ];

    let native_usd = 590.0;
    println!(
        "\n=== BID PARAMETER SWEEP ({} unique winning blocks, min_profit=${min_profit_usd:.4}) ===",
        verified_best_per_block.len()
    );
    println!(
        "{:<26} | {:>7} | {:>11} | {:>11} | {:>9}",
        "Profile", "EligBlk", "TotalTip$", "KeepNet$@100%", "Keep@15%Win"
    );
    for prof in profiles {
        let mut elig = 0usize;
        let mut total_tip = 0.0;
        let mut total_keep = 0.0;
        for row in verified_best_per_block.values() {
            let gas = row.sim_gas.unwrap_or(350_000);
            let sim_net = row.sim_net_usd.unwrap_or(0.0);
            let base_gas_usd = ((gas as u128 * 51_000_000u128) as f64 / 1e18) * native_usd;
            let gross_usd = sim_net + base_gas_usd;
            let is_wbnb = row.base_token == "WBNB";
            if let Some((tip_usd, keep_usd)) = sweep_bid_profile(
                prof,
                is_wbnb,
                row.edges,
                gas,
                gross_usd,
                native_usd,
                min_profit_usd,
            ) {
                elig += 1;
                total_tip += tip_usd;
                total_keep += keep_usd;
            }
        }
        println!(
            "{:<26} | {:>7} | {:>11.4} | {:>11.4} | {:>9.4}",
            prof.name,
            elig,
            total_tip,
            total_keep,
            total_keep * 0.15
        );
    }

    Ok(())
}

async fn fetch_block_receipts_range(
    rpc: &RpcPool,
    start_bn: u64,
    end_bn: u64,
) -> Vec<(u64, Vec<TxReceipt>)> {
    let mut out = Vec::new();
    let bns: Vec<u64> = (start_bn..=end_bn).collect();
    for chunk in bns.chunks(6) {
        let mut futs = Vec::with_capacity(chunk.len());
        for &bn in chunk {
            let rpc_c = rpc.clone();
            futs.push(async move {
                let tag = format!("0x{bn:x}");
                let mut rx = rpc_c.get_block_receipts(&tag).await.ok();
                if rx.is_none() {
                    rx = rpc_c.get_block_receipts(&tag).await.ok();
                }
                (bn, rx)
            });
        }
        let res = futures_util::future::join_all(futs).await;
        for (bn, rx_opt) in res {
            if let Some(rx) = rx_opt {
                out.push((bn, rx));
            }
        }
    }
    out.sort_by_key(|(bn, _)| *bn);
    out
}

fn route_signature(c: &RouteCandidate) -> Vec<Address> {
    let mut pools: Vec<Address> = c.flows.iter().map(|f| f.pool).collect();
    pools.sort();
    pools.dedup();
    pools
}

fn scale_candidate_for_sim(cand: &RouteCandidate, factor: f64) -> RouteCandidate {
    let mut scaled = cand.clone();
    scaled.borrow_human *= factor;
    scaled.est_gross_profit_human *= factor;
    scaled.est_gross_profit_usd *= factor;
    for f in &mut scaled.flows {
        f.flow_in_human *= factor;
        f.flow_out_human *= factor;
    }
    scaled
}

fn format_route_summary(c: &RouteCandidate) -> String {
    c.flows
        .iter()
        .map(|f| f.label.as_str())
        .collect::<Vec<_>>()
        .join(" | ")
}

async fn verify_and_record_block(
    rpc: &RpcPool,
    contract: &CompiledContract,
    market: &mut MarketState,
    bn: u64,
    cands: &[RouteCandidate],
    min_profit_usd: f64,
    cooldown: &mut HashMap<Vec<Address>, u64>,
    max_verify: usize,
    log_wins: bool,
) -> (Vec<(String, f64, f64, String)>, usize) {
    let tag = format!("0x{bn:x}");
    let min_raw = U256::from(1u64);
    let mut seen_in_block: HashSet<Vec<Address>> = HashSet::new();
    let mut sim_tasks = Vec::new();
    for c in cands {
        if sim_tasks.len() >= max_verify {
            break;
        }
        let sig = route_signature(c);
        if cooldown.get(&sig).copied().unwrap_or(0) > bn || !seen_in_block.insert(sig.clone()) {
            continue;
        }
        if let Ok(mut prep) = prepare_simulation(
            market,
            contract,
            None,
            BACKTEST_CALLER_ADDR,
            c,
            min_raw,
            false,
        ) {
            prep.block_tag = tag.clone();
            let prep_half = if c.est_net_profit_usd >= 0.02 {
                let half_c = scale_candidate_for_sim(c, 0.50);
                prepare_simulation(
                    market,
                    contract,
                    None,
                    BACKTEST_CALLER_ADDR,
                    &half_c,
                    min_raw,
                    false,
                )
                .ok()
                .map(|mut ph| {
                    ph.block_tag = tag.clone();
                    ph
                })
            } else {
                None
            };
            sim_tasks.push((c, sig, prep, prep_half));
        }
    }
    if sim_tasks.is_empty() {
        return (Vec::new(), 0);
    }
    let checked = sim_tasks.len();
    let futs = sim_tasks
        .iter()
        .map(|(_, _, prep, _)| execute_simulation(rpc, prep));
    let results = futures_util::future::join_all(futs).await;

    let mut wins = Vec::new();
    let mut diag_left = 2usize;
    for ((c, sig, prep, prep_half), res) in sim_tasks.into_iter().zip(results.into_iter()) {
        let mut non_hub: Vec<Address> = c
            .flows
            .iter()
            .flat_map(|f| [f.token_in, f.token_out])
            .filter(|a| !market.is_hub_token(*a))
            .collect();
        non_hub.sort();
        non_hub.dedup();

        let outcome = match res {
            Ok(sim) if sim.net_profit_usd >= min_profit_usd => Ok(sim),
            other => {
                let try_half = matches!(&other, Ok(_))
                    || other
                        .as_ref()
                        .is_err_and(|e| e.to_string().starts_with("[EVM_REVERT]"));
                if try_half {
                    if let Some(ph) = prep_half {
                        execute_simulation(rpc, &ph).await
                    } else {
                        other
                    }
                } else {
                    other
                }
            }
        };

        match outcome {
            Ok(sim) if sim.net_profit_usd >= min_profit_usd => {
                let summary = format_route_summary(c);
                market.record_sim_success(&sig, &non_hub);
                if log_wins {
                    println!(
                        "  [EVM VERIFIED] block={bn} strategy={} sim_net=${:.4} gas={} route={summary}",
                        c.strategy, sim.net_profit_usd, sim.gas_used
                    );
                }
                wins.push((
                    c.strategy.clone(),
                    sim.gross_profit_usd,
                    sim.net_profit_usd,
                    summary,
                ));
            }
            Ok(_) => {
                cooldown.insert(sig, bn.saturating_add(6));
            }
            Err(e) => {
                let mut err_str = e.to_string();
                if diag_left > 0 {
                    diag_left -= 1;
                    if let Some(d_err) = diagnose_simulation(rpc, &prep).await {
                        err_str = d_err;
                    }
                }
                let diag = decode_sim_error(
                    &err_str,
                    market.token_decimals(c.base_token),
                    market.token_usd(c.base_token),
                );
                market.record_sim_diagnostic(&sig, &non_hub, &diag);
                cooldown.insert(sig, bn.saturating_add(25));
            }
        }
    }
    (wins, checked)
}

#[derive(Default)]
struct AbProfileSummary {
    name: &'static str,
    p50_us: u128,
    p95_us: u128,
    max_us: u128,
    total_cands: usize,
    deep_cands: usize,
    verified_checked: usize,
    verified_opps: usize,
    verified_blocks: usize,
    sum_best_gross: f64,
    sum_best_net: f64,
    winning_blocks: HashSet<u64>,
    win_details: Vec<(u64, String, f64, String)>,
}

pub async fn run_chain_backtest(
    chain: &str,
    blocks: u64,
    end_block: Option<u64>,
    min_profit_usd: f64,
    verify_evm: bool,
    ab_pruning: bool,
) -> Result<()> {
    let cfg = load_chain_config(chain)?;
    let mut rpc = RpcPool::from_config(&cfg)?;
    rpc.prioritize_archive_rpcs();
    let contract = if verify_evm {
        Some(load_compiled_executor()?)
    } else {
        None
    };

    let live_head = rpc.block_number().await?;
    let head = end_block.unwrap_or_else(|| live_head.saturating_sub(2));
    let start_bn = head.saturating_sub(blocks.saturating_sub(1));
    let init_bn = start_bn.saturating_sub(1);

    let mut base_market = MarketState::new(cfg.clone());
    base_market.bootstrap_at_block(&rpc, init_bn).await?;

    // Warm up persistent toxic/broken pools at init_bn before timed replay begins.
    if let Some(ref ct) = contract {
        let warm_cands = router::solve_all(&base_market, None, min_profit_usd);
        if !warm_cands.is_empty() {
            let mut warm_cd = HashMap::new();
            let (warm_wins, warm_chk) = verify_and_record_block(
                &rpc,
                ct,
                &mut base_market,
                init_bn,
                &warm_cands,
                min_profit_usd,
                &mut warm_cd,
                8,
                false,
            )
            .await;
            println!(
                "Steady-state warmup at block {init_bn}: checked={warm_chk}, passed={}, pools={}",
                warm_wins.len(),
                base_market.pools.len()
            );
        }
    }

    let block_receipts = fetch_block_receipts_range(&rpc, start_bn, head).await;
    println!(
        "=== HISTORICAL BLOCK BACKTEST ({}/{} blocks fetched: {start_bn}..={head}, init_at={init_bn}, pools={}) ===",
        block_receipts.len(),
        head.saturating_sub(start_bn) + 1,
        base_market.pools.len()
    );

    let profiles: Vec<SolverPruningConfig> = if ab_pruning {
        router::pruning_ab_profiles().to_vec()
    } else {
        vec![SolverPruningConfig::default()]
    };

    let mut summaries: Vec<AbProfileSummary> = Vec::with_capacity(profiles.len());

    for prof in profiles {
        let mut market = base_market.clone();
        let mut cooldown: HashMap<Vec<Address>, u64> = HashMap::new();
        let mut solve_times_us: Vec<u128> = Vec::with_capacity(block_receipts.len());
        let mut hop_counts: BTreeMap<&'static str, (usize, f64, f64)> = BTreeMap::new();
        let mut sum = AbProfileSummary {
            name: prof.name,
            ..Default::default()
        };

        for (bn, receipts) in &block_receipts {
            let delta = market.apply_receipts_in_place(*bn, receipts);
            let t0 = Instant::now();
            let cands = router::solve_all_with_config(
                &market,
                Some(&delta.dirty_pools),
                min_profit_usd,
                &prof,
            );
            solve_times_us.push(t0.elapsed().as_micros());

            for c in &cands {
                sum.total_cands += 1;
                if c.flows.len() >= 4 {
                    sum.deep_cands += 1;
                }
                let e = hop_counts
                    .entry(hop_bucket(c.flows.len()))
                    .or_insert((0, 0.0, 0.0));
                e.0 += 1;
                e.1 += c.est_net_profit_usd;
                if c.est_net_profit_usd > e.2 {
                    e.2 = c.est_net_profit_usd;
                }
            }

            if let Some(ref ct) = contract {
                let (wins, checked) = verify_and_record_block(
                    &rpc,
                    ct,
                    &mut market,
                    *bn,
                    &cands,
                    min_profit_usd,
                    &mut cooldown,
                    8,
                    !ab_pruning,
                )
                .await;
                sum.verified_checked += checked;
                if !wins.is_empty() {
                    sum.verified_blocks += 1;
                    sum.verified_opps += wins.len();
                    sum.winning_blocks.insert(*bn);
                    let best_win = wins
                        .iter()
                        .max_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
                        .unwrap();
                    sum.sum_best_gross += best_win.1;
                    sum.sum_best_net += best_win.2;
                    sum.win_details
                        .push((*bn, best_win.0.clone(), best_win.2, best_win.3.clone()));
                }
            }
        }

        solve_times_us.sort_unstable();
        sum.p50_us = solve_times_us
            .get(solve_times_us.len() / 2)
            .copied()
            .unwrap_or(0);
        sum.p95_us = solve_times_us
            .get((solve_times_us.len() * 95) / 100)
            .copied()
            .unwrap_or(0);
        sum.max_us = solve_times_us.last().copied().unwrap_or(0);

        if !ab_pruning {
            println!("Solve latency: p50={}us p95={}us max={}us", sum.p50_us, sum.p95_us, sum.max_us);
            println!(
                "{:<12} | {:>8} | {:>11} | {:>10}",
                "HopBucket", "Cands", "SumEstNet$", "MaxEstNet$"
            );
            for (name, (cnt, sum_net, max_net)) in hop_counts {
                println!("{:<12} | {:>8} | {:>11.4} | {:>10.4}", name, cnt, sum_net, max_net);
            }
            if verify_evm {
                println!(
                    "Historical EVM eth_call verified: {} opps across {} blocks (checked {}, sum_gross=${:.4}, sum_net=${:.4})",
                    sum.verified_opps, sum.verified_blocks, sum.verified_checked, sum.sum_best_gross, sum.sum_best_net
                );
            }
        }
        summaries.push(sum);
    }

    if ab_pruning && !summaries.is_empty() {
        println!("\n=== PRUNING A/B COMPARISON ({} blocks: {start_bn}..={head}) ===", block_receipts.len());
        println!(
            "{:<25} | {:>7} | {:>7} | {:>7} | {:>6} | {:>5} | {:>7} | {:>8} | {:>9} | {:>9}",
            "Profile", "p50_us", "p95_us", "max_us", "Cands", "Deep", "VerBlks", "VerOpps", "SumGross$", "SumNet$"
        );
        for s in &summaries {
            println!(
                "{:<25} | {:>7} | {:>7} | {:>7} | {:>6} | {:>5} | {:>4}/{:<2} | {:>8} | {:>9.4} | {:>9.4}",
                s.name,
                s.p50_us,
                s.p95_us,
                s.max_us,
                s.total_cands,
                s.deep_cands,
                s.verified_blocks,
                s.verified_checked,
                s.verified_opps,
                s.sum_best_gross,
                s.sum_best_net
            );
        }
        let base_blks = &summaries[0].winning_blocks;
        for s in summaries.iter().skip(1) {
            let extra: Vec<_> = s
                .win_details
                .iter()
                .filter(|(bn, _, _, _)| !base_blks.contains(bn))
                .collect();
            if !extra.is_empty() {
                println!("\nExtra winning blocks found by {} vs baseline:", s.name);
                for (bn, strat, net, route) in extra {
                    println!("  block={bn} strategy={strat} sim_net=${net:.4} route={route}");
                }
            }
        }
    }

    Ok(())
}
