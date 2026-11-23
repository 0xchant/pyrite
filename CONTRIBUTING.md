# Contributing to Pyrite

Thank you for contributing to **Pyrite**!

## Development Prerequisites

- **Rust** (`stable` toolchain via `rustup`)
- **Foundry** (`forge`, `cast`, `anvil`) for building & testing `contracts/`
- **Python 3.10+** (standard library only, used by `scripts/dashboard.py` and `scripts/test_dashboard.py`)
- **Optional**: NVIDIA CUDA Toolkit (`nvcc`) if modifying `cuda/`

## Local Verification Checklist

Before opening a pull request, run the full check suite:

```bash
# 1. Build contracts & run Foundry unit tests
cd contracts
forge build
forge test --match-path test/DagArbExecutor.t.sol
cd ..

# 2. Run Rust unit tests & release build (0 compiler warnings expected)
cargo test --release
cargo build --release

# 3. Smoke-test offline backtester against sample data
./target/release/pyrite backtest --opps-file examples/sample_opps.jsonl --min-profit-usd 0.001

# 4. Validate dashboard syntax & security unit tests
python3 -m py_compile scripts/dashboard.py
python3 -m unittest -v scripts/test_dashboard.py
```

If modifying CUDA kernels (`cuda/cfmm_solver.cu`), also verify:

```bash
make -C cuda CUDA_ARCH=sm_89
```

## Configuration Boundary

- Store shareable runtime knobs in `pyrite.json` (modeled after `pyrite.example.json`).
- Keep `PRIVATE_KEY`, `EXTRA_PRIVATE_KEYS`, and `FLASHBOTS_AUTH_KEY` strictly in `.env` (`chmod 0600`) — never add secret fields to `pyrite.json` or `GET` HTTP responses.

## Adding a New EVM Chain

1. Add `chains/<chain>.yaml` defining `chain_id`, `gas`, `submission`, `bidding`, `rpcs`, `singletons`, `hub_tokens`, `factories`, and `seed_flash_pools`.
2. Verify revert-protection semantics and builder availability before recommending live mode.
3. Add a unit test in `src/config.rs` validating the new chain config.

## Pull Request Guidelines

- Never commit `.env`, `.env.bak*`, `pyrite.json`, private keys, personal RPC keys, or local host IPs/paths.
- Keep documentation (`README.md`, `docs/pitfalls/`) aligned with code behavior.
