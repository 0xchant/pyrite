# Fool's Gold: 27 Hard-Won Mainnet Lessons from Building a Rust + CUDA + MEV-Share Arbitrage Engine

> **Pyrite** is named after Fool's Gold: a shiny mineral that looks like easy gold on paper until you run real capital against adversarial mainnet orderflow.

Building a multi-chain atomic arbitrage engine in **Rust + Solidity + CUDA** is the easy 30%. Finding out **why profitable-looking simulations burn gas or never land on-chain** is the other 70%.

Below is an unfiltered engineering & economic postmortem across **Ethereum Mainnet**, **BNB Chain (BSC)**, **Robinhood Chain (105ms Arbitrum Orbit)**, and **CUDA CFMM GPU solving** — every bug and number observed directly in production.

> One-sentence core lesson: **On a zero-sum MEV battlefield, carrying over default assumptions from one chain to another will almost certainly tank your inclusion rate silently; and on a small-capital account, every redundant mainnet contract redeployment burns days or even months of net arbitrage profit.**

---

## 1. The Brutal Mainnet Truth: Our Code Worked, and Still Landed 0 on ETH

After fixing our EIP-1559 bidding math, deploying a compact V2 contract (`DagArbExecutor`), upgrading to dedicated RPCs, wiring 9 MEV-Boost builders, and streaming **Flashbots MEV-Share SSE**:

- **Live ETH telemetry**: `~700–1,000` SSE events/min (`0` parse errors), **`100%` of `mev_sendBundle` requests accepted** by `relay.flashbots.net` in initial validation (including a `~$1.6` gross / `~$1.5` bribe backrun), plus block-head bundles, with **`$0` revert gas lost**.
- **Yet landed fills stayed `0`**. Why?
  1. **2-hop Huff / Yul assembly + pre-funded inventory**: Top ETH atomic bots skip flash loans completely and swap out of contract balances in **`110k–140k gas`**. A general multi-protocol flash-loan DAG uses `220k–300k+ gas`. When builders run `99%+` margin auctions, saving `$0.02–$0.05` in gas wins every tie.
  2. **CEX–DEX & Private PropAMM Orderflow**: Most chunky mispricings on ETH (and Metric PropAMM oracle updates, which empirically land with `0–0.001 gwei` priority fee directly via builders rather than the public mempool) never touch public SSE hints.
  3. **Public MEV-Share is a maximum-competition auction**: Every searcher sees the exact same hint simultaneously and pays almost the entire surplus in user refund + builder bribe.

---

## 2. Six ETH EIP-1559 Bidding Bugs That Silently Destroyed Inclusion

Before fixing these in [02-eth-eip1559-bidding-and-builders.md](./02-eth-eip1559-bidding-and-builders.md), our solver found real spreads (`$0.80–$0.95` gross) that never had a chance:

1. **Using BSC's receipt `p25` gas price on ETH** (~`2.0 gwei` instead of actual `0.13 gwei` EIP-1559 `nextBaseFee`), inflating imputed gas cost 15x and starving the `block.coinbase` bribe.
2. **Checking `exec_gp` instead of `block.baseFee`**: EIP-1559 burns `baseFee`; only `coinbase_bribe + priority_fee` pays the builder. Applying BSC's `+12%` `exec_gp` replacement bump on ETH *burned more baseFee* or priced us out cleanly.
3. **Including burned `baseFee` in `effective_builder_wei`** caused a bloated `1.5M gas` DAG (`$0.71` gross) to falsely outrank a crisp `300k gas` route (`$0.48` gross) in our own local ranking.
4. **BSC's `>= 6 hop` bribe cap (`44%–76%`)** leaked unchanged into ETH builders' `98%+` auctions, losing every multi-hop race.
5. **Single-EOA cross-block lockout**: Waiting on `bundle_span_blocks = 2` (`24s`) blocked the next block's bundle unless `tip > 1.12x` previous block's bundle.
6. **Inclusive expiry check (`bn <= pb.max_block`) + optimistic `cached_nonce` reset** locked fresh nonces on missed targets.

---

## 3. Solidity V2 Compact Binary & Contract Pitfalls

Detailed in [01-solidity-gas-and-contracts.md](./01-solidity-gas-and-contracts.md):

- **Profit-retention cross-route leakage**: Retaining profits inside the executor saved owner-transfer gas, but V1 computed profit via `balanceAfter - balanceBefore` or leaked residual token dust across consecutive routes; V2 isolates every call strictly with signed per-tx `delta[]` accounting and zero-unwrap native bribes.
- **V2 redeployments eat small-wallet profits**: Three BSC contract redeployments cost `~$4` — more than an entire day of thin atomic profit. Now we treat `DagArbExecutor` as immutable unless mathematically necessary.
- **16-token compact header limit & inline V2 flash collision**: Compact bit-packed routes support up to `16` unique tokens (`transient slot 32..47`); routing both sides of an inline UniV2 flash swap through token-hopping collides in the callback if unguarded.

