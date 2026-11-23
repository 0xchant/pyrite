# 04. DEX Protocol Math, PropAMM, Singleton Vaults, and Graph Routing Pitfalls

Core files involved:
- [src/amm.rs](../../src/amm.rs)
- [src/discovery.rs](../../src/discovery.rs)
- [src/router/mod.rs](../../src/router/mod.rs)
- [src/router/dag.rs](../../src/router/dag.rs)

---

## Pitfall 1: V3 Parked-Tick Liquidity Spoofing & Toxic Pools Flooding Top Sim Slots

- **Symptom**: Apparent `$30 ~ $500` jackpots on illiquid long-tail tokens routinely traced back to broken pools with fake wide-tick depth, hidden transfer taxes, or whitelist/callback restrictions.
- **Two Failure Modes**:
  1. **Tick-Boundary Phantom Depth**: When a V3 pool's `sqrtPrice` sits directly on an initialized tick edge, single-tick off-chain quotes assume full active `L` depth, while live EVM swaps cross into empty ticks or revert immediately. We added side-specific known-depth validation and automatic `allow_multi_tick = false` degradation in `src/amm.rs` upon `StepSwapRevert` or non-tax `FlashShortfall`.
  2. **Morpho `transferFrom reverted` Escaping Quarantine**: With Morpho Blue flash loans, unprofitable routes fail inside `Morpho.flashLoan` repayment with a raw ERC-20 `transferFrom reverted`. Because the hot loop ran full `DagArbDiagExecutor` state-override diagnostics only on the top 2 failing candidates per block, candidates #3+ fell into generic `OTHER_REVERT`, bypassing `FLASH_SHORTFALL` cooldowns and clogging the `top_cands.take(8)` simulation queue on every 12-block full rescan.

---

## Pitfall 2: PropAMM (Metric OMM) `extsload` State Decoding & Intra-Block Oracle Front-Runs

- **Protocol Quirks**:
  - Metric pools do not strictly enforce `token0 < token1` address sorting (unsorted token ordering exists). Decoding requires targeted `extsload` slices for `sell_price_x64 / buy_price_x64 / active_bin / paused`, plus explicit `tokenIn == pool.token0` direction flags at swap time.
- **Live Competition Reality**:
  - Inspecting live Titan and Quasar blocks on Ethereum revealed that quoting against end-of-block-`N` Metric prices loses systematically: the market maker's own oracle updates land at the top of block `N+1` (`tx[1..2]`), repricing or capturing the spread atomically before public searchers (e.g., bundling directly alongside the oracle update for `~$4`).

---

## Pitfall 3: UniV4 / InfinityCL / BalancerV2 Singleton Architecture Pitfalls

- **Virtual Pool IDs vs Self-Collisions**:
  - UniV4 and BalancerV2 route every pool through one shared Singleton address (`PoolManager` / `Vault`). Discovery derives deterministic virtual node addresses via `keccak256(manager || poolId)` / `keccak256(vault || poolId || t0 || t1)` for graph indexing, then restores true singleton addresses and `extraData` (`PoolKey` / `poolId`) in `populate_singleton_steps`.
  - **BalancerV2 Flash Collision (`FLASH_BALANCER_V2`)**: Borrowing via `FLASH_BALANCER_V2` (`flash_mode = 7`) while simultaneously swapping on any pool hosted on the same Balancer V2 Vault trips internal Vault transient accounting / reentrancy guards. `execution_flash_source` therefore checks step pools and switches flash providers automatically when a collision exists.
  - **BalancerV2 Composable Stable BPT & Rate Scaling**: Composable Stable Pools embed their own BPT token inside `tokens[]` alongside non-18 `scalingFactors` and external `tokenRates` (`wstETH`, `rETH`). Ignoring `tokenRates` fabricates phantom spreads across yield-bearing pools.

---

## Pitfall 4: Graph Bloat Triggering Solver Timeouts -> 2-Core Pruning

- **Problem**: After running discovery for tens of minutes, the local graph grows to `3,500–10,000` pools, mostly dead one-sided `GenericV2` leaf pairs that bog down `bn % 12 == 0` full scans.
- **Fixes** ([src/router/mod.rs](../../src/router/mod.rs)):
  1. **2-Core Graph Pruning**: Strip degree-1 non-hub leaf tokens prior to graph search (they can never form closed arbitrage cycles mathematically, while retaining recently discovered pairs within a grace age so fresh hot launches are not pruned early).
  2. **8-Block Sliding `dirty` Window**: Expand BFS 3 hops around active non-hub pools touched within the last 8 blocks rather than strictly the single latest block.
  3. **Discovery Budget Guard**: Trigger `discover_unknowns` only when no live bundle was submitted in the current block and block elapsed time remains below `discovery_budget_ms`, ensuring pool crawling never delays the next block's quotes.

---

## Pitfall 5: Never Test High-Spread Opportunities at Only One Analytical Borrow Size

- **Finding**: On steep V3 spreads near tick boundaries, off-chain analytical sizing `borrow_human` suffers from single-tick / hybrid approximation error.
- **Fix**: For promising top candidates, fan out concurrent `eth_call` size probes across `[0.22x, 0.50x, 0.68x, 1.0x, 1.45x, 2.05x]` — and extend to `[4.5x, 11.0x, 24.0x]` when V3 saturation suggests deep multi-tick spread — guarded by a `16ms` straggler deadline. Selecting the winning simulated size routinely captures 2x–5x more gross profit than single-point sizing.
