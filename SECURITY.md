# Security Policy

## Scope & Disclaimer

**Pyrite** is published as an **open-source engineering reference and MEV research platform**. It is **not** financial advice and offers **no guarantee of profitability**. On-chain MEV markets are adversarial: capital can be lost to reverted transaction gas fees, unbundled transactions, builder competition, malicious/fee-on-transfer tokens, or smart-contract bugs. Use strictly at your own risk.

## Separation Between `pyrite.json` and Secrets (`.env`)

Pyrite intentionally separates **shareable strategy configuration** from **wallet secrets**:

- **`pyrite.json`** stores only non-secret settings (chain selection, RPC/WSS endpoints, MEV-Share toggles, profit thresholds, GPU solver options, and public `DagArbExecutor` contract addresses). **Never place private keys inside `pyrite.json`** — both the Web Control Panel and validator explicitly reject secret-like keys.
- **`.env` (`chmod 0600`)** holds `PRIVATE_KEY`, optional `EXTRA_PRIVATE_KEYS`, and `FLASHBOTS_AUTH_KEY`.
- **Write-Only Web Wallet Tab (`scripts/dashboard.py`)**:
  - Supports 1-click burner wallet generation and password-masked key import directly into `.env` (`chmod 0600`).
  - **No `GET` endpoint ever reads or returns private keys** — only boolean `configured` status and derived public addresses are ever exposed.
  - All mutating endpoints (`POST /api/config`, `POST /api/process/*`, `POST /api/wallet/*`) require the per-session `X-Pyrite-Token` bearer header.
  - Wallet write endpoints additionally enforce **loopback-only** (`127.0.0.1` / `::1`) access by default unless explicitly overridden via `--allow-remote-wallet`.

## Operational Security Checklist

1. **Use a dedicated burner wallet**: Flash-loan atomic arbitrage requires zero trading principal. Fund only a minimal gas buffer (`$5–$20` equivalent).
2. **Never commit `.env`, `.env.bak*`, `pyrite.json`, or `logs/`** to version control.
3. **Always run `scan` or `run --dry-run` first** before enabling live broadcasting.
4. **Separate Flashbots reputation identity**: Prefer setting `FLASHBOTS_AUTH_KEY` separately from `PRIVATE_KEY`.
5. **Know chain revert exposure**: Ethereum MEV-Boost bundles (`chains/eth.yaml`) drop failing transactions with `$0` gas cost; unprotected public RPC chains (`chains/base.yaml` or unbundled L1s) charge full gas on every revert.

## Reporting a Vulnerability

If you discover a security issue in `contracts/src/DagArbExecutor.sol`, the Rust engine (`src/`), or `scripts/dashboard.py`, please **do not disclose exploit details publicly**:

1. Report privately via **GitHub Security Advisories** (`Security` -> `Advisories` -> `Report a vulnerability`).
2. Or open a minimal issue requesting a private maintainer contact without attaching exploit payloads.
