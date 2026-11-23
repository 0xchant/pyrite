mod abi;
mod amm;
mod backtest;
mod config;
mod discovery;
mod executor;
mod mempool;
mod mev_share;
mod router;
mod rpc;
mod simulator;

use alloy::primitives::{Address, B256, U256};
use alloy::signers::local::PrivateKeySigner;
use alloy::sol_types::{SolCall, SolEvent, SolValue};
use chrono::Utc;
use clap::{Parser, Subcommand};
use eyre::Result;
use futures_util::future::join_all;
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::time::{Duration, Instant};

use crate::abi::IDagArbExecutor;
use crate::amm::{human_to_u256, u256_to_human};
use crate::config::{
    load_chain_config, load_runtime_json_config, BiddingMode, ChainConfig, RuntimeJsonConfig,
    SubmissionMode, TxType,
};
use crate::discovery::MarketState;
use crate::executor::{
    deploy_executor, load_or_create_wallet, send_arb_tx, submit_arb_bundle_nonblocking,
    upsert_env_var, BscTxSpec, PnlTracker,
};
use crate::rpc::{RpcPool, TxReceipt};
use crate::simulator::{
    diagnose_simulation, execute_simulation, load_compiled_executor, prepare_simulation,
    PreparedSimulation, VIRTUAL_EXECUTOR_ADDR,
};

#[derive(Parser)]
#[command(name = "pyrite", about = "Pyrite (Fool's Gold) — Multi-Chain EVM DAG Atomic Arbitrage Engine")]
struct Cli {
    /// Optional path to runtime JSON config (`pyrite.json` auto-detected if present)
    #[arg(long, global = true)]
    config: Option<String>,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate (or inspect existing) local searcher wallet in .env (0600 permissions)
    InitWallet {
        #[arg(long)]
        chain: Option<String>,
    },
    /// Deploy DagArbExecutor smart contract and save EXECUTOR_ADDRESS into .env
    Deploy {
        #[arg(long)]
        chain: Option<String>,
    },
    /// Scan live mainnet blocks, build multi-DEX pool graph, solve Parallel/Cyclic/DAG routes & verify via eth_call
    Scan {
        #[arg(long)]
        chain: Option<String>,
        #[arg(long, default_value_t = 20)]
        blocks: u64,
        #[arg(long)]
        min_profit_usd: Option<f64>,
    },
    /// Autonomous searcher + live tx executor until target realized USD profit is reached
    Run {
        #[arg(long)]
        chain: Option<String>,
        #[arg(long)]
        target_usd: Option<f64>,
        #[arg(long)]
        min_profit_usd: Option<f64>,
        #[arg(long, default_missing_value = "true", num_args = 0..=1)]
        dry_run: Option<bool>,
        #[arg(long)]
        max_blocks: Option<u64>,
    },
    /// Backtest historical blocks or offline opportunities.jsonl to tune hop & bidding parameters
    Backtest {
        #[arg(long)]
        chain: Option<String>,
        #[arg(long, default_value_t = 40)]
        blocks: u64,
        #[arg(long)]
        end_block: Option<u64>,
        #[arg(long)]
        min_profit_usd: Option<f64>,
        #[arg(long)]
        opps_file: Option<String>,
        #[arg(long, default_value_t = false)]
        verify_evm: bool,
        /// Run side-by-side baseline/medium/wide pruning A/B backtest on identical historical blocks
        #[arg(long, default_value_t = false)]
        ab_pruning: bool,
    },
}

#[derive(Clone)]
struct PendingBundle {
    signer_addr: Address,
    tx_hash: B256,
    candidate_hashes: Vec<B256>,
    submitted_block: u64,
    max_block: u64,
    nonce: u64,
    strategy: String,
    base_token: Address,
    gross_profit_usd: f64,
    bid_gas_usd: f64,
    sim_gas: u64,
    l1_fee_wei: u128,
    bid_gas_price_wei: Option<u128>,
    tip_wei: u128,
}

#[allow(clippy::too_many_arguments)]
fn finalize_live_receipt(
    market: &MarketState,
    pnl: &mut PnlTracker,
    pnl_path: &str,
    tx_hash: B256,
    block_number: u64,
    strategy: &str,
    base_token: Address,
    gross_profit_usd: f64,
    bid_gas_usd: f64,
    sim_gas: u64,
    l1_fee_wei: u128,
    bid_gas_price_wei: Option<u128>,
    tip_wei: u128,
    receipt_opt: Option<TxReceipt>,
) {
    pnl.total_trades += 1;
    let won = receipt_opt
        .as_ref()
        .is_some_and(|r| r.status.as_deref() == Some("0x1"));
    let mut realized_gross_usd = 0.0;
    let mut realized_gas_usd = bid_gas_usd * 0.4;

    if let Some(ref rx) = receipt_opt {
        let g_used = rx
            .gas_used
            .as_deref()
            .and_then(|s| rpc::parse_hex_u128(s).ok())
            .unwrap_or(sim_gas as u128);
        let eff_gp = rx
            .effective_gas_price
            .as_deref()
            .and_then(|s| rpc::parse_hex_u128(s).ok())
            .unwrap_or(bid_gas_price_wei.unwrap_or(market.gas_price_wei));
        let mut decoded_bribe_wei: Option<u128> = None;

        if won {
            realized_gross_usd = gross_profit_usd;
            for lg in &rx.logs {
                if lg.topics.first() == Some(&IDagArbExecutor::ArbExecuted::SIGNATURE_HASH) {
                    if let Ok((_, net_p_raw, bribe_paid_raw, _)) =
                        <(U256, U256, U256, U256)>::abi_decode(&lg.data)
                    {
                        let dec = market.token_decimals(base_token);
                        realized_gross_usd =
                            u256_to_human(net_p_raw, dec) * market.token_usd(base_token);
                        decoded_bribe_wei = Some(bribe_paid_raw.saturating_to::<u128>());
                    }
                }
            }
        }
        let actual_bribe_wei = if won {
            decoded_bribe_wei.unwrap_or(tip_wei)
        } else {
            0
        };
        realized_gas_usd =
            (((g_used * eff_gp + l1_fee_wei + actual_bribe_wei) as f64) / 1e18) * market.native_usd;
    }

    let realized_net_usd = if won {
        realized_gross_usd - realized_gas_usd
    } else {
        -realized_gas_usd
    };

    if won {
        pnl.winning_trades += 1;
    } else {
        pnl.reverted_trades += 1;
    }
    pnl.cumulative_gross_usd += realized_gross_usd;
    pnl.cumulative_gas_usd += realized_gas_usd;
    pnl.cumulative_net_usd += realized_net_usd;

    pnl.trades.push(executor::ExecutedTradeLog {
        timestamp: Utc::now().to_rfc3339(),
        tx_hash: format!("{tx_hash:#x}"),
        block_number,
        strategy: strategy.to_string(),
        status: if won { "CONFIRMED" } else { "REVERTED/PENDING" }.to_string(),
        gross_usd: realized_gross_usd,
        gas_usd: realized_gas_usd,
        net_usd: realized_net_usd,
    });
    let _ = pnl.save(pnl_path);
    tracing::info!(
        %tx_hash,
        won,
        bid_gwei = format!(
            "{:.3}",
            bid_gas_price_wei.unwrap_or(market.gas_price_wei) as f64 / 1e9
        ),
        realized_net_usd = format!("${:.4}", realized_net_usd),
        cum_net_usd = format!("${:.4}", pnl.cumulative_net_usd),
        "LIVE TX RESULT"
    );
}

#[derive(Serialize)]
struct OppLog {
    timestamp: String,
    chain: String,
    block: u64,
    strategy: String,
    base_token: String,
    borrow_human: f64,
    est_net_usd: f64,
    sim_ok: bool,
    sim_net_usd: Option<f64>,
    sim_gas: Option<u64>,
    sim_err: Option<String>,
    sim_err_cat: Option<String>,
    edges: usize,
    route: String,
}

