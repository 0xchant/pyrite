# 05. GPU CUDA Solver, RPC Ops, and L2 FCFS Forensic Pitfalls

Core files involved:
- [src/router/gpu.rs](../../src/router/gpu.rs)
- [cuda/cfmm_solver.cu](../../cuda/cfmm_solver.cu)
- [cuda/README.md](../../cuda/README.md)
- [src/rpc.rs](../../src/rpc.rs)
- [src/main.rs](../../src/main.rs)

---

## Pitfall 1: Negative Array Index Bounds Bug in CUDA Stage-2B Basin Polish

- **Bug**: In `cuda/cfmm_solver.cu` Stage-2B basin polish, when the incumbent best route was a `2-hop` path and `pb < half_p`, the edge-addition slot computed `(pb - half_p) >> 2`, indexing negatively out of bounds.
- **Fix**: Dedicate the first 4 lanes (`pb < 4`) explicitly to exact seed-mask perturbations across 4 learning-rate buckets, restrict edge-removal branches to `active_c.size() >= 3`, and clamp addition indices via `add_idx = (pb >= half_p) ? (pb - half_p) : (pb - 4)` with strict `n_cands > 0/1` guards.

---

## Pitfall 2: GPU Router Limits Diverging from On-Chain V2 Compact Executor Caps

- **Symptom**: Early GPU passes allowed up to 256 pool indices in the graph without enforcing per-candidate compact binary limits, causing valid-looking GPU solutions to fail late inside Rust `encode_compact_arb`.
- **Fix** ([src/router/gpu.rs](../../src/router/gpu.rs#L25-L27)): Enforce `MAX_COMPACT_TOKENS = 16`, `MAX_DAG_STEPS = 32`, and `u128` amount bounds upfront inside both GPU serialization and `gpu_steps_to_candidate`, while exposing `GPU_GAS_SCALE` / `GPU_BASE_TX_GAS` for per-chain calibration. Dynamic loading via `libloading` (`libcfmm_gpu.so`) ensures servers without CUDA compile and degrade cleanly to CPU routing.

---

## Pitfall 3: Dedicated Paid RPCs Diluted by `rr_idx % primary` Round-Robin & Cross-Chain Quota Exhaustion

- **Symptom 1 (Latency Dilution)**: Placing a fast paid Alchemy node at `RPC_URLS[0]` still routed 2/3 of `RpcPool::call_with_failover` and `eth_call` calls onto trailing free public nodes (`publicnode` / `drpc`) via `rr_idx % 3`, dragging single-block latency from `118–240ms` out to `400–2500ms`.
- **Fix**: Add `has_dedicated_primary()` so URLs carrying dedicated API paths (such as `/v2/`) receive 100% of primary read traffic, failing over to trailing free RPCs only on timeout or error.
- **Symptom 2 (Shared Key Cross-Chain 429 Cascade)**: Reusing the same Alchemy API key to probe a 10-block/sec Arbitrum Orbit chain exhausted monthly/second compute unit (CU) limits in seconds (`429 Too Many Requests`), starving mainnet L1 processes sharing that key.

---

## Pitfall 4: Cross-Chain `.env` `EXECUTOR_ADDRESS` Clobbering & `tmux kill-session` Prefix Accident

- **Ops Bug 1 (Clobbering BSC Executor Address)**: Running `deploy --chain eth` previously wrote `EXECUTOR_ADDRESS_ETH` *and* unconditionally overwrote legacy `EXECUTOR_ADDRESS` in `.env`, silently wiping the production BSC contract address on shared hosts. **Fix**: Only update legacy `EXECUTOR_ADDRESS` when `cfg.chain == "bsc"`.
- **Ops Bug 2 (`tmux` Prefix Match Killing Dashboard)**: Running `tmux kill-session -t pyrite` to restart the searcher after `pyrite` had already exited used `tmux`'s default prefix match and killed **`pyrite-dash`** instead! **Lesson**: Always use exact session targeting: `tmux kill-session -t "=pyrite"`.

---

## Pitfall 5: Arbitrum Orbit / Robinhood Chain (105ms FCFS) Reality & False-Positive Backtest Trap

- **Physics Trap**: Robinhood Chain Mainnet (`chainId = 4663`, Arbitrum Orbit) produces **`105ms` blocks** under **100% pure FCFS ordering** (`effectiveGasPrice == baseFee ≈ 0.02 gwei` — priority tips buy zero queue position) with no revert-protected private builders.
  - Public RPC `rpc.mainnet.chain.robinhood.com` measured `p50 = 1,205ms` (`eth_call = 1,237ms`, no WSS). Polling once every 1.2s against 0.105s blocks puts your bot >20 blocks behind reality — guaranteed revert burn.
  - Worse, unit economics leave zero margin for error: weekend chain-wide atomic profit median was only **`$0.010`** (`~$32/day` total chain-wide, `~$5–$7/day` for top bots), while a single late-arrival collision revert costs **`$0.006–$0.020`** — **anything below a ~60% win rate loses money**.
- **Backtest False-Positive Trap**:
  - An initial naive scan flagging every transaction with "zero negative token outflow and positive token inflow" across 2,000 blocks reported `~$53k/day` total chain profit, led by a `~$142 USDG` transaction.
  - **Inspecting raw `Transfer` logs refuted all top anomalies**:
    1. That `~$142 USDG + ~43 AI` transaction was an `xAERO` liquidity mining reward harvest / mint (`0x000000 -> pool`), not atomic arbitrage;
    2. A large `GIGA/TAO` hit was a `GIGA-POS` LP NFT rebalance (`ERC721` transfer);
    3. Filtering strictly for **complete borrow -> multi-pool swap -> full flash-loan repayment closed loops** shrank estimated chain-wide atomic profit by **1,600x** from `~$53k/day` down to **`~$32/day`**.
  - **Core Lesson**: Never size a chain opportunity by net receiver token balance deltas alone; always strip mints, staking rewards, LP rebalances, and unhedged inventory moves before spending money on dedicated nodes or deployments.
