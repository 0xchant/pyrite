# 01. Solidity Contracts, Compact Encoding, and Gas Pitfalls

Core files involved:
- [contracts/src/DagArbExecutor.sol](../../contracts/src/DagArbExecutor.sol)
- [src/router/mod.rs](../../src/router/mod.rs)
- [src/simulator.rs](../../src/simulator.rs)
- [src/executor.rs](../../src/executor.rs)

---

## Pitfall 1: Avoid Frequent Contract Redeployments on Small-Capital Live Accounts — Deployment Gas Is Costlier Than You Think

- **Scenario**: During rapid iteration, multiple upgraded contract iterations (V1, V2, etc.) were deployed on BSC and ETH mainnets.
- **Lesson**: When net profit per small-capital atomic arbitrage trade is `$0.001 ~ $0.05`, each `~3.3M gas` mainnet deployment costs `~$0.84` on ETH at `0.15 gwei` and can add up to several dollars on BSC — **a single unnecessary redeployment wipes out days or even weeks of live net profit**.
- **Engineering Discipline**:
  1. Exhaust local and simulated validation before deploying: Foundry unit tests + live fork tests + offline `eth_call` bytecode `stateOverrides` virtual-address simulation (`VIRTUAL_EXECUTOR_ADDR`).
  2. Solve everything feasible off-chain in Rust (bidding math, slippage haircuts, candidate ranking, RPC failover, pool/token blacklisting) instead of trial-and-erroring on-chain.
  3. Design contract binary layouts and protocol versions cleanly upfront so minor encoding tweaks never force a full redeployment.

---

## Pitfall 2: Cross-Route Balance Leakage When Retaining Profits in the Executor

- **Symptom**: To save per-trade ERC-20 `safeTransfer` gas to `owner` (`payoutToOwner = false`), V1 computed profit across the flash loan callback via `IERC20(baseToken).balanceOf(address(this))`. However, whenever a prior trade left residual profit in token X inside the contract, any later route touching token X swept that existing contract balance into the active swap step, inflating `netProfit` and leaking historical profits straight into builder bribes.
- **V2 Fix** ([contracts/src/DagArbExecutor.sol](../../contracts/src/DagArbExecutor.sol#L321-L342)):
  - Maintain a **strictly per-tx isolated memory ledger** `uint256[16] memory delta` inside `_execPlan`.
  - Every intermediate swap step debits `delta[inIdx]` and credits actual net `delta[outIdx]` rather than sweeping `balanceOf(address(this))`.
  - When paying a `wrappedNative` bribe and the contract already holds native gas reserve (`address(this).balance >= bribeWei`), take the **zero-unwrap fast path** (skipping `WETH.withdraw`, and falling back to unwrapping exactly `bribeWei` only when native balance is insufficient).

---

## Pitfall 3: Hard `16-token` Compact Encoding Cap vs `MAX_DAG_STEPS = 32`

- **Root Cause**: To minimize calldata gas (4 gas per zero byte, 16 gas per non-zero byte) and stack/transient overhead, V2 compact encoding packs each step's `(inIdx, outIdx)` into a single byte `(inIdx << 4) | outIdx` (4 bits + 4 bits), backed by a fixed stack array `uint256[16] memory delta` and EIP-1153 transient slots `TSLOT_DELTA_BASE = 32..47` ([contracts/src/DagArbExecutor.sol](../../contracts/src/DagArbExecutor.sol#L37-L38), [src/router/mod.rs](../../src/router/mod.rs#L383-L395)).
- **Impact**: While 99% of standard paths use only 2–8 tokens, deep multi-branch merge routes approaching `MAX_DAG_STEPS = 32` can exceed 16 distinct tokens, causing `encode_compact_arb` to reject those candidates.
- **Design Tradeoff**: When designing compact binary formats from scratch, align the token-index width with `MAX_DAG_STEPS` (e.g., 1 byte per index supporting up to 256 tokens): the extra `1 byte` per step adds just `16 gas/step` in calldata, far cheaper than redeploying a live contract later.

---

## Pitfall 4: Inline `FLASH_V2_SWAP` Colliding with V2 Token-Hopping

- **Symptom**: Certain routes stacked two gas optimizations simultaneously: using `step[0]` as a zero-overhead inline UniV2 flash swap (`Pair.swap(..., data)`, `FLASH_V2_SWAP`) while also enabling token-hopping (`step[0].recipient = steps[1].pool`, `inputInPool = true`) to skip an ERC-20 transfer.
- **Root Cause**: Uniswap V2 `pair.swap(a0, a1, to, data)` invokes the `uniswapV2Call` callback on the **`to` address**! Setting `to = steps[1].pool` dispatches the flash-loan callback to a DEX pool contract that does not implement `uniswapV2Call`, reverting immediately.
- **Fix** ([src/router/mod.rs](../../src/router/mod.rs#L294-L303)):
  - Whenever `steps[0].recipient != executor_addr`, token-hopping touches `step[0]`, or the pool uses a non-callback V2 fork (such as Mdex/BabySwap/Generic), fall back to Morpho Blue, Balancer V2, or an external UniV3/PancakeV3 flash loan instead of inline `FLASH_V2_SWAP`.

---

## Pitfall 5: `eth_call` Simulation Gas Distortion Skewing Builder `surplus` Bids

- **Problems**:
  1. Early constant estimates for calldata overhead and bribe transfer gas drifted from actual on-chain usage by thousands to tens of thousands of gas.
  2. Simulating with `bribeBps == 0` never executed `WETH.withdraw` or `coinbase.call{value: ...}("")`, undercounting `gas_used` in `eth_call` and turning razor-thin trades into marginal losses once bribes were enabled live.
- **Fix** ([src/simulator.rs](../../src/simulator.rs#L417-L423), [src/simulator.rs](../../src/simulator.rs#L503-L540)):
  - Force `bribeRecipient = caller, bribeBps = 1` on `wrappedNative` routes during `eth_call` simulation so the EVM executes the full `withdraw + call` bribe branch and captures true `inner_gas_used`.
  - Compute exact byte-level calldata overhead off-chain via `evm_tx_overhead_gas(&calldata)` (`21,000 + 4*zero + 16*nonzero`).

---

## Pitfall 6: Zero Error Strings in Production V2 + Companion `DagArbDiagExecutor`

- **Background**: Solidity `require(..., "long string")` and verbose custom errors bloat deployed bytecode and runtime gas. Yet stripping everything to bare `revert()` (`empty(0x)`) blinds simulations — making it impossible to distinguish fee-on-transfer tax tokens, stale reserves, or 1-wei shortfalls.
- **Dual-Contract Architecture** ([contracts/src/DagArbExecutor.sol](../../contracts/src/DagArbExecutor.sol), [src/simulator.rs](../../src/simulator.rs#L431-L479)):
  - **Minimal Mainnet Contract** `DagArbExecutor` (Solady `SafeTransferLib`, compact bytecode **`15,034 B`**).
  - **Offline Diagnostic Companion** `DagArbDiagExecutor`: swapped transiently onto `exec_addr` via `eth_call` `stateOverrides` only when a candidate fails within diagnostic budget, decoding structured errors such as `DiagFlashRepayShortfall` (including measured `maxTaxPpm`), `DiagStepZeroReceived`, and `DiagStepSwapReverted` — feeding the tax-token cache and pool quarantine with **zero mainnet gas overhead and X-ray offline diagnostics**.
