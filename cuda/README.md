# CUDA CFMM Convex Arbitrage Solver (`libcfmm_gpu.so`)

Optional GPU-accelerated multi-protocol CFMM arbitrage solver loaded dynamically at runtime via `libloading` (`src/router/gpu.rs`).
Rust builds (`cargo build`, `cargo check`, `cargo test`) require **no CUDA toolkit or NVIDIA driver**; when `libcfmm_gpu.so` is absent or `SOLVER_MODE=cpu`, execution stays purely on the CPU router.

## Building on a CUDA Host

Specify `CUDA_ARCH` explicitly for your NVIDIA GPU architecture (e.g., `sm_80` for A100, `sm_89` for Ada L4 / RTX 4090, `sm_90` for H100, or `native` for host auto-detection):

```bash
make -C cuda CUDA_ARCH=sm_89
```

Building without `CUDA_ARCH` fails intentionally so you target the right compute capability. This produces `cuda/libcfmm_gpu.so` (ignored by git via `cuda/*.so`).

## Runtime Configuration

| Variable | Default | Description |
| --- | --- | --- |
| `SOLVER_MODE` | `cpu` | `cpu` (default), `gpu` (GPU primary with auto CPU fallback if unavailable), or `hybrid` (CPU fast-path + GPU deep solver) |
| `CFMM_GPU_LIB` | auto-search | Explicit path to `libcfmm_gpu.so`; falls back to `./cuda/libcfmm_gpu.so`, `./libcfmm_gpu.so`, `<exe_dir>/cuda/libcfmm_gpu.so`, `<exe_dir>/libcfmm_gpu.so` |
| `GPU_BUDGET_MS` | `35` (fast chain) / `120` (slow chain) | Per-anchor GPU solve wall-clock budget in milliseconds |
| `GPU_MAX_GAS_LIMIT` | `3200000` | Maximum atomic transaction gas cap enforced inside GPU DAG pruning (`>= 250000`) |
| `GPU_ENABLE_LAYER2` | `1` | Enable GPU Stage-2 continuous DAG pruning & parallel basin polish (`1`/`true` or `0`/`false`) |
| `GPU_BASE_TX_GAS` | `65000` | Base transaction gas passed to GPU pruning (`21000..=500000`) |
| `GPU_GAS_SCALE` | `1.0` | Multiplier applied to per-pool swap gas units during GPU batch encoding (`0.25..=4.0`) |

## Decoupling & Protocol Capability Whitelist

- Supported GPU pool models: `UniV2`, `Solidly` (volatile & stable), `UniV3`, `PancakeV3`, `InfinityCl`, `UniV4`, `MetricOmm`, `BalancerV2` (`Weighted` and 2-token unscaled `Stable`).
- Any unsupported or future `DexKind` variant (or 3+-token / rate-scaled Balancer pools) is skipped cleanly during GPU serialization and handled by CPU without breaking CUDA builds.
- V2 compact binary `DagArbExecutor` constraints (`max_steps_cap <= 32`): the GPU solver and `gpu_steps_to_candidate` enforce `<= 32` steps, `<= 16` unique tokens per route (transient slot range `32..47`), and `uint128` raw bounds for `borrowAmount` and `minNetProfit`.
- All GPU candidates pass through Rust's exact `U256` `verify_and_refine_candidate` replay, flash-source selection, and `eth_call` / REVM simulator pipeline before execution.

## Benchmarks (CPU vs CUDA)

Reproduce the parity checks and synthetic stress suite on any CUDA host using only the Python 3 standard library:

```bash
make -C cuda CUDA_ARCH=sm_89
python3 cuda/bench.py
```

Useful flags: `--self-test-only` (runs only the 6 `fp64` parity tests), `--quick` (smaller subset), `--lib /path/to/libcfmm_gpu.so`, and `--json /tmp/bench.json`.

**Reference hardware and verification:** Recorded on an NVIDIA DGX Spark (`NVIDIA GB10`, CC `12.1`, `48` SMs, Unified Memory) running the exact `cuda/cfmm_solver.cu` kernel in this repository compiled with `-O3` strict IEEE-754 `fp64` (no `--use_fast_math`, `0` bytes register spills). Every solver output is checked sequentially by a Python causal replay verifier (`max_deficit <= 1e-9`, `rel_err <= 1e-4`, exact step gas sum).

