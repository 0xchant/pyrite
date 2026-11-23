# 02. ETH EIP-1559 Bidding, Builder Coverage, and Lockout Pitfalls

Core files involved:
- [src/main.rs](../../src/main.rs)
- [src/rpc.rs](../../src/rpc.rs)
- [src/discovery.rs](../../src/discovery.rs)
- [chains/eth.yaml](../../chains/eth.yaml)

These bugs were exceptionally deceptive: **signatures, authorizations, and builder RPCs all returned valid `bundleHash` receipts with `0` system errors, yet dozens of consecutive submissions landed `0` blocks**.

---

## Pitfall 1: Treating Retail Tx `p25(effectiveGasPrice)` as `baseFeePerGas`, Quietly Starving Builder Bribes

- **Symptom**: Live ETH bundles repeatedly submitted routes with `$0.10–$0.21` gross surplus while offering only `$0.005–$0.014` in `coinbase` bribes.
- **Root Cause**: The ETH runtime inherited BSC's block gas sampling heuristic `gps[gps.len() / 4]` (25th percentile `effectiveGasPrice` of block txs). On Ethereum EIP-1559, normal user transactions attach `0.05–2.0 gwei` priority tips, whereas winning MEV bundles clear cleanly at `effectiveGasPrice == baseFeePerGas` (`priorityFee = 0`). Sampling `p25` overstated gas cost by **1.4x–2.0x** (e.g., deducing costs at `~0.20 gwei` when true `nextBaseFee` was `~0.14 gwei`), siphoning builder bribe budget into fake `base_gas_wei` and `minNetProfit`.
- **Fix**: Implement exact EIP-1559 next-block base fee calculation (`calc_next_eip1559_base_fee` using parent `baseFeePerGas`, `gasUsed`, `gasLimit`) on `newHeads` and block boundaries, and completely disable `p25` sampling under `TxType::Eip1559`.

---

## Pitfall 2: Inheriting BSC's `44%–76% tip_share` Caps & the Self-Sabotaging `+12% exec_gp` Bump

- **Symptom**: In one live ETH block, a 14-step WETH DAG had **`~$0.95`** gross profit with `--min-profit-usd 0.0001`, yet bid only `~$0.44` (`~76%` bribe), pocketing `~$0.14` in minimum floor instead of competing for inclusion.
- **Root Cause (Two Compounding BSC Legacies)**:
  1. `compute_bsc_bid` capped `tip_share` at `0.44–0.76` (`6500–7800 bps`) on `flow_len >= 6 / >= 10` routes to buffer BSC's on-chain revert exposure. Under ETH Flashbots bundles (**`$0` cost on non-inclusion**), withholding ~14 cents simply hands the slot to competitors.
  2. When consecutive blocks held active unlanded bundles, replacement logic scaled `exec_gp` by **+12%** (`~0.14 -> ~0.15 -> ~0.17 gwei`). Under `TxType::Eip1559`, `sign_tx` pins `maxPriorityFeePerGas` to `bundle_priority_fee_wei` (`0.001 gwei`) — raising `exec_gp` paid **zero extra wei to the builder** and only inflated `base_gas_wei` inside the off-chain pricer, **shrinking the `coinbase` bribe on every retry**!
- **Fix**: Introduce `BidChainProfile::EthEip1559Atomic`, unlocking `97.5%–99.4%` `tip_share` (`max_bps = 10_000`) across all hop counts and hub combinations, and restrict the `+12%` gas-price replacement bump exclusively to `TxType::Legacy`.

---

## Pitfall 3: Counting Burned EIP-1559 `baseFee` Toward `effective_builder_wei`

- **Root Cause**: `effective_builder_wei = tip_wei + (base_gas_wei * 9) / 10` modeled legacy BSC validator fee retention (90% of gas paid to validator). Under Ethereum EIP-1559, **`baseFee * gasUsed` is 100% burned** — the builder receives zero wei from `baseFee`.
- **Impact**: Whenever the same block produced both a bloated `1.5M gas` DAG (`$0.34` burned `baseFee`) and a lean `350k gas` cycle, local ranking awarded the fat route an imaginary `$0.30` builder bonus, causing the uncompetitive route to win internal selection.
- **Fix**: Under `EthEip1559Atomic`, compute `effective_builder_wei = tip_wei + eff_gas_used * priority_fee_wei`, stripping burned `baseFee` completely.

---

## Pitfall 4: Single-EOA Cross-Block `tip_beats_prev (1.12x)` Lockout & `cached_nonce` Rewind

- **Symptom**: After block `N` submitted a `$0.50` bribe bundle (`bundle_span_blocks = 2`), genuine `$0.20` opportunities arriving over the next **24–36 seconds** were silently dropped.
- **Root Cause**:
  1. `non_backrun_active` failed to scope `tip_beats_prev` (`tip > prev * 1.12`) to the submission block, forcing fresh block `N+1`/`N+2` bundles to outbid a dead `$0.50` bundle from block `N`.
  2. Using `bn <= pb.max_block` kept `[N+1..N+2]` bundles artificially alive into `N+3` after receipts already settled.
  3. When sibling bundles sharing `(signer, nonce)` had one land on-chain and another expire in the same block, the expiry handler blindly reset `cached_nonce = Some(pb.nonce)`, rewinding nonce state backward instead of advancing to `nonce + 1`.
- **Fix**: Under `FlashbotsRelay`, enforce `tip_beats_prev` only when `b.submitted_block == bn`, tighten expiry to `bn < pb.max_block`, and track `landed_signer_nonces` so expired sibling bundles never rewind a landed nonce.

---

## Pitfall 5: Hidden Floor Constants Silently Overriding `--min-profit-usd 0.0001`

- **Lesson**: Passing `--min-profit-usd 0.0001` via CLI still silently dropped every `$0.0001–$0.015` spread due to four hardcoded internal floors:
  1. Main router loop enforced `min_profit_usd.max(0.006)`.
  2. ETH bidding deducted fixed `drift_gas_wei = 6,000 * unit_gas` (`~$0.0013`) plus `min_tip_floor = 0.000001 ETH` (`~$0.0017`).
  3. 2-pool hub-to-hub filter required `sim.net_profit_usd >= 0.015`.
  4. Mempool backrun filters enforced `0.015` / `+0.03` minimum deltas.
- **Takeaway**: When exposing CLI or `pyrite.json` profitability thresholds, ensure downstream filters respect the configured threshold rather than scattering magic-number overrides across modules.

---

## Pitfall 6: ETH Builder Coverage Blind Spots & CEX–DEX Dominance

- **Mainnet Telemetry**:
  - Starting with 4 builders (Titan / BeaverBuild / Flashbots / Quasar) missed **23%** of blocks mined by Eureka and Bombora. Probing and wiring 5 additional builders within 60–75ms RTT (Buildernet / Eureka / Bombora / BobTheBuilder / BTCS) expanded coverage to 9 builders, yet unintegrated builders such as `builder.ultrasound.money` (e.g., winning an `~$8.5`-bribe memecoin arb) still create blind spots.
  - Top-of-block ETH opportunities (such as `UniV3 WETH/USDC`) are overwhelmingly captured in `tx[0..3]` by inventory-backed CEX–DEX market makers. Paying `98%–99.5%` bribes leaves pure flash-loan atomic bots acutely sensitive to colocation RTT, complete builder coverage, and sub-140k assembly gas efficiency.
