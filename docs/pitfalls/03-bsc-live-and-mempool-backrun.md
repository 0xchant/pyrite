# 03. BSC BEP-322, Parlia Block Production, and Mempool Backrun Pitfalls

Core files involved:
- [src/executor.rs](../../src/executor.rs)
- [src/rpc.rs](../../src/rpc.rs)
- [src/mempool.rs](../../src/mempool.rs)
- [src/main.rs](../../src/main.rs)

---

## Pitfall 1: Heterogeneous BEP-322 `bribeRecipient` Rules & `rpc.48.club` 429 Throttling

- **Root Cause**: Major BSC builders (BlockRazor / 48Club Puissant / NodeReal) enforce distinct `bribeRecipient` payment addresses, requiring builder-specific calldata signatures per submission while strictly maintaining the **exact same `nonce`** across all candidate signatures (`test_bsc_bundle_payment_eoas_and_nonce_consistency`).
- **Pitfalls**:
  1. Waiting for a slow regional endpoint inside one builder stalls the entire hot loop (`450ms` block times on BSC).
  2. Sending regular read RPC traffic or unthrottled requests to `rpc.48.club` triggers HTTP `429`, blocking bundle submission during critical slots.
- **Fix** ([src/rpc.rs](../../src/rpc.rs#L1072-L1104)):
  - Implement `backend_map`-deduplicated `wait_for_builders`: once `>= 2` distinct builder backends acknowledge and the grace window `grace_end` elapses, call `submissions.detach_all()` to return control to the main loop immediately while trailing HTTP tasks finish in the background.

---

## Pitfall 2: Parlia Validator Turn Rotation Dropping Short `+1..+2` Block Windows

- **Symptom**: Sending short `+1..+2` block expiry bundles (standard on 12s L1 chains) fails frequently on BSC's 450ms blocks whenever minor jitter occurs or consecutive block proposers belong to validators outside your connected builder set.
- **Fix** ([src/config.rs](../../src/config.rs#L333-L341)):
  - Derive multi-turn target spans dynamically: `bundle_span_blocks = 6` (~2.7 seconds, spanning multiple Parlia validator turns), extending uncontested stale/passive drift opportunities (`is_passive`) to `passive_bundle_span_blocks = 10` (~4.5 seconds).

---

## Pitfall 3: Single-EOA Serialization Bottleneck -> Multi-EOA Staggered Pool (`EoaPool`)

- **Problem**: Whenever the primary EOA submits a 6-block bundle at block `N`, it cannot emit fresh non-conflicting bundles on blocks `N+1..N+5` without overwriting the in-flight nonce.
- **Fix** ([src/executor.rs](../../src/executor.rs#L98-L332)):
  - Implement `EoaPool`: authorize multiple worker EOAs in executor slot 0 `callers` and schedule them staggered by `(busy_until_block, last_submitted_block)` with independent nonces.
  - Across the top 4 concurrent submission slots in a block, deduplicate `pool_set` and reserve diversity quotas for WBNB-denominated and short routes so all 4 workers never pile onto the exact same risky token pool.

---

## Pitfall 4: Phantom Cycles & `0.78x` Slippage Self-Reverts in Mempool Pending Backruns

- **Symptom 1 (Phantom Backrun Profit)**: Observed live when a small `$45–$150` public pending swap entered `shadow` state simulation and reported a **`~$10.3`** backrun profit. In reality, stale/unliquidated pools along that swap path already carried `~$10.3` of phantom spread prior to the victim swap, causing `solve_all(&shadow)` to falsely attribute pre-existing fake spread to the tiny victim trade.
- **Fix** ([src/main.rs](../../src/main.rs#L638-L660)):
  - Solve a pre-victim baseline `pre_best_usd` across the exact same `dirty` pools at the current block tip.
  - Require strictly positive incremental profit above baseline and enforce conservation sanity bounds: `lead.est_gross_profit_usd <= swap_usd * 0.45` (a small swap cannot legitimately conjure arbitrage surplus exceeding half its own notional without pre-existing distortion).
- **Symptom 2 (ERC-20 Base Backruns Reverting on `MIN_PROFIT`)**: On BSC, a BUSD shadow quote estimated `~$2.57` while real EVM execution yielded `~$2.53` — pinning on-chain `minNetProfit` to the unverified float reverted on-chain and burned gas.
- **Fix** (`compute_backrun_bid_with_profile`): apply a **0.78x slippage haircut** on unverified non-wrapped-native shadow backruns so minor decimal/quote drift never causes `MIN_PROFIT` self-reverts.

---

## Pitfall 5: On-Chain Gas Drift & Over-Bribing Uncontested Long DAGs on BSC

- **Lessons**:
  1. Between `eth_call` simulation and live BSC inclusion, state drift can add `+12k ~ +38k` extra gas (`~38k` extra gas observed live). Bidding 99% straight against `sim_gas` loses money on thin trades upon inclusion. BSC therefore reserves a `38_000 * gp` `drift_gas_wei` buffer on thin-margin routes.
  2. On uncontested 6–15 step long-tail DAGs, applying the `98.5%` bribe curve needed for 2-hop races unnecessarily gives away uncontested surplus. BSC preserves tiered `tip_share` scaling by `eff_flow_len` while bidding maximally (`98.5%–99.65%`) on contested 2–4 hop and dual-hub lanes.