### 1. Protocol parity and gas-pruning self-tests (`6/6 PASS`)

| Test | What it verifies | Measured result |
| :--- | :--- | :--- |
| UniV2 2-pool optimum | Closed-form analytical solution match | GPU `$387.7487` vs Exact `$387.7487` (`rel_err = 6.10e-12`) |
| Solidly Stable (`x^3y+xy^3=k`) | Exact curve quote and cycle net profit | Quote `rel_err < 1e-12`, cycle net `$11.5694` |
| Balancer V2 Weighted | `80/20` and `50/50` pool quote and arbitrage | Quote `rel_err < 1e-12`, cycle net `$22.6521` |
| MetricOmm PropAMM | Spread/inventory skew and `80%` cap check | `q_zero = 547.8182 > q_skew = 547.6867`, net `0.0075` base |
| 32-tick CLMM | Cross-tick segment traversal and replay | Crossed `12` ticks, net `$751.42` (`rel_err = 6.05e-16`) |
| Hard gas cap pruning | `105`-pool market (`45` dust pools) under `500k` gas | Greedy `$2,767.15`, GPU `500k` cap `$3,420.23` (`487,000` gas), unlimited `$17,122.47` |

### 2. 7-protocol synthetic stress suite (`300` to `5,000` pools)

Markets mix `UniV2`, multi-tick CLMM (`UniV3`/`UniV4`, `6-16` ticks), `Curve Stable`, `Solidly Stable`, `Balancer V2 Weighted`, `MetricOmm`, `PropBins`, and low-TVL noisy dust pools:

| Pools (Tokens) | CPU Greedy (`2-3` hop) | CPU Dual (`32` restarts) | CUDA `@50ms` budget | CUDA `@500ms` budget | L1 restart speedup |
| ---: | ---: | ---: | ---: | ---: | ---: |
| `300` (`12`) | `$865.11` (`4.3ms`, 3 steps) | `$4,285.15` (`23.8ms`) | `$4,789.70` (`24.7ms`, 59 steps) | `$5,189.27` (`56.4ms`, 53 steps) | `7.7x` |
| `1,000` (`18`) | `$6,803.66` (`17.0ms`, 3 steps) | `$25,386.82` (`92.2ms`) | `$32,589.63` (`52.1ms`, 19 steps) | `$32,620.88` (`144.3ms`, 19 steps) | `9.5x` |
| `2,500` (`24`) | `$5,191.95` (`40.8ms`, 3 steps) | `$28,894.37` (`200.0ms`) | `$51,163.47` (`93.4ms`, 176 steps) | `$49,238.51` (`221.4ms`, 178 steps) | `12.6x` |
| `5,000` (`32`) | `$9,978.72` (`77.4ms`, 3 steps) | `$54,153.81` (`442.3ms`) | `$90,853.43` (`115.6ms`, 103 steps) | `$74,374.49` (`345.5ms`, 119 steps) | `15.1x` |

Three behaviors matter in practice:

1. **Scale crossover**: At `5,000` pools, CUDA (`@50ms`) finishes in `115.6ms` (`3.8x` faster than single-core CPU Dual at `442.3ms`) while capturing `1.68x` higher net profit (`$90,853.43` vs `$54,153.81`) and `9.1x` more than CPU Greedy (`$9,978.72`).
2. **Stage-2 pruning necessity**: Pure Layer-1 continuous convex relaxation splits tiny dust flows across `2,068` edges on `5,000` pools, yielding `-$1.62M` net profit after fixed per-swap gas. GPU Stage-2 discrete pruning and basin polish remove unprofitable dust legs and recover verified positive net profit (`max_deficit = 0.0`).
3. **Budget non-monotonicity**: Larger budgets (`@500ms` vs `@50ms`) explore more Layer-1 iterations (`-$1.6217M` vs `-$1.6138M` unpruned) that can settle into different discrete Stage-2 basins under non-convex per-swap gas costs, so higher time budgets do not strictly dominate shorter ones on every seed.

### 3. Open-book mainnet snapshots (quiet blocks vs `35 bps` PropAMM shock)