---

## 4. BSC BEP-322 & Mempool Backrun Traps

Detailed in [03-bsc-live-and-mempool-backrun.md](./03-bsc-live-and-mempool-backrun.md) and [04-dex-protocols-and-graph-routing.md](./04-dex-protocols-and-graph-routing.md):

- **Shared-backend `429` & regional builder drops**: Firing identical bundles to 3 BlockRazor regional endpoints from one IP triggers `429`; counting a single regional ack as quorum misses blocks.
- **Phantom mempool cycles & slippage self-reverts**: Decoding a pending router swap and backrunning across `6+` hops fails on-chain when intermediate liquidity is thin — ERC-20 base backruns need strict slippage haircuts and phantom-cycle checks against `pre_best_usd`.
- **UniV3 parked-tick liquidity spoofing & Metric intra-block oracle updates**: Thin liquidity parked one tick away inflates `usable_depth` unless bounded; Metric PropAMM oracle updates invalidate pre-block reserve snapshots mid-block.

---

## 5. CUDA Solver & L2/Alt-Chain Forensics (Don't Chase Hype)

Detailed in [05-gpu-cuda-and-multichain-reality.md](./05-gpu-cuda-and-multichain-reality.md):

- **Chain economics beat micro-optimizations**: Before adding a new chain, measure **RTT vs block time**, **reverts**, **real DEX volume**, and **true winner PnL**.
  - On **Robinhood Chain (`chainId=4663`, Arbitrum Orbit)**, `105ms` blocks are 100% pure FCFS (`priorityFee = 0`), public RPC RTT was `1,205ms`, and our naive backtest initially flashed `$53k/day` before raw `Transfer` log inspection revealed false positives (`xAERO` gauge harvest + `GIGA-POS` LP rebalance) — real weekend chain-wide atomic profit was only **`~$32/day`** (`$0.01` median per arb, wiped out by a single `$0.006–$0.020` revert).
  - On **Unichain**, live DefiLlama 24h DEX volume was `$19.5M` with virtually zero cross-DEX fragmentation (`avg 1.0 tx/block`) and `492ms` public RPC RTT.
  - On **HyperEVM (`chainId=999`)**, pure AMM DEX volume averaged `$80M+/day` across 7–8 fragmented DEXs (`~$750M+/7d`) at **`20ms` RPC RTT** (`0.95s` blocks), though still lacking builder-level revert protection.

---

## Deep-Dive Pitfall Chronicles

| Doc | Topic | Pitfalls | Key Symptoms & Real Costs | Link |
| :---: | :--- | :---: | :--- | :--- |
| 01 | Solidity Compact Binary, Calldata & Gas | 6 | Cross-route balance leakage draining profits as bribes, frequent V2 redeployments burning gas, 16-token limit, V2 token-hop flash callback collision | [01-solidity-gas-and-contracts.md](./01-solidity-gas-and-contracts.md) |
| 02 | ETH EIP-1559 Bidding, Builders & Lockout | 6 | Inflated `p25` gas stealing bribes, `+12%` bump lowering ETH bribes, burned `baseFee` counted as builder revenue, 24s cross-block lockout | [02-eth-eip1559-bidding-and-builders.md](./02-eth-eip1559-bidding-and-builders.md) |
| 03 | BSC BEP-322 & Mempool Backrun Engineering | 5 | 48Club `429` throttling, phantom cycle inflation, missed Parlia validator turns, single-EOA blocking & multi-hop slippage self-reverts | [03-bsc-live-and-mempool-backrun.md](./03-bsc-live-and-mempool-backrun.md) |
| 04 | DEX Math (UniV3/V4, Balancer V2, Metric) & Routing | 5 | V3 parked-tick spoofing, Metric intra-block oracle front-runs, Morpho revert bypassing quarantine, singleton vault collisions | [04-dex-protocols-and-graph-routing.md](./04-dex-protocols-and-graph-routing.md) |
| 05 | CUDA Kernel Bugs, RPC Failover & L2 FCFS Forensics | 5 | CUDA Stage-2B negative index bug, Alchemy `429` cascade, `tmux` prefix collision killing dashboard, 105ms FCFS false-positive backtest trap | [05-gpu-cuda-and-multichain-reality.md](./05-gpu-cuda-and-multichain-reality.md) |

## Related Architecture & Config Docs

- **Algorithm & Architecture Audit**: [docs/architecture.md](../architecture.md)
- **CUDA CFMM GPU Solver Guide**: [cuda/README.md](../../cuda/README.md)
- **Chain Config Templates**: [chains/eth.yaml](../../chains/eth.yaml), [chains/bsc.yaml](../../chains/bsc.yaml), [chains/base.yaml](../../chains/base.yaml)