fn resolve_cli_chain(cli_chain: Option<String>, json_cfg: Option<&RuntimeJsonConfig>) -> String {
    if let Some(c) = cli_chain.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return c;
    }
    if let Some(c) = json_cfg
        .and_then(|j| j.chain.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return c.to_string();
    }
    if let Ok(env_c) = std::env::var("CHAIN") {
        let trimmed = env_c.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    "bsc".to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();
    let json_cfg = load_runtime_json_config(cli.config.as_deref())?;
    if let Some(ref jc) = json_cfg {
        jc.apply_to_env_if_unset();
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let jc_ref = json_cfg.as_ref();
    match cli.command {
        Commands::InitWallet { chain } => {
            let chain = resolve_cli_chain(chain, jc_ref);
            cmd_init_wallet(&chain).await
        }
        Commands::Deploy { chain } => {
            let chain = resolve_cli_chain(chain, jc_ref);
            cmd_deploy(&chain).await
        }
        Commands::Scan {
            chain,
            blocks,
            min_profit_usd,
        } => {
            let chain = resolve_cli_chain(chain, jc_ref);
            let min_p = min_profit_usd
                .or_else(|| jc_ref.and_then(|j| j.mode.min_profit_usd))
                .unwrap_or(0.02);
            cmd_scan_or_run(&chain, min_p, 1.0, true, Some(blocks)).await
        }
        Commands::Run {
            chain,
            target_usd,
            min_profit_usd,
            dry_run,
            max_blocks,
        } => {
            let chain = resolve_cli_chain(chain, jc_ref);
            let target = target_usd
                .or_else(|| jc_ref.and_then(|j| j.mode.target_usd))
                .unwrap_or(1.0);
            let min_p = min_profit_usd
                .or_else(|| jc_ref.and_then(|j| j.mode.min_profit_usd))
                .unwrap_or(0.05);
            let is_dry = dry_run
                .or_else(|| jc_ref.and_then(|j| j.mode.dry_run))
                .unwrap_or(false);
            let max_b = max_blocks.or_else(|| jc_ref.and_then(|j| j.mode.max_blocks));
            cmd_scan_or_run(&chain, min_p, target, is_dry, max_b).await
        }
        Commands::Backtest {
            chain,
            blocks,
            end_block,
            min_profit_usd,
            opps_file,
            verify_evm,
            ab_pruning,
        } => {
            let min_p = min_profit_usd
                .or_else(|| jc_ref.and_then(|j| j.mode.min_profit_usd))
                .unwrap_or(0.0008);
            if let Some(path) = opps_file {
                backtest::run_opps_backtest(&path, min_p)
            } else {
                let chain = resolve_cli_chain(chain, jc_ref);
                backtest::run_chain_backtest(
                    &chain,
                    blocks,
                    end_block,
                    min_p,
                    verify_evm,
                    ab_pruning,
                )
                .await
            }
        }
    }
}

async fn cmd_init_wallet(chain: &str) -> Result<()> {
    let cfg = load_chain_config(chain)?;
    let rpc = RpcPool::from_config(&cfg)?;

    let (signer, created) = load_or_create_wallet(".env", true)?;
    let addr = signer.address();
    let bal = rpc.get_balance(addr).await.unwrap_or(U256::ZERO);
    let bal_native = u256_to_human(bal, 18);

    println!("============================================================");
    println!(
        "Searcher Wallet ({}) | Chain: {} (id={})",
        if created { "NEWLY GENERATED in .env" } else { "LOADED from .env" },
        cfg.as_str(),
        cfg.chain_id
    );
    println!("Public Address : {addr:#x}");
    println!("Live Balance   : {bal_native:.6} {}", cfg.native_symbol);
    println!("Gas Needed     : {}", cfg.bidding.init_gas_hint);
    println!("============================================================");
    Ok(())
}

async fn cmd_deploy(chain: &str) -> Result<()> {
    let cfg = load_chain_config(chain)?;
    let rpc = RpcPool::from_config(&cfg)?;
    let (signer, _) = load_or_create_wallet(".env", false)?;
    let mut market = MarketState::new(cfg.clone());
    market.bootstrap(&rpc).await?;
    let contract = load_compiled_executor()?;

    let bal = rpc.get_balance(signer.address()).await?;
    if bal.is_zero() {
        eyre::bail!(
            "Wallet {:#x} has 0 {}; deposit gas first before deploying.",
            signer.address(),
            cfg.native_symbol
        );
    }

    let addr = deploy_executor(&rpc, &market, &contract, &signer).await?;
    let env_key = cfg.executor_env_key();
    upsert_env_var(".env", &env_key, &format!("{addr:#x}"))?;
    if cfg.chain == "bsc" {
        upsert_env_var(".env", "EXECUTOR_ADDRESS", &format!("{addr:#x}"))?;
    }
    println!("DagArbExecutor deployed at {addr:#x} and saved to .env ({env_key})");
    Ok(())
}

async fn ensure_executor_deployed(
    rpc: &RpcPool,
    market: &MarketState,
    contract: &simulator::CompiledContract,
    signer: &PrivateKeySigner,
    cfg: &ChainConfig,
    existing: Option<Address>,
) -> Result<Address> {
    if let Some(exec_addr) = existing {
        let code = rpc.get_code(exec_addr).await.unwrap_or_default();
        if !code.is_empty() {
            let c = IDagArbExecutor::ownerCall {}.abi_encode();
            if rpc
                .eth_call(None, exec_addr, &c, "latest", None)
                .await
                .ok()
                .and_then(|r| IDagArbExecutor::ownerCall::abi_decode_returns(&r).ok())
                == Some(signer.address())
            {
                return Ok(exec_addr);
            }
        }
    }

    tracing::info!(
        wallet = %signer.address(),
        chain = cfg.as_str(),
        "auto-deploying DagArbExecutor..."
    );
    let addr = deploy_executor(rpc, market, contract, signer).await?;
    let env_key = cfg.executor_env_key();
    upsert_env_var(".env", &env_key, &format!("{addr:#x}"))?;
    if cfg.chain == "bsc" {
        upsert_env_var(".env", "EXECUTOR_ADDRESS", &format!("{addr:#x}"))?;
    }
    tracing::info!(executor = %addr, chain = cfg.as_str(), "DagArbExecutor live on-chain");
    Ok(addr)
}

fn load_chain_executor_env(cfg: &ChainConfig) -> Option<Address> {
    std::env::var(cfg.executor_env_key())
        .ok()
        .or_else(|| {
            if cfg.chain == "bsc" {
                std::env::var("EXECUTOR_ADDRESS").ok()
            } else {
                None
            }
        })
        .and_then(|s| s.trim().parse::<Address>().ok())
}

async fn cmd_scan_or_run(
    chain: &str,
    min_profit_usd: f64,
    target_usd: f64,
    dry_run: bool,
    max_blocks: Option<u64>,
) -> Result<()> {
    let contract = load_compiled_executor()?;
    let signer_opt: Option<PrivateKeySigner> =
        load_or_create_wallet(".env", !dry_run).ok().map(|(s, _)| s);
    let caller = signer_opt
        .as_ref()
        .map(|s| s.address())
        .unwrap_or(VIRTUAL_EXECUTOR_ADDR);

    let cfg = load_chain_config(chain)?;
    let rpc = RpcPool::from_config(&cfg)?;

    let mut deployed_executor = load_chain_executor_env(&cfg);
    if dry_run {
        if let Some(exec_addr) = deployed_executor {
            let code = rpc.get_code(exec_addr).await.unwrap_or_default();
            let owned = if !code.is_empty() {
                let c = IDagArbExecutor::ownerCall {}.abi_encode();
                rpc.eth_call(None, exec_addr, &c, "latest", None)
                    .await
                    .ok()
                    .and_then(|r| IDagArbExecutor::ownerCall::abi_decode_returns(&r).ok())
                    == Some(caller)
            } else {
                false
            };
            if !owned {
                deployed_executor = None;
            }
        }
    }
    let mut market = MarketState::new(cfg.clone());
    let t_boot = Instant::now();
    market.bootstrap(&rpc).await?;
    tracing::info!(
        chain = cfg.as_str(),
        block = market.last_block,
        pools = market.pools.len(),
        tokens = market.tokens.len(),
        gas_gwei = format!("{:.4}", market.gas_price_wei as f64 / 1e9),
        native_usd = format!("{:.2}", market.native_usd),
        boot_ms = t_boot.elapsed().as_millis(),
        "bootstrap complete"
    );

    let mut live_ready = false;
    let mut cached_nonce: Option<u64> = None;
    let mut wallet_bal_wei = U256::ZERO;
    let eoa_pool: Option<std::sync::Arc<executor::EoaPool>> = signer_opt.as_ref().map(|s| {
        let extras = executor::load_extra_signers_from_env();
        std::sync::Arc::new(executor::EoaPool::new(s.clone(), extras))
    });
    if !dry_run {
        if let Some(ref signer) = signer_opt {
            let min_bal = cfg.bidding.min_funding_native;
            let bal_wei = rpc.get_balance(signer.address()).await.unwrap_or(U256::ZERO);
            wallet_bal_wei = bal_wei;
            let bal_f = u256_to_human(bal_wei, 18);
            if bal_f >= min_bal {
                let addr = ensure_executor_deployed(
                    &rpc,
                    &market,
                    &contract,
                    signer,
                    &cfg,
                    deployed_executor,
                )
                .await?;
                deployed_executor = Some(addr);
                live_ready = true;
                cached_nonce = rpc.get_transaction_count(signer.address()).await.ok();
                if let Some(ref pool) = eoa_pool {
                    if pool.len() > 1 {
                        if let Err(e) = executor::sync_executor_callers(
                            &rpc,
                            &market,
                            signer,
                            addr,
                            &pool.extra_addresses(),
                        )
                        .await
                        {
                            tracing::warn!(err = %e, "sync_executor_callers failed");
                        }
                    }
                    pool.refresh_all(&rpc, cfg.multicall3).await;
                    wallet_bal_wei = pool.max_balance_wei().max(bal_wei);
                    tracing::info!(
                        eoas = pool.len(),
                        primary = %signer.address(),
                        max_native = format!("{:.6}", u256_to_human(wallet_bal_wei, 18)),
                        "EOA concurrency pool ready"
                    );
                } else if let Ok(b) = rpc.get_balance(signer.address()).await {
                    wallet_bal_wei = b;
                }
            } else {
                tracing::warn!(
                    wallet = %signer.address(),
                    balance = format!("{bal_f:.6} {}", cfg.native_symbol),
                    needed = format!("{min_bal} {}", cfg.native_symbol),
                    "wallet unfunded — warming pool graph & watching for deposit arrival"
                );
            }
        }
    }

    let pnl_path = format!("logs/realized_pnl_{}.json", cfg.as_str());
    let mut pnl = PnlTracker::load(&pnl_path, cfg.as_str());
    std::fs::create_dir_all("logs")?;

    let initial_cands = router::solve_all(&market, None, min_profit_usd);
    if !initial_cands.is_empty() {
        tracing::info!(count = initial_cands.len(), "initial graph candidates found");
    }

    let mut processed_blocks = 0u64;
    let mut last_seen_block = market.last_block.saturating_sub(1);
    let mut total_verified_opps = 0u64;
    let mut total_candidates = 0u64;
    let mut carryover_dirty_pools: HashSet<Address> = HashSet::new();
    let mut recent_dirty_ring: std::collections::VecDeque<(u64, Vec<Address>)> =
        std::collections::VecDeque::with_capacity(8);
    let mut failed_route_cooldown: HashMap<Vec<Address>, (u64, u64)> = HashMap::new();
    let mut backrun_pool_cooldowns = HashMap::new();
    let mut active_bundles: Vec<PendingBundle> = Vec::new();
    let (backrun_tx, mut backrun_rx) = tokio::sync::mpsc::unbounded_channel::<PendingBundle>();
    let (late_win_tx, mut late_win_rx) =
        tokio::sync::mpsc::unbounded_channel::<(PendingBundle, B256, TxReceipt)>();
    let backrun_busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut receipt_retry: (u64, u8) = (0, 0);
    let mut last_block_instant = Instant::now() - Duration::from_secs(1);
    rpc.spawn_keep_warm();
    rpc.spawn_ws_head_watcher();
    let watched = cfg.watched_router_addrs();
    let mut pending_rx = if cfg.is_mev_bundle_enabled() && !watched.is_empty() {
        Some(mempool::spawn_ws_pending_watcher(&cfg.ws_rpcs, watched))
    } else {
        None
    };
    let mut mev_share_rx = if cfg.is_mev_share_enabled() {
        tracing::info!(
            sse_url = %cfg.mev_share.sse_url,
            relay_url = %cfg.mev_share.relay_url,
            builders = cfg.mev_share.builders.len(),
            min_swap_usd = cfg.mev_share_min_swap_usd(),
            "starting Flashbots MEV-Share SSE backrun listener"
        );
        Some(mev_share::spawn_mev_share_sse_watcher(
            cfg.mev_share.sse_url.clone(),
        ))
    } else {
        None
    };
    let mut market_snap = std::sync::Arc::new(market.clone());
    let mev_share_inflight = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mev_share_block_subs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut mev_share_seen_hashes: HashSet<B256> = HashSet::new();
    let rpc_kw = rpc.clone();
    let kw_handle = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(18)).await;
            rpc_kw.spawn_keep_warm();
        }
    });

    loop {
        if let Some(max_b) = max_blocks {
            if processed_blocks >= max_b {
                break;
            }
        }
        if live_ready && pnl.cumulative_net_usd >= target_usd {
            println!(
                "GOAL ACHIEVED: cumulative realized net profit ${:.4} >= ${:.2} on {}!",
                pnl.cumulative_net_usd,
                target_usd,
                cfg.as_str()
            );
            kw_handle.abort();
            return Ok(());
        }

        if !dry_run && !live_ready && processed_blocks > 0 && processed_blocks % 8 == 0 {
            if let Some(ref signer) = signer_opt {
                let min_bal = cfg.bidding.min_funding_native;
                let bal_wei = rpc.get_balance(signer.address()).await.unwrap_or(U256::ZERO);
                wallet_bal_wei = bal_wei;
                let bal_f = u256_to_human(bal_wei, 18);
                if bal_f >= min_bal {
                    tracing::info!(
                        wallet = %signer.address(),
                        chain = cfg.as_str(),
                        balance = format!("{bal_f:.6} {}", cfg.native_symbol),
                        "FUNDING DETECTED — LIVE EXECUTION ENABLED"
                    );
                    let addr = ensure_executor_deployed(
                        &rpc,
                        &market,
                        &contract,
                        signer,
                        &cfg,
                        deployed_executor,
                    )
                    .await?;
                    deployed_executor = Some(addr);
                    live_ready = true;
                    cached_nonce = rpc.get_transaction_count(signer.address()).await.ok();
                    if let Ok(b) = rpc.get_balance(signer.address()).await {
                        wallet_bal_wei = b;
                    }
                }
            }
        }

        let poll_ms = cfg.timing.poll_interval(last_block_instant.elapsed());
            let wait_fut = rpc.wait_next_block(last_seen_block, poll_ms);
            tokio::pin!(wait_fut);
            let current_block = loop {
                tokio::select! {
                    biased;
                    res = &mut wait_fut => {
                        match res {
                            Ok(b) => break b,
                            Err(e) => {
                                tracing::warn!(err = %e, "block_number error");
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                break last_seen_block;
                            }
                        }
                    }
                    opt_ptx = recv_opt(&mut pending_rx) => {
                        let Some(ptx) = opt_ptx else {
                            pending_rx = None;
                            continue;
                        };
                        if backrun_busy.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        if let Some((shadow, dirty, swap_usd)) =
                            mempool::evaluate_and_project_victim(
                                &market, &ptx, 35.0, &mut backrun_pool_cooldowns,
                            )
                        {
                            let busy_c = std::sync::Arc::clone(&backrun_busy);
                            busy_c.store(true, std::sync::atomic::Ordering::Release);
                            let rpc_c = rpc.clone();
                            let contract_c = contract.clone();
                            let pool_c = eoa_pool.clone();
                            let btx = backrun_tx.clone();
                            let cfg_min_gp = cfg.min_gas_price_wei();
                            let gp_floor = cfg.gas.effective_floor_wei();
                            let backrun_span = cfg.timing.backrun_span_blocks;
                            let cfg_wnative = cfg.wrapped_native;
                            let bid_profile = bid_chain_profile(&cfg);
                            let can_submit = live_ready
                                && !dry_run
                                && (active_bundles.is_empty()
                                    || eoa_pool.as_ref().is_some_and(|p| p.len() > 1)
                                    || (cfg.submission.mode == SubmissionMode::FlashbotsRelay
                                        && active_bundles
                                            .iter()
                                            .all(|b| b.submitted_block < last_seen_block)));
                            let pre_best_usd = router::solve_all(&market, Some(&dirty), min_profit_usd)
                                .first()
                                .map_or(0.0, |c| c.est_net_profit_usd);
                            let target_blk = last_seen_block.saturating_add(1);
                            tokio::spawn(async move {
                                struct Reset(std::sync::Arc<std::sync::atomic::AtomicBool>);
                                impl Drop for Reset {
                                    fn drop(&mut self) {
                                        self.0.store(false, std::sync::atomic::Ordering::Release);
                                    }
                                }
                                let _reset = Reset(busy_c);
                                let cands = router::solve_all(&shadow, Some(&dirty), min_profit_usd);
                                let chosen = pick_backrun_candidates_with_min(
                                    &cands,
                                    cfg_wnative,
                                    min_profit_usd,
                                );
                                if let Some(&first_idx) = chosen.first() {
                                    let lead = &cands[first_idx];
                                    if lead.est_net_profit_usd
                                        < pre_best_usd + min_profit_usd.max(0.0001)
                                        || lead.est_gross_profit_usd > swap_usd * 0.45
                                    {
                                        tracing::debug!(
                                            victim_tx = %ptx.hash,
                                            swap_usd = format!("${swap_usd:.2}"),
                                            est_gross_usd = format!("${:.4}", lead.est_gross_profit_usd),
                                            est_net_usd = format!("${:.4}", lead.est_net_profit_usd),
                                            pre_best_usd = format!("${:.4}", pre_best_usd),
                                            "MEMPOOL BACKRUN IGNORED (pre-existing/phantom cycle)"
                                        );
                                        return;
                                    }
                                    tracing::info!(
                                        victim_tx = %ptx.hash,
                                        swap_usd = format!("${swap_usd:.2}"),
                                        strategy = %lead.strategy,
                                        est_net_usd = format!("${:.4}", lead.est_net_profit_usd),
                                        slots = chosen.len(),
                                        "MEMPOOL BACKRUN CANDIDATE"
                                    );
                                    if can_submit {
                                        let min_need = U256::from(50_000_000_000_000u128);
                                        if let (Some(ref pool), Some(exec_addr)) = (&pool_c, deployed_executor) {
                                            let max_blk = last_seen_block.saturating_add(backrun_span);
                                            for (slot_ord, &cand_idx) in chosen.iter().enumerate() {
                                                let best = &cands[cand_idx];
                                                let prefer_primary = slot_ord == 0;
                                                let is_wbnb = best.base_token == cfg_wnative;
                                                if let Some((_, signer, nonce, slot_bal_wei)) =
                                                    pool.pick_for_window(
                                                        target_blk,
                                                        max_blk,
                                                        min_need,
                                                        prefer_primary,
                                                    )
                                                {
                                                    // V2 contract uses per-tx `delta[]` accounting and retains realized profit inside
                                                    // `DagArbExecutor` safely (`payoutToOwner = false`) to save owner-transfer gas
                                                    // and fund zero-unwrap native ETH bribes without cross-route balance leakage.
                                                    let payout_owner = false;
                                                    let sim_min = U256::from(1u64);
                                                    let mut submitted = false;
                                                    if let Ok(prep) = prepare_simulation(
                                                        &shadow,
                                                        &contract_c,
                                                        Some(exec_addr),
                                                        caller,
                                                        best,
                                                        sim_min,
                                                        payout_owner,
                                                    ) {
                                                        let base_dec = shadow.token_decimals(best.base_token);
                                                        let base_usd = shadow.token_usd(best.base_token);
                                                        let gross_raw = human_to_u256(
                                                            best.est_gross_profit_human,
                                                            base_dec,
                                                        );
                                                        let est_gas = router::estimate_flows_gas(&best.flows);
                                                        let exec_gp = shadow
                                                            .gas_price_wei
                                                            .max(cfg_min_gp)
                                                            .max(gp_floor);
                                                        let paid_bid = compute_backrun_bid_with_profile(
                                                            is_wbnb,
                                                            est_gas,
                                                            exec_gp,
                                                            gross_raw,
                                                            best.est_gross_profit_usd,
                                                            shadow.native_usd,
                                                            base_usd,
                                                            base_dec,
                                                            min_profit_usd,
                                                            slot_bal_wei,
                                                            bid_profile,
                                                        );
                                                        if paid_bid.wallet_affordable
                                                            && paid_bid.expected_net_usd + 1e-6 >= min_profit_usd
                                                            && !gross_raw.is_zero()
                                                        {
                                                            let sim_proxy = simulator::SimulationResult {
                                                                executor_addr: exec_addr,
                                                                calldata: prep.calldata,
                                                                params: prep.params,
                                                                net_profit_raw: gross_raw,
                                                                net_profit_human: best.est_gross_profit_human,
                                                                gross_profit_usd: best.est_gross_profit_usd,
                                                                gas_used: est_gas,
                                                                l1_fee_wei: 0,
                                                                gas_cost_usd: est_gas as f64
                                                                    * exec_gp as f64
                                                                    / 1e18
                                                                    * shadow.native_usd,
                                                                net_profit_usd: best.est_net_profit_usd,
                                                            };
                                                            let p_spec = BscTxSpec {
                                                                exec_gp: paid_bid.exec_gp,
                                                                min_profit_raw: paid_bid.floor_min_raw,
                                                                bribe_bps: paid_bid.bribe_bps,
                                                                msg_value_wei: paid_bid.msg_value_wei,
                                                            };
                                                            if let Ok((tx_hash, cand_hashes)) =
                                                                executor::submit_backrun_bundle_nonblocking(
                                                                    &rpc_c,
                                                                    &shadow,
                                                                    &signer,
                                                                    exec_addr,
                                                                    &sim_proxy,
                                                                    None,
                                                                    Some(p_spec),
                                                                    nonce,
                                                                    &ptx.raw_tx_hex,
                                                                )
                                                                .await
                                                            {
                                                                submitted = true;
                                                                tracing::info!(
                                                                    victim_tx = %ptx.hash,
                                                                    eoa = %signer.address(),
                                                                    slot = slot_ord,
                                                                    %tx_hash,
                                                                    strategy = %best.strategy,
                                                                    gross_usd = format!("${:.4}", best.est_gross_profit_usd),
                                                                    tip_usd = format!("${:.4}", paid_bid.tip_wei as f64 / 1e18 * shadow.native_usd),
                                                                    expected_net_usd = format!("${:.4}", paid_bid.expected_net_usd),
                                                                    "SUBMITTED MEMPOOL BACKRUN BUNDLE"
                                                                );
                                                                let _ = btx.send(PendingBundle {
                                                                    signer_addr: signer.address(),
                                                                    tx_hash,
                                                                    candidate_hashes: cand_hashes,
                                                                    submitted_block: last_seen_block,
                                                                    max_block: max_blk,
                                                                    nonce,
                                                                    strategy: format!("Backrun({})", best.strategy),
                                                                    base_token: best.base_token,
                                                                    gross_profit_usd: best.est_gross_profit_usd,
                                                                    bid_gas_usd: paid_bid.bid_gas_usd,
                                                                    sim_gas: est_gas,
                                                                    l1_fee_wei: 0,
                                                                    bid_gas_price_wei: Some(paid_bid.exec_gp),
                                                                    tip_wei: paid_bid.tip_wei,
                                                                });
                                                            }
                                                        }
                                                    }
                                                    if !submitted {
                                                        pool.release_block_claim(signer.address(), target_blk);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                        }
                    }
                    opt_ev = recv_opt(&mut mev_share_rx) => {
                        let Some(ev) = opt_ev else {
                            mev_share_rx = None;
                            continue;
                        };
                        if !mev_share_seen_hashes.insert(ev.hash) {
                            continue;
                        }
                        if mev_share_inflight.load(std::sync::atomic::Ordering::Acquire) >= 2 {
                            continue;
                        }
                        mev_share_inflight.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                        let inflight_c = std::sync::Arc::clone(&mev_share_inflight);
                        let block_subs_c = std::sync::Arc::clone(&mev_share_block_subs);
                        let snap = std::sync::Arc::clone(&market_snap);
                        let rpc_c = rpc.clone();
                        let contract_c = contract.clone();
                        let pool_c = eoa_pool.clone();
                        let btx = backrun_tx.clone();
                        let cfg_min_gp = cfg.min_gas_price_wei();
                        let gp_floor = cfg.gas.effective_floor_wei();
                        let cfg_wnative = cfg.wrapped_native;
                        let mev_share_min_swap_usd = cfg.mev_share_min_swap_usd();
                        let mev_share_max_ahead = cfg.mev_share_max_blocks_ahead();
                        let bid_profile = bid_chain_profile(&cfg);
                        let can_submit = live_ready && !dry_run;
                        tokio::spawn(async move {
                            struct DecOnDrop(std::sync::Arc<std::sync::atomic::AtomicUsize>);
                            impl Drop for DecOnDrop {
                                fn drop(&mut self) {
                                    self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                                }
                            }
                            let _dec = DecOnDrop(inflight_c);

                            let target_blk = last_seen_block.saturating_add(1);
                            let max_blk = last_seen_block.saturating_add(mev_share_max_ahead);
                            let Some((shadow, dirty, swap_usd)) =
                                mev_share::evaluate_and_project_mev_share(
                                    &snap,
                                    &ev,
                                    mev_share_min_swap_usd,
                                    target_blk,
                                )
                            else {
                                return;
                            };

                            let cands = router::solve_all(&shadow, Some(&dirty), min_profit_usd);
                            let chosen = pick_backrun_candidates_with_min(
                                &cands,
                                cfg_wnative,
                                min_profit_usd,
                            );
                            let Some(&first_idx) = chosen.first() else {
                                return;
                            };
                            let lead = &cands[first_idx];
                            let pre_best_usd = router::solve_all(&snap, Some(&dirty), min_profit_usd)
                                .first()
                                .map_or(0.0, |c| c.est_net_profit_usd);
                            if lead.est_net_profit_usd < pre_best_usd + min_profit_usd.max(0.0001)
                                || lead.est_gross_profit_usd > swap_usd * 0.45
                            {
                                tracing::debug!(
                                    victim_hash = %ev.hash,
                                    swap_usd = format!("${swap_usd:.2}"),
                                    est_gross_usd = format!("${:.4}", lead.est_gross_profit_usd),
                                    est_net_usd = format!("${:.4}", lead.est_net_profit_usd),
                                    pre_best_usd = format!("${:.4}", pre_best_usd),
                                    "MEV-SHARE BACKRUN IGNORED (pre-existing/phantom cycle)"
                                );
                                return;
                            }

                            tracing::info!(
                                victim_hash = %ev.hash,
                                swap_usd = format!("${swap_usd:.2}"),
                                strategy = %lead.strategy,
                                est_gross_usd = format!("${:.4}", lead.est_gross_profit_usd),
                                est_net_usd = format!("${:.4}", lead.est_net_profit_usd),
                                pre_best_usd = format!("${:.4}", pre_best_usd),
                                "MEV-SHARE BACKRUN CANDIDATE"
                            );

                            if !can_submit
                                || block_subs_c.load(std::sync::atomic::Ordering::Acquire) >= 12
                            {
                                return;
                            }
                            let min_need = U256::from(50_000_000_000_000u128);
                            let (Some(ref pool), Some(exec_addr)) = (&pool_c, deployed_executor) else {
                                return;
                            };
                            let Some((_, signer, nonce, slot_bal_wei)) =
                                pool.peek_for_flashbots_backrun(min_need, true)
                            else {
                                return;
                            };

                            let payout_owner = false;
                            let sim_min = U256::from(1u64);
                            let Ok(prep) = prepare_simulation(
                                &shadow,
                                &contract_c,
                                Some(exec_addr),
                                caller,
                                lead,
                                sim_min,
                                payout_owner,
                            ) else {
                                return;
                            };
                            let base_dec = shadow.token_decimals(lead.base_token);
                            let base_usd = shadow.token_usd(lead.base_token);
                            let gross_raw = human_to_u256(lead.est_gross_profit_human, base_dec);
                            if gross_raw.is_zero() {
                                return;
                            }
                            let est_gas = router::estimate_flows_gas(&lead.flows);
                            let exec_gp = shadow
                                .gas_price_wei
                                .max(cfg_min_gp)
                                .max(gp_floor);
                            let is_wbnb = lead.base_token == cfg_wnative;
                            let paid_bid = compute_mev_share_bid(
                                is_wbnb,
                                est_gas,
                                exec_gp,
                                gross_raw,
                                lead.est_gross_profit_usd,
                                shadow.native_usd,
                                base_usd,
                                base_dec,
                                min_profit_usd,
                                slot_bal_wei,
                                bid_profile,
                            );
                            if !paid_bid.wallet_affordable
                                || paid_bid.expected_net_usd + 1e-6 < min_profit_usd
                            {
                                return;
                            }

                            let sim_proxy = simulator::SimulationResult {
                                executor_addr: exec_addr,
                                calldata: prep.calldata,
                                params: prep.params,
                                net_profit_raw: gross_raw,
                                net_profit_human: lead.est_gross_profit_human,
                                gross_profit_usd: lead.est_gross_profit_usd,
                                gas_used: est_gas,
                                l1_fee_wei: 0,
                                gas_cost_usd: est_gas as f64
                                    * exec_gp as f64
                                    / 1e18
                                    * shadow.native_usd,
                                net_profit_usd: lead.est_net_profit_usd,
                            };
                            let p_spec = BscTxSpec {
                                exec_gp: paid_bid.exec_gp,
                                min_profit_raw: paid_bid.floor_min_raw,
                                bribe_bps: paid_bid.bribe_bps,
                                msg_value_wei: paid_bid.msg_value_wei,
                            };
                            block_subs_c.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                            if let Ok((tx_hash, cand_hashes, bundle_hash)) =
                                executor::submit_mev_share_backrun_nonblocking(
                                    &rpc_c,
                                    &shadow,
                                    &signer,
                                    exec_addr,
                                    &sim_proxy,
                                    p_spec,
                                    nonce,
                                    ev.hash,
                                    target_blk,
                                    max_blk,
                                )
                                .await
                            {
                                tracing::info!(
                                    victim_hash = %ev.hash,
                                    eoa = %signer.address(),
                                    %tx_hash,
                                    bundle_hash = %bundle_hash,
                                    strategy = %lead.strategy,
                                    target_block = target_blk,
                                    max_block = max_blk,
                                    gross_usd = format!("${:.4}", lead.est_gross_profit_usd),
                                    tip_usd = format!("${:.4}", paid_bid.tip_wei as f64 / 1e18 * shadow.native_usd),
                                    expected_net_usd = format!("${:.4}", paid_bid.expected_net_usd),
                                    "SUBMITTED MEV-SHARE BACKRUN BUNDLE"
                                );
                                let _ = btx.send(PendingBundle {
                                    signer_addr: signer.address(),
                                    tx_hash,
                                    candidate_hashes: cand_hashes,
                                    submitted_block: last_seen_block,
                                    max_block: max_blk,
                                    nonce,
                                    strategy: format!("MevShare({})", lead.strategy),
                                    base_token: lead.base_token,
                                    gross_profit_usd: lead.est_gross_profit_usd,
                                    bid_gas_usd: paid_bid.bid_gas_usd,
                                    sim_gas: est_gas,
                                    l1_fee_wei: 0,
                                    bid_gas_price_wei: Some(paid_bid.exec_gp),
                                    tip_wei: paid_bid.tip_wei,
                                });
                            }
                        });
                    }
                }
            };

            if current_block <= last_seen_block {
                continue;
            }
            while let Ok(pb) = backrun_rx.try_recv() {
                if !active_bundles.iter().any(|b| b.tx_hash == pb.tx_hash) {
                    active_bundles.push(pb);
                }
            }
            while let Ok((pb, mined_hash, rx)) = late_win_rx.try_recv() {
                finalize_live_receipt(
                    &market,
                    &mut pnl,
                    &pnl_path,
                    mined_hash,
                    current_block,
                    &pb.strategy,
                    pb.base_token,
                    pb.gross_profit_usd,
                    pb.bid_gas_usd,
                    pb.sim_gas,
                    pb.l1_fee_wei,
                    pb.bid_gas_price_wei,
                    pb.tip_wei,
                    Some(rx),
                );
                if let Some(ref pool) = eoa_pool {
                    pool.advance_nonce_and_clear_busy(pb.signer_addr, pb.nonce + 1);
                    wallet_bal_wei = pool.max_balance_wei();
                }
                if signer_opt.as_ref().is_some_and(|s| s.address() == pb.signer_addr) {
                    cached_nonce = Some(pb.nonce + 1);
                }
            }

            // Always process the freshest tip block directly to avoid chasing stale opportunities
            let bn = current_block;
            let large_block_gap = processed_blocks > 0 && bn > last_seen_block.saturating_add(3);
            let t_blk = Instant::now();
            backrun_pool_cooldowns.retain(|_, until| *until > t_blk);
            let receipts = match rpc.get_block_receipts_by_number(bn).await {
                Ok(r) => {
                    receipt_retry = (0, 0);
                    last_block_instant = t_blk;
                    r
                }
                Err(e) => {
                    let tries = if receipt_retry.0 == bn {
                        receipt_retry.1.saturating_add(1)
                    } else {
                        1
                    };
                    receipt_retry = (bn, tries);
                    if tries <= 4 {
                        tokio::time::sleep(Duration::from_millis(6)).await;
                        continue;
                    }
                    tracing::warn!(block = bn, tries, err = %e, "get_block_receipts failed after retries");
                    last_seen_block = bn;
                    continue;
                }
            };

            if cfg.gas.tx_type == TxType::Eip1559 {
                if let Some(bf) = rpc.latest_ws_next_base_fee(bn) {
                    market.gas_price_wei = bf.max(cfg.min_gas_price_wei());
                }
            }

            // Non-blocking bundle receipt reconciliation on BSC (supports multi-EOA concurrent bundles);
            // defer slow per-tx RPC checks and pool refreshes until after submission so solve_all starts immediately.
            let mut deferred_expired_bundles: Vec<PendingBundle> = Vec::new();
            let mut deferred_eoa_refreshes: Vec<(Address, Option<u64>)> = Vec::new();
            if !active_bundles.is_empty() {
                let mut remaining = Vec::with_capacity(active_bundles.len());
                let mut landed_signer_nonces: HashSet<(Address, u64)> = HashSet::new();
                for pb in active_bundles.drain(..) {
                    let mut found = None;
                    for &h in &pb.candidate_hashes {
                        if let Some(rx) = receipts
                            .iter()
                            .find(|r| r.transaction_hash == Some(h))
                            .cloned()
                        {
                            found = Some((h, rx));
                            break;
                        }
                    }
                    let keep_active = if cfg.submission.mode == SubmissionMode::FlashbotsRelay {
                        bn < pb.max_block
                    } else {
                        bn <= pb.max_block
                    };
                    if let Some((mined_hash, rx)) = found {
                        landed_signer_nonces.insert((pb.signer_addr, pb.nonce));
                        finalize_live_receipt(
                            &market,
                            &mut pnl,
                            &pnl_path,
                            mined_hash,
                            bn,
                            &pb.strategy,
                            pb.base_token,
                            pb.gross_profit_usd,
                            pb.bid_gas_usd,
                            pb.sim_gas,
                            pb.l1_fee_wei,
                            pb.bid_gas_price_wei,
                            pb.tip_wei,
                            Some(rx),
                        );
                        if let Some(ref pool) = eoa_pool {
                            pool.advance_nonce_and_clear_busy(pb.signer_addr, pb.nonce + 1);
                            deferred_eoa_refreshes.push((pb.signer_addr, Some(pb.nonce + 1)));
                        }
                        if signer_opt.as_ref().is_some_and(|s| s.address() == pb.signer_addr) {
                            cached_nonce =
                                Some(cached_nonce.unwrap_or(0).max(pb.nonce + 1));
                        }
                    } else if keep_active {
                        remaining.push(pb);
                    } else {
                        if let Some(ref pool) = eoa_pool {
                            pool.clear_busy(pb.signer_addr);
                        }
                        if signer_opt.as_ref().is_some_and(|s| s.address() == pb.signer_addr)
                            && !landed_signer_nonces.contains(&(pb.signer_addr, pb.nonce))
                            && cached_nonce.map_or(true, |c| c <= pb.nonce)
                        {
                            cached_nonce = Some(pb.nonce);
                        }
                        deferred_expired_bundles.push(pb);
                    }
                }
                if !landed_signer_nonces.is_empty() {
                    remaining.retain(|pb| !landed_signer_nonces.contains(&(pb.signer_addr, pb.nonce)));
                    deferred_expired_bundles
                        .retain(|pb| !landed_signer_nonces.contains(&(pb.signer_addr, pb.nonce)));
                }
                active_bundles = remaining;
            }

            let mut delta = market.apply_receipts_in_place(bn, &receipts);
            market
                .refresh_metric_pools_for_delta(&rpc, bn, &mut delta)
                .await;
            delta.dirty_pools.extend(carryover_dirty_pools.drain());

            let cur_dirty_vec: Vec<Address> = delta.dirty_pools.iter().copied().collect();
            recent_dirty_ring.push_back((bn, cur_dirty_vec));
            while recent_dirty_ring.len() > 8
                || recent_dirty_ring
                    .front()
                    .is_some_and(|(old_bn, _)| old_bn.saturating_add(12) < bn)
            {
                recent_dirty_ring.pop_front();
            }
            let mut rolling_dirty: HashSet<Address> = delta.dirty_pools.clone();
            for (_, addrs) in &recent_dirty_ring {
                for &pa in addrs {
                    if market
                        .pools
                        .get(&pa)
                        .is_some_and(|p| market.is_pool_routable(p))
                    {
                        rolling_dirty.insert(pa);
                    }
                }
            }

            let t_solve = Instant::now();
            let dirty_pools_opt = if processed_blocks == 0 {
                None
            } else {
                Some(&rolling_dirty)
            };
            let solve_min_usd = min_profit_usd;
            let candidates = router::solve_all(&market, dirty_pools_opt, solve_min_usd);
            let solve_us = t_solve.elapsed().as_micros();
            total_candidates += candidates.len() as u64;

            // Profit-first selection up to 5 slots (across all hop counts), then reserve remaining up to 8 for multi-hop (>= 4 flows)
            let mut non_hub_counts: HashMap<Address, u8> = HashMap::new();
            let mut generic_counts: HashMap<Address, u8> = HashMap::new();
            let mut seen_route_sigs: HashSet<Vec<Address>> = HashSet::new();
            let mut picked_indices: HashSet<usize> = HashSet::new();
            let mut top_cands = Vec::with_capacity(8);
            for pass in 0..2 {
                let multi_only = pass == 1;
                for (ci, c) in candidates.iter().enumerate() {
                    if top_cands.len() >= 8 {
                        break;
                    }
                    if !multi_only && top_cands.len() >= 5 {
                        break;
                    }
                    if picked_indices.contains(&ci) {
                        continue;
                    }
                    let is_multi = c.flows.len() >= 4;
                    if multi_only && !is_multi {
                        continue;
                    }
                    let mut sig: Vec<Address> = c.flows.iter().map(|f| f.pool).collect();
                    sig.sort();
                    sig.dedup();
                    if seen_route_sigs.contains(&sig) {
                        continue;
                    }
                    if let Some(&(failed_bn, cooldown_blks)) = failed_route_cooldown.get(&sig) {
                        let mut has_non_hub_pool = false;
                        let mut non_hub_upd = 0u64;
                        let mut all_upd = 0u64;
                        for a in &sig {
                            if let Some(p) = market.pools.get(a) {
                                all_upd = all_upd.max(p.last_updated_block);
                                if !market.is_hub_token(p.token0) || !market.is_hub_token(p.token1) {
                                    has_non_hub_pool = true;
                                    non_hub_upd = non_hub_upd.max(p.last_updated_block);
                                }
                            }
                        }
                        let eff_upd = if has_non_hub_pool { non_hub_upd } else { all_upd };
                        if eff_upd <= failed_bn && bn < failed_bn + cooldown_blks {
                            continue;
                        }
                    }
                    let gen_pools: Vec<Address> = sig
                        .iter()
                        .copied()
                        .filter(|a| {
                            market
                                .pools
                                .get(a)
                                .is_some_and(|p| p.factory_name.starts_with("Generic"))
                        })
                        .collect();
                    if gen_pools
                        .iter()
                        .any(|g| generic_counts.get(g).copied().unwrap_or(0) >= 2)
                    {
                        continue;
                    }
                    let mut non_hub_toks: Vec<Address> = Vec::new();
                    for f in &c.flows {
                        if !market.is_hub_token(f.token_in) && !non_hub_toks.contains(&f.token_in) {
                            non_hub_toks.push(f.token_in);
                        }
                        if !market.is_hub_token(f.token_out) && !non_hub_toks.contains(&f.token_out) {
                            non_hub_toks.push(f.token_out);
                        }
                    }
                    let per_tok_limit = if multi_only { 3u8 } else { 2u8 };
                    if non_hub_toks
                        .iter()
                        .any(|tk| non_hub_counts.get(tk).copied().unwrap_or(0) >= per_tok_limit)
                    {
                        continue;
                    }
                    picked_indices.insert(ci);
                    seen_route_sigs.insert(sig);
                    for g in gen_pools {
                        *generic_counts.entry(g).or_insert(0) += 1;
                    }
                    for tk in non_hub_toks {
                        *non_hub_counts.entry(tk).or_insert(0) += 1;
                    }
                    top_cands.push(c.clone());
                }
            }

            if let Some(best_c) = top_cands.first() {
                let _ = std::fs::write("logs/latest_dag.mmd", &best_c.mermaid);
            }

            struct SimTaskOutcome {
                idx: usize,
                is_probe: bool,
                used_pools: Vec<Address>,
                prep_full: Option<Box<PreparedSimulation>>,
                result: std::result::Result<(simulator::SimulationResult, f64), String>,
            }

            let sim_timeout_ms = cfg.timing.sim_timeout_ms;
            let sim_fallback_budget_ms = cfg.timing.sim_fallback_budget_ms;
            let sim_futs: FuturesUnordered<tokio::task::JoinHandle<SimTaskOutcome>> =
                FuturesUnordered::new();
            let sim_min_profit_raw = U256::from(1u64);
            for (idx, cand) in top_cands.iter().enumerate() {
                // V2 retains profits in `DagArbExecutor` safely (`payoutToOwner = false`) with isolated
                // per-tx `delta[]` accounting so later routes cannot leak prior token balances.
                let cand_payout_to_owner = false;
                if let Ok(prep_full) = prepare_simulation(
                    &market,
                    &contract,
                    deployed_executor,
                    caller,
                    cand,
                    sim_min_profit_raw,
                    cand_payout_to_owner,
                ) {
                    let (prep_half, prep_quarter) = if cand.est_net_profit_usd >= 0.03 {
                        let build_scaled = |factor: f64| {
                            let s_cand = scale_candidate(cand, factor);
                            let s_borrow = s_cand.borrow_human;
                            prepare_simulation(
                                &market,
                                &contract,
                                deployed_executor,
                                caller,
                                &s_cand,
                                sim_min_profit_raw,
                                cand_payout_to_owner,
                            )
                            .ok()
                            .map(|p| (p, s_borrow))
                        };
                        (build_scaled(0.50), build_scaled(0.22))
                    } else {
                        (None, None)
                    };
                    let used_pools = prep_full.used_pools.clone();
                    let full_borrow = cand.borrow_human;
                    let rpc_c = rpc.clone();

                    if idx < 2 && cand.est_net_profit_usd >= 0.04 {
                        let base_borrow_usd =
                            cand.borrow_human * market.token_usd(cand.base_token);
                        let high_margin_v3 = idx == 0
                            && (cand.est_net_profit_usd >= 0.25
                                || cand.est_gross_profit_usd / base_borrow_usd.max(1.0) >= 0.006)
                            && used_pools.iter().any(|p| {
                                market.pools.get(p).is_some_and(|ps| {
                                    matches!(
                                        ps.dex_kind,
                                        config::DexKind::UniV3
                                            | config::DexKind::PancakeV3
                                            | config::DexKind::InfinityCl
                                            | config::DexKind::UniV4
                                            | config::DexKind::MetricOmm
                                    )
                                })
                            });
                        let probe_factors: &[f64] = if high_margin_v3 {
                            &[0.68, 1.45, 2.05, 4.5, 11.0, 24.0]
                        } else {
                            &[0.68, 1.45, 2.05]
                        };
                        let base_dec = market.token_decimals(cand.base_token);
                        let uses_inline_v3_flash = cand.flows.len() >= 2
                            && market.pools.get(&cand.flows[0].pool).is_some_and(|ps| {
                                (matches!(
                                    ps.dex_kind,
                                    config::DexKind::UniV3
                                        | config::DexKind::PancakeV3
                                        | config::DexKind::UniV2
                                ) && market.chain.factory_supports_flash(&ps.factory_name)
                                    || ps.dex_kind == config::DexKind::MetricOmm)
                                    && (ps.dex_kind != config::DexKind::UniV2
                                        || cand.flows.get(1).is_none_or(|next| {
                                            !matches!(
                                                next.dex_kind,
                                                config::DexKind::UniV2 | config::DexKind::Solidly
                                            )
                                        }))
                            })
                            && cand.flows[0].token_in == cand.base_token
                            && !cand.flows[1..].iter().any(|f| f.pool == cand.flows[0].pool);

                        for &probe_factor in probe_factors {
                            let mut p_cand = scale_candidate(cand, probe_factor);
                            if probe_factor > 2.5 && !uses_inline_v3_flash {
                                let p_raw = amm::human_to_u256(p_cand.borrow_human, base_dec);
                                if let Some(flash) =
                                    market.select_flash_source(p_cand.base_token, p_raw, &used_pools)
                                {
                                    p_cand.flash_mode = flash.flash_mode;
                                    p_cand.flash_source = flash.pool;
                                    p_cand.flash_fee_bps = flash.fee_bps;
                                } else {
                                    continue;
                                }
                            }
                            let p_borrow = p_cand.borrow_human;
                            if let Ok(prep_probe) = prepare_simulation(
                                &market,
                                &contract,
                                deployed_executor,
                                caller,
                                &p_cand,
                                sim_min_profit_raw,
                                cand_payout_to_owner,
                            ) {
                                let rpc_p = rpc.clone();
                                let probe_pools = used_pools.clone();
                                sim_futs.push(tokio::spawn(async move {
                                    let res = match tokio::time::timeout(
                                        Duration::from_millis(sim_timeout_ms),
                                        async move {
                                            execute_simulation(&rpc_p, &prep_probe)
                                                .await
                                                .map(|sim| (sim, p_borrow))
                                                .map_err(|e| e.to_string())
                                        },
                                    )
                                    .await
                                    {
                                        Ok(r) => r,
                                        Err(_) => Err("sim_timeout".to_string()),
                                    };
                                    SimTaskOutcome {
                                        idx,
                                        is_probe: true,
                                        used_pools: probe_pools,
                                        prep_full: None,
                                        result: res,
                                    }
                                }));
                            }
                        }
                    }

                    sim_futs.push(tokio::spawn(async move {
                        let out = tokio::time::timeout(Duration::from_millis(sim_timeout_ms), async move {
                            let t_sim = Instant::now();
                            match execute_simulation(&rpc_c, &prep_full).await {
                                Ok(sim) => {
                                    if sim.net_profit_usd <= 0.0
                                        && t_sim.elapsed() < Duration::from_millis(sim_fallback_budget_ms)
                                    {
                                        if let Some((ph, h_borrow)) = &prep_half {
                                            if let Ok(sim_h) = execute_simulation(&rpc_c, ph).await {
                                                if sim_h.net_profit_usd > 0.0 {
                                                    return (None, Ok((sim_h, *h_borrow)));
                                                }
                                            }
                                        }
                                        if let Some((pq, q_borrow)) = &prep_quarter {
                                            if let Ok(sim_q) = execute_simulation(&rpc_c, pq).await {
                                                if sim_q.net_profit_usd > 0.0 {
                                                    return (None, Ok((sim_q, *q_borrow)));
                                                }
                                            }
                                        }
                                    }
                                    (None, Ok((sim, full_borrow)))
                                }
                                Err(e) => {
                                    let es = e.to_string();
                                    let es_lc = es.to_ascii_lowercase();
                                    let allow_fallback = (es.starts_with("[EVM_REVERT]")
                                        || es_lc.contains("execution reverted"))
                                        && !es_lc.contains("out of gas")
                                        && t_sim.elapsed() < Duration::from_millis(sim_fallback_budget_ms);
                                    if allow_fallback {
                                        if let Some((ph, h_borrow)) = &prep_half {
                                            if let Ok(sim_h) = execute_simulation(&rpc_c, ph).await {
                                                return (None, Ok((sim_h, *h_borrow)));
                                            }
                                        }
                                        if let Some((pq, q_borrow)) = &prep_quarter {
                                            if let Ok(sim_q) = execute_simulation(&rpc_c, pq).await {
                                                return (None, Ok((sim_q, *q_borrow)));
                                            }
                                        }
                                    }
                                    (Some(Box::new(prep_full)), Err(es))
                                }
                            }
                        })
                        .await;
                        match out {
                            Ok((prep_opt, res)) => SimTaskOutcome {
                                idx,
                                is_probe: false,
                                used_pools,
                                prep_full: prep_opt,
                                result: res,
                            },
                            Err(_) => SimTaskOutcome {
                                idx,
                                is_probe: false,
                                used_pools,
                                prep_full: None,
                                result: Err("sim_timeout".to_string()),
                            },
                        }
                    }));
                }
            }

            let mut passing_sims = Vec::new();
            let mut deferred_failures: Vec<(
                usize,
                Vec<Address>,
                Vec<Address>,
                String,
                String,
                Option<Box<PreparedSimulation>>,
            )> = Vec::new();
            let mut straggler_deadline: Option<tokio::time::Instant> = None;
            let mut sim_futs = sim_futs;
            loop {
                let next_item = if let Some(dl) = straggler_deadline {
                    match tokio::time::timeout_at(dl, sim_futs.next()).await {
                        Ok(Some(item)) => Some(item),
                        Ok(None) | Err(_) => break,
                    }
                } else {
                    sim_futs.next().await
                };
                let Some(joined) = next_item else {
                    break;
                };
                let outcome = match joined {
                    Ok(o) => o,
                    Err(e) => {
                        tracing::debug!(block = bn, err = %e, "sim task join err");
                        continue;
                    }
                };
                let idx = outcome.idx;
                let is_probe = outcome.is_probe;
                let used_pools = outcome.used_pools;
                let cand = &top_cands[idx];
                let route_desc = cand
                    .flows
                    .iter()
                    .map(|f| f.label.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ");
                let mut unique_non_hub = Vec::new();
                for f in &cand.flows {
                    if !market.is_hub_token(f.token_in) && !unique_non_hub.contains(&f.token_in) {
                        unique_non_hub.push(f.token_in);
                    }
                    if !market.is_hub_token(f.token_out) && !unique_non_hub.contains(&f.token_out) {
                        unique_non_hub.push(f.token_out);
                    }
                }
                match outcome.result {
                    Ok((sim, eff_borrow)) => {
                        if sim.net_profit_usd <= 0.0 {
                            if !is_probe {
                                let mut sig = used_pools.clone();
                                sig.sort();
                                sig.dedup();
                                failed_route_cooldown.insert(sig, (bn, 6));
                            }
                            continue;
                        }
                        if !is_probe {
                            total_verified_opps += 1;
                            market.record_sim_success(&used_pools, &unique_non_hub);
                        }
                        if live_ready
                            && !dry_run
                            && straggler_deadline.is_none()
                            && sim.net_profit_usd >= min_profit_usd
                        {
                            straggler_deadline =
                                Some(tokio::time::Instant::now() + Duration::from_millis(16));
                        }
                        tracing::info!(
                            block = bn,
                            strategy = %cand.strategy,
                            probe = is_probe,
                            base = %market.token_symbol(cand.base_token),
                            borrow = format!("{:.2}", eff_borrow),
                            sim_net_usd = format!("${:.4}", sim.net_profit_usd),
                            gas_used = sim.gas_used,
                            route = %route_desc,
                            "ETH_CALL VERIFIED ARB"
                        );
                        append_opp_log(OppLog {
                            timestamp: Utc::now().to_rfc3339(),
                            chain: cfg.as_str().to_string(),
                            block: bn,
                            strategy: cand.strategy.clone(),
                            base_token: market.token_symbol(cand.base_token),
                            borrow_human: eff_borrow,
                            est_net_usd: cand.est_net_profit_usd,
                            sim_ok: true,
                            sim_net_usd: Some(sim.net_profit_usd),
                            sim_gas: Some(sim.gas_used),
                            sim_err: None,
                            sim_err_cat: None,
                            edges: cand.flows.len(),
                            route: route_desc,
                        });
                        passing_sims.push((idx, sim));
                    }
                    Err(err_str) => {
                        if is_probe {
                            continue;
                        }
                        deferred_failures.push((
                            idx,
                            used_pools,
                            unique_non_hub,
                            route_desc,
                            err_str,
                            outcome.prep_full,
                        ));
                    }
                }
            }
            for h in sim_futs {
                h.abort();
            }

            // Filter out hyper-contested low-margin 2-pool hub-to-hub races, rank uncontested routes higher,
            // compute dynamic BSC priority gas bid, and broadcast once per block with cached nonce.
            let mut ranked_sims: Vec<(usize, simulator::SimulationResult)> = passing_sims
                .into_iter()
                .filter(|(idx, sim)| {
                    if cfg.chain != "bsc" {
                        return sim.net_profit_usd >= min_profit_usd;
                    }
                    let cand = &top_cands[*idx];
                    let is_pure_2pool_hub = cand.flows.len() == 2
                        && cand.flows.iter().all(|f| {
                            market.is_hub_token(f.token_in) && market.is_hub_token(f.token_out)
                        });
                    let both_primary_dex = cand.flows.iter().all(|f| {
                        market.pools.get(&f.pool).is_some_and(|p| {
                            matches!(
                                p.factory_name.as_str(),
                                "PancakeV3" | "UniV3Bsc" | "PancakeV2" | "ThenaV2"
                            )
                        })
                    });
                    !(is_pure_2pool_hub && both_primary_dex) || sim.net_profit_usd >= 0.015
                })
                .collect();
            ranked_sims.sort_by(|a, b| {
                let bucket_a = (a.1.net_profit_usd * 500.0).round() as i64;
                let bucket_b = (b.1.net_profit_usd * 500.0).round() as i64;
                bucket_b
                    .cmp(&bucket_a)
                    .then_with(|| top_cands[a.0].flows.len().cmp(&top_cands[b.0].flows.len()))
                    .then_with(|| {
                        b.1.net_profit_usd
                            .partial_cmp(&a.1.net_profit_usd)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    })
            });

            let mut submitted_this_block = false;
            let max_loss_usd = -cfg.bidding.circuit_breaker_loss_usd;
            let gp_floor = cfg.gas.effective_floor_wei();
            let circuit_tripped =
                (pnl.reverted_trades >= 6 && pnl.winning_trades == 0) || pnl.cumulative_net_usd <= max_loss_usd;
            if circuit_tripped {
                tracing::error!(
                    reverts = pnl.reverted_trades,
                    cum_net_usd = format!("${:.4}", pnl.cumulative_net_usd),
                    "CIRCUIT BREAKER TRIPPED — halting live broadcasts until audited"
                );
            } else if live_ready
                && cfg.is_mev_bundle_enabled()
                && eoa_pool.as_ref().is_some_and(|p| p.len() > 1)
                && active_bundles.len() < 16
            {
                if let (Some(ref pool), Some(exec_addr)) = (&eoa_pool, deployed_executor) {
                    let bid_profile = bid_chain_profile(&cfg);
                    let base_exec_gp = market
                        .gas_price_wei
                        .max(cfg.min_gas_price_wei())
                        .max(gp_floor);
                    let max_bal = pool.max_balance_wei();
                    let mut scored: Vec<(usize, simulator::SimulationResult, u128, u128, bool, usize)> =
                        Vec::new();
                    for (idx, sim) in ranked_sims {
                        let c = &top_cands[idx];
                        let is_wbnb = c.base_token == cfg.wrapped_native;
                        let is_dual_hub = c.flows.iter().all(|f| {
                            market.is_hub_token(f.token_in) && market.is_hub_token(f.token_out)
                        });
                        let (eff_flow_len, _) =
                            effective_bid_flow_len_and_passive(&market, c, is_dual_hub, bn);
                        let b_usd = market.token_usd(c.base_token);
                        let b_dec = market.token_decimals(c.base_token);
                        let probe_bid = compute_bid_with_profile(
                            false,
                            is_wbnb,
                            is_dual_hub,
                            eff_flow_len,
                            sim.gas_used,
                            base_exec_gp,
                            sim.net_profit_raw,
                            sim.gross_profit_usd,
                            market.native_usd,
                            b_usd,
                            b_dec,
                            min_profit_usd,
                            max_bal,
                            bid_profile,
                        );
                        if probe_bid.wallet_affordable
                            && probe_bid.expected_net_usd + 1e-6 >= min_profit_usd
                        {
                            let expected_net_wei = ((probe_bid.expected_net_usd.max(0.0)
                                / market.native_usd.max(1.0))
                                * 1e18) as u128;
                            let wbnb_bonus = if is_wbnb {
                                (probe_bid.effective_builder_wei / 25).max(3_000_000_000_000)
                            } else {
                                0
                            };
                            let compact_bonus = if c.flows.len() <= 4 {
                                probe_bid.effective_builder_wei / 50
                            } else {
                                0
                            };
                            let score = probe_bid
                                .effective_builder_wei
                                .saturating_add(expected_net_wei / 4)
                                .saturating_add(wbnb_bonus)
                                .saturating_add(compact_bonus);
                            scored.push((
                                idx,
                                sim,
                                score,
                                probe_bid.msg_value_wei,
                                is_wbnb,
                                c.flows.len(),
                            ));
                        }
                    }
                    scored.sort_by(|a, b| b.2.cmp(&a.2));
                    if scored.len() > 2 && !scored[0].4 && !scored[1].4 {
                        if let Some(pos) = scored.iter().skip(2).position(|x| x.4).map(|p| p + 2) {
                            let item = scored.remove(pos);
                            scored.insert(2, item);
                        }
                    }
                    let has_compact_top3 = scored
                        .iter()
                        .take(3)
                        .any(|x| x.5 <= 4 && x.1.gross_profit_usd >= 0.03);
                    if scored.len() > 2
                        && scored[0].5 >= 8
                        && scored[1].5 >= 8
                        && !has_compact_top3
                    {
                        let insert_at = if !scored[0].4 && !scored[1].4 && scored[2].4 {
                            3
                        } else {
                            2
                        };
                        if insert_at < scored.len() {
                            if let Some(pos) = scored
                                .iter()
                                .skip(insert_at)
                                .position(|x| x.5 <= 4 && x.1.gross_profit_usd >= 0.03)
                                .map(|p| p + insert_at)
                            {
                                let item = scored.remove(pos);
                                scored.insert(insert_at, item);
                            }
                        }
                    }

                    struct PreparedDispatch {
                        strategy: String,
                        base_token: Address,
                        eoa_signer: PrivateKeySigner,
                        nonce: u64,
                        max_blk: u64,
                        paid_bid: BscBidOutcome,
                        p_spec: BscTxSpec,
                        sim: simulator::SimulationResult,
                    }

                    let mut seen_pool_sets_this_block: HashSet<Vec<Address>> = HashSet::new();
                    let mut prepared: Vec<PreparedDispatch> = Vec::with_capacity(4);
                    for (idx, sim, _, est_msg_val_wei, _, _) in scored {
                        if prepared.len() >= 4 || active_bundles.len() + prepared.len() >= 16 {
                            break;
                        }
                        let cand = &top_cands[idx];
                        let mut pool_set: Vec<Address> =
                            cand.flows.iter().map(|f| f.pool).collect();
                        pool_set.sort();
                        pool_set.dedup();
                        if seen_pool_sets_this_block.contains(&pool_set) {
                            continue;
                        }
                        let is_wbnb = cand.base_token == cfg.wrapped_native;
                        let is_dual_hub = cand.flows.iter().all(|f| {
                            market.is_hub_token(f.token_in) && market.is_hub_token(f.token_out)
                        });
                        let (eff_flow_len, is_passive) =
                            effective_bid_flow_len_and_passive(&market, cand, is_dual_hub, bn);
                        let b_usd = market.token_usd(cand.base_token);
                        let b_dec = market.token_decimals(cand.base_token);
                        let reserve_gp = if cfg.gas.tx_type == TxType::Eip1559 {
                            ((base_exec_gp * cfg.gas.max_fee_multiplier_bps) / 10_000)
                                .saturating_add(cfg.gas.bundle_priority_fee_wei)
                        } else {
                            base_exec_gp
                        };
                        let arb_gas_lim =
                            (((sim.gas_used as f64) * 1.35).ceil() as u128 + 35_000).max(280_000);
                        let min_reserve = U256::from(arb_gas_lim * reserve_gp);
                        let min_need = min_reserve + U256::from(est_msg_val_wei);
                        let prefer_primary =
                            prepared.is_empty() || min_need > U256::from(2_100_000_000_000_000u128);
                        let span_blocks = if is_passive {
                            cfg.timing.passive_bundle_span_blocks
                        } else {
                            cfg.timing.bundle_span_blocks
                        };
                        let max_blk = bn.saturating_add(span_blocks);
                        let Some((_, eoa_signer, nonce, slot_bal)) =
                            pool.pick_for_window(bn, max_blk, min_need, prefer_primary)
                        else {
                            continue;
                        };
                        let max_prev_gp = active_bundles
                            .iter()
                            .filter(|b| b.signer_addr == eoa_signer.address() && b.nonce == nonce)
                            .filter(|b| {
                                !b.strategy.starts_with("Backrun(")
                                    && !b.strategy.starts_with("MevShare(")
                            })
                            .filter_map(|b| b.bid_gas_price_wei)
                            .max()
                            .unwrap_or(0);
                        let exec_gp = if cfg.gas.tx_type == TxType::Legacy && max_prev_gp > 0 {
                            base_exec_gp
                                .max((max_prev_gp * 112) / 100)
                                .max(max_prev_gp + 5_000_000)
                        } else {
                            base_exec_gp
                        };
                        let paid_bid = compute_bid_with_profile(
                            false,
                            is_wbnb,
                            is_dual_hub,
                            eff_flow_len,
                            sim.gas_used,
                            exec_gp,
                            sim.net_profit_raw,
                            sim.gross_profit_usd,
                            market.native_usd,
                            b_usd,
                            b_dec,
                            min_profit_usd,
                            slot_bal,
                            bid_profile,
                        );
                        if !paid_bid.wallet_affordable
                            || paid_bid.expected_net_usd + 1e-6 < min_profit_usd
                        {
                            pool.release_block_claim(eoa_signer.address(), bn);
                            continue;
                        }
                        let p_spec = BscTxSpec {
                            exec_gp: paid_bid.exec_gp,
                            min_profit_raw: paid_bid.floor_min_raw,
                            bribe_bps: paid_bid.bribe_bps,
                            msg_value_wei: paid_bid.msg_value_wei,
                        };
                        seen_pool_sets_this_block.insert(pool_set);
                        prepared.push(PreparedDispatch {
                            strategy: cand.strategy.clone(),
                            base_token: cand.base_token,
                            eoa_signer,
                            nonce,
                            max_blk,
                            paid_bid,
                            p_spec,
                            sim,
                        });
                    }

                    if !prepared.is_empty() {
                        let broadcast_futs = prepared.iter().map(|item| {
                            submit_arb_bundle_nonblocking(
                                &rpc,
                                &market,
                                &item.eoa_signer,
                                exec_addr,
                                &item.sim,
                                None,
                                Some(item.p_spec),
                                item.nonce,
                                item.max_blk,
                            )
                        });
                        let results = join_all(broadcast_futs).await;
                        for (item, res) in prepared.into_iter().zip(results) {
                            match res {
                                Ok((tx_hash, candidate_hashes)) => {
                                    submitted_this_block = true;
                                    let tip_bnb = item.paid_bid.tip_wei as f64 / 1e18;
                                    let tip_usd = tip_bnb * market.native_usd;
                                    tracing::info!(
                                        block = bn,
                                        eoa = %item.eoa_signer.address(),
                                        %tx_hash,
                                        strategy = %item.strategy,
                                        bribe_bps = item.paid_bid.bribe_bps,
                                        gross_usd = format!("${:.4}", item.sim.gross_profit_usd),
                                        tip_bnb = format!("{:.6}", tip_bnb),
                                        tip_usd = format!("${:.4}", tip_usd),
                                        expected_net_usd = format!("${:.4}", item.paid_bid.expected_net_usd),
                                        pre_submit_ms = t_blk.elapsed().as_millis(),
                                        "SUBMITTED MEV BUNDLE"
                                    );
                                    active_bundles.push(PendingBundle {
                                        signer_addr: item.eoa_signer.address(),
                                        tx_hash,
                                        candidate_hashes,
                                        submitted_block: bn,
                                        max_block: item.max_blk,
                                        nonce: item.nonce,
                                        strategy: item.strategy,
                                        base_token: item.base_token,
                                        gross_profit_usd: item.sim.gross_profit_usd,
                                        bid_gas_usd: item.paid_bid.bid_gas_usd,
                                        sim_gas: item.sim.gas_used,
                                        l1_fee_wei: item.sim.l1_fee_wei,
                                        bid_gas_price_wei: Some(item.paid_bid.exec_gp),
                                        tip_wei: item.paid_bid.tip_wei,
                                    });
                                }
                                Err(e) => {
                                    pool.release_block_claim(item.eoa_signer.address(), bn);
                                    let es = e.to_string();
                                    if es.contains("all MEV builders rejected bundle") {
                                        tracing::info!(block = bn, eoa = %item.eoa_signer.address(), err = %es, "MEV bundle pre-rejected ($0 gas)");
                                    } else {
                                        pool.refresh_single(&rpc, item.eoa_signer.address(), None).await;
                                        tracing::warn!(eoa = %item.eoa_signer.address(), err = %es, "broadcast error");
                                    }
                                }
                            }
                        }
                    }
                }
            } else if live_ready
                && (active_bundles.is_empty()
                    || active_bundles.len() < 3
                    || (cfg.submission.mode == SubmissionMode::FlashbotsRelay
                        && active_bundles.len() < 6))
            {
                let selected_exec = match cfg.bidding.mode {
                    BiddingMode::CoinbaseBribe => {
                        let mut exec_gp = market
                            .gas_price_wei
                            .max(cfg.min_gas_price_wei())
                            .max(gp_floor);
                        if cfg.gas.tx_type == TxType::Legacy && !active_bundles.is_empty() {
                            let max_prev_gp = active_bundles
                                .iter()
                                .filter(|b| !b.strategy.starts_with("Backrun("))
                                .filter_map(|b| b.bid_gas_price_wei)
                                .max()
                                .unwrap_or(0);
                            if max_prev_gp > 0 {
                                exec_gp = exec_gp
                                    .max((max_prev_gp * 112) / 100)
                                    .max(max_prev_gp + 5_000_000);
                            }
                        }
                        let bid_profile = bid_chain_profile(&cfg);
                        let mut best_choice: Option<(
                            usize,
                            simulator::SimulationResult,
                            BscBidOutcome,
                            u128,
                        )> = None;
                        let mut any_below_reserve: Option<u128> = None;
                        for (idx, sim) in ranked_sims {
                            let c = &top_cands[idx];
                            let is_wbnb = c.base_token == cfg.wrapped_native;
                            let is_dual_hub = c.flows.iter().all(|f| {
                                market.is_hub_token(f.token_in) && market.is_hub_token(f.token_out)
                            });
                            let (eff_flow_len, _) =
                                effective_bid_flow_len_and_passive(&market, c, is_dual_hub, bn);
                            let b_usd = market.token_usd(c.base_token);
                            let b_dec = market.token_decimals(c.base_token);
                            let paid_bid = compute_bid_with_profile(
                                false,
                                is_wbnb,
                                is_dual_hub,
                                eff_flow_len,
                                sim.gas_used,
                                exec_gp,
                                sim.net_profit_raw,
                                sim.gross_profit_usd,
                                market.native_usd,
                                b_usd,
                                b_dec,
                                min_profit_usd,
                                wallet_bal_wei,
                                bid_profile,
                            );
                            if paid_bid.below_gas_reserve && any_below_reserve.is_none() {
                                any_below_reserve = Some(paid_bid.reserve_gas_wei);
                            }
                            if paid_bid.wallet_affordable
                                && paid_bid.expected_net_usd + 1e-6 >= min_profit_usd
                            {
                                let expected_net_wei = ((paid_bid.expected_net_usd.max(0.0)
                                    / market.native_usd.max(1.0))
                                    * 1e18) as u128;
                                let score = paid_bid
                                    .effective_builder_wei
                                    .saturating_add(expected_net_wei / 4)
                                    .saturating_add(if is_wbnb { 2_000_000_000_000 } else { 0 });
                                if best_choice.as_ref().map_or(true, |b| score > b.3) {
                                    best_choice = Some((idx, sim, paid_bid, score));
                                }
                            }
                        }
                        if best_choice.is_none() {
                            if let Some(reserve_gas_wei) = any_below_reserve {
                                tracing::warn!(
                                    block = bn,
                                    reserve_bnb = format!("{:.6}", reserve_gas_wei as f64 / 1e18),
                                    wallet_bnb = format!("{:.6}", u256_to_human(wallet_bal_wei, 18)),
                                    "wallet BNB below 1-tx reserve gas — skipping bundle submit"
                                );
                            }
                        }
                        best_choice.map(|(idx, sim, paid_bid, _)| {
                            let p_spec = BscTxSpec {
                                exec_gp: paid_bid.exec_gp,
                                min_profit_raw: paid_bid.floor_min_raw,
                                bribe_bps: paid_bid.bribe_bps,
                                msg_value_wei: paid_bid.msg_value_wei,
                            };
                            (
                                idx,
                                sim,
                                true,
                                Some(paid_bid.exec_gp),
                                paid_bid.tip_wei,
                                paid_bid.bid_gas_usd,
                                paid_bid.floor_min_raw,
                                paid_bid.bribe_bps,
                                None,
                                Some(p_spec),
                                paid_bid.expected_net_usd,
                            )
                        })
                    }
                    BiddingMode::PriorityFee => ranked_sims.into_iter().next().map(|(best_idx, best_sim)| {
                        let cand = &top_cands[best_idx];
                        let base_usd = market.token_usd(cand.base_token);
                        let base_dec = market.token_decimals(cand.base_token);
                        let safe_base_usd = base_usd.max(1e-9);
                        let req_gross_usd = best_sim.gas_cost_usd + min_profit_usd * 0.95;
                        let raw_req = human_to_u256(req_gross_usd / safe_base_usd, base_dec);
                        let max_safe_raw =
                            (best_sim.net_profit_raw * U256::from(997u64)) / U256::from(1000u64);
                        let safe_floor = raw_req.min(max_safe_raw).max(U256::from(1u64));
                        let gas_cost_usd = best_sim.gas_cost_usd;
                        let expected_net = best_sim.gross_profit_usd - gas_cost_usd;
                        let ok_floor = safe_floor < best_sim.net_profit_raw;
                        (
                            best_idx,
                            best_sim,
                            ok_floor,
                            None,
                            0,
                            gas_cost_usd,
                            safe_floor,
                            0u16,
                            None,
                            None,
                            expected_net,
                        )
                    }),
                };

                if let Some((
                    best_idx,
                    best_sim,
                    wallet_affordable,
                    bid_gas_price_wei,
                    tip_wei,
                    bid_gas_usd,
                    floor_min_raw,
                    bribe_bps,
                    zero_gas_spec,
                    paid_gas_spec,
                    expected_net_usd,
                )) = selected_exec
                {
                    let cand = &top_cands[best_idx];
                    let non_backrun_active: Vec<&PendingBundle> = active_bundles
                        .iter()
                        .filter(|b| {
                            !b.strategy.starts_with("Backrun(")
                                && !b.strategy.starts_with("MevShare(")
                                && (cfg.submission.mode != SubmissionMode::FlashbotsRelay
                                    || b.submitted_block == bn)
                        })
                        .collect();
                    let max_prev_tip = non_backrun_active.iter().map(|b| b.tip_wei).max().unwrap_or(0);
                    let tip_beats_prev =
                        non_backrun_active.is_empty() || tip_wei > (max_prev_tip * 112) / 100;

                    if wallet_affordable && tip_beats_prev && expected_net_usd + 1e-6 >= min_profit_usd {
                        if let (Some(ref signer), Some(exec_addr)) = (&signer_opt, deployed_executor) {
                            let is_replacement = active_bundles
                                .iter()
                                .any(|b| b.signer_addr == signer.address());
                            let nonce_res = if is_replacement {
                                Ok(active_bundles
                                    .iter()
                                    .find(|b| b.signer_addr == signer.address())
                                    .map(|b| b.nonce)
                                    .unwrap_or(active_bundles[0].nonce))
                            } else {
                                match cached_nonce {
                                    Some(n) => Ok(n),
                                    None => rpc.get_transaction_count(signer.address()).await,
                                }
                            };
                            match nonce_res {
                                Ok(nonce) => {
                                    if cfg.is_mev_bundle_enabled() {
                                        let is_dual_hub = cand.flows.iter().all(|f| {
                                            market.is_hub_token(f.token_in)
                                                && market.is_hub_token(f.token_out)
                                        });
                                        let (_, is_passive) = effective_bid_flow_len_and_passive(
                                            &market,
                                            cand,
                                            is_dual_hub,
                                            bn,
                                        );
                                        let span = if is_passive {
                                            cfg.timing.passive_bundle_span_blocks
                                        } else {
                                            cfg.timing.bundle_span_blocks
                                        };
                                        let max_blk = bn.saturating_add(span);
                                        match submit_arb_bundle_nonblocking(
                                            &rpc,
                                            &market,
                                            signer,
                                            exec_addr,
                                            &best_sim,
                                            zero_gas_spec,
                                            paid_gas_spec,
                                            nonce,
                                            max_blk,
                                        )
                                        .await
                                        {
                                            Ok((tx_hash, candidate_hashes)) => {
                                                submitted_this_block = true;
                                                let tip_bnb = tip_wei as f64 / 1e18;
                                                let tip_usd = tip_bnb * market.native_usd;
                                                tracing::info!(
                                                    block = bn,
                                                    %tx_hash,
                                                    strategy = %cand.strategy,
                                                    zero_gas = zero_gas_spec.is_some(),
                                                    paid_fallback = paid_gas_spec.is_some(),
                                                    bribe_bps,
                                                    gross_usd = format!("${:.4}", best_sim.gross_profit_usd),
                                                    tip_bnb = format!("{:.6}", tip_bnb),
                                                    tip_usd = format!("${:.4}", tip_usd),
                                                    expected_net_usd = format!("${:.4}", expected_net_usd),
                                                    pre_submit_ms = t_blk.elapsed().as_millis(),
                                                    "SUBMITTED MEV BUNDLE"
                                                );
                                                active_bundles.push(PendingBundle {
                                                    signer_addr: signer.address(),
                                                    tx_hash,
                                                    candidate_hashes,
                                                    submitted_block: bn,
                                                    max_block: max_blk,
                                                    nonce,
                                                    strategy: cand.strategy.clone(),
                                                    base_token: cand.base_token,
                                                    gross_profit_usd: best_sim.gross_profit_usd,
                                                    bid_gas_usd,
                                                    sim_gas: best_sim.gas_used,
                                                    l1_fee_wei: best_sim.l1_fee_wei,
                                                    bid_gas_price_wei,
                                                    tip_wei,
                                                });
                                            }
                                            Err(e) => {
                                                let es = e.to_string();
                                                if es.contains("all MEV builders rejected bundle") {
                                                    if !is_replacement {
                                                        cached_nonce = Some(nonce);
                                                    }
                                                    tracing::info!(block = bn, err = %es, "MEV bundle pre-rejected ($0 gas)");
                                                } else {
                                                    if !is_replacement {
                                                        cached_nonce = None;
                                                    }
                                                    tracing::warn!(err = %es, "broadcast error");
                                                }
                                            }
                                        }
                                    } else {
                                        match send_arb_tx(
                                            &rpc,
                                            &market,
                                            signer,
                                            exec_addr,
                                            &best_sim,
                                            floor_min_raw,
                                            nonce,
                                            bid_gas_price_wei,
                                            0,
                                            0,
                                        )
                                        .await
                                        {
                                            Ok((tx_hash, receipt_opt)) => {
                                                cached_nonce = if receipt_opt.is_some() {
                                                    Some(nonce + 1)
                                                } else {
                                                    None
                                                };
                                                finalize_live_receipt(
                                                    &market,
                                                    &mut pnl,
                                                    &pnl_path,
                                                    tx_hash,
                                                    bn,
                                                    &cand.strategy,
                                                    cand.base_token,
                                                    best_sim.gross_profit_usd,
                                                    bid_gas_usd,
                                                    best_sim.gas_used,
                                                    best_sim.l1_fee_wei,
                                                    bid_gas_price_wei,
                                                    0,
                                                    receipt_opt,
                                                );
                                            }
                                            Err(e) => {
                                                cached_nonce = None;
                                                tracing::warn!(err = %e, "broadcast error");
                                            }
                                        }
                                    }
                                }
                                Err(e) => {
                                    cached_nonce = None;
                                    tracing::warn!(err = %e, "nonce fetch error");
                                }
                            }
                        }
                    }
                }
            }

            if !deferred_eoa_refreshes.is_empty() {
                if let Some(ref pool) = eoa_pool {
                    let pool_c = pool.clone();
                    let rpc_c = rpc.clone();
                    tokio::spawn(async move {
                        for (addr, min_n) in deferred_eoa_refreshes {
                            pool_c.refresh_single(&rpc_c, addr, min_n).await;
                        }
                    });
                    wallet_bal_wei = pool.max_balance_wei();
                }
            }
            if !deferred_expired_bundles.is_empty() {
                for pb in &deferred_expired_bundles {
                    tracing::info!(
                        tx_hash = %pb.tx_hash,
                        eoa = %pb.signer_addr,
                        block = pb.submitted_block,
                        strategy = %pb.strategy,
                        "MEV bundle expired unmined ($0 gas — revert protected)"
                    );
                }
                let still_active_addrs: HashSet<Address> =
                    active_bundles.iter().map(|b| b.signer_addr).collect();
                let rpc_c = rpc.clone();
                let pool_c = eoa_pool.clone();
                let lw_tx = late_win_tx.clone();
                tokio::spawn(async move {
                    let mut nonce_cache: HashMap<Address, u64> = HashMap::new();
                    let mut seen_mined: HashSet<B256> = HashSet::new();
                    for pb in deferred_expired_bundles {
                        let onchain_nonce = match nonce_cache.get(&pb.signer_addr).copied() {
                            Some(n) => Some(n),
                            None => match rpc_c.get_transaction_count(pb.signer_addr).await {
                                Ok(n) => {
                                    nonce_cache.insert(pb.signer_addr, n);
                                    Some(n)
                                }
                                Err(_) => None,
                            },
                        };
                        if let Some(n) = onchain_nonce {
                            if n > pb.nonce {
                                for &h in &pb.candidate_hashes {
                                    if seen_mined.contains(&h) {
                                        continue;
                                    }
                                    if let Ok(Some(rx)) = rpc_c.get_receipt(h).await {
                                        seen_mined.insert(h);
                                        if let Some(ref pool) = pool_c {
                                            pool.refresh_single(
                                                &rpc_c,
                                                pb.signer_addr,
                                                Some(pb.nonce + 1),
                                            )
                                            .await;
                                        }
                                        let _ = lw_tx.send((pb.clone(), h, rx));
                                        break;
                                    }
                                }
                                continue;
                            }
                        }
                        if !still_active_addrs.contains(&pb.signer_addr)
                            && bn > pb.max_block.saturating_add(1)
                        {
                            if let Some(ref pool) = pool_c {
                                pool.refresh_single(&rpc_c, pb.signer_addr, Some(pb.nonce))
                                    .await;
                            }
                        }
                    }
                });
            }

            let mut diag_count = 0usize;
            for (idx, used_pools, unique_non_hub, route_desc, mut err_str, prep_opt) in
                deferred_failures
            {
                if !submitted_this_block && diag_count < 2 {
                    if let Some(prep) = prep_opt {
                        diag_count += 1;
                        if let Some(diag_err) = diagnose_simulation(&rpc, &prep).await {
                            err_str = diag_err;
                        }
                    }
                }
                let cand = &top_cands[idx];
                let base_dec = market.token_decimals(cand.base_token);
                let base_usd = market.token_usd(cand.base_token);
                let diag = simulator::decode_sim_error(&err_str, base_dec, base_usd);
                let cat = diag.category().to_string();
                let summary = diag.summary(&market);
                let mut sig = used_pools.clone();
                sig.sort();
                sig.dedup();
                let has_non_hub_pool = sig.iter().any(|a| {
                    market.pools.get(a).is_some_and(|p| {
                        !market.is_hub_token(p.token0) || !market.is_hub_token(p.token1)
                    })
                });
                let cd_blks = if matches!(diag, simulator::SimDiagnostic::ProfitBelowMin { .. }) {
                    6
                } else if has_non_hub_pool {
                    25
                } else {
                    20
                };
                failed_route_cooldown.insert(sig, (bn, cd_blks));
                market.record_sim_diagnostic(&used_pools, &unique_non_hub, &diag);
                tracing::info!(
                    block = bn,
                    strategy = %cand.strategy,
                    cat = %cat,
                    est_usd = format!("${:.4}", cand.est_net_profit_usd),
                    diag = %summary,
                    route = %route_desc,
                    "ETH_CALL SIM REJECTED"
                );
                append_opp_log(OppLog {
                    timestamp: Utc::now().to_rfc3339(),
                    chain: cfg.as_str().to_string(),
                    block: bn,
                    strategy: cand.strategy.clone(),
                    base_token: market.token_symbol(cand.base_token),
                    borrow_human: cand.borrow_human,
                    est_net_usd: cand.est_net_profit_usd,
                    sim_ok: false,
                    sim_net_usd: None,
                    sim_gas: None,
                    sim_err: Some(summary),
                    sim_err_cat: Some(cat),
                    edges: cand.flows.len(),
                    route: route_desc,
                });
            }

            if !submitted_this_block {
                if bn % cfg.timing.gas_refresh_blocks.max(1) == 0 {
                    if cfg.gas.tx_type == TxType::Eip1559 {
                        if let Some(bf) = rpc.latest_ws_next_base_fee(bn) {
                            market.gas_price_wei = bf.max(cfg.min_gas_price_wei());
                        } else if let Ok(Some(bf)) = rpc.get_block_next_base_fee(bn).await {
                            market.gas_price_wei = bf.max(cfg.min_gas_price_wei());
                        } else if let Ok(gp) = rpc.gas_price().await {
                            market.gas_price_wei = gp.max(cfg.min_gas_price_wei());
                        }
                    } else {
                        let mut gps: Vec<u128> = receipts
                            .iter()
                            .filter_map(|rx| {
                                rx.effective_gas_price
                                    .as_deref()
                                    .and_then(|s| rpc::parse_hex_u128(s).ok())
                                    .filter(|&gp| gp >= cfg.min_gas_price_wei())
                            })
                            .collect();
                        if gps.len() >= 8 {
                            gps.sort_unstable();
                            market.gas_price_wei = gps[gps.len() / 4].max(cfg.min_gas_price_wei());
                        } else if let Ok(gp) = rpc.gas_price().await {
                            market.gas_price_wei = gp.max(cfg.min_gas_price_wei());
                        }
                    }
                    let _ = market.refresh_flash_balances(&rpc).await;
                }
                if live_ready && processed_blocks > 0 && processed_blocks % 45 == 0 {
                    rpc.spawn_keep_warm();
                    if active_bundles.is_empty() {
                        if let Some(ref signer) = signer_opt {
                            if let Ok(n) = rpc.get_transaction_count(signer.address()).await {
                                cached_nonce = Some(n);
                            }
                        }
                    }
                }
                if processed_blocks > 0 && processed_blocks % 80 == 0 {
                    market.prune_dead_pools();
                }
                if large_block_gap || (processed_blocks > 0 && processed_blocks % 160 == 0) {
                    let _ = market.resync_active_pools(&rpc).await;
                }
                if live_ready && processed_blocks > 0 && processed_blocks % 120 == 0 {
                    if let Some(ref pool) = eoa_pool {
                        pool.refresh_all(&rpc, cfg.multicall3).await;
                        wallet_bal_wei = pool.max_balance_wei();
                    }
                }
                if processed_blocks > 0 && processed_blocks % cfg.timing.cache_save_blocks.max(1) == 0 {
                    market.save_pools_cache();
                    if live_ready {
                        if let Some(ref signer) = signer_opt {
                            if let Ok(b) = rpc.get_balance(signer.address()).await {
                                wallet_bal_wei = b;
                            }
                        }
                    }
                }
            }

            // Post-hot-path pool expansion (rate-limited and budget-guarded so discovery never stalls block cadence)
            let run_disc = !submitted_this_block
                && t_blk.elapsed() < Duration::from_millis(cfg.timing.discovery_budget_ms)
                && if market.pools.len() < 1200 {
                    true
                } else if market.pools.len() > 6000 {
                    bn % 6 == 0
                } else {
                    bn % 2 == 0
                };
            if run_disc {
                if let Err(e) = market.discover_unknowns(&rpc, bn, &mut delta).await {
                    tracing::debug!(block = bn, err = %e, "post-solve discovery warning");
                } else if delta.discovered_pools > 0 {
                    carryover_dirty_pools.extend(delta.dirty_pools.iter().copied());
                }
            }

            tracing::info!(
                block = bn,
                txs = receipts.len(),
                touched = delta.touched_pools,
                new_pools = delta.discovered_pools,
                total_pools = market.pools.len(),
                solve_us,
                cands = candidates.len(),
                blk_ms = t_blk.elapsed().as_millis(),
                "scanned block"
            );

            last_seen_block = bn;
            processed_blocks += 1;
            market_snap = std::sync::Arc::new(market.clone());
            mev_share_seen_hashes.clear();
            mev_share_block_subs.store(0, std::sync::atomic::Ordering::Release);
        }

        kw_handle.abort();

        println!(
            "Scan summary [{}]: blocks={}, pools={}, tokens={}, raw_cands={}, eth_call_verified={}",
            cfg.as_str(),
            processed_blocks,
            market.pools.len(),
            market.tokens.len(),
            total_candidates,
            total_verified_opps
        );
    Ok(())
}

struct BscBidOutcome {
    wallet_affordable: bool,
    below_gas_reserve: bool,
    reserve_gas_wei: u128,
    exec_gp: u128,
    #[allow(dead_code)]
    base_gas_wei: u128,
    tip_wei: u128,
    effective_builder_wei: u128,
    bid_gas_usd: f64,
    floor_min_raw: U256,
    bribe_bps: u16,
    msg_value_wei: u128,
    expected_net_usd: f64,
}

fn effective_bid_flow_len_and_passive(
    market: &MarketState,
    cand: &router::RouteCandidate,
    is_dual_hub: bool,
    bn: u64,
) -> (usize, bool) {
    if is_dual_hub {
        return (cand.flows.len(), false);
    }
    let mut non_hub_tokens: HashSet<Address> = HashSet::new();
    let mut any_fresh_non_hub = false;
    for f in &cand.flows {
        let in_hub = market.is_hub_token(f.token_in);
        let out_hub = market.is_hub_token(f.token_out);
        if !in_hub {
            non_hub_tokens.insert(f.token_in);
        }
        if !out_hub {
            non_hub_tokens.insert(f.token_out);
        }
        if (!in_hub || !out_hub)
            && market
                .pools
                .get(&f.pool)
                .is_some_and(|p| p.last_updated_block.saturating_add(1) >= bn)
        {
            any_fresh_non_hub = true;
        }
    }
    let is_passive = !any_fresh_non_hub || (non_hub_tokens.len() >= 3 && cand.flows.len() >= 8);
    let eff_flow_len = if cand.flows.len() >= 6 && !is_passive {
        5
    } else {
        cand.flows.len()
    };
    (eff_flow_len, is_passive)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BidChainProfile {
    BscBep322,
    EthEip1559Atomic {
        priority_fee_wei: u128,
        max_fee_multiplier_bps: u128,
    },
}

fn bid_chain_profile(cfg: &ChainConfig) -> BidChainProfile {
    if cfg.gas.tx_type == TxType::Eip1559 && cfg.submission.mode == SubmissionMode::FlashbotsRelay {
        BidChainProfile::EthEip1559Atomic {
            priority_fee_wei: cfg.gas.bundle_priority_fee_wei,
            max_fee_multiplier_bps: cfg.gas.max_fee_multiplier_bps,
        }
    } else {
        BidChainProfile::BscBep322
    }
}

#[allow(clippy::too_many_arguments, dead_code)]
fn compute_bsc_bid(
    use_zero_gas: bool,
    is_wbnb_base: bool,
    is_dual_hub: bool,
    flow_len: usize,
    sim_gas_used: u64,
    exec_gp: u128,
    net_profit_raw: U256,
    gross_profit_usd: f64,
    native_usd: f64,
    base_usd: f64,
    base_dec: u8,
    min_profit_usd: f64,
    wallet_bal_wei: U256,
) -> BscBidOutcome {
    compute_bid_with_profile(
        use_zero_gas,
        is_wbnb_base,
        is_dual_hub,
        flow_len,
        sim_gas_used,
        exec_gp,
        net_profit_raw,
        gross_profit_usd,
        native_usd,
        base_usd,
        base_dec,
        min_profit_usd,
        wallet_bal_wei,
        BidChainProfile::BscBep322,
    )
}

#[allow(clippy::too_many_arguments)]
fn compute_bid_with_profile(
    use_zero_gas: bool,
    is_wbnb_base: bool,
    is_dual_hub: bool,
    flow_len: usize,
    sim_gas_used: u64,
    exec_gp: u128,
    net_profit_raw: U256,
    gross_profit_usd: f64,
    native_usd: f64,
    base_usd: f64,
    base_dec: u8,
    min_profit_usd: f64,
    wallet_bal_wei: U256,
    profile: BidChainProfile,
) -> BscBidOutcome {
    let is_eth_atomic = matches!(profile, BidChainProfile::EthEip1559Atomic { .. });
    let priority_fee_wei = match profile {
        BidChainProfile::EthEip1559Atomic { priority_fee_wei, .. } => priority_fee_wei,
        BidChainProfile::BscBep322 => 0,
    };
    let arb_gas_limit = (((sim_gas_used as f64) * 1.35).ceil() as u128 + 35_000).max(280_000);
    let eff_gas_used = sim_gas_used;
    let effective_exec_gp = if use_zero_gas { 0 } else { exec_gp };
    let unit_gas_cost_wei = if use_zero_gas {
        0
    } else {
        effective_exec_gp.saturating_add(priority_fee_wei)
    };
    let base_gas_wei = (eff_gas_used as u128) * unit_gas_cost_wei;
    let safe_native_usd = native_usd.max(1.0);
    let gross_wei = if is_wbnb_base {
        net_profit_raw.saturating_to::<u128>()
    } else {
        ((gross_profit_usd / safe_native_usd) * 1e18) as u128
    };
    let min_keep_wei =
        (((min_profit_usd * 1.0005) / safe_native_usd) * 1e18).ceil() as u128 + 10_000;
    let min_builder_floor_wei = if use_zero_gas {
        (eff_gas_used as u128) * 51_000_000
    } else {
        0
    };
    let surplus_wei = gross_wei.saturating_sub(base_gas_wei);
    let surplus_usd = (surplus_wei as f64 / 1e18) * safe_native_usd;
    let non_wbnb_cushion_wei = if is_wbnb_base {
        0u128
    } else if is_eth_atomic {
        if surplus_usd >= 0.02 {
            (gross_wei / 1_000).max(100_000_000_000)
        } else {
            0u128
        }
    } else {
        (gross_wei / 400).max(300_000_000_000)
    };
    let drift_gas_wei: u128 = if use_zero_gas {
        0
    } else if is_eth_atomic {
        if surplus_usd >= 0.02 {
            4_000u128 * unit_gas_cost_wei
        } else {
            0
        }
    } else if surplus_usd < 0.15 {
        38_000 * effective_exec_gp
    } else {
        12_000 * effective_exec_gp
    };
    let required_surplus_wei = min_keep_wei
        .saturating_add(drift_gas_wei)
        .saturating_add(non_wbnb_cushion_wei);
    let reserve_gas_wei = if use_zero_gas {
        0
    } else {
        match profile {
            BidChainProfile::EthEip1559Atomic {
                priority_fee_wei,
                max_fee_multiplier_bps,
            } => {
                let max_fee_wei = ((effective_exec_gp * max_fee_multiplier_bps) / 10_000)
                    .max(effective_exec_gp)
                    .saturating_add(priority_fee_wei);
                arb_gas_limit * max_fee_wei
            }
            BidChainProfile::BscBep322 => arb_gas_limit * effective_exec_gp,
        }
    };
    let max_wallet_wei = (wallet_bal_wei.saturating_to::<u128>() * 97) / 100;

    let mut wallet_affordable = true;
    let mut below_gas_reserve = false;
    let max_tip_from_surplus = surplus_wei.saturating_sub(required_surplus_wei);
    let raw_tip_target = if surplus_wei <= required_surplus_wei
        || max_tip_from_surplus < min_builder_floor_wei
    {
        wallet_affordable = false;
        0
    } else if (!use_zero_gas || !is_wbnb_base)
        && max_wallet_wei <= reserve_gas_wei.max(min_builder_floor_wei)
    {
        wallet_affordable = false;
        below_gas_reserve = true;
        0
    } else {
        let tip_share = if is_eth_atomic {
            if surplus_usd >= 0.50 {
                if is_dual_hub { 0.994 } else { 0.990 }
            } else if surplus_usd >= 0.10 {
                0.985
            } else {
                0.975
            }
        } else if !is_dual_hub && flow_len >= 10 {
            if surplus_usd >= 0.50 {
                0.76
            } else if surplus_usd >= 0.15 {
                0.54
            } else {
                0.44
            }
        } else if !is_dual_hub && flow_len >= 6 {
            if surplus_usd >= 0.50 {
                0.76
            } else if surplus_usd >= 0.15 {
                0.58
            } else {
                0.48
            }
        } else if !is_dual_hub {
            if surplus_usd >= 0.50 {
                0.985
            } else if surplus_usd >= 0.08 {
                0.988
            } else {
                0.985
            }
        } else if surplus_usd >= 0.20 {
            0.9965
        } else if surplus_usd >= 0.04 {
            0.992
        } else {
            0.982
        };
        let min_tip_floor = if is_eth_atomic {
            min_builder_floor_wei.max(10_000_000_000)
        } else {
            min_builder_floor_wei.max(if !is_dual_hub && flow_len >= 6 {
                5_500_000_000_000
            } else {
                2_000_000_000_000
            })
        };
        let mut raw_tip_wei = ((surplus_wei as f64 * tip_share) as u128).min(max_tip_from_surplus);
        if max_tip_from_surplus >= min_tip_floor {
            raw_tip_wei = raw_tip_wei.max(min_tip_floor);
        }
        if is_wbnb_base {
            raw_tip_wei
        } else {
            raw_tip_wei
                .min(max_wallet_wei - reserve_gas_wei)
                .min(15_000_000_000_000_000)
        }
    };

    let (floor_min_raw, bribe_bps, msg_value_wei, tip_wei, expected_net_usd) = if is_wbnb_base {
        let floor_gross_wei = base_gas_wei + drift_gas_wei + min_keep_wei;
        if floor_gross_wei >= gross_wei {
            wallet_affordable = false;
        }
        let avail = gross_wei.saturating_sub(floor_gross_wei);
        let max_bps = if is_eth_atomic {
            10_000.0
        } else if !use_zero_gas && !is_dual_hub && flow_len >= 6 && surplus_usd < 0.50 {
            if avail >= 9_000_000_000_000 {
                6_500.0
            } else {
                7_800.0
            }
        } else if !use_zero_gas && surplus_usd < 0.08 {
            9_850.0
        } else {
            10_000.0
        };
        let bps = (((raw_tip_target as f64) / (avail.max(1) as f64)) * 10_000.0)
            .round()
            .clamp(100.0, max_bps) as u16;
        let synced_tip_wei = if wallet_affordable {
            (avail * (bps as u128)) / 10_000
        } else {
            0
        };
        let exp_net =
            ((gross_wei.saturating_sub(base_gas_wei + synced_tip_wei)) as f64 / 1e18) * safe_native_usd;
        (U256::from(floor_gross_wei), bps, 0u128, synced_tip_wei, exp_net)
    } else {
        let safe_base_usd = base_usd.max(1e-9);
        let req_gross_wei = base_gas_wei + drift_gas_wei + raw_tip_target + min_keep_wei;
        let req_gross_usd = (req_gross_wei as f64 / 1e18) * safe_native_usd;
        let raw_req = human_to_u256(req_gross_usd / safe_base_usd, base_dec);
        let max_safe_raw = (net_profit_raw * U256::from(998u64)) / U256::from(1000u64);
        let safe_floor = raw_req.min(max_safe_raw).max(U256::from(1u64));
        let erc20_gas_usd = ((base_gas_wei + raw_tip_target) as f64 / 1e18) * safe_native_usd;
        if safe_floor >= net_profit_raw
            || u256_to_human(safe_floor, base_dec) * safe_base_usd + 1e-9
                < erc20_gas_usd + min_profit_usd * 0.90
        {
            wallet_affordable = false;
        }
        let exp_net = gross_profit_usd - erc20_gas_usd;
        (safe_floor, 0u16, raw_tip_target, raw_tip_target, exp_net)
    };

    let gas_usd = ((base_gas_wei + tip_wei) as f64 / 1e18) * safe_native_usd;
    let effective_builder_wei = match profile {
        BidChainProfile::EthEip1559Atomic {
            priority_fee_wei, ..
        } => tip_wei.saturating_add((eff_gas_used as u128) * priority_fee_wei),
        BidChainProfile::BscBep322 => tip_wei.saturating_add((base_gas_wei * 9) / 10),
    };

    BscBidOutcome {
        wallet_affordable,
        below_gas_reserve,
        reserve_gas_wei,
        exec_gp: effective_exec_gp,
        base_gas_wei,
        tip_wei,
        effective_builder_wei,
        bid_gas_usd: gas_usd,
        floor_min_raw,
        bribe_bps,
        msg_value_wei,
        expected_net_usd,
    }
}

/// Compute slippage-safe mempool backrun bid from unverified `shadow` profit projections.
/// ERC20-base routes apply a 0.78x haircut to prevent EVM `MIN_PROFIT` self-reverts when
/// post-victim on-chain profit lands slightly below local float estimates.
#[allow(clippy::too_many_arguments, dead_code)]
fn compute_bsc_backrun_bid(
    is_wbnb_base: bool,
    sim_gas_used: u64,
    exec_gp: u128,
    shadow_gross_raw: U256,
    shadow_gross_usd: f64,
    native_usd: f64,
    base_usd: f64,
    base_dec: u8,
    min_profit_usd: f64,
    wallet_bal_wei: U256,
) -> BscBidOutcome {
    compute_backrun_bid_with_profile(
        is_wbnb_base,
        sim_gas_used,
        exec_gp,
        shadow_gross_raw,
        shadow_gross_usd,
        native_usd,
        base_usd,
        base_dec,
        min_profit_usd,
        wallet_bal_wei,
        BidChainProfile::BscBep322,
    )
}

#[allow(clippy::too_many_arguments)]
fn compute_backrun_bid_with_profile(
    is_wbnb_base: bool,
    sim_gas_used: u64,
    exec_gp: u128,
    shadow_gross_raw: U256,
    shadow_gross_usd: f64,
    native_usd: f64,
    base_usd: f64,
    base_dec: u8,
    min_profit_usd: f64,
    wallet_bal_wei: U256,
    profile: BidChainProfile,
) -> BscBidOutcome {
    let (adj_raw, adj_usd) = if is_wbnb_base {
        (shadow_gross_raw, shadow_gross_usd)
    } else {
        (
            (shadow_gross_raw * U256::from(78u64)) / U256::from(100u64),
            shadow_gross_usd * 0.78,
        )
    };
    compute_bid_with_profile(
        false,
        is_wbnb_base,
        false,
        6,
        sim_gas_used,
        exec_gp,
        adj_raw,
        adj_usd,
        native_usd,
        base_usd,
        base_dec,
        min_profit_usd,
        wallet_bal_wei,
        profile,
    )
}

#[allow(clippy::too_many_arguments)]
fn compute_mev_share_bid(
    is_wbnb_base: bool,
    sim_gas_used: u64,
    exec_gp: u128,
    shadow_gross_raw: U256,
    shadow_gross_usd: f64,
    native_usd: f64,
    base_usd: f64,
    base_dec: u8,
    min_profit_usd: f64,
    wallet_bal_wei: U256,
    profile: BidChainProfile,
) -> BscBidOutcome {
    let (adj_raw, adj_usd) = if is_wbnb_base {
        (shadow_gross_raw, shadow_gross_usd)
    } else {
        (
            (shadow_gross_raw * U256::from(92u64)) / U256::from(100u64),
            shadow_gross_usd * 0.92,
        )
    };
    compute_bid_with_profile(
        false,
        is_wbnb_base,
        false,
        6,
        sim_gas_used,
        exec_gp,
        adj_raw,
        adj_usd,
        native_usd,
        base_usd,
        base_dec,
        min_profit_usd,
        wallet_bal_wei,
        profile,
    )
}

async fn recv_opt<T>(rx: &mut Option<tokio::sync::mpsc::Receiver<T>>) -> Option<T> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// Select up to 2 diverse mempool backrun candidates, prioritizing a slippage-resilient
/// WBNB-base or compact (<= 3-hop) route alongside the highest-profit DAG candidate.
#[allow(dead_code)]
fn pick_backrun_candidates(cands: &[router::RouteCandidate], wnative: Address) -> Vec<usize> {
    pick_backrun_candidates_with_min(cands, wnative, 0.02)
}

fn pick_backrun_candidates_with_min(
    cands: &[router::RouteCandidate],
    wnative: Address,
    min_profit_usd: f64,
) -> Vec<usize> {
    let min_p = min_profit_usd.max(0.0);
    let second_floor = (min_p * 1.25).max(0.0001);
    let split_floor = (min_p * 1.75).max(0.0001);
    let eligible: Vec<usize> = cands
        .iter()
        .enumerate()
        .filter_map(|(i, c)| (c.est_net_profit_usd >= min_p).then_some(i))
        .collect();
    let Some(&lead_idx) = eligible.first() else {
        return Vec::new();
    };
    let is_safe = |c: &router::RouteCandidate| -> bool {
        (c.base_token == wnative && c.flows.len() <= 5) || c.flows.len() <= 3
    };
    let lead_net = cands[lead_idx].est_net_profit_usd;
    let safe_idx = eligible
        .iter()
        .copied()
        .find(|&i| is_safe(&cands[i]) && cands[i].est_net_profit_usd >= lead_net * 0.65)
        .or_else(|| {
            (cands[lead_idx].flows.len() > 4).then(|| {
                eligible
                    .iter()
                    .copied()
                    .find(|&i| is_safe(&cands[i]) && cands[i].est_net_profit_usd >= second_floor)
            })?
        });

    let mut out = Vec::with_capacity(2);
    if let Some(si) = safe_idx {
        out.push(si);
        if si != lead_idx && cands[lead_idx].est_net_profit_usd >= split_floor {
            out.push(lead_idx);
        }
    } else {
        out.push(lead_idx);
        if let Some(alt_idx) = eligible.iter().copied().find(|&j| {
            j != lead_idx
                && cands[j].est_net_profit_usd >= second_floor
                && ((cands[j].base_token == wnative) != (cands[lead_idx].base_token == wnative)
                    || cands[j].flows.len() < cands[lead_idx].flows.len())
        }) {
            out.push(alt_idx);
        }
    }
    out
}

fn scale_candidate(cand: &router::RouteCandidate, factor: f64) -> router::RouteCandidate {
    let mut scaled = cand.clone();
    scaled.borrow_human *= factor;
    scaled.est_gross_profit_human *= factor;
    scaled.est_gross_profit_usd *= factor;
    scaled.est_net_profit_usd *= factor;
    for f in &mut scaled.flows {
        f.flow_in_human *= factor;
        f.flow_out_human *= factor;
    }
    scaled
}

fn append_opp_log(row: OppLog) {
    if let Ok(line) = serde_json::to_string(&row) {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("logs/opportunities.jsonl")
        {
            let _ = writeln!(f, "{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_bsc_bid_no_self_revert_and_competitive() {
        let native_usd = 591.0;
        let exec_gp = 50_000_000u128; // 0.05 gwei
        let wallet_bal = U256::from(4_510_370_000_000_000u128);
        let min_profit_usd = 0.0020;

        // 1) Case 1: USDT 5-step DAG ($0.1736 gross, gas=626664) — paid & zero-gas
        let gross_1 = 0.1736;
        let raw_1 = human_to_u256(gross_1, 18);
        let b1 = compute_bsc_bid(
            false,
            false,
            false,
            5,
            626_664,
            exec_gp,
            raw_1,
            gross_1,
            native_usd,
            1.0,
            18,
            min_profit_usd,
            wallet_bal,
        );
        assert!(b1.wallet_affordable);
        assert!(b1.floor_min_raw < raw_1);
        assert_eq!(b1.bribe_bps, 0);
        assert!(b1.msg_value_wei == b1.tip_wei);
        assert!(b1.expected_net_usd >= min_profit_usd);

        // 2) Case 2: USDT 2-hop IRYS ($0.0437 gross, gas=257446) — paid mode
        let gross_2 = 0.0437;
        let raw_2 = human_to_u256(gross_2, 18);
        let b2 = compute_bsc_bid(
            false,
            false,
            true,
            2,
            257_446,
            exec_gp,
            raw_2,
            gross_2,
            native_usd,
            1.0,
            18,
            min_profit_usd,
            wallet_bal,
        );
        assert!(b2.wallet_affordable);
        assert!(b2.floor_min_raw < raw_2);
        let tip2_usd = (b2.tip_wei as f64 / 1e18) * native_usd;
        assert!(tip2_usd > 0.030, "expected tip > $0.030 to beat $0.0239 competitor, got {tip2_usd}");
        assert!(b2.expected_net_usd >= min_profit_usd);

        // 3) Case 3: WBNB 2-hop FOREST ($0.2897 gross, gas=402234) — zero-gas & paid
        let gross_3_usd = 0.2897;
        let raw_3 = U256::from(((gross_3_usd / native_usd) * 1e18) as u128);
        let b3_paid = compute_bsc_bid(
            false,
            true,
            true,
            2,
            402_234,
            exec_gp,
            raw_3,
            gross_3_usd,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
        );
        assert!(b3_paid.wallet_affordable);
        assert!(b3_paid.floor_min_raw < raw_3);
        assert!(b3_paid.bribe_bps >= 9900);
        let b3_zero = compute_bsc_bid(
            true,
            true,
            true,
            2,
            402_234,
            exec_gp,
            raw_3,
            gross_3_usd,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
        );
        assert!(b3_zero.wallet_affordable);
        assert_eq!(b3_zero.exec_gp, 0);
        assert!(b3_zero.floor_min_raw < raw_3);
        assert_eq!(b3_zero.msg_value_wei, 0);
        let tip3_zero_usd = (b3_zero.tip_wei as f64 / 1e18) * native_usd;
        assert!(tip3_zero_usd >= 0.285, "expected zero-gas tip >= $0.285, got {tip3_zero_usd}");
        assert!(b3_zero.expected_net_usd >= min_profit_usd);

        // 4) Case 4: USDT 3-hop R2/UUSD ($0.0189 gross, gas=369546) — zero-gas clears $0.0155+
        let gross_4 = 0.0189;
        let raw_4 = human_to_u256(gross_4, 18);
        let b4_zero = compute_bsc_bid(
            true,
            false,
            true,
            3,
            369_546,
            exec_gp,
            raw_4,
            gross_4,
            native_usd,
            1.0,
            18,
            min_profit_usd,
            wallet_bal,
        );
        assert!(b4_zero.wallet_affordable);
        assert_eq!(b4_zero.exec_gp, 0);
        assert!(b4_zero.floor_min_raw < raw_4);
        let tip4_usd = (b4_zero.tip_wei as f64 / 1e18) * native_usd;
        assert!(tip4_usd >= 0.0155, "expected zero-gas tip >= $0.0155 in Case 4, got {tip4_usd}");
        assert!(b4_zero.expected_net_usd >= min_profit_usd);

        // 5) Case 5: WBNB 2-hop FOREST ($0.2866 gross, 291309 gas) beats $0.2757 winner
        let b5_usd = 0.2866;
        let raw_5 = U256::from(((b5_usd / native_usd) * 1e18) as u128);
        let b5 = compute_bsc_bid(
            false,
            true,
            true,
            2,
            291_309,
            51_000_000,
            raw_5,
            b5_usd,
            native_usd,
            native_usd,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b5.wallet_affordable);
        assert!(b5.floor_min_raw < raw_5);
        let tip5_usd = (b5.tip_wei as f64 / 1e18) * native_usd;
        assert!(tip5_usd >= 0.2764, "expected Case 5 tip >= $0.2764, got {tip5_usd}");
        assert!(b5.expected_net_usd >= 0.0008, "expected net >= $0.0008, got {}", b5.expected_net_usd);

        // 6) Case 6: WBNB ($3.2590, 412635 gas) beats wallet-capped USDT ($3.2930, 677284 gas)
        let raw_usdt_6 = human_to_u256(3.2930, 18);
        let b6_usdt = compute_bsc_bid(
            false,
            false,
            false,
            4,
            677_284,
            51_000_000,
            raw_usdt_6,
            3.2930,
            native_usd,
            1.0,
            18,
            0.0008,
            wallet_bal,
        );
        let raw_wbnb_6 = U256::from(((3.2590 / native_usd) * 1e18) as u128);
        let b6_wbnb = compute_bsc_bid(
            false,
            true,
            true,
            3,
            412_635,
            51_000_000,
            raw_wbnb_6,
            3.2590,
            native_usd,
            native_usd,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b6_usdt.wallet_affordable && b6_wbnb.wallet_affordable);
        assert!(
            b6_wbnb.effective_builder_wei > b6_usdt.effective_builder_wei,
            "WBNB bid {} must beat capped USDT bid {}",
            b6_wbnb.effective_builder_wei,
            b6_usdt.effective_builder_wei
        );
        let wbnb6_usd = (b6_wbnb.effective_builder_wei as f64 / 1e18) * native_usd;
        assert!(wbnb6_usd > 3.23, "expected WBNB builder value > $3.23, got {wbnb6_usd}");

        // 7) Case 7: WBNB 3-hop YES ($0.7506 gross, 394835 gas) clears >= $0.735 tip
        let b7_usd = 0.7506;
        let raw_7 = U256::from(((b7_usd / native_usd) * 1e18) as u128);
        let b7 = compute_bsc_bid(
            false,
            true,
            true,
            3,
            394_835,
            51_000_000,
            raw_7,
            b7_usd,
            native_usd,
            native_usd,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b7.wallet_affordable);
        let tip7_usd = (b7.tip_wei as f64 / 1e18) * native_usd;
        assert!(tip7_usd >= 0.735, "expected Case 7 tip >= $0.735, got {tip7_usd}");

        // 8) Complexity-aware profit retention on 6-edge & 15-edge uncontested DAGs
        let b8_usd = 1.1869;
        let raw_8 = U256::from(((b8_usd / native_usd) * 1e18) as u128);
        let b8 = compute_bsc_bid(
            false,
            true,
            false,
            15,
            820_000,
            51_000_000,
            raw_8,
            b8_usd,
            native_usd,
            native_usd,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b8.wallet_affordable);
        assert!(
            b8.expected_net_usd >= 0.18,
            "expected 15-edge DAG to retain >= $0.18 net, got {}",
            b8.expected_net_usd
        );
        let tip8_usd = (b8.tip_wei as f64 / 1e18) * native_usd;
        assert!(
            tip8_usd >= 0.85,
            "expected 15-edge DAG tip >= $0.85, got {tip8_usd}"
        );

        // 9) Non-dual-hub 2-hop ($1.00 gross) bids competitively (0.985) while still retaining >= $0.012 net
        let b9_usd = 1.00;
        let raw_9 = U256::from(((b9_usd / native_usd) * 1e18) as u128);
        let b9 = compute_bsc_bid(
            false,
            true,
            false,
            2,
            280_000,
            51_000_000,
            raw_9,
            b9_usd,
            native_usd,
            native_usd,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b9.wallet_affordable);
        assert!(
            b9.expected_net_usd >= 0.012,
            "expected non-dual-hub 2-hop to retain >= $0.012 net, got {}",
            b9.expected_net_usd
        );
        let tip9_usd = (b9.tip_wei as f64 / 1e18) * native_usd;
        assert!(
            tip9_usd >= 0.97,
            "expected non-dual-hub 2-hop tip >= $0.97, got {tip9_usd}"
        );

        // 10) Case 10: +38,483 gas drift + on-chain slippage still yields > $0 net
        let native_10 = 581.8;
        let b10_usd = 0.0313;
        let raw_10 = U256::from(((b10_usd / native_10) * 1e18) as u128);
        let b10 = compute_bsc_bid(
            false,
            true,
            false,
            3,
            399_835,
            51_000_000,
            raw_10,
            b10_usd,
            native_10,
            native_10,
            18,
            0.0008,
            U256::from(2_500_000_000_000_000u128),
        );
        assert!(b10.wallet_affordable);
        assert!(b10.bribe_bps <= 9_850);
        let onchain_profit_wei = 49_330_000_000_000u128; // 0.00004933 BNB (~$0.0287)
        let onchain_gas_wei = 438_318u128 * 51_000_000u128;
        let floor_wei = b10.floor_min_raw.saturating_to::<u128>();
        assert!(onchain_profit_wei > floor_wei);
        let actual_bribe_wei = ((onchain_profit_wei - floor_wei) * (b10.bribe_bps as u128)) / 10_000;
        let realized_net_wei = onchain_profit_wei as i128 - actual_bribe_wei as i128 - onchain_gas_wei as i128;
        assert!(
            realized_net_wei > 0,
            "expected strictly positive on-chain net even under +38k gas drift, got {realized_net_wei} wei"
        );

        // 11) Case 11a & Case 11b: uncontested >=6-step DAGs avoid 9850 cliff & cap windfall bribe
        let b11a_sim_usd = 0.0450;
        let raw_11a = U256::from(((b11a_sim_usd / native_10) * 1e18) as u128);
        let b11a = compute_bsc_bid(
            false,
            true,
            false,
            8,
            657_090,
            51_000_000,
            raw_11a,
            b11a_sim_usd,
            native_10,
            native_10,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b11a.wallet_affordable);
        assert!(
            b11a.bribe_bps <= 6_500,
            "expected 8-step DAG bribe_bps <= 6500, got {}",
            b11a.bribe_bps
        );
        let onchain_11a_wei = ((0.0698188758 / native_10) * 1e18) as u128;
        let gas_11a_wei = 657_090u128 * 51_000_000u128;
        let floor_11a = b11a.floor_min_raw.saturating_to::<u128>();
        let bribe_11a = ((onchain_11a_wei - floor_11a) * (b11a.bribe_bps as u128)) / 10_000;
        let net_11a_usd = ((onchain_11a_wei - bribe_11a - gas_11a_wei) as f64 / 1e18) * native_10;
        assert!(
            net_11a_usd >= 0.020,
            "expected >= $0.020 realized net in Case 11a, got {net_11a_usd}"
        );

        let b11b_sim_usd = 0.0203;
        let raw_11b = U256::from(((b11b_sim_usd / native_10) * 1e18) as u128);
        let b11b = compute_bsc_bid(
            false,
            true,
            false,
            6,
            499_691,
            51_000_000,
            raw_11b,
            b11b_sim_usd,
            native_10,
            native_10,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b11b.wallet_affordable);
        assert!(
            b11b.bribe_bps <= 7_800,
            "expected 6-step DAG bribe_bps <= 7800 (no cliff fallthrough), got {}",
            b11b.bribe_bps
        );
        let onchain_11b_wei = ((0.037884137 / native_10) * 1e18) as u128;
        let gas_11b_wei = 499_691u128 * 51_000_000u128;
        let floor_11b = b11b.floor_min_raw.saturating_to::<u128>();
        let bribe_11b = ((onchain_11b_wei - floor_11b) * (b11b.bribe_bps as u128)) / 10_000;
        let net_11b_usd = ((onchain_11b_wei - bribe_11b - gas_11b_wei) as f64 / 1e18) * native_10;
        assert!(
            net_11b_usd >= 0.0055,
            "expected >= $0.0055 realized net in Case 11b, got {net_11b_usd}"
        );

        // 12) Case 12: DAG(20edges) (pure dual-hub) & Cyclic(4hop) (contested)
        let native_12 = 590.0;
        let b12a_usd = 0.5777;
        let raw_12a = U256::from(((b12a_usd / native_12) * 1e18) as u128);
        let b12a = compute_bsc_bid(
            false,
            true,
            true,
            20,
            1_186_476,
            51_000_000,
            raw_12a,
            b12a_usd,
            native_12,
            native_12,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b12a.wallet_affordable);
        assert!(
            b12a.bribe_bps >= 9_900,
            "expected dual-hub 20-step DAG to bid competitively (bribe_bps >= 9900), got {}",
            b12a.bribe_bps
        );
        let tip12a_usd = (b12a.tip_wei as f64 / 1e18) * native_12;
        assert!(
            tip12a_usd >= 0.52,
            "expected dual-hub 20-step DAG tip >= $0.52, got {tip12a_usd}"
        );

        let b12b_usd = 0.1552;
        let raw_12b = U256::from(((b12b_usd / native_12) * 1e18) as u128);
        let b12b = compute_bsc_bid(
            false,
            true,
            false,
            4,
            455_780,
            51_000_000,
            raw_12b,
            b12b_usd,
            native_12,
            native_12,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b12b.wallet_affordable);
        assert!(
            b12b.bribe_bps >= 9_500,
            "expected contested Cyclic(4hop) bribe_bps >= 9500, got {}",
            b12b.bribe_bps
        );
        let tip12b_usd = (b12b.tip_wei as f64 / 1e18) * native_12;
        assert!(
            tip12b_usd >= 0.132,
            "expected contested Cyclic(4hop) tip >= $0.132, got {tip12b_usd}"
        );

        // 13) Case 13a (Cyclic 2hop, $0.4443 gross) & Case 13b (Cyclic 3hop, $0.5375 gross)
        let b13a_usd = 0.4443;
        let raw_13a = U256::from(((b13a_usd / native_12) * 1e18) as u128);
        let b13a = compute_bsc_bid(
            false,
            true,
            false,
            2,
            299_242,
            51_000_000,
            raw_13a,
            b13a_usd,
            native_12,
            native_12,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b13a.wallet_affordable);
        let tip13a_usd = (b13a.tip_wei as f64 / 1e18) * native_12;
        assert!(
            tip13a_usd >= 0.428,
            "expected Case 13a tip >= $0.428, got {tip13a_usd}"
        );
        assert!(
            b13a.expected_net_usd >= 0.0045,
            "expected Case 13a net >= $0.0045, got {}",
            b13a.expected_net_usd
        );

        let b13b_usd = 0.5375;
        let raw_13b = U256::from(((b13b_usd / native_12) * 1e18) as u128);
        let b13b = compute_bsc_bid(
            false,
            true,
            false,
            3,
            439_301,
            51_000_000,
            raw_13b,
            b13b_usd,
            native_12,
            native_12,
            18,
            0.0008,
            wallet_bal,
        );
        assert!(b13b.wallet_affordable);
        let tip13b_usd = (b13b.tip_wei as f64 / 1e18) * native_12;
        assert!(
            tip13b_usd >= 0.514,
            "expected Case 13b tip >= $0.514, got {tip13b_usd}"
        );
        assert!(
            b13b.expected_net_usd >= 0.0055,
            "expected Case 13b net >= $0.0055, got {}",
            b13b.expected_net_usd
        );
    }

    #[test]
    fn test_effective_bid_flow_len_and_passive_classification() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let usdt = cfg.usd_anchor;
        let moolah = cfg.seed_flash_pools[0].pool;
        let mut market = MarketState::new(cfg);
        let bn = 2_000u64;
        market.last_block = bn;

        let tok_a = Address::repeat_byte(0xAA);
        let tok_b = Address::repeat_byte(0xBB);
        let tok_c = Address::repeat_byte(0xCC);
        let p_active = Address::repeat_byte(0x11);
        let p_quiet = Address::repeat_byte(0x22);
        let p_quiet2 = Address::repeat_byte(0x33);

        for (addr, t_x, lub) in [
            (p_active, tok_a, bn),
            (p_quiet, tok_b, bn - 500),
            (p_quiet2, tok_c, bn - 500),
        ] {
            market.insert_pool(amm::PoolState {
                address: addr,
                factory_name: "PancakeV2".into(),
                dex_kind: config::DexKind::UniV2,
                token0: wbnb.min(t_x),
                token1: wbnb.max(t_x),
                dec0: 18,
                dec1: 18,
                fee_num: 9975,
                fee_ppm: 2500,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: U256::from(10u128).pow(U256::from(20)),
                r1_raw: U256::from(10u128).pow(U256::from(20)),
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 0,
                allow_multi_tick: false,
                is_hub_0: false,
                is_hub_1: false,
                cached_depth_usd: 1_000.0,
                last_updated_block: lub,
                v4_key: None,
                metric: None,
                balancer: None,
            });
        }

        let mk_flow = |pool: Address, ti: Address, to: Address| router::DirectedFlow {
            pool,
            label: "P".into(),
            token_in: ti,
            token_out: to,
            dex_kind: config::DexKind::UniV2,
            fee_num: 9975,
            flow_in_human: 1.0,
            flow_out_human: 1.0,
        };

        // 1) 6 flows touching 1 fresh active non-hub pool (`bn`) + quiet counter-pool (`bn - 500`) -> contested (5, false)
        let active_with_quiet_counter = router::RouteCandidate {
            strategy: "DAG(6edges)".into(),
            base_token: wbnb,
            borrow_human: 1.0,
            est_gross_profit_human: 0.01,
            est_gross_profit_usd: 5.0,
            est_net_profit_usd: 4.9,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                mk_flow(p_active, wbnb, tok_a),
                mk_flow(p_active, tok_a, usdt),
                mk_flow(p_quiet, usdt, tok_b),
                mk_flow(p_quiet, tok_b, wbnb),
                mk_flow(p_active, wbnb, tok_a),
                mk_flow(p_active, tok_a, wbnb),
            ],
            mermaid: String::new(),
        };
        assert_eq!(
            effective_bid_flow_len_and_passive(&market, &active_with_quiet_counter, false, bn),
            (5, false)
        );

        // 2) 6 flows touching ONLY quiet non-hub pools (`bn - 500`) -> passive latent drift (6, true)
        let quiet_only = router::RouteCandidate {
            flows: vec![
                mk_flow(p_quiet, wbnb, tok_b),
                mk_flow(p_quiet, tok_b, usdt),
                mk_flow(p_quiet, usdt, tok_b),
                mk_flow(p_quiet, tok_b, wbnb),
                mk_flow(p_quiet, wbnb, tok_b),
                mk_flow(p_quiet, tok_b, wbnb),
            ],
            ..active_with_quiet_counter.clone()
        };
        assert_eq!(
            effective_bid_flow_len_and_passive(&market, &quiet_only, false, bn),
            (6, true)
        );

        // 3) 8 flows coalescing 3 distinct non-hub tokens -> passive multi-branch coalesced DAG (8, true)
        let coalesced_3_tokens = router::RouteCandidate {
            strategy: "DAG(8steps)".into(),
            flows: vec![
                mk_flow(p_active, wbnb, tok_a),
                mk_flow(p_active, tok_a, usdt),
                mk_flow(p_quiet, usdt, tok_b),
                mk_flow(p_quiet, tok_b, wbnb),
                mk_flow(p_quiet2, wbnb, tok_c),
                mk_flow(p_quiet2, tok_c, usdt),
                mk_flow(p_active, usdt, tok_a),
                mk_flow(p_active, tok_a, wbnb),
            ],
            ..active_with_quiet_counter.clone()
        };
        assert_eq!(
            effective_bid_flow_len_and_passive(&market, &coalesced_3_tokens, false, bn),
            (8, true)
        );

        // 4) Pure dual-hub keeps full flow length with passive=false
        assert_eq!(
            effective_bid_flow_len_and_passive(&market, &active_with_quiet_counter, true, bn),
            (6, false)
        );
    }

    #[test]
    fn test_backrun_bid_prevents_erc20_self_revert() {
        let native_usd = 620.0;
        let exec_gp = 51_000_000u128;
        let wallet_bal = U256::from(15_900_000_000_000_000u128); // 0.0159 BNB
        let min_profit_usd = 0.0008;

        // Reproduce ERC-20 shadow estimate $2.570 vs real EVM profit $2.5328
        let busd_gross_usd = 2.570;
        let busd_gross_raw = human_to_u256(busd_gross_usd, 18);
        let busd_bid = compute_bsc_backrun_bid(
            false,
            345_000,
            exec_gp,
            busd_gross_raw,
            busd_gross_usd,
            native_usd,
            1.0,
            18,
            min_profit_usd,
            wallet_bal,
        );
        assert!(busd_bid.wallet_affordable);
        let tip_usd = (busd_bid.tip_wei as f64 / 1e18) * native_usd;
        assert!(tip_usd > 1.20, "expected tip > $1.20, got {tip_usd}");
        let floor_busd = u256_to_human(busd_bid.floor_min_raw, 18);
        assert!(
            floor_busd < 2.05,
            "expected floor < 2.05 BUSD (below $2.5328 actual EVM profit), got {floor_busd}"
        );

        // WBNB-base backrun sets floor purely from gas + min_keep and scales tip via bribe_bps
        let wbnb_gross_raw = human_to_u256(busd_gross_usd / native_usd, 18);
        let wbnb_bid = compute_bsc_backrun_bid(
            true,
            345_000,
            exec_gp,
            wbnb_gross_raw,
            busd_gross_usd,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
        );
        assert!(wbnb_bid.wallet_affordable);
        assert_eq!(wbnb_bid.msg_value_wei, 0);
        assert!(
            wbnb_bid.bribe_bps >= 7000,
            "expected bribe_bps >= 7000, got {}",
            wbnb_bid.bribe_bps
        );
        let wbnb_floor_usd = u256_to_human(wbnb_bid.floor_min_raw, 18) * native_usd;
        assert!(
            wbnb_floor_usd < 0.03,
            "expected WBNB floor < $0.03, got {wbnb_floor_usd}"
        );
    }

    #[test]
    fn test_pick_backrun_candidates_pairs_compact_or_wbnb_with_dag() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let busd = alloy::primitives::address!("e9e7CEA3DedcA5984780Bafc599bD69ADd087D56");
        let usdt = cfg.usd_anchor;
        let moolah = cfg.seed_flash_pools[0].pool;

        let mk_flow = |pool: Address, ti: Address, to: Address| router::DirectedFlow {
            pool,
            label: "P".into(),
            token_in: ti,
            token_out: to,
            dex_kind: config::DexKind::UniV2,
            fee_num: 9975,
            flow_in_human: 100.0,
            flow_out_human: 100.0,
        };
        let p = Address::repeat_byte(0x44);

        let dag6 = router::RouteCandidate {
            strategy: "DAG(6edges)".into(),
            base_token: busd,
            borrow_human: 625.0,
            est_gross_profit_human: 2.57,
            est_gross_profit_usd: 2.57,
            est_net_profit_usd: 2.55,
            flash_mode: 1,
            flash_source: moolah,
            flash_fee_bps: 0.0,
            flows: vec![
                mk_flow(p, busd, usdt),
                mk_flow(p, usdt, wbnb),
                mk_flow(p, wbnb, busd),
                mk_flow(p, busd, usdt),
                mk_flow(p, usdt, wbnb),
                mk_flow(p, wbnb, busd),
            ],
            mermaid: String::new(),
        };
        let cycle3 = router::RouteCandidate {
            strategy: "Cyclic(3hop)".into(),
            est_gross_profit_human: 2.53,
            est_gross_profit_usd: 2.53,
            est_net_profit_usd: 2.51,
            flows: vec![
                mk_flow(p, busd, usdt),
                mk_flow(p, usdt, wbnb),
                mk_flow(p, wbnb, busd),
            ],
            ..dag6.clone()
        };

        assert_eq!(pick_backrun_candidates(&[dag6, cycle3], wbnb), vec![1, 0]);
    }

    #[test]
    fn test_compute_eth_eip1559_atomic_bid_and_builder_ranking() {
        let cfg = load_chain_config("eth").unwrap();
        let profile = bid_chain_profile(&cfg);
        assert!(matches!(profile, BidChainProfile::EthEip1559Atomic { .. }));

        let native_usd = 1700.0;
        let wallet_bal = U256::from(3_781_950_000_000_000u128);
        let min_profit_usd = 0.0001;

        // 1) Case 1: 14-step WETH DAG ($0.9517 gross, 1,430,766 gas, 0.14 gwei baseFee)
        let gross_1 = 0.9517;
        let raw_1 = U256::from(((gross_1 / native_usd) * 1e18) as u128);
        let b1 = compute_bid_with_profile(
            false,
            true,
            false,
            14,
            1_430_766,
            140_000_000,
            raw_1,
            gross_1,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
            profile,
        );
        assert!(b1.wallet_affordable);
        assert!(
            b1.bribe_bps >= 9_850,
            "expected ETH 14-step DAG bribe_bps >= 9850 (no BSC cap), got {}",
            b1.bribe_bps
        );
        let tip1_usd = (b1.tip_wei as f64 / 1e18) * native_usd;
        assert!(
            tip1_usd >= 0.60,
            "expected ETH 14-step DAG tip >= $0.60, got {tip1_usd:.4}"
        );
        assert!(b1.expected_net_usd >= min_profit_usd);

        // 2) Case 2: 3-hop WETH ($0.8045 gross, 406,935 gas, 0.1395 gwei baseFee)
        let gross_2 = 0.8045;
        let raw_2 = U256::from(((gross_2 / native_usd) * 1e18) as u128);
        let b2 = compute_bid_with_profile(
            false,
            true,
            false,
            3,
            406_935,
            139_500_000,
            raw_2,
            gross_2,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
            profile,
        );
        assert!(b2.wallet_affordable);
        assert!(b2.bribe_bps >= 9_850, "got bribe_bps {}", b2.bribe_bps);
        let tip2_usd = (b2.tip_wei as f64 / 1e18) * native_usd;
        assert!(
            tip2_usd >= 0.61,
            "expected ETH 3-hop tip >= $0.61, got {tip2_usd:.4}"
        );
        assert!(b2.expected_net_usd >= min_profit_usd);

        // 3) Burned baseFee gets 0 builder credit: compact high-bribe route beats high-gas route
        let compact_usd = 0.48;
        let compact_raw = U256::from(((compact_usd / native_usd) * 1e18) as u128);
        let compact_bid = compute_bid_with_profile(
            false,
            true,
            false,
            2,
            300_000,
            140_000_000,
            compact_raw,
            compact_usd,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
            profile,
        );
        let bloated_usd = 0.71;
        let bloated_raw = U256::from(((bloated_usd / native_usd) * 1e18) as u128);
        let bloated_bid = compute_bid_with_profile(
            false,
            true,
            false,
            12,
            1_500_000,
            140_000_000,
            bloated_raw,
            bloated_usd,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
            profile,
        );
        assert!(
            compact_bid.effective_builder_wei > bloated_bid.effective_builder_wei,
            "compact ETH route (tip_wei={}, builder={}) must beat bloated high-gas route (tip_wei={}, builder={})",
            compact_bid.tip_wei,
            compact_bid.effective_builder_wei,
            bloated_bid.tip_wei,
            bloated_bid.effective_builder_wei
        );

        // 4) Micro-profit ETH route ($0.0002 surplus with min_profit_usd=0.0001) must submit
        let micro_gas_usd = (250_000f64 * 141_000_000f64 / 1e18) * native_usd;
        let micro_gross_usd = micro_gas_usd + 0.0002;
        let micro_raw = U256::from(((micro_gross_usd / native_usd) * 1e18) as u128);
        let micro_bid = compute_bid_with_profile(
            false,
            true,
            false,
            2,
            250_000,
            140_000_000,
            micro_raw,
            micro_gross_usd,
            native_usd,
            native_usd,
            18,
            min_profit_usd,
            wallet_bal,
            profile,
        );
        assert!(
            micro_bid.wallet_affordable,
            "micro-profit $0.0002 ETH route must be affordable at min_profit_usd=$0.0001"
        );
        assert!(micro_bid.tip_wei > 0);
        assert!(micro_bid.expected_net_usd >= min_profit_usd);
    }
}