| Snapshot case | Pools | Protocol mix | CPU Greedy | CUDA solver | Difference |
| :--- | ---: | :--- | :--- | :--- | :--- |
| Quiet mainnet blocks | `105-149` | `UniV2`, `UniV3`, `UniV4`, `BalancerV2`, `MetricOmm`, `InfinityCL`, `Solidly` | `$0.000` or `$0.123` (`0.03-0.12ms`) | `$0.000` or `$0.123` (`18-32ms`) | CPU fast-path wins on latency |
| ETH + `35 bps` PropAMM shock | `128` | Multi-tick `V3`, `BalancerV2`, `UniV4` | `$53.13` (`0.11ms`, 2 steps) | `$60.68` (`35.52ms`, 10 steps) | `+14.2%` verified net |
| BSC + `35 bps` PropAMM shock | `150` | Multi-tick CLMM, `MetricOmm`, `InfinityCL`, `Solidly`, `UniV4` | `$17.48` (`0.10ms`, 3 steps) | `$28.30` (`39.86ms`, 12 steps) | `+61.9%` verified net |

### 4. Live Ethereum Mainnet `SOLVER_MODE=hybrid` run (`~2.6` hours, `eth_call` verified)

In a continuous `~2.6`-hour Ethereum Mainnet run on DGX Spark with CPU (`DAG`, `Cyclic`, `ParallelSplit`) and CUDA (`GPU_CFMM`) running side by side across `~400` logged opportunities and `~220` `eth_call` verified candidates, `53` distinct blocks produced positive verified net profit:

| Metric | Value | Notes |
| :--- | :--- | :--- |
| Blocks found by CPU routers only | `18` | Sub-millisecond 2-4 hop cycles and tail paths |
| Blocks found by `GPU_CFMM` only | `19` | Missed by all three CPU routers |
| Overlapping blocks (both positive) | `16` | GPU beat CPU on `12 / 16` (`75%`); CPU beat GPU on `4 / 16` |
| Sum of best verified net (CPU only) | `~$10.6` | `18 + 16 = 34` active blocks |
| Sum of best verified net (GPU only) | `~$6.8` | `19 + 16 = 35` active blocks |
| Sum of best verified net (`hybrid`) | `~$13.5` | `+27%` vs CPU-only, `+98%` vs GPU-only |

CPU routers stay essential for sub-millisecond 2-3 hop paths and calm blocks; CUDA finds multi-protocol splits the discrete heuristics miss. Running `SOLVER_MODE=hybrid` captures both sets.

### 5. Resident Unified Memory dirty sync and ETH gas-limit mega-packing

Resident incremental updates (`cfmm_update_dirty_pools` updating `50` mutated pools) and synthetic ETH block-cap packing (`5,000` pools, `128` tokens, `base_tx_gas = 65,000`):

| Pool count (`dirty`) | UM micro-sync | `cfmm_update_dirty_pools` | Resident `@50ms` / `@100ms` | Verified net |
| ---: | ---: | ---: | ---: | ---: |
| `50` (`10`) | `0.2092 ms` | - | - | `$0.00` |
| `500` (`50`) | `0.5744 ms` | `0.0006 ms` | `36.55 ms` / `64.59 ms` | `$7,559.05` |
| `2,000` (`50`) | `1.8227 ms` | `0.0006 ms` | `59.30 ms` / `108.65 ms` | `$98,317.14` |
| `5,000` (`50`) | `4.3477 ms` | `0.0006 ms` | `100.10 ms` / `187.57 ms` | `$178,294.80` |

| ETH gas regime | Gas limit | Gas used (`%` cap) | Steps | Verified net | Solve time | Max deficit |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| `2M_TxCap` | `2,000,000` | `1,817,000` (`90.8%`) | `18` | `$43,698.28` | `308.2 ms` | `0.0` |
| `16.5M_EIP7825` | `16,500,000` | `4,642,000` (`28.1%`) | `45` | `$54,754.72` | `322.6 ms` | `0.0` |
| `30M_BlockCap` | `30,000,000` | `4,642,000` (`15.5%`) | `45` | `$54,754.72` | `322.8 ms` | `0.0` |
| `Unlimited` | Unlimited | `4,642,000` (`-`) | `45` | `$54,754.72` | `323.7 ms` | `0.0` |
