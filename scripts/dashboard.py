#!/usr/bin/env python3
"""Lightweight, zero-flicker arbitrage dashboard & control panel for Pyrite.

Zero external Python or CDN dependencies:
- CLI mode (--watch) for terminal/tmux monitoring
- Web Terminal (--web) pushed over SSE (/api/stream) with:
  * Incremental log/JSON caching & non-blocking EOA RPC refresh
  * Visual `pyrite.json` config editor & local process start/stop (`X-Pyrite-Token` gated)
  * Write-only beginner wallet manager (writes burner/imported keys straight into chmod 0600 `.env`;
    never writes keys to `pyrite.json` and never returns private keys in any HTTP GET response)
"""

from __future__ import annotations

import argparse
import calendar
import json
import os
import re
import secrets
import select
import socket
import subprocess
import threading
import time
import urllib.parse
import urllib.request
from collections import Counter
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any

try:
    import yaml
except ImportError:
    yaml = None  # type: ignore[assignment]


ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")
REVERT_REASON_RE = re.compile(r"execution reverted:\s*([^:\n\"]+)")
SCAN_LOG_RE = re.compile(
    r"block=(\d+)\s+txs=(\d+)\s+touched=(\d+)\s+new_pools=(\d+)\s+"
    r"total_pools=(\d+)\s+solve_us=(\d+)\s+cands=(\d+)\s+blk_ms=(\d+)"
)
SUBMIT_LOG_RE = re.compile(
    r"^(\S+)\s+INFO.*?SUBMITTED\s+(MEV-SHARE BACKRUN BUNDLE|MEMPOOL BACKRUN BUNDLE|MEV BUNDLE)\s+(.*)$"
)
KV_RE = re.compile(r'(\w+)=("[^"]*"|\S+)')
WALLET_ADDR_RE = re.compile(r"(?:primary|wallet)=(0x[0-9a-fA-F]{40})")

FORBIDDEN_CONFIG_KEYS = frozenset({
    "private_key",
    "extra_private_keys",
    "flashbots_auth_key",
    "secret",
    "mnemonic",
    "seed_phrase",
})

DEFAULT_PYRITE_CONFIG: dict[str, Any] = {
    "chain": "eth",
    "mode": {
        "dry_run": True,
        "min_profit_usd": 0.001,
        "target_usd": 50.0,
        "max_blocks": None,
    },
    "rpcs": {
        "read": [],
        "ws": [],
        "broadcast": [],
        "bundle": [],
    },
    "mev_share": {
        "enabled": True,
        "sse_url": "https://mev-share.flashbots.net",
        "relay_url": "https://relay.flashbots.net",
        "min_swap_usd": 50.0,
        "max_blocks_ahead": 3,
        "builders": [],
    },
    "solver": {
        "mode": "cpu",
        "gpu_lib": "",
        "gpu_budget_ms": 25,
        "gpu_max_gas_limit": 1200000,
        "gpu_enable_layer2": True,
        "gpu_base_tx_gas": 120000,
        "gpu_gas_scale": 1.0,
    },
    "executors": {
        "eth": "",
        "bsc": "",
        "base": "",
    },
    "logging": {
        "rust_log": "info",
    },
}

_FALLBACK_CHAIN_CONFIGS: dict[str, dict[str, Any]] = {
    "bsc": {
        "chain": "bsc",
        "native_symbol": "BNB",
        "wrapped_native_symbol": "WBNB",
        "default_native_usd": 620.0,
        "explorer_tx_url": "https://bscscan.com/tx/{hash}",
        "phalcon_chain": "bsc",
        "rpcs": [
            "https://bsc-dataseed.bnbchain.org",
            "https://rpc.48.club",
            "https://bsc-rpc.publicnode.com",
        ],
        "hub_tokens": [
            ("WBNB", "0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c", 18, False),
            ("USDT", "0x55d398326f99059fF775485246999027B3197955", 18, True),
            ("USDC", "0x8AC76a51cc950d9822D68b83fE1Ad97B32Cd580d", 18, True),
        ],
    },
    "base": {
        "chain": "base",
        "native_symbol": "ETH",
        "wrapped_native_symbol": "WETH",
        "default_native_usd": 3100.0,
        "explorer_tx_url": "https://basescan.org/tx/{hash}",
        "phalcon_chain": "base",
        "rpcs": [
            "https://mainnet.base.org",
            "https://base-rpc.publicnode.com",
        ],
        "hub_tokens": [
            ("WETH", "0x4200000000000000000000000000000000000006", 18, False),
            ("USDC", "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913", 6, True),
            ("USDbC", "0xd9aAEc86B65D86f6A7B5B1b0c42FFA531710b6CA", 6, True),
        ],
    },
    "eth": {
        "chain": "eth",
        "native_symbol": "ETH",
        "wrapped_native_symbol": "WETH",
        "default_native_usd": 3100.0,
        "explorer_tx_url": "https://etherscan.io/tx/{hash}",
        "phalcon_chain": "ethereum",
        "rpcs": [
            "https://ethereum-rpc.publicnode.com",
            "https://eth.drpc.org",
            "https://rpc.mevblocker.io",
        ],
        "hub_tokens": [
            ("WETH", "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2", 18, False),
            ("USDC", "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48", 6, True),
            ("USDT", "0xdAC17F958D2ee523a2206206994597C13D831ec7", 6, True),
            ("DAI", "0x6B175474E89094C44Da98b954EedeAC495271d0F", 18, True),
        ],
    },
}

_CHAIN_YAML_LOCK = threading.Lock()
_CHAIN_YAML_CACHE: dict[str, dict[str, Any]] = {}


def load_chain_yaml(chain: str) -> dict[str, Any]:
    with _CHAIN_YAML_LOCK:
        cached = _CHAIN_YAML_CACHE.get(chain)
        if cached is not None:
            return cached

    cand_path = Path(chain)
    if not (cand_path.suffix in (".yaml", ".yml") and cand_path.is_file()):
        name = chain.strip().lower()
        cand_path = Path(__file__).resolve().parent.parent / "chains" / f"{name}.yaml"

    parsed: dict[str, Any] | None = None
    if yaml is not None and cand_path.is_file():
        try:
            raw = yaml.safe_load(cand_path.read_text(encoding="utf-8"))
            if isinstance(raw, dict):
                contracts = raw.get("contracts") or {}
                wnative_addr = str(contracts.get("wrapped_native") or "").lower()
                raw_tokens = raw.get("hub_tokens") or []
                tokens: list[tuple[str, str, int, bool]] = []
                wnative_sym = "WETH"
                for item in raw_tokens:
                    if not isinstance(item, dict):
                        continue
                    sym = str(item.get("symbol") or "")
                    addr = str(item.get("address") or "")
                    dec = int(item.get("decimals") or 18)
                    is_stable = bool(item.get("is_pinned_stable", False))
                    if wnative_addr and addr.lower() == wnative_addr:
                        wnative_sym = sym or wnative_sym
                    if sym and addr:
                        tokens.append((sym, addr, dec, is_stable))
                if not any(t[0] == wnative_sym for t in tokens):
                    wnative_sym = next((sym for sym, _, _, is_st in tokens if not is_st), wnative_sym)
                rpcs = [str(u) for u in ((raw.get("rpcs") or {}).get("read") or []) if u]
                ch_name = str(raw.get("chain") or chain)
                parsed = {
                    "chain": ch_name,
                    "native_symbol": str(raw.get("native_symbol") or "ETH"),
                    "wrapped_native_symbol": wnative_sym,
                    "default_native_usd": float(raw.get("default_native_usd") or 3100.0),
                    "explorer_tx_url": str(raw.get("explorer_tx_url") or "https://etherscan.io/tx/{hash}"),
                    "phalcon_chain": str(raw.get("phalcon_chain") or ch_name),
                    "rpcs": rpcs,
                    "hub_tokens": tokens,
                }
        except Exception:
            parsed = None

    if parsed is None:
        parsed = _FALLBACK_CHAIN_CONFIGS.get(chain.lower(), _FALLBACK_CHAIN_CONFIGS["bsc"])

    with _CHAIN_YAML_LOCK:
        _CHAIN_YAML_CACHE[chain] = parsed
    return parsed

_RPC_TTL_SEC = 15.0
_RPC_CACHE: dict[str, Any] = {
    "ts": 0.0,
    "eoa_data": {},
    "owner_tokens": {},
    "exec_bnb": 0.0,
    "exec_tokens": {},
    "refreshing": False,
    "attempted": False,
    "lock": threading.Lock(),
}

_BUNDLE_LOCK = threading.Lock()
_BUNDLE_INDEX: dict[str, Any] = {
    "path": "",
    "ino": -1,
    "offset": 0,
    "items": [],
    "eoa_counts": Counter(),
}

_FILE_CACHE_LOCK = threading.Lock()
_JSON_FILE_CACHE: dict[str, tuple[tuple[int, int, int], Any]] = {}
_ENV_EXECUTOR_CACHE: dict[str, tuple[tuple[int, int, int], str]] = {}
_OPP_CACHE: dict[str, Any] = {"key": None, "sig": None, "data": None}
_TOPO_CACHE: dict[str, Any] = {"key": None, "sig": None, "data": None}
_NATIVE_USD_CACHE: dict[str, float] = {}


def _stat_sig(path: Path) -> tuple[int, int, int] | None:
    try:
        st = path.stat()
        return (st.st_ino, st.st_size, st.st_mtime_ns)
    except OSError:
        return None


def read_cached_json(path: Path) -> Any:
    sig = _stat_sig(path)
    if sig is None:
        return None
    key = str(path)
    with _FILE_CACHE_LOCK:
        cached = _JSON_FILE_CACHE.get(key)
        if cached is not None and cached[0] == sig:
            return cached[1]
    try:
        parsed = json.loads(path.read_text(encoding="utf-8"))
    except Exception:
        with _FILE_CACHE_LOCK:
            cached = _JSON_FILE_CACHE.get(key)
            return cached[1] if cached is not None else None
    with _FILE_CACHE_LOCK:
        _JSON_FILE_CACHE[key] = (sig, parsed)
    return parsed


def parse_iso_epoch(ts: str) -> int:
    try:
        return int(calendar.timegm(time.strptime(ts[:19], "%Y-%m-%dT%H:%M:%S")))
    except Exception:
        return 0


def get_bundle_history(runtime_path: Path, cutoff_epoch: int = 0) -> tuple[list[dict[str, Any]], Counter[str]]:
    with _BUNDLE_LOCK:
        if not runtime_path.exists():
            return [], Counter()
        try:
            st = runtime_path.stat()
        except OSError:
            return [], Counter()
        if st.st_size == 0:
            _BUNDLE_INDEX["path"] = str(runtime_path)
            _BUNDLE_INDEX["ino"] = st.st_ino
            _BUNDLE_INDEX["offset"] = 0
            _BUNDLE_INDEX["items"] = []
            _BUNDLE_INDEX["eoa_counts"] = Counter()
            return [], Counter()

        path_str = str(runtime_path)
        if (
            _BUNDLE_INDEX["path"] != path_str
            or _BUNDLE_INDEX["ino"] != st.st_ino
            or st.st_size < _BUNDLE_INDEX["offset"]
        ):
            _BUNDLE_INDEX["path"] = path_str
            _BUNDLE_INDEX["ino"] = st.st_ino
            _BUNDLE_INDEX["offset"] = 0
            _BUNDLE_INDEX["items"] = []
            _BUNDLE_INDEX["eoa_counts"] = Counter()

        cur_offset = _BUNDLE_INDEX["offset"]
        if st.st_size == cur_offset:
            return list(_BUNDLE_INDEX["items"]), Counter(_BUNDLE_INDEX["eoa_counts"])

        with runtime_path.open("rb") as f:
            f.seek(cur_offset)
            chunk = f.read(st.st_size - cur_offset)
        last_nl = chunk.rfind(b"\n")
        if last_nl >= 0:
            complete = chunk[: last_nl + 1]
            _BUNDLE_INDEX["offset"] = cur_offset + last_nl + 1
            items = _BUNDLE_INDEX["items"]
            eoa_counts = _BUNDLE_INDEX["eoa_counts"]
            for raw_ln in complete.splitlines():
                if b"SUBMITTED " not in raw_ln:
                    continue
                ln = ANSI_RE.sub("", raw_ln.decode("utf-8", errors="replace"))
                ms = SUBMIT_LOG_RE.search(ln)
                if not ms:
                    continue
                ts, kind_raw, rest = ms.group(1), ms.group(2), ms.group(3)
                epoch = parse_iso_epoch(ts)
                if cutoff_epoch and epoch < cutoff_epoch:
                    continue
                kv = {k: v.strip('"') for k, v in KV_RE.findall(rest)}
                eoa_addr = kv.get("eoa", "").lower()
                if eoa_addr:
                    eoa_counts[eoa_addr] += 1
                blk_str = kv.get("block", "-")
                blk_int = int(blk_str) if blk_str.isdigit() else 0
                gross_str = kv.get("gross_usd", "-")
                try:
                    gross_val = float(gross_str.lstrip("$"))
                except ValueError:
                    gross_val = 0.0
                items.append(
                    {
                        "timestamp": ts,
                        "epoch": epoch,
                        "kind": "BACKRUN" if "BACKRUN" in kind_raw else "TOB",
                        "block": blk_str,
                        "block_int": blk_int,
                        "eoa": kv.get("eoa", "-"),
                        "tx_hash": kv.get("tx_hash", ""),
                        "strategy": kv.get("strategy", ""),
                        "gross_usd": gross_str,
                        "gross_val": round(gross_val, 4),
                        "tip_usd": kv.get("tip_usd", "-"),
                        "expected_net_usd": kv.get("expected_net_usd", "-"),
                        "pre_submit_ms": kv.get("pre_submit_ms", "-"),
                    }
                )
            if len(items) > 10000:
                items[:] = items[-10000:]

        return list(_BUNDLE_INDEX["items"]), Counter(_BUNDLE_INDEX["eoa_counts"])


def read_executor_from_env(logs_dir: Path, chain: str) -> str:
    env_path = logs_dir.parent / ".env"
    sig = _stat_sig(env_path)
    if sig is None:
        return ""
    cache_key = f"{env_path}:{chain}"
    with _FILE_CACHE_LOCK:
        cached = _ENV_EXECUTOR_CACHE.get(cache_key)
        if cached is not None and cached[0] == sig:
            return cached[1]
    want_chain = f"EXECUTOR_ADDRESS_{chain.upper()}"
    found_chain = ""
    found_default = ""
    try:
        for line in env_path.read_text(encoding="utf-8").splitlines():
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            k, v = line.split("=", 1)
            ks = k.strip()
            vs = v.strip().strip('"').strip("'")
            if ks == want_chain and vs:
                found_chain = vs
            elif ks == "EXECUTOR_ADDRESS" and vs:
                found_default = vs
    except Exception:
        pass
    val = found_chain or found_default
    with _FILE_CACHE_LOCK:
        _ENV_EXECUTOR_CACHE[cache_key] = (sig, val)
    return val


def _do_onchain_rpc_refresh(
    addrs: list[str],
    owner_addr: str,
    executor_addr: str,
    chain: str,
) -> None:
    chain_cfg = load_chain_yaml(chain)
    rpcs = chain_cfg["rpcs"]
    tokens = chain_cfg["hub_tokens"]
    batch: list[dict[str, Any]] = []
    tags: list[tuple[Any, ...]] = []

    def add_call(tag: tuple[Any, ...], method: str, params: list[Any]) -> None:
        idx = len(batch)
        tags.append(tag)
        batch.append({"jsonrpc": "2.0", "id": idx, "method": method, "params": params})

    for addr in addrs:
        low = addr.lower()
        add_call(("eoa_bal", low), "eth_getBalance", [addr, "latest"])
        add_call(("eoa_nonce", low), "eth_getTransactionCount", [addr, "latest"])

    if executor_addr:
        add_call(("exec_bal",), "eth_getBalance", [executor_addr, "latest"])

    for sym, tok_addr, dec, _ in tokens:
        if owner_addr:
            call_data = "0x70a08231" + owner_addr.lower().removeprefix("0x").rjust(64, "0")
            add_call(("owner_tok", sym, dec), "eth_call", [{"to": tok_addr, "data": call_data}, "latest"])
        if executor_addr:
            call_data = "0x70a08231" + executor_addr.lower().removeprefix("0x").rjust(64, "0")
            add_call(("exec_tok", sym, dec), "eth_call", [{"to": tok_addr, "data": call_data}, "latest"])

    by_id: dict[int, Any] | None = None
    payload = json.dumps(batch).encode("utf-8")
    for rpc_url in rpcs:
        try:
            req = urllib.request.Request(
                rpc_url,
                data=payload,
                headers={"Content-Type": "application/json", "User-Agent": "curl/8.5"},
            )
            with urllib.request.urlopen(req, timeout=4) as resp:
                items = json.loads(resp.read().decode("utf-8"))
            if isinstance(items, list):
                by_id = {
                    int(item["id"]): item.get("result")
                    for item in items
                    if isinstance(item, dict) and "id" in item and "result" in item
                }
                if by_id:
                    break
        except Exception:
            continue

    eoa_bals: dict[str, float] = {}
    eoa_nonces: dict[str, int] = {}
    owner_tokens: dict[str, float] = {}
    exec_bnb = 0.0
    exec_tokens: dict[str, float] = {}

    if by_id:
        for idx, tag in enumerate(tags):
            raw = by_id.get(idx)
            if not isinstance(raw, str) or not raw.startswith("0x"):
                continue
            try:
                val_int = int(raw, 16)
            except ValueError:
                continue
            kind = tag[0]
            if kind == "eoa_bal":
                eoa_bals[tag[1]] = val_int / 1e18
            elif kind == "eoa_nonce":
                eoa_nonces[tag[1]] = val_int
            elif kind == "exec_bal":
                exec_bnb = val_int / 1e18
            elif kind == "owner_tok":
                owner_tokens[tag[1]] = val_int / (10 ** tag[2])
            elif kind == "exec_tok":
                exec_tokens[tag[1]] = val_int / (10 ** tag[2])

    eoa_out = {
        a.lower(): (eoa_bals.get(a.lower(), 0.0), eoa_nonces.get(a.lower(), 0))
        for a in addrs
    } if by_id else {}

    with _RPC_CACHE["lock"]:
        _RPC_CACHE["ts"] = time.time()
        _RPC_CACHE["attempted"] = True
        _RPC_CACHE["refreshing"] = False
        if by_id:
            _RPC_CACHE["eoa_data"] = eoa_out
            _RPC_CACHE["owner_tokens"] = owner_tokens
            _RPC_CACHE["exec_bnb"] = exec_bnb
            _RPC_CACHE["exec_tokens"] = exec_tokens


def fetch_onchain_state(
    addrs: list[str],
    owner_addr: str,
    executor_addr: str,
    chain: str,
    blocking_if_cold: bool = False,
) -> dict[str, Any]:
    if not addrs and not executor_addr:
        return {"eoa_data": {}, "owner_tokens": {}, "exec_bnb": 0.0, "exec_tokens": {}}
    now = time.time()
    need_bg = False
    do_blocking = False

    with _RPC_CACHE["lock"]:
        snap = {
            "eoa_data": dict(_RPC_CACHE["eoa_data"]),
            "owner_tokens": dict(_RPC_CACHE["owner_tokens"]),
            "exec_bnb": float(_RPC_CACHE["exec_bnb"]),
            "exec_tokens": dict(_RPC_CACHE["exec_tokens"]),
        }
        attempted = bool(_RPC_CACHE["attempted"])
        refreshing = bool(_RPC_CACHE["refreshing"])
        stale = (now - float(_RPC_CACHE["ts"])) >= _RPC_TTL_SEC or not snap["eoa_data"]
        if stale and not refreshing:
            _RPC_CACHE["refreshing"] = True
            if blocking_if_cold and not attempted:
                do_blocking = True
            else:
                need_bg = True

    if do_blocking:
        _do_onchain_rpc_refresh(list(addrs), owner_addr, executor_addr, chain)
        with _RPC_CACHE["lock"]:
            return {
                "eoa_data": dict(_RPC_CACHE["eoa_data"]),
                "owner_tokens": dict(_RPC_CACHE["owner_tokens"]),
                "exec_bnb": float(_RPC_CACHE["exec_bnb"]),
                "exec_tokens": dict(_RPC_CACHE["exec_tokens"]),
            }

    if need_bg:
        args = (list(addrs), owner_addr, executor_addr, chain)
        threading.Thread(
            target=_do_onchain_rpc_refresh,
            args=args,
            name="onchain-rpc-refresh",
            daemon=True,
        ).start()

    return snap


def classify_error(err: str | None) -> str:
    if not err:
        return "none"
    m = REVERT_REASON_RE.search(err)
    if m:
        return m.group(1).strip()
    if "FLASH_SHORTFALL" in err:
        return "FLASH_SHORTFALL (tax/fee)"
    if "STEP_SWAP_REVERT" in err:
        return "STEP_SWAP_REVERT"
    if "429" in err or "rate limit" in err.lower():
        return "RPC_429_RATE_LIMIT"
    if "timeout" in err.lower() or "timed out" in err.lower():
        return "RPC_TIMEOUT"
    if "insufficient" in err.lower():
        return "INSUFFICIENT"
    return err[:56].replace("\n", " ")


def tail_lines(path: Path, max_lines: int, chunk_limit: int = 8 * 1024 * 1024) -> list[str]:
    if not path.exists():
        return []
    try:
        file_size = path.stat().st_size
    except OSError:
        return []
    if file_size == 0:
        return []
    with path.open("rb") as f:
        if file_size > chunk_limit:
            f.seek(file_size - chunk_limit)
            f.readline()
        data = f.read().decode("utf-8", errors="replace")
    lines = [ln for ln in data.splitlines() if ln.strip()]
    return lines[-max_lines:]


def load_opps_summary(opp_path: Path, max_lines: int, since_iso: str = "") -> dict[str, Any]:
    sig = _stat_sig(opp_path)
    cache_key = f"{opp_path}:{max_lines}:{since_iso}"
    if sig is None:
        return {
            "sig": None,
            "file_kb": 0.0,
            "sampled": 0,
            "sim_ok_count": 0,
            "sim_fail_count": 0,
            "best_sim_usd": 0.0,
            "best_est_usd": 0.0,
            "block_min": 0,
            "block_max": 0,
            "latest_ts": "-",
            "top_strategies": [],
            "top_errors": [],
            "recent": [],
        }

    with _FILE_CACHE_LOCK:
        if _OPP_CACHE["key"] == cache_key and _OPP_CACHE["sig"] == sig and _OPP_CACHE["data"] is not None:
            return _OPP_CACHE["data"]

    cutoff_iso = since_iso
    lines = tail_lines(opp_path, max_lines, chunk_limit=2 * 1024 * 1024)
    records: list[dict[str, Any]] = []
    for ln in lines:
        try:
            rec = json.loads(ln)
            if not cutoff_iso or str(rec.get("timestamp", "")) >= cutoff_iso:
                records.append(rec)
        except json.JSONDecodeError:
            continue

    total = len(records)
    sim_ok_recs = [r for r in records if r.get("sim_ok") is True]
    strategies = Counter(r.get("strategy", "unknown") for r in records)
    errors = Counter(classify_error(r.get("sim_err")) for r in records if not r.get("sim_ok"))
    blocks = [r.get("block", 0) for r in records if r.get("block")]

    recent_opps = []
    for r in reversed(records[-80:]):
        recent_opps.append(
            {
                "timestamp": r.get("timestamp", ""),
                "block": r.get("block", 0),
                "strategy": r.get("strategy", ""),
                "base_token": r.get("base_token", ""),
                "borrow_human": float(r.get("borrow_human") or 0.0),
                "est_net_usd": float(r.get("est_net_usd") or 0.0),
                "sim_ok": bool(r.get("sim_ok")),
                "sim_net_usd": r.get("sim_net_usd"),
                "sim_gas": r.get("sim_gas"),
                "err_short": "" if r.get("sim_ok") else classify_error(r.get("sim_err")),
                "route": r.get("route", ""),
            }
        )

    res = {
        "sig": sig,
        "file_kb": round(sig[1] / 1024.0, 1),
        "sampled": total,
        "sim_ok_count": len(sim_ok_recs),
        "sim_fail_count": total - len(sim_ok_recs),
        "best_sim_usd": max((r.get("sim_net_usd") or 0.0 for r in sim_ok_recs), default=0.0),
        "best_est_usd": max((r.get("est_net_usd") or 0.0 for r in records), default=0.0),
        "block_min": min(blocks) if blocks else 0,
        "block_max": max(blocks) if blocks else 0,
        "latest_ts": records[-1].get("timestamp", "-") if records else "-",
        "top_strategies": strategies.most_common(8),
        "top_errors": errors.most_common(8),
        "recent": recent_opps,
    }
    with _FILE_CACHE_LOCK:
        _OPP_CACHE["key"] = cache_key
        _OPP_CACHE["sig"] = sig
        _OPP_CACHE["data"] = res
    return res


_SAFE_SYM_RE = re.compile(r"^[A-Za-z0-9_.$+\-/ ]{1,20}$")
_SAFE_CHAIN_RE = re.compile(r"^[a-z0-9_-]{1,32}$")
_ROUTE_LEG_RE = re.compile(r"^\s*([A-Za-z0-9_-]{1,32}):\s*(.+?)\s*->\s*(.+?)\s*$")


def _clean_symbol(raw: Any, addr: str = "") -> str:
    sym = str(raw or "").strip()
    if sym and _SAFE_SYM_RE.fullmatch(sym):
        return sym
    if len(addr) >= 12 and addr.startswith("0x"):
        return f"{addr[:6]}..{addr[-4:]}"
    return sym[:12] if sym and sym.isascii() else (addr[:10] or "?")


def load_topology(logs_dir: Path, chain: str) -> dict[str, Any]:
    """Build a chain-agnostic liquidity topology snapshot for live Canvas rendering.

    Reads `logs/pools_cache_<chain>.json` when available, and falls back cleanly
    to unique route edges parsed from `logs/opportunities.jsonl` on cold start
    or offline demos. Results are cached by file stat signatures.
    """
    ch_clean = str(chain or "eth").strip().lower()
    if not _SAFE_CHAIN_RE.fullmatch(ch_clean):
        ch_clean = "eth"

    cache_path = logs_dir / f"pools_cache_{ch_clean}.json"
    opp_path = logs_dir / "opportunities.jsonl"
    cache_sig = _stat_sig(cache_path)
    opp_sig = _stat_sig(opp_path) if cache_sig is None else None
    sig = (cache_sig, opp_sig)
    cache_key = f"{logs_dir}:{ch_clean}"

    with _FILE_CACHE_LOCK:
        if _TOPO_CACHE["key"] == cache_key and _TOPO_CACHE["sig"] == sig and _TOPO_CACHE["data"] is not None:
            return _TOPO_CACHE["data"]

    tokens: list[str] = []
    token_idx: dict[str, int] = {}
    venues: list[str] = []
    venue_idx: dict[str, int] = {}
    pools: list[list[Any]] = []
    seen_pools: set[str] = set()

    def get_tok_id(key: str, label: str) -> int:
        tid = token_idx.get(key)
        if tid is None:
            tid = len(tokens)
            token_idx[key] = tid
            tokens.append(label)
        return tid

    def get_venue_id(name: str) -> int:
        v = str(name or "DEX").strip()[:32] or "DEX"
        if not v.isascii():
            v = "DEX"
        vid = venue_idx.get(v)
        if vid is None:
            vid = len(venues)
            venue_idx[v] = vid
            venues.append(v)
        return vid

    source = "empty"
    updated_ms = int(time.time() * 1000)

    if cache_sig is not None:
        raw_cache = read_cached_json(cache_path)
        if isinstance(raw_cache, dict):
            sym_by_addr: dict[str, str] = {}
            for entry in raw_cache.get("tokens") or []:
                if isinstance(entry, (list, tuple)) and len(entry) >= 2:
                    addr_l = str(entry[0] or "").strip().lower()
                    if addr_l:
                        sym_by_addr[addr_l] = _clean_symbol(entry[1], addr_l)
                elif isinstance(entry, dict):
                    addr_l = str(entry.get("address") or "").strip().lower()
                    if addr_l:
                        sym_by_addr[addr_l] = _clean_symbol(entry.get("symbol"), addr_l)

            for item in raw_cache.get("pools") or []:
                if not isinstance(item, dict):
                    continue
                addr = str(item.get("address") or "").strip().lower()
                t0 = str(item.get("token0") or "").strip().lower()
                t1 = str(item.get("token1") or "").strip().lower()
                if not addr or not t0 or not t1 or t0 == t1 or addr in seen_pools:
                    continue
                seen_pools.add(addr)
                id0 = get_tok_id(t0, sym_by_addr.get(t0) or _clean_symbol("", t0))
                id1 = get_tok_id(t1, sym_by_addr.get(t1) or _clean_symbol("", t1))
                vid = get_venue_id(str(item.get("factory_name") or item.get("dex_kind") or "DEX"))
                pools.append([addr, id0, id1, vid])

            if pools:
                source = "pools_cache"
                updated_ms = int(cache_sig[2] // 1_000_000)

    if not pools and opp_sig is not None:
        lines = tail_lines(opp_path, 500, chunk_limit=1024 * 1024)
        for ln in lines:
            try:
                rec = json.loads(ln)
            except json.JSONDecodeError:
                continue
            if not isinstance(rec, dict):
                continue
            rec_chain = str(rec.get("chain") or "").strip().lower()
            if rec_chain and rec_chain != ch_clean:
                continue
            route_str = str(rec.get("route") or "")
            for raw_leg in route_str.split(" | "):
                cleaned_leg = re.sub(
                    r"\s*\(\+\d+\s+more(?:,\s*\d+\s+steps)?\)\s*$",
                    "",
                    raw_leg,
                    flags=re.IGNORECASE,
                ).strip()
                m = _ROUTE_LEG_RE.fullmatch(cleaned_leg)
                if not m:
                    continue
                venue_name, sym_a_raw, sym_b_raw = m.group(1), m.group(2), m.group(3)
                sym_a = _clean_symbol(sym_a_raw)
                sym_b = _clean_symbol(sym_b_raw)
                if not sym_a or not sym_b or sym_a == sym_b:
                    continue
                pair_lo, pair_hi = (sym_a, sym_b) if sym_a < sym_b else (sym_b, sym_a)
                edge_key = f"{pair_lo}|{pair_hi}|{venue_name}"
                if edge_key in seen_pools:
                    continue
                seen_pools.add(edge_key)
                id0 = get_tok_id(sym_a, sym_a)
                id1 = get_tok_id(sym_b, sym_b)
                vid = get_venue_id(venue_name)
                pools.append([edge_key, id0, id1, vid])
        if pools:
            source = "opportunities_fallback"
            updated_ms = int(opp_sig[2] // 1_000_000)

    deg: Counter[int] = Counter()
    for _, t0_id, t1_id, _ in pools:
        deg[t0_id] += 1
        deg[t1_id] += 1
    top_hubs = deg.most_common(8)
    hubs = [idx for idx, _ in top_hubs]
    hub_counts = [cnt for _, cnt in top_hubs]

    res = {
        "chain": ch_clean,
        "source": source,
        "updated_ms": updated_ms,
        "pool_count": len(pools),
        "token_count": len(tokens),
        "venue_count": len(venues),
        "tokens": tokens,
        "venues": venues,
        "hubs": hubs,
        "hub_counts": hub_counts,
        "pools": pools,
    }
    with _FILE_CACHE_LOCK:
        _TOPO_CACHE["key"] = cache_key
        _TOPO_CACHE["sig"] = sig
        _TOPO_CACHE["data"] = res
    return res


def read_vm_stats() -> dict[str, Any]:
    stats: dict[str, Any] = {"load_1m": None, "mem_used_mb": None, "mem_total_mb": None}
    try:
        load_txt = Path("/proc/loadavg").read_text(encoding="utf-8").split()
        stats["load_1m"] = float(load_txt[0])
    except Exception:
        pass
    try:
        meminfo: dict[str, int] = {}
        for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
            parts = line.split()
            if len(parts) >= 2:
                meminfo[parts[0].rstrip(":")] = int(parts[1])
        total_kb = meminfo.get("MemTotal", 0)
        avail_kb = meminfo.get("MemAvailable", 0)
        if total_kb > 0:
            stats["mem_total_mb"] = round(total_kb / 1024.0)
            stats["mem_used_mb"] = round((total_kb - avail_kb) / 1024.0)
    except Exception:
        pass
    return stats


def _extract_latest_scan(
    runtime_path: Path, default_native_usd: float = 3100.0
) -> tuple[dict[str, int] | None, float]:
    # Only read last 64 KB for scan telemetry instead of scanning up to 8 MB every tick.
    runtime_tail = [ANSI_RE.sub("", ln) for ln in tail_lines(runtime_path, 160, chunk_limit=64 * 1024)]
    latest_scan: dict[str, int] | None = None
    path_key = str(runtime_path)
    native_usd = _NATIVE_USD_CACHE.get(path_key, default_native_usd)
    found_usd = False

    for ln in reversed(runtime_tail):
        if not found_usd and "native_usd=" in ln:
            m_usd = re.search(r'native_usd="([0-9.]+)"', ln)
            if m_usd:
                try:
                    native_usd = float(m_usd.group(1))
                    _NATIVE_USD_CACHE[path_key] = native_usd
                    found_usd = True
                except ValueError:
                    pass
        if latest_scan is None:
            m = SCAN_LOG_RE.search(ln)
            if m:
                latest_scan = {
                    "block": int(m.group(1)),
                    "txs": int(m.group(2)),
                    "touched": int(m.group(3)),
                    "new_pools": int(m.group(4)),
                    "total_pools": int(m.group(5)),
                    "solve_us": int(m.group(6)),
                    "cands": int(m.group(7)),
                    "blk_ms": int(m.group(8)),
                }
        if found_usd and latest_scan is not None:
            break

    if not found_usd and path_key not in _NATIVE_USD_CACHE and runtime_path.exists():
        # Read only startup 8 KB once per runtime.log lifecycle to capture bootstrap native_usd.
        try:
            with runtime_path.open("rb") as f:
                head = ANSI_RE.sub("", f.read(8192).decode("utf-8", errors="replace"))
            m_usd = re.search(r'native_usd="([0-9.]+)"', head)
            if m_usd:
                native_usd = float(m_usd.group(1))
                _NATIVE_USD_CACHE[path_key] = native_usd
        except Exception:
            pass

    return latest_scan, native_usd


def load_summary(
    logs_dir: Path,
    chain: str,
    max_lines: int = 2000,
    blocking_rpc_if_cold: bool = False,
    since_iso: str = "",
) -> dict[str, Any]:
    chain_cfg = load_chain_yaml(chain)
    chain_name = chain_cfg["chain"]
    opp_path = logs_dir / "opportunities.jsonl"
    quarantine_path = logs_dir / f"quarantine_{chain_name}.json"
    pnl_path = logs_dir / f"realized_pnl_{chain_name}.json"
    runtime_path = logs_dir / "runtime.log"

    opp = load_opps_summary(opp_path, max_lines, since_iso=since_iso)

    quarantined = 0
    qdata = read_cached_json(quarantine_path)
    if isinstance(qdata, list):
        quarantined = len(qdata)

    pnl_data = read_cached_json(pnl_path)
    realized_pnl: dict[str, Any] | None = None
    clean_trades_count = clean_wins = clean_reverts = 0
    clean_gross = clean_gas = clean_net = 0.0

    cutoff_iso = since_iso
    cutoff_epoch = parse_iso_epoch(since_iso) if since_iso else 0

    if isinstance(pnl_data, dict):
        raw_trades = pnl_data.get("trades") or []
        clean_trades_list = []
        for t in raw_trades:
            if not isinstance(t, dict):
                continue
            ts = str(t.get("timestamp", ""))
            if cutoff_iso and ts < cutoff_iso:
                continue
            tc = dict(t)
            won = "CONFIRMED" in str(tc.get("status", ""))
            clean_trades_count += 1
            if won:
                clean_wins += 1
            else:
                clean_reverts += 1
            clean_gross += float(tc.get("gross_usd") or 0.0)
            clean_gas += float(tc.get("gas_usd") or 0.0)
            clean_net += float(tc.get("net_usd") or 0.0)
            clean_trades_list.append(tc)

        realized_pnl = {
            "chain": pnl_data.get("chain", chain),
            "cutoff_iso": cutoff_iso,
            "total_trades": clean_trades_count,
            "winning_trades": clean_wins,
            "reverted_trades": clean_reverts,
            "cumulative_gross_usd": round(clean_gross, 6),
            "cumulative_gas_usd": round(clean_gas, 6),
            "cumulative_net_usd": round(clean_net, 6),
            "trades": clean_trades_list,
        }

    latest_scan, native_usd = _extract_latest_scan(runtime_path, chain_cfg["default_native_usd"])
    all_bundles, eoa_submit_counts = get_bundle_history(runtime_path, cutoff_epoch)
    if cutoff_epoch:
        all_bundles = [b for b in all_bundles if int(b.get("epoch") or 0) >= cutoff_epoch]
    recent_bundles = list(reversed(all_bundles[-50:]))
    now_epoch = int(time.time())
    cutoff_24h = now_epoch - 86400
    bundle_series = [
        [b["epoch"], 1 if b["kind"] == "BACKRUN" else 0, b["block_int"], b["gross_val"]]
        for b in all_bundles
        if b["epoch"] >= cutoff_24h
    ]

    executor_addr = read_executor_from_env(logs_dir, chain)
    eoa_meta: list[dict[str, str]] = []
    raw_eoas = read_cached_json(logs_dir / "eoas.json")
    if isinstance(raw_eoas, list):
        for item in raw_eoas:
            if isinstance(item, dict) and item.get("address"):
                eoa_meta.append(
                    {
                        "role": str(item.get("role", "worker")),
                        "address": str(item["address"]),
                    }
                )

    owner_addr = next((e["address"] for e in eoa_meta if e["role"] == "primary"), "") or (
        eoa_meta[0]["address"] if eoa_meta else ""
    )
    onchain_snap = fetch_onchain_state(
        [e["address"] for e in eoa_meta],
        owner_addr,
        executor_addr,
        chain,
        blocking_if_cold=blocking_rpc_if_cold,
    )
    rpc_balances = onchain_snap["eoa_data"]
    eoa_accounts = []
    eoa_native_total = 0.0
    for idx, item in enumerate(eoa_meta):
        addr = item["address"]
        bal_native, nonce = rpc_balances.get(addr.lower(), (0.0, 0))
        eoa_native_total += bal_native
        eoa_accounts.append(
            {
                "index": idx,
                "role": item["role"],
                "address": addr,
                "balance_native": round(bal_native, 6),
                "balance_usd": round(bal_native * native_usd, 2),
                "nonce": nonce,
                "recent_submits": eoa_submit_counts.get(addr.lower(), 0),
            }
        )

    tokens_spec = chain_cfg["hub_tokens"]
    stable_syms = {sym for sym, _, _, is_st in tokens_spec if is_st}
    wnative_sym = chain_cfg["wrapped_native_symbol"]

    owner_tokens = {
        sym: round(val, 6)
        for sym, val in onchain_snap["owner_tokens"].items()
        if val > 1e-8 or sym in ("USDT", "USDC", wnative_sym)
    }
    owner_stables_usd = sum(val for sym, val in owner_tokens.items() if sym in stable_syms)
    owner_wnative = owner_tokens.get(wnative_sym, 0.0)
    owner_tokens_usd = owner_stables_usd + owner_wnative * native_usd

    exec_bnb = float(onchain_snap["exec_bnb"])
    exec_tokens = {
        sym: round(val, 6)
        for sym, val in onchain_snap["exec_tokens"].items()
        if val > 1e-8
    }
    exec_stables_usd = sum(val for sym, val in exec_tokens.items() if sym in stable_syms)
    exec_wnative = exec_tokens.get(wnative_sym, 0.0)
    exec_tokens_usd = exec_stables_usd + exec_wnative * native_usd

    eoa_bnb_usd = eoa_native_total * native_usd
    total_equity_usd = eoa_bnb_usd + owner_tokens_usd + exec_bnb * native_usd + exec_tokens_usd

    baseline = read_cached_json(logs_dir / f"wallet_baseline_{chain}.json")
    delta_bnb = delta_stables_usd = delta_arb_usd = delta_equity_usd = None
    baseline_ts = None
    if isinstance(baseline, dict):
        baseline_ts = baseline.get("timestamp")
        cur_total_bnb = eoa_native_total + owner_wnative + exec_bnb + exec_wnative
        base_total_bnb = float(baseline.get("eoa_bnb_total") or 0.0) + float(baseline.get("owner_wnative") or 0.0) + float(baseline.get("executor_bnb") or 0.0)
        cur_total_stables = owner_stables_usd + exec_stables_usd
        base_total_stables = float(baseline.get("owner_stables_usd") or 0.0) + float(baseline.get("executor_stables_usd") or 0.0)
        delta_bnb = round(cur_total_bnb - base_total_bnb, 6)
        delta_stables_usd = round(cur_total_stables - base_total_stables, 4)
        delta_arb_usd = round(delta_bnb * native_usd + delta_stables_usd, 4)
        base_eq_usd = float(baseline.get("total_equity_usd") or 0.0)
        if base_eq_usd > 0:
            delta_equity_usd = round(total_equity_usd - base_eq_usd, 4)

    onchain_equity = {
        "eoa_bnb_total": round(eoa_native_total, 6),
        "eoa_bnb_usd": round(eoa_bnb_usd, 4),
        "owner_tokens": owner_tokens,
        "owner_stables_usd": round(owner_stables_usd, 4),
        "owner_tokens_usd": round(owner_tokens_usd, 4),
        "executor_bnb": round(exec_bnb, 6),
        "executor_tokens": exec_tokens,
        "executor_tokens_usd": round(exec_tokens_usd, 4),
        "total_equity_usd": round(total_equity_usd, 4),
        "baseline_ts": baseline_ts,
        "delta_bnb": delta_bnb,
        "delta_stables_usd": delta_stables_usd,
        "delta_arb_usd": delta_arb_usd,
        "delta_equity_usd": delta_equity_usd,
    }

    topo_cache_sig = _stat_sig(logs_dir / f"pools_cache_{chain_name}.json")
    topology_mtime_ms = int(topo_cache_sig[2] // 1_000_000) if topo_cache_sig else 0

    return {
        "msg_type": "state",
        "chain": chain_name,
        "chain_meta": {
            "chain": chain_name,
            "native_symbol": chain_cfg["native_symbol"],
            "explorer_tx_url": chain_cfg["explorer_tx_url"],
            "phalcon_chain": chain_cfg["phalcon_chain"],
        },
        "server_time": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "server_epoch": now_epoch,
        "topology_mtime_ms": topology_mtime_ms,
        "file_kb": opp["file_kb"],
        "sampled": opp["sampled"],
        "sim_ok_count": opp["sim_ok_count"],
        "sim_fail_count": opp["sim_fail_count"],
        "best_sim_usd": opp["best_sim_usd"],
        "best_est_usd": opp["best_est_usd"],
        "block_min": opp["block_min"],
        "block_max": opp["block_max"],
        "latest_ts": opp["latest_ts"],
        "quarantined": quarantined,
        "realized_pnl": realized_pnl,
        "onchain_equity": onchain_equity,
        "top_strategies": opp["top_strategies"],
        "top_errors": opp["top_errors"],
        "recent": opp["recent"],
        "latest_scan": latest_scan,
        "executor": executor_addr,
        "native_usd": native_usd,
        "eoa_accounts": eoa_accounts,
        "recent_bundles": recent_bundles,
        "total_bundles_indexed": len(all_bundles),
        "bundle_series": bundle_series,
        "vm": read_vm_stats(),
    }


def _sse_client_closed(conn: socket.socket) -> bool:
    try:
        readable, _, _ = select.select([conn], [], [], 0)
        if readable:
            data = conn.recv(1, socket.MSG_PEEK)
            if not data:
                return True
    except OSError:
        return True
    return False


class StateBroadcaster:
    """Coalesces log/RPC polling and broadcasts delta SSE frames only when clients are connected."""

    def __init__(self, logs_dir: Path, chain: str, max_lines: int, since_iso: str = "") -> None:
        self.logs_dir = logs_dir
        self.chain = chain
        self.max_lines = max_lines
        self.since_iso = since_iso
        self._state_lock = threading.Lock()
        self._cached_state: dict[str, Any] | None = None
        self._cached_ts: float = 0.0

        self._sse_cond = threading.Condition()
        self._sse_clients: set[int] = set()
        self._wake_event = threading.Event()
        self._stop_event = threading.Event()
        self._broadcaster_started = False
        self._next_client_id = 0
        self._sse_seq = 0
        self._sse_frame_bytes = b""
        self._last_data_sig: tuple[Any, ...] | None = None
        self._last_tick_sig: tuple[Any, ...] | None = None

    def get_state(self, force: bool = False, blocking_rpc_if_cold: bool = False) -> dict[str, Any]:
        now = time.time()
        with self._state_lock:
            if not force and self._cached_state is not None and (now - self._cached_ts) < 1.2:
                return self._cached_state
        fresh = load_summary(
            self.logs_dir,
            self.chain,
            self.max_lines,
            blocking_rpc_if_cold=blocking_rpc_if_cold,
            since_iso=self.since_iso,
        )
        with self._state_lock:
            self._cached_state = fresh
            self._cached_ts = time.time()
            return fresh

    def _compute_sigs(self, s: dict[str, Any]) -> tuple[tuple[Any, ...], tuple[Any, ...]]:
        opp_sig = _stat_sig(self.logs_dir / "opportunities.jsonl")
        pnl_sig = _stat_sig(self.logs_dir / f"realized_pnl_{self.chain}.json")
        q_sig = _stat_sig(self.logs_dir / f"quarantine_{self.chain}.json")
        topo_sig = _stat_sig(self.logs_dir / f"pools_cache_{self.chain}.json")
        eoa_sig = tuple(
            (e["address"], e["balance_native"], e["nonce"], e["recent_submits"])
            for e in s.get("eoa_accounts", [])
        )
        eq = s.get("onchain_equity") or {}
        data_sig = (
            opp_sig,
            pnl_sig,
            q_sig,
            topo_sig,
            s.get("quarantined"),
            s.get("total_bundles_indexed"),
            eoa_sig,
            eq.get("owner_tokens_usd"),
            eq.get("executor_tokens_usd"),
            eq.get("executor_bnb"),
            s.get("executor"),
            int(s.get("server_epoch", 0)) // 60,
        )
        sc = s.get("latest_scan") or {}
        vm = s.get("vm") or {}
        tick_sig = (
            sc.get("block"),
            sc.get("blk_ms"),
            sc.get("solve_us"),
            sc.get("total_pools"),
            sc.get("txs"),
            vm.get("load_1m"),
            vm.get("mem_used_mb"),
        )
        return data_sig, tick_sig

    def register_sse_client(self) -> tuple[int, int, bytes]:
        init_state = self.get_state(force=False)
        init_frame = f"data: {json.dumps(init_state, ensure_ascii=False)}\n\n".encode("utf-8")
        data_sig, tick_sig = self._compute_sigs(init_state)
        with self._sse_cond:
            if self._last_data_sig is None:
                self._last_data_sig = data_sig
                self._last_tick_sig = tick_sig
            if not self._broadcaster_started:
                self._broadcaster_started = True
                threading.Thread(
                    target=self._sse_broadcast_loop,
                    name="pyrite-sse-broadcaster",
                    daemon=True,
                ).start()
            self._next_client_id += 1
            cid = self._next_client_id
            self._sse_clients.add(cid)
            self._wake_event.set()
            self._sse_cond.notify_all()
            return cid, self._sse_seq, init_frame

    def unregister_sse_client(self, client_id: int) -> None:
        with self._sse_cond:
            self._sse_clients.discard(client_id)
            self._sse_cond.notify_all()

    def wait_next_sse_frame(self, last_seq: int, timeout: float = 10.0) -> tuple[int, bytes | None]:
        with self._sse_cond:
            if self._sse_seq == last_seq and not self._stop_event.is_set():
                self._sse_cond.wait(timeout=timeout)
            if self._sse_seq != last_seq and self._sse_frame_bytes:
                return self._sse_seq, self._sse_frame_bytes
            return last_seq, None

    def _sse_broadcast_loop(self) -> None:
        while not self._stop_event.is_set():
            with self._sse_cond:
                active = len(self._sse_clients) > 0
            if not active:
                self._wake_event.wait(timeout=2.0)
                self._wake_event.clear()
                continue

            s = self.get_state(force=True)
            data_sig, tick_sig = self._compute_sigs(s)
            frame: bytes | None = None
            if data_sig != self._last_data_sig:
                self._last_data_sig = data_sig
                self._last_tick_sig = tick_sig
                frame = f"data: {json.dumps(s, ensure_ascii=False)}\n\n".encode("utf-8")
            elif tick_sig != self._last_tick_sig:
                self._last_tick_sig = tick_sig
                tick = {
                    "msg_type": "tick",
                    "server_time": s["server_time"],
                    "server_epoch": s["server_epoch"],
                    "latest_scan": s["latest_scan"],
                    "vm": s["vm"],
                }
                frame = f"data: {json.dumps(tick, ensure_ascii=False)}\n\n".encode("utf-8")

            if frame is not None:
                with self._sse_cond:
                    self._sse_seq += 1
                    self._sse_frame_bytes = frame
                    self._sse_cond.notify_all()

            self._stop_event.wait(0.5)

    def set_chain(self, chain: str) -> None:
        with self._state_lock:
            self.chain = chain
            self._cached_state = None
            self._cached_ts = 0.0
        with self._sse_cond:
            self._last_data_sig = None
            self._wake_event.set()
            self._sse_cond.notify_all()

    def shutdown(self) -> None:
        self._stop_event.set()
        self._wake_event.set()
        with self._sse_cond:
            self._sse_cond.notify_all()


class DashboardHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def is_loopback_host(host: str) -> bool:
    return host.strip().lower() in ("127.0.0.1", "localhost", "::1", "::ffff:127.0.0.1")


def _find_forbidden_key(obj: Any, path: str = "") -> str | None:
    if isinstance(obj, dict):
        for k, v in obj.items():
            kl = str(k).strip().lower()
            cur = f"{path}.{k}" if path else str(k)
            if kl in FORBIDDEN_CONFIG_KEYS:
                return cur
            sub = _find_forbidden_key(v, cur)
            if sub:
                return sub
    elif isinstance(obj, list):
        for idx, item in enumerate(obj):
            sub = _find_forbidden_key(item, f"{path}[{idx}]")
            if sub:
                return sub
    return None


def validate_pyrite_config(data: Any) -> tuple[bool, str]:
    if not isinstance(data, dict):
        return False, "Config root must be a JSON object"
    bad_key = _find_forbidden_key(data)
    if bad_key:
        return (
            False,
            f"Forbidden secret key '{bad_key}' in pyrite.json; use the write-only Wallet tab (.env chmod 0600)",
        )
    chain = str(data.get("chain") or "").strip()
    if not chain or not re.fullmatch(r"[A-Za-z0-9_.\-/]+", chain):
        return False, "Field 'chain' must be a non-empty chain name (e.g. eth, bsc, base)"

    mode = data.get("mode") or {}
    if not isinstance(mode, dict):
        return False, "Field 'mode' must be an object"
    min_profit = mode.get("min_profit_usd", 0.001)
    if not isinstance(min_profit, (int, float)) or min_profit < 0:
        return False, "Field 'mode.min_profit_usd' must be a non-negative number"
    target_usd = mode.get("target_usd", 50.0)
    if not isinstance(target_usd, (int, float)) or target_usd <= 0:
        return False, "Field 'mode.target_usd' must be positive"

    rpcs = data.get("rpcs") or {}
    if not isinstance(rpcs, dict):
        return False, "Field 'rpcs' must be an object"
    for rk in ("read", "ws", "broadcast", "bundle"):
        rv = rpcs.get(rk, [])
        if not isinstance(rv, list) or not all(isinstance(u, str) for u in rv):
            return False, f"Field 'rpcs.{rk}' must be an array of strings"

    solver = data.get("solver") or {}
    if not isinstance(solver, dict):
        return False, "Field 'solver' must be an object"
    smode = str(solver.get("mode") or "cpu").strip().lower()
    if smode not in ("cpu", "gpu", "hybrid"):
        return False, "Field 'solver.mode' must be one of cpu, gpu, hybrid"

    execs = data.get("executors") or {}
    if not isinstance(execs, dict):
        return False, "Field 'executors' must be an object"
    for ch, addr in execs.items():
        if not isinstance(addr, str):
            return False, f"Field 'executors.{ch}' must be a string"
        s = addr.strip()
        if s and not re.fullmatch(r"0x[0-9a-fA-F]{40}", s):
            return False, f"Field 'executors.{ch}' must be a valid 20-byte hex address or empty"

    return True, ""


def _deep_merge_config(base: dict[str, Any], override: dict[str, Any]) -> dict[str, Any]:
    out = json.loads(json.dumps(base))
    for k, v in override.items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k].update(v)
        else:
            out[k] = v
    return out


def load_pyrite_config(config_path: Path, example_path: Path | None = None) -> dict[str, Any]:
    base = json.loads(json.dumps(DEFAULT_PYRITE_CONFIG))
    for cand in (example_path, config_path):
        if cand is not None and cand.is_file():
            try:
                parsed = json.loads(cand.read_text(encoding="utf-8"))
                if isinstance(parsed, dict):
                    base = _deep_merge_config(base, parsed)
            except Exception:
                pass
    return base


def save_pyrite_config(config_path: Path, data: dict[str, Any]) -> None:
    ok, err = validate_pyrite_config(data)
    if not ok:
        raise ValueError(err)
    config_path.parent.mkdir(parents=True, exist_ok=True)
    tmp_path = config_path.with_name(f".{config_path.name}.tmp.{os.getpid()}")
    tmp_path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    os.replace(tmp_path, config_path)


def find_pyrite_binary(repo_root: Path) -> Path | None:
    for rel in ("target/release/pyrite", "target/debug/pyrite"):
        cand = repo_root / rel
        if cand.is_file() and os.access(cand, os.X_OK):
            return cand
    return None


def _parse_env_flags(env_path: Path) -> dict[str, str]:
    out: dict[str, str] = {}
    if not env_path.is_file():
        return out
    try:
        for line in env_path.read_text(encoding="utf-8").splitlines():
            s = line.strip()
            if not s or s.startswith("#") or "=" not in s:
                continue
            k, v = s.split("=", 1)
            out[k.strip()] = v.strip().strip('"').strip("'")
    except Exception:
        pass
    return out


def upsert_env_secret(env_path: Path, key: str, value: str) -> None:
    lines: list[str] = []
    if env_path.is_file():
        lines = env_path.read_text(encoding="utf-8").splitlines()
    prefix = f"{key}="
    replaced = False
    new_lines: list[str] = []
    for ln in lines:
        if ln.startswith(prefix):
            new_lines.append(f"{key}={value}")
            replaced = True
        else:
            new_lines.append(ln)
    if not replaced:
        new_lines.append(f"{key}={value}")
    env_path.parent.mkdir(parents=True, exist_ok=True)
    env_path.write_text("\n".join(new_lines) + "\n", encoding="utf-8")
    try:
        os.chmod(env_path, 0o600)
    except OSError:
        pass


def _derive_address_via_binary(env_path: Path, bin_path: Path | None, chain: str) -> str:
    if bin_path is None or not bin_path.is_file():
        return ""
    env_flags = _parse_env_flags(env_path)
    if not env_flags.get("PRIVATE_KEY", "").strip():
        return ""
    try:
        proc = subprocess.run(
            [str(bin_path), "init-wallet", "--chain", chain],
            cwd=str(env_path.parent),
            capture_output=True,
            text=True,
            timeout=8,
            check=False,
        )
        m = re.search(r"Public Address\s*:\s*(0x[0-9a-fA-F]{40})", proc.stdout)
        if m:
            return m.group(1)
    except Exception:
        pass
    return ""


def _resolve_address_from_logs(logs_dir: Path) -> str:
    raw_eoas = read_cached_json(logs_dir / "eoas.json")
    if isinstance(raw_eoas, list):
        for item in raw_eoas:
            if isinstance(item, dict) and item.get("role") == "primary" and item.get("address"):
                return str(item["address"])
        for item in raw_eoas:
            if isinstance(item, dict) and item.get("address"):
                return str(item["address"])
    runtime_path = logs_dir / "runtime.log"
    for ln in reversed(tail_lines(runtime_path, 200, chunk_limit=64 * 1024)):
        clean = ANSI_RE.sub("", ln)
        m = WALLET_ADDR_RE.search(clean)
        if m:
            return m.group(1)
    return ""


def read_wallet_status(
    env_path: Path,
    logs_dir: Path,
    bin_path: Path | None = None,
    chain: str = "eth",
) -> dict[str, Any]:
    """Return non-secret wallet presence and public address only (never reads or exposes keys)."""
    flags = _parse_env_flags(env_path)
    has_pk = bool(flags.get("PRIVATE_KEY", "").strip())
    has_fb = bool(flags.get("FLASHBOTS_AUTH_KEY", "").strip())
    extras_raw = flags.get("EXTRA_PRIVATE_KEYS", "").strip()
    extra_count = len([x for x in extras_raw.split(",") if x.strip()]) if extras_raw else 0
    addr = _resolve_address_from_logs(logs_dir)
    if not addr and has_pk:
        addr = _derive_address_via_binary(env_path, bin_path, chain)
    mode_ok = True
    if env_path.is_file():
        try:
            mode_ok = (env_path.stat().st_mode & 0o077) == 0
        except OSError:
            mode_ok = False
    return {
        "address": addr,
        "has_private_key": has_pk,
        "has_flashbots_auth_key": has_fb,
        "extra_keys_count": extra_count,
        "env_exists": env_path.is_file(),
        "env_mode_0600": mode_ok,
    }


def generate_burner_wallet(
    env_path: Path,
    logs_dir: Path,
    bin_path: Path | None,
    chain: str,
    force: bool = False,
) -> dict[str, Any]:
    flags = _parse_env_flags(env_path)
    if flags.get("PRIVATE_KEY", "").strip() and not force:
        raise ValueError("PRIVATE_KEY already exists in .env; pass force=true to replace burner wallet")
    pk_hex = "0x" + secrets.token_hex(32)
    upsert_env_secret(env_path, "PRIVATE_KEY", pk_hex)
    status = read_wallet_status(env_path, logs_dir, bin_path, chain)
    return {"ok": True, "created": True, "wallet": status}


def import_wallet_key(
    env_path: Path,
    logs_dir: Path,
    key_name: str,
    raw_key: str,
    bin_path: Path | None,
    chain: str,
) -> dict[str, Any]:
    allowed = {"PRIVATE_KEY", "FLASHBOTS_AUTH_KEY"}
    if key_name not in allowed:
        raise ValueError(f"Unsupported key name '{key_name}'; allowed: PRIVATE_KEY, FLASHBOTS_AUTH_KEY")
    cleaned = raw_key.strip()
    if not re.fullmatch(r"(0x)?[0-9a-fA-F]{64}", cleaned):
        raise ValueError("Key must be a valid 32-byte (64 hex chars) private key")
    hex_body = cleaned.lower().removeprefix("0x")
    if hex_body == "0" * 64:
        raise ValueError("Key cannot be all zeros")
    upsert_env_secret(env_path, key_name, "0x" + hex_body)
    status = read_wallet_status(env_path, logs_dir, bin_path, chain)
    return {"ok": True, "key_name": key_name, "configured": True, "wallet": status}


class ProcessManager:
    """Thread-safe local subprocess controller for starting/stopping Pyrite from Web Control."""

    def __init__(self, repo_root: Path, config_path: Path, logs_dir: Path) -> None:
        self.repo_root = repo_root
        self.config_path = config_path
        self.logs_dir = logs_dir
        self._lock = threading.Lock()
        self._proc: subprocess.Popen[bytes] | None = None
        self._log_file: Any = None
        self._action: str = ""
        self._command: list[str] = []
        self._started_at: str = ""
        self._last_exit_code: int | None = None

    def _refresh_locked(self) -> None:
        if self._proc is not None:
            rc = self._proc.poll()
            if rc is not None:
                self._last_exit_code = rc
                self._proc = None
                if self._log_file is not None:
                    try:
                        self._log_file.close()
                    except Exception:
                        pass
                    self._log_file = None

    def status(self) -> dict[str, Any]:
        with self._lock:
            self._refresh_locked()
            running = self._proc is not None
            pid = self._proc.pid if running and self._proc is not None else None
            return {
                "running": running,
                "pid": pid,
                "action": self._action if running else "",
                "command": " ".join(self._command) if self._command else "",
                "started_at": self._started_at if running else "",
                "last_exit_code": self._last_exit_code,
                "binary_found": find_pyrite_binary(self.repo_root) is not None,
            }

    def start(self, action: str, chain: str, confirm_live: bool = False) -> dict[str, Any]:
        if action not in ("scan", "dry_run", "live"):
            raise ValueError("Action must be one of: scan, dry_run, live")
        if action == "live" and not confirm_live:
            raise ValueError("Live execution requires explicit confirm_live=true acknowledgement")
        with self._lock:
            self._refresh_locked()
            if self._proc is not None:
                raise RuntimeError(f"Process already running (pid={self._proc.pid}, action={self._action})")

            bin_path = find_pyrite_binary(self.repo_root)
            base_cmd = (
                [str(bin_path)]
                if bin_path is not None
                else ["cargo", "run", "--release", "--"]
            )
            cfg_args = ["--config", str(self.config_path)] if self.config_path.is_file() else []
            if action == "scan":
                sub_args = ["scan", "--chain", chain, "--blocks", "20"]
            elif action == "dry_run":
                sub_args = ["run", "--chain", chain, "--dry-run"]
            else:
                sub_args = ["run", "--chain", chain]
            cmd = base_cmd + cfg_args + sub_args

            self.logs_dir.mkdir(parents=True, exist_ok=True)
            log_path = self.logs_dir / "runtime.log"
            lf = log_path.open("ab")
            env = os.environ.copy()
            env.setdefault("RUST_LOG", "info")
            self._proc = subprocess.Popen(
                cmd,
                cwd=str(self.repo_root),
                stdout=lf,
                stderr=subprocess.STDOUT,
                env=env,
            )
            self._log_file = lf
            self._action = action
            self._command = cmd
            self._started_at = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
            self._last_exit_code = None
            return {
                "running": True,
                "pid": self._proc.pid,
                "action": self._action,
                "command": " ".join(cmd),
                "started_at": self._started_at,
                "last_exit_code": None,
                "binary_found": bin_path is not None,
            }

    def stop(self) -> dict[str, Any]:
        with self._lock:
            self._refresh_locked()
            if self._proc is None:
                return self.status()
            proc = self._proc
            try:
                proc.terminate()
                proc.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait(timeout=2.0)
            except Exception:
                pass
            self._refresh_locked()
        return self.status()


def render_cli(s: dict[str, Any]) -> str:
    out = []
    out.append(f"=== Pyrite | EVM Arbitrage Dashboard ({s['chain'].upper()}) ===")
    out.append(
        f"Log: {s['file_kb']} KB | Sampled: {s['sampled']} | "
        f"Sim OK: {s['sim_ok_count']} | Sim Fail: {s['sim_fail_count']} | "
        f"Quarantined Pools: {s['quarantined']}"
    )
    if s["latest_scan"]:
        sc = s["latest_scan"]
        out.append(
            f"Live Block: {sc['block']} | Pools: {sc['total_pools']} | "
            f"solve_us: {sc['solve_us']} | blk_ms: {sc['blk_ms']}"
        )
    if s["realized_pnl"] is not None:
        p = s["realized_pnl"]
        out.append(
            f"Realized PnL: trades={p.get('total_trades',0)} win={p.get('winning_trades',0)} "
            f"net=${p.get('cumulative_net_usd',0):.4f} "
            f"(gross=${p.get('cumulative_gross_usd',0):.4f}, gas=${p.get('cumulative_gas_usd',0):.4f})"
        )
    out.append("\n[Top Strategies] " + ", ".join(f"{k}:{v}" for k, v in s["top_strategies"]))
    out.append("[Top Errors]     " + ", ".join(f"{k}:{v}" for k, v in s["top_errors"]))
    out.append("\n--- Recent Opportunities (latest 15) ---")
    out.append(f"{'Block':<10} {'Strategy':<20} {'Token':<6} {'Borrow':>10} {'Est$':>9} {'Sim':<5} {'Err/Route'}")
    for r in s["recent"][:15]:
        sim = "OK" if r.get("sim_ok") else "FAIL"
        detail = f"${r.get('sim_net_usd', 0):.3f}" if r.get("sim_ok") else r.get("err_short", "")
        route = (r.get("route") or "")[:45]
        out.append(
            f"{str(r.get('block', '')):<10} "
            f"{str(r.get('strategy', ''))[:19]:<20} "
            f"{str(r.get('base_token', ''))[:5]:<6} "
            f"{float(r.get('borrow_human') or 0):>10.2f} "
            f"{float(r.get('est_net_usd') or 0):>9.2f} "
            f"{sim:<5} "
            f"{detail} | {route}"
        )
    return "\n".join(out)


WEB_HTML = r"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Pyrite | EVM Arbitrage Terminal</title>
<style>
  :root {
    --bg: #070b12;
    --panel: #0d1524;
    --panel-border: rgba(245, 158, 11, 0.22);
    --text: #e2e8f0;
    --muted: #7c8da6;
    --cyan: #38bdf8;
    --emerald: #34d399;
    --rose: #fb7185;
    --amber: #fbbf24;
    --violet: #a78bfa;
  }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    padding: 20px 24px 36px;
    font-family: Inter, -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
    color: var(--text);
    background:
      radial-gradient(900px 500px at 10% -10%, rgba(56,189,248,0.10), transparent 60%),
      radial-gradient(800px 500px at 95% 0%, rgba(52,211,153,0.08), transparent 55%),
      var(--bg);
    min-height: 100vh;
  }
  .mono {
    font-family: "JetBrains Mono", "SF Mono", Menlo, Consolas, monospace;
    font-variant-numeric: tabular-nums;
  }
  .topbar {
    display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between;
    gap: 12px; margin-bottom: 18px;
  }
  .brand { display: flex; align-items: center; gap: 12px; }
  .pulse {
    width: 10px; height: 10px; border-radius: 50%; background: var(--emerald);
    box-shadow: 0 0 8px var(--emerald);
    transition: background 0.25s ease, box-shadow 0.25s ease;
  }
  .title { font-size: 18px; font-weight: 700; letter-spacing: 0.6px; }
  .badge {
    font-size: 11px; padding: 3px 9px; border-radius: 999px;
    border: 1px solid var(--panel-border); background: rgba(56,189,248,0.08); color: var(--cyan);
  }
  .controls { display: flex; align-items: center; gap: 10px; font-size: 12px; color: var(--muted); }
  .btn {
    cursor: pointer; border: 1px solid var(--panel-border); background: var(--panel);
    color: var(--text); padding: 6px 12px; border-radius: 8px; font-size: 12px;
    transition: border-color .15s ease, color .15s ease, background .15s ease;
  }
  .btn:hover { border-color: var(--amber); color: var(--amber); }
  .btn.active { background: rgba(245,158,11,0.16); border-color: var(--amber); color: var(--amber); }
  .btn.danger { border-color: rgba(251,113,133,0.45); color: var(--rose); }
  .btn.danger:hover { background: rgba(251,113,133,0.14); }
  .btn.ok { border-color: rgba(52,211,153,0.45); color: var(--emerald); }
  .btn.ok:hover { background: rgba(52,211,153,0.14); }
  .tab-panel { display: none; }
  .tab-panel.active { display: block; }
  .form-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(260px, 1fr)); gap: 14px; }
  .field { display: flex; flex-direction: column; gap: 5px; margin-bottom: 10px; }
  .field label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.6px; color: var(--muted); }
  .field input, .field select, .field textarea {
    background: #070b12; border: 1px solid rgba(148,163,184,0.22); color: var(--text);
    border-radius: 8px; padding: 7px 10px; font-size: 12.5px; outline: none;
    font-family: "JetBrains Mono", "SF Mono", Menlo, Consolas, monospace;
  }
  .field input:focus, .field select:focus, .field textarea:focus { border-color: var(--amber); }
  .banner {
    border: 1px solid rgba(251,191,36,0.35); background: rgba(251,191,36,0.08);
    color: #fde68a; border-radius: 10px; padding: 10px 14px; font-size: 12.5px; line-height: 1.5; margin-bottom: 14px;
  }
  .status-msg { font-size: 12px; min-height: 18px; margin-top: 8px; }

  .kpi-grid {
    display: grid; grid-template-columns: repeat(auto-fit, minmax(190px, 1fr));
    gap: 14px; margin-bottom: 16px;
  }
  .card {
    background: var(--panel);
    border: 1px solid var(--panel-border);
    border-radius: 14px;
    padding: 14px 16px;
    box-shadow: 0 6px 20px rgba(0,0,0,0.22);
    contain: layout style;
  }
  .kpi-label { font-size: 11px; text-transform: uppercase; letter-spacing: 0.7px; color: var(--muted); }
  .kpi-val { font-size: 24px; font-weight: 700; margin-top: 6px; }
  .kpi-sub { font-size: 12px; color: var(--muted); margin-top: 4px; }
  .pos { color: var(--emerald); }
  .neg { color: var(--rose); }
  .cy { color: var(--cyan); }

  .main-grid {
    display: grid; grid-template-columns: 1.35fr 1fr; gap: 14px; margin-bottom: 16px;
  }
  @media (max-width: 1050px) { .main-grid { grid-template-columns: 1fr; } }

  .sec-head {
    display: flex; align-items: center; justify-content: space-between;
    margin-bottom: 10px; gap: 8px;
  }
  .sec-title { font-size: 13px; font-weight: 600; letter-spacing: 0.4px; color: #cbd5e1; }

  .chart-wrap { width: 100%; height: 150px; }
  svg { width: 100%; height: 100%; overflow: visible; }

  .bar-row { margin-bottom: 8px; }
  .bar-meta { display: flex; justify-content: space-between; font-size: 12px; margin-bottom: 3px; }
  .bar-track { height: 6px; background: rgba(148,163,184,0.12); border-radius: 99px; overflow: hidden; }
  .bar-fill { height: 100%; border-radius: 99px; background: linear-gradient(90deg, var(--cyan), var(--violet)); }
  .bar-fill.err { background: linear-gradient(90deg, var(--amber), var(--rose)); }

  .table-wrap { overflow-x: auto; max-height: 360px; overflow-y: auto; contain: layout style; }
  table { width: 100%; border-collapse: collapse; font-size: 12.5px; }
  th {
    position: sticky; top: 0; background: #0e1624; color: var(--muted);
    font-weight: 600; text-align: left; padding: 8px 10px;
    border-bottom: 1px solid rgba(148,163,184,0.16);
  }
  td {
    padding: 7px 10px; border-bottom: 1px solid rgba(148,163,184,0.08);
    white-space: nowrap;
  }
  tr:hover td { background: rgba(56,189,248,0.05); }
  .pill {
    display: inline-block; padding: 2px 7px; border-radius: 6px; font-size: 11px; font-weight: 600;
  }
  .pill.ok { background: rgba(52,211,153,0.15); color: var(--emerald); }
  .pill.fail { background: rgba(251,113,133,0.15); color: var(--rose); }
  .pill.warn { background: rgba(251,191,36,0.15); color: var(--amber); }
  a.tx { color: var(--cyan); text-decoration: none; }
  a.tx:hover { text-decoration: underline; }

  .toolbar { display: flex; flex-wrap: wrap; gap: 8px; align-items: center; }
  input.search {
    background: #070b12; border: 1px solid var(--panel-border); color: var(--text);
    border-radius: 8px; padding: 5px 10px; font-size: 12px; outline: none; width: 210px;
  }
  input.search:focus { border-color: var(--cyan); }

  .swatch { display: inline-block; width: 9px; height: 9px; border-radius: 2px; margin-right: 5px; vertical-align: middle; }
  .b-strip { display: flex; flex-wrap: wrap; gap: 14px; align-items: center; font-size: 11.5px; color: var(--muted); margin-bottom: 8px; }
  .b-tip {
    position: absolute; display: none; top: 8px; background: rgba(7,11,18,0.96);
    border: 1px solid rgba(56,189,248,0.45); border-radius: 8px; padding: 7px 10px;
    font-size: 11px; line-height: 1.5; pointer-events: none; z-index: 6;
    box-shadow: 0 8px 22px rgba(0,0,0,0.48);
  }

  .topo-grid {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 365px;
    gap: 14px;
    align-items: stretch;
  }
  @media (max-width: 1120px) { .topo-grid { grid-template-columns: 1fr; } }
  .topo-stage {
    position: relative;
    height: 590px;
    min-height: 440px;
    border-radius: 10px;
    overflow: hidden;
    border: 1px solid rgba(148,163,184,0.16);
    background:
      radial-gradient(620px 440px at 44% 50%, rgba(56,189,248,0.06), transparent 72%),
      #060a11;
  }
  #topoCanvas { width: 100%; height: 100%; display: block; cursor: grab; touch-action: none; }
  #topoCanvas.dragging { cursor: grabbing; }
  #topoCanvas.clickable { cursor: pointer; }
  .topo-inspector {
    position: absolute;
    top: 12px;
    right: 12px;
    width: 264px;
    max-width: calc(100% - 24px);
    background: rgba(13,21,36,0.94);
    border: 1px solid rgba(56,189,248,0.42);
    border-radius: 10px;
    padding: 10px 12px;
    font-size: 12px;
    box-shadow: 0 10px 26px rgba(0,0,0,0.55);
    z-index: 5;
  }
  .topo-inspector[hidden] { display: none; }
  .topo-kv { display: flex; justify-content: space-between; gap: 8px; margin-top: 5px; font-size: 11.5px; }
  .topo-kv span:first-child { color: var(--muted); }
  .topo-legend {
    position: absolute;
    left: 12px;
    bottom: 12px;
    display: flex;
    flex-wrap: wrap;
    gap: 12px;
    align-items: center;
    background: rgba(7,11,18,0.86);
    border: 1px solid rgba(148,163,184,0.18);
    border-radius: 999px;
    padding: 5px 12px;
    font-size: 11px;
    color: var(--muted);
    pointer-events: none;
  }
  .topo-leg-item { display: inline-flex; align-items: center; gap: 5px; }
  .topo-leg-dot { width: 8px; height: 8px; border-radius: 50%; display: inline-block; }
  .topo-route-bar {
    margin-top: 10px;
    padding: 10px 12px;
    border-radius: 10px;
    border: 1px solid rgba(148,163,184,0.14);
    background: rgba(7,11,18,0.72);
  }
  .topo-route-meta { font-size: 12px; color: var(--muted); margin-bottom: 7px; }
  .topo-legs { display: flex; flex-wrap: wrap; gap: 6px; align-items: center; }
  .topo-hop {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 3px 8px;
    border-radius: 6px;
    font-size: 11px;
    background: rgba(56,189,248,0.09);
    border: 1px solid rgba(56,189,248,0.22);
    color: var(--text);
  }
  .topo-feed-list {
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-height: 590px;
    overflow-y: auto;
    padding-right: 2px;
  }
  .topo-card {
    display: block;
    width: 100%;
    text-align: left;
    padding: 10px 11px;
    border-radius: 10px;
    border: 1px solid rgba(148,163,184,0.16);
    background: rgba(7,11,18,0.68);
    color: var(--text);
    cursor: pointer;
    transition: border-color .15s ease, background .15s ease;
  }
  .topo-card:hover { border-color: var(--cyan); background: rgba(56,189,248,0.06); }
  .topo-card.selected { border-color: var(--amber); background: rgba(245,158,11,0.11); }
  .topo-card-top { display: flex; justify-content: space-between; align-items: center; gap: 6px; margin-bottom: 4px; }
  .topo-card-sub { font-size: 11px; color: var(--muted); }
  .topo-card-route { font-size: 11px; color: #93c5fd; margin-top: 4px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .topo-feed-foot {
    margin-top: 10px;
    padding-top: 8px;
    border-top: 1px solid rgba(148,163,184,0.12);
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: var(--muted);
  }
</style>
</head>
<body>
  <div class="topbar">
    <div class="brand">
      <div class="pulse" id="pulseDot"></div>
      <div class="title"><span style="color:var(--amber)">PYRITE</span> | <span data-i18n="brand_sub">EVM ARBITRAGE TERMINAL</span></div>
      <span class="badge mono" id="chainBadge">ETH</span>
      <span class="badge mono" id="vmBadge" data-i18n="vm_local">VM Local</span>
      <span class="badge mono" id="procBadge" data-i18n="proc_idle">PROC IDLE</span>
    </div>
    <div class="controls">
      <button class="btn active" data-tab="monitor" data-i18n="tab_monitor" onclick="switchTab('monitor')">Monitor</button>
      <button class="btn" data-tab="topology" data-i18n="tab_topology" onclick="switchTab('topology')">Topology</button>
      <button class="btn" data-tab="config" data-i18n="tab_config" onclick="switchTab('config')">Config &amp; Control</button>
      <button class="btn" data-tab="wallet" data-i18n="tab_wallet" onclick="switchTab('wallet')">Wallet</button>
      <span class="mono" id="syncTime" data-i18n="connecting_sse">Connecting SSE...</span>
      <button class="btn active" id="togglePoll" onclick="togglePolling()">LIVE SSE</button>
      <button class="btn" data-i18n="btn_refresh" onclick="fetchState(true); loadControlState(); fetchTopology(true);">Refresh</button>
      <select class="btn" id="langSelect" aria-label="Language" onchange="setLang(this.value)">
        <option value="en">EN</option>
        <option value="zh">中文</option>
      </select>
    </div>
  </div>

  <div id="tab-monitor" class="tab-panel active">

  <div class="kpi-grid">
    <div class="card">
      <div class="kpi-label" data-i18n="kpi_net_pnl">Realized Net PnL</div>
      <div class="kpi-val mono" id="kpiNet">$0.0000</div>
      <div class="kpi-sub mono" id="kpiNetSub" data-i18n="kpi_net_sub_init">Gross $0.00 | Gas $0.00</div>
    </div>
    <div class="card">
      <div class="kpi-label" data-i18n="kpi_equity">On-Chain Total Equity</div>
      <div class="kpi-val mono cy" id="kpiEquity">--</div>
      <div class="kpi-sub mono" id="kpiEquitySub" data-i18n="kpi_equity_sub_init">Native -- | Stables -- | Exec --</div>
    </div>
    <div class="card">
      <div class="kpi-label" data-i18n="kpi_trades">Executed Trades</div>
      <div class="kpi-val mono cy" id="kpiTrades">0</div>
      <div class="kpi-sub mono" id="kpiWinRate" data-i18n="kpi_win_rate_init">Win rate --</div>
    </div>
    <div class="card">
      <div class="kpi-label" data-i18n="kpi_block">Live Block &amp; Speed</div>
      <div class="kpi-val mono" id="kpiBlock">--</div>
      <div class="kpi-sub mono" id="kpiSpeed" data-i18n="kpi_speed_init">solve -- us | blk -- ms</div>
    </div>
    <div class="card">
      <div class="kpi-label" data-i18n="kpi_graph">Liquidity Graph</div>
      <div class="kpi-val mono" id="kpiPools">--</div>
      <div class="kpi-sub mono" id="kpiQuarantine" data-i18n="kpi_quarantine_init">Quarantined --</div>
    </div>
    <div class="card">
      <div class="kpi-label" data-i18n="kpi_opps">Sim Opportunities</div>
      <div class="kpi-val mono" id="kpiOpps">--</div>
      <div class="kpi-sub mono" id="kpiBest" data-i18n="kpi_best_init">Best Est -- | Sim --</div>
    </div>
  </div>

  <div class="main-grid">
    <div class="card">
      <div class="sec-head">
        <div class="sec-title" data-i18n="sec_pnl_curve">Cumulative Net PnL Curve &amp; Executed Trades</div>
        <span class="badge mono" id="tradeCountBadge">0 trades</span>
      </div>
      <div class="chart-wrap" id="pnlChart"></div>
      <div class="table-wrap" style="max-height:220px;margin-top:10px">
        <table>
          <thead><tr><th data-i18n="th_time">Time</th><th data-i18n="th_block">Block</th><th data-i18n="th_strategy">Strategy</th><th data-i18n="th_status">Status</th><th data-i18n="th_gross">Gross</th><th data-i18n="th_gas">Gas</th><th data-i18n="th_net">Net</th><th data-i18n="th_tx">Tx</th></tr></thead>
          <tbody id="tradesBody"></tbody>
        </table>
      </div>
    </div>

    <div class="card" style="display:flex;flex-direction:column;gap:14px">
      <div>
        <div class="sec-head"><div class="sec-title" data-i18n="sec_top_strats">Top Strategies (Sampled Opps)</div></div>
        <div id="stratBars"></div>
      </div>
      <div>
        <div class="sec-head"><div class="sec-title" data-i18n="sec_top_errors">Top Revert / Sim Failures</div></div>
        <div id="errBars"></div>
      </div>
    </div>
  </div>

  <div class="card" style="margin-bottom:16px">
    <div class="sec-head">
      <div class="sec-title" data-i18n="sec_eoa_pool">Multi-EOA Concurrency Pool &amp; Executor</div>
      <div class="toolbar">
        <span class="badge mono" id="execBadge">Executor --</span>
        <span class="badge mono" id="ownerStablesBadge">Owner Stables: --</span>
        <span class="badge mono" id="execResidBadge">Executor Residual: --</span>
        <span class="badge mono" id="eoaPoolBadge">0 EOAs | 0.0000 BNB</span>
      </div>
    </div>
    <div class="table-wrap" style="max-height:240px">
      <table>
        <thead><tr><th data-i18n="th_idx">#</th><th data-i18n="th_role">Role</th><th data-i18n="th_address">Address</th><th data-i18n="th_balance">Balance</th><th data-i18n="th_usd">USD</th><th data-i18n="th_nonce">Nonce</th><th data-i18n="th_recent_submits">Recent Submits</th></tr></thead>
        <tbody id="eoasBody"></tbody>
      </table>
    </div>
  </div>

  <div class="card" style="margin-bottom:16px">
    <div class="sec-head">
      <div class="sec-title" data-i18n="sec_bundles">Recent Bundle Submissions (TOB &amp; Mempool Backrun)</div>
      <div class="toolbar">
        <button class="btn" data-bwin="3600" data-i18n="btn_win_1h" onclick="setBundleWin(3600, 120, this)">1H (2m)</button>
        <button class="btn active" data-bwin="21600" data-i18n="btn_win_6h" onclick="setBundleWin(21600, 600, this)">6H (10m)</button>
        <button class="btn" data-bwin="86400" data-i18n="btn_win_24h" onclick="setBundleWin(86400, 1800, this)">24H (30m)</button>
        <span class="badge mono" id="bundleCountBadge">0 bundles</span>
      </div>
    </div>
    <div class="b-strip mono" id="bundleSummaryStrip"></div>
    <div class="chart-wrap" id="bundleChart" style="height:175px;margin-bottom:12px;position:relative">
      <div class="b-tip mono" id="bundleTooltip"></div>
      <div id="bundleSvgWrap" style="width:100%;height:100%"></div>
    </div>
    <div class="table-wrap" style="max-height:240px">
      <table>
        <thead><tr><th data-i18n="th_time">Time</th><th data-i18n="th_type">Type</th><th data-i18n="th_block">Block</th><th data-i18n="th_eoa">EOA</th><th data-i18n="th_strategy">Strategy</th><th data-i18n="th_gross">Gross</th><th data-i18n="th_tip">Tip</th><th data-i18n="th_est_net">Est Net</th><th data-i18n="th_latency">Latency</th><th data-i18n="th_tx">Tx</th></tr></thead>
        <tbody id="bundlesBody"></tbody>
      </table>
    </div>
  </div>

  <div class="card" style="margin-bottom:16px">
    <div class="sec-head">
      <div class="sec-title" data-i18n="sec_sim_opps">Recent Simulation Candidates</div>
      <div class="toolbar">
        <button class="btn active" data-filter="all" data-i18n="filter_all" onclick="setFilter('all', this)">All</button>
        <button class="btn" data-filter="ok" data-i18n="filter_ok" onclick="setFilter('ok', this)">Sim OK</button>
        <button class="btn" data-filter="fail" data-i18n="filter_fail" onclick="setFilter('fail', this)">Sim Fail</button>
        <input class="search mono" id="searchBox" data-i18n-placeholder="search_placeholder" placeholder="Filter token / route / error..." oninput="renderOpps(true)">
      </div>
    </div>
    <div class="table-wrap" id="oppsWrap">
      <table>
        <thead><tr><th data-i18n="th_time">Time</th><th data-i18n="th_block">Block</th><th data-i18n="th_strategy">Strategy</th><th data-i18n="th_base">Base</th><th data-i18n="th_borrow">Borrow</th><th data-i18n="th_est_usd">Est$</th><th data-i18n="th_sim_usd">Sim$</th><th data-i18n="th_result">Result / Reason</th><th data-i18n="th_route">Route</th></tr></thead>
        <tbody id="oppsBody"></tbody>
      </table>
    </div>
  </div>
  </div><!-- /tab-monitor -->

  <div id="tab-topology" class="tab-panel">
    <div class="topo-grid">
      <div class="card">
        <div class="sec-head" style="flex-wrap:wrap">
          <div style="display:flex;align-items:center;gap:10px;flex-wrap:wrap">
            <div class="sec-title" data-i18n="topo_title">Live Liquidity Topology</div>
            <span class="badge mono" id="topoStatsBadge" data-i18n="topo_loading">Loading topology...</span>
          </div>
          <div class="toolbar">
            <button class="btn active" id="topoFollowBtn" onclick="toggleTopoFollow()">Auto-Follow: ON</button>
            <button class="btn mono" onclick="topoZoomBy(1.28)" title="Zoom in">+</button>
            <button class="btn mono" onclick="topoZoomBy(0.78)" title="Zoom out">-</button>
            <button class="btn" data-i18n="topo_zoom_reset" onclick="topoResetView()">Reset</button>
          </div>
        </div>
        <div class="topo-stage" id="topoStage">
          <canvas id="topoCanvas"></canvas>
          <div class="topo-inspector" id="topoPoolCard" hidden>
            <div style="display:flex;justify-content:space-between;align-items:center;gap:8px">
              <strong style="color:var(--cyan)" data-i18n="topo_pool_selected">Selected Pool</strong>
              <button class="btn" style="padding:2px 7px;font-size:11px" data-i18n="topo_close" onclick="closeTopoPoolCard()">Close</button>
            </div>
            <div class="topo-kv"><span data-i18n="topo_pool_pair">Pair</span><strong class="mono" id="tpPair">-</strong></div>
            <div class="topo-kv"><span data-i18n="topo_pool_venue">DEX</span><span class="mono" id="tpVenue">-</span></div>
            <div class="topo-kv"><span data-i18n="topo_pool_addr">Address</span><span class="mono" id="tpAddr">-</span></div>
            <div style="display:flex;gap:8px;margin-top:8px;align-items:center">
              <button class="btn" id="tpCopyBtn" style="padding:3px 9px;font-size:11px" onclick="copyTopoPoolAddr()">Copy</button>
              <a class="tx mono" id="tpExplorerLink" href="#" target="_blank" data-i18n="topo_explorer" style="font-size:11px;display:none">Explorer</a>
            </div>
          </div>
          <div class="topo-legend">
            <span class="topo-leg-item"><i class="topo-leg-dot" style="background:var(--cyan)"></i><span data-i18n="topo_legend_hub">Hub Cluster</span></span>
            <span class="topo-leg-item"><i class="topo-leg-dot" style="background:var(--emerald)"></i><span data-i18n="topo_legend_flow">SIM OK Pulse</span></span>
            <span class="topo-leg-item"><i class="topo-leg-dot" style="background:var(--rose)"></i><span data-i18n="topo_legend_fail">Probe / Fail (5s)</span></span>
            <span class="topo-leg-item"><i class="topo-leg-dot" style="background:var(--amber)"></i><span data-i18n="topo_legend_match">Matched Pool</span></span>
          </div>
        </div>
        <div class="topo-route-bar" id="topoRouteBar">
          <div class="topo-route-meta mono" id="topoRouteMeta" data-i18n="topo_route_none">Select a live opportunity on the right or wait for the next scanned candidate.</div>
          <div class="topo-legs" id="topoLegsWrap"></div>
        </div>
      </div>

      <div class="card" style="display:flex;flex-direction:column">
        <div class="sec-head">
          <div class="sec-title" data-i18n="topo_feed_title">Live Route Stream</div>
          <span class="badge mono" id="topoFeedBadge">0 / 0 opps</span>
        </div>
        <div class="toolbar" style="margin-bottom:10px">
          <button class="btn active" data-tfilter="all" data-i18n="topo_f_all" onclick="setTopoFilter('all', this)">All</button>
          <button class="btn" data-tfilter="ok" data-i18n="topo_f_ok" onclick="setTopoFilter('ok', this)">Sim OK</button>
          <button class="btn" data-tfilter="gpu" data-i18n="topo_f_gpu" onclick="setTopoFilter('gpu', this)">GPU</button>
          <button class="btn" data-tfilter="ms" data-i18n="topo_f_ms" onclick="setTopoFilter('ms', this)">MEV-Share</button>
          <button class="btn" data-tfilter="deep" data-i18n="topo_f_deep" onclick="setTopoFilter('deep', this)">4+ Hops</button>
          <input class="search mono" id="topoSearchBox" style="width:100%" data-i18n-placeholder="topo_search_ph" placeholder="Filter token / venue / strategy..." oninput="renderTopoFeed()">
        </div>
        <div class="topo-feed-list" id="topoFeedList" style="flex:1"></div>
        <div class="topo-feed-foot mono">
          <span id="topoSourceLabel">-</span>
          <span id="topoUpdatedLabel">-</span>
        </div>
      </div>
    </div>
  </div><!-- /tab-topology -->

  <div id="tab-config" class="tab-panel">
    <div class="card" style="margin-bottom:14px">
      <div class="sec-head">
        <div class="sec-title" data-i18n="sec_proc_ctrl">Process Control</div>
        <span class="badge mono" id="procDetailBadge" data-i18n="proc_stopped">STOPPED</span>
      </div>
      <div style="display:flex;flex-wrap:wrap;gap:10px;align-items:center;margin-bottom:10px">
        <input class="search mono" type="password" id="adminTokenInput" data-i18n-placeholder="token_placeholder" placeholder="Paste access token (auto-filled from ?token=)" style="width:310px" oninput="saveAdminToken(this.value)">
        <button class="btn" data-i18n="btn_start_scan" onclick="startProcess('scan')">Start Scan (Read-Only)</button>
        <button class="btn ok" data-i18n="btn_start_dry" onclick="startProcess('dry_run')">Start Dry-Run</button>
        <label style="font-size:12px;color:var(--muted);display:flex;align-items:center;gap:5px">
          <input type="checkbox" id="confirmLiveChk"> <span data-i18n="chk_confirm_live">I understand live trading risks</span>
        </label>
        <button class="btn danger" data-i18n="btn_start_live" onclick="startProcess('live')">Start Live</button>
        <button class="btn danger" data-i18n="btn_stop_proc" onclick="stopProcess()">Stop Process</button>
      </div>
      <div class="mono" id="procCmdLine" style="font-size:11.5px;color:var(--muted)"></div>
      <div class="status-msg mono" id="procStatusMsg"></div>
    </div>

    <div class="card">
      <div class="sec-head">
        <div class="sec-title" data-i18n="sec_runtime_cfg">Runtime Config (pyrite.json)</div>
        <button class="btn ok" data-i18n="btn_save_cfg" onclick="saveConfig()">Save pyrite.json</button>
      </div>
      <div class="banner" data-i18n-html="cfg_security_banner">
        <strong>Note:</strong> <code>pyrite.json</code> only saves chain, RPC, and strategy settings. Private keys are never stored here; manage them in the Wallet tab (saved separately to <code>.env</code>).
      </div>
      <div class="form-grid">
        <div>
          <div class="field"><label data-i18n="lbl_chain">Chain</label><select id="cfgChain"><option value="eth" data-i18n="opt_chain_eth">eth (Ethereum)</option><option value="bsc" data-i18n="opt_chain_bsc">bsc (BNB Chain)</option><option value="base" data-i18n="opt_chain_base">base (Base)</option></select></div>
          <div class="field"><label data-i18n="lbl_dry_run">Dry Run Default (mode.dry_run)</label><select id="cfgDryRun"><option value="true" data-i18n="opt_dry_true">true (safe default)</option><option value="false" data-i18n="opt_dry_false">false (allow live when launched)</option></select></div>
          <div class="field"><label data-i18n="lbl_min_profit">Min Profit USD (mode.min_profit_usd)</label><input id="cfgMinProfit" type="number" step="0.0001"></div>
          <div class="field"><label data-i18n="lbl_target_usd">Target Profit USD (mode.target_usd)</label><input id="cfgTargetUsd" type="number" step="0.1"></div>
        </div>
        <div>
          <div class="field"><label data-i18n="lbl_read_rpcs">Read RPC URLs (one per line, optional override)</label><textarea id="cfgReadRpcs" rows="3" data-i18n-placeholder="ph_read_rpcs" placeholder="Leave blank to use chains/<chain>.yaml defaults"></textarea></div>
          <div class="field"><label data-i18n="lbl_ws_rpcs">WebSocket RPC URLs (one per line, optional)</label><textarea id="cfgWsRpcs" rows="3"></textarea></div>
        </div>
        <div>
          <div class="field"><label data-i18n="lbl_ms_enabled">Flashbots MEV-Share (mev_share.enabled)</label><select id="cfgMsEnabled"><option value="true" data-i18n="opt_enabled">enabled</option><option value="false" data-i18n="opt_disabled">disabled</option></select></div>
          <div class="field"><label data-i18n="lbl_ms_min_swap">MEV-Share Min Swap USD (mev_share.min_swap_usd)</label><input id="cfgMsMinSwap" type="number" step="1"></div>
          <div class="field"><label data-i18n="lbl_solver_mode">Solver Mode (solver.mode)</label><select id="cfgSolverMode"><option value="cpu" data-i18n="opt_solver_cpu">cpu</option><option value="gpu" data-i18n="opt_solver_gpu">gpu (CUDA .so)</option><option value="hybrid" data-i18n="opt_solver_hybrid">hybrid</option></select></div>
          <div class="field"><label data-i18n="lbl_exec_addr">Executor Address Override (current chain)</label><input id="cfgExecAddr" data-i18n-placeholder="ph_exec_addr" placeholder="0x... (optional)"></div>
        </div>
      </div>
      <div class="status-msg mono" id="cfgStatusMsg"></div>
    </div>
  </div>

  <div id="tab-wallet" class="tab-panel">
    <div class="banner" data-i18n-html="wallet_banner">
      <strong>Wallet tip:</strong> Flash-loan arbitrage does not need trading principal. Create a dedicated small-balance wallet with just $5-$20 for gas. Private keys are written straight to local <code>.env</code> (<code>chmod 0600</code>) over localhost only, never placed in <code>pyrite.json</code>, and cannot be viewed again in the browser.
    </div>
    <div class="form-grid">
      <div class="card">
        <div class="sec-head"><div class="sec-title" data-i18n="sec_wallet_status">Current Wallet</div></div>
        <div class="field"><label data-i18n="lbl_pub_addr">Wallet Address</label><input id="wAddr" readonly data-i18n-placeholder="ph_pub_addr" placeholder="Not set yet (create or import on the right)"></div>
        <div class="field"><label data-i18n="lbl_pk_status">PRIVATE_KEY Status</label><input id="wHasPk" readonly></div>
        <div class="field"><label data-i18n="lbl_fb_status">FLASHBOTS_AUTH_KEY Status</label><input id="wHasFb" readonly></div>
        <div class="field"><label data-i18n="lbl_env_perm">.env Permissions (0600)</label><input id="wEnvMode" readonly></div>
      </div>
      <div class="card">
        <div class="sec-head"><div class="sec-title" data-i18n="sec_wallet_actions">Create or Import Private Key</div></div>
        <div style="display:flex;flex-wrap:wrap;gap:10px;margin-bottom:14px">
          <button class="btn ok" data-i18n="btn_gen_burner" onclick="genBurnerWallet(false)">Generate New Wallet</button>
          <button class="btn danger" data-i18n="btn_replace_burner" onclick="genBurnerWallet(true)">Replace Current Wallet</button>
        </div>
        <div class="field"><label data-i18n="lbl_key_slot">Key Slot</label><select id="wImportSlot"><option value="PRIVATE_KEY" data-i18n="opt_slot_pk">PRIVATE_KEY (Gas &amp; Signing Wallet)</option><option value="FLASHBOTS_AUTH_KEY" data-i18n="opt_slot_fb">FLASHBOTS_AUTH_KEY (Flashbots Auth)</option></select></div>
        <div class="field"><label data-i18n="lbl_key_hex">Private Key (hidden while typing, not readable from UI after save)</label><input id="wImportVal" type="password" autocomplete="off" data-i18n-placeholder="ph_key_hex" placeholder="0x... (64 hex characters)"></div>
        <button class="btn ok" data-i18n="btn_save_env" onclick="importWalletKey()">Save to .env</button>
        <div class="status-msg mono" id="walletStatusMsg"></div>
      </div>
    </div>
  </div><!-- /tab-wallet -->

<script>
// I18N_START
const I18N = {
  "en": {
    "page_title": "Pyrite | EVM Arbitrage Terminal",
    "brand_sub": "EVM ARBITRAGE TERMINAL",
    "proc_idle": "PROC IDLE",
    "proc_running_badge": "PROC {action} #{pid}",
    "tab_monitor": "Monitor",
    "tab_topology": "Topology",
    "tab_config": "Config & Control",
    "tab_wallet": "Wallet",
    "topo_title": "Live Liquidity Topology",
    "topo_stats_fmt": "{pools} pools | {tokens} tokens | {venues} DEXs",
    "topo_loading": "Loading topology...",
    "topo_zoom_reset": "Reset",
    "topo_follow_on": "Auto-Follow: ON",
    "topo_follow_off": "Auto-Follow: OFF",
    "topo_legend_hub": "Hub Cluster",
    "topo_legend_match": "Matched Pool",
    "topo_legend_flow": "SIM OK Pulse",
    "topo_legend_fail": "Probe / Fail (5s)",
    "topo_pool_selected": "Selected Pool",
    "topo_close": "Close",
    "topo_pool_pair": "Pair",
    "topo_pool_venue": "DEX",
    "topo_pool_addr": "Address",
    "topo_copy": "Copy",
    "topo_copied": "Copied",
    "topo_explorer": "Explorer",
    "topo_route_none": "Select a live opportunity on the right or wait for the next scanned candidate.",
    "topo_route_meta": "Block {block} | Matched {matched} pools | Est ${est} | Sim {sim}",
    "topo_more_hops": "+{n} truncated in log",
    "topo_feed_title": "Live Route Stream",
    "topo_feed_badge": "{shown} / {total} opps",
    "topo_f_all": "All",
    "topo_f_ok": "Sim OK",
    "topo_f_gpu": "GPU",
    "topo_f_ms": "MEV-Share",
    "topo_f_deep": "4+ Hops",
    "topo_search_ph": "Filter token / venue / strategy...",
    "topo_feed_empty": "No matching live candidates in tail yet.",
    "topo_src_pools_cache": "Source: pools_cache_{chain}.json",
    "topo_src_opportunities_fallback": "Source: opportunities.jsonl (warm-up)",
    "topo_src_empty": "No pool cache found yet",
    "topo_updated_fmt": "Updated {ts}",
    "topo_card_sub": "Blk {block} | {steps} legs | Sim {sim}",
    "connecting_sse": "Connecting SSE...",
    "live_sse": "LIVE SSE",
    "live_3s": "LIVE 3s",
    "paused_btn": "PAUSED",
    "sync_paused": "Paused",
    "sync_reconnecting": "SSE reconnecting...",
    "sync_lost": "Connection lost",
    "btn_refresh": "Refresh",
    "vm_local": "VM Local",
    "vm_badge": "Load {load} | RAM {used}/{total}MB",
    "kpi_net_pnl": "Realized Net PnL",
    "kpi_net_sub_init": "Gross $0.00 | Gas $0.00",
    "kpi_net_sub": "Gross ${gross} | Gas ${gas}",
    "kpi_net_sub_onchain": " | On-Chain {delta}",
    "kpi_equity": "On-Chain Total Equity",
    "kpi_equity_sub_init": "Native -- | Stables -- | Exec --",
    "kpi_equity_sub": "{sym} {native} | Stables ${stables} | Exec ${exec}",
    "waiting_rpc": "Waiting for RPC",
    "kpi_trades": "Executed Trades",
    "kpi_win_rate_init": "Win rate --",
    "kpi_win_rate": "Win rate {rate}% | Reverted {rev}",
    "no_trades_yet": "No executed trades yet",
    "no_trades_short": "No trades yet",
    "kpi_block": "Live Block & Speed",
    "kpi_speed_init": "solve -- us | blk -- ms",
    "kpi_speed": "solve {solve_ms}ms | blk {blk_ms}ms | txs {txs}",
    "waiting_runtime_log": "Waiting for runtime.log",
    "kpi_graph": "Liquidity Graph",
    "kpi_quarantine_init": "Quarantined --",
    "kpi_quarantine": "Quarantined {count} pools",
    "kpi_opps": "Sim Opportunities",
    "kpi_best_init": "Best Est -- | Sim --",
    "kpi_best": "Best Est ${est} | Sim ${sim}",
    "sec_pnl_curve": "Cumulative Net PnL Curve & Executed Trades",
    "trade_count": "{n} trades",
    "th_time": "Time",
    "th_block": "Block",
    "th_strategy": "Strategy",
    "th_status": "Status",
    "th_gross": "Gross",
    "th_gas": "Gas",
    "th_net": "Net",
    "th_tx": "Tx",
    "sec_top_strats": "Top Strategies (Sampled Opps)",
    "sec_top_errors": "Top Revert / Sim Failures",
    "no_data": "No data",
    "chart_stats": "max ${max} | min ${min} | cur ${cur}",
    "sec_eoa_pool": "Multi-EOA Concurrency Pool & Executor",
    "executor_idle": "Executor --",
    "executor_lbl": "Executor",
    "owner_stables_fmt": "Owner Stables: ${usd} ({sym} {amt})",
    "exec_resid_fmt": "Executor Residual: ${usd}",
    "eoa_pool_fmt": "{n} EOAs | {native} {sym} (${usd})",
    "th_idx": "#",
    "th_role": "Role",
    "th_address": "Address",
    "th_balance": "Balance",
    "th_usd": "USD",
    "th_nonce": "Nonce",
    "th_recent_submits": "Recent Submits",
    "no_eoas": "No EOAs configured (logs/eoas.json)",
    "sec_bundles": "Recent Bundle Submissions (TOB & Mempool Backrun)",
    "btn_win_1h": "1H (2m)",
    "btn_win_6h": "6H (10m)",
    "btn_win_24h": "24H (30m)",
    "bundle_count_badge": "{win} in window | {rows} rows / {idx} idx",
    "lbl_tob": "TOB",
    "lbl_backrun": "Backrun",
    "lbl_opp_blocks": "Opp Blocks",
    "lbl_confirmed_wins": "Confirmed Wins",
    "lbl_peak_bucket": "Peak/Bucket",
    "tip_bundles": "Bundles",
    "tip_max_gross": "Max Gross",
    "no_bundles_win": "No bundle submissions in selected window",
    "th_type": "Type",
    "th_eoa": "EOA",
    "th_tip": "Tip",
    "th_est_net": "Est Net",
    "th_latency": "Latency",
    "no_bundles_tail": "No recent bundle submissions in tail",
    "sec_sim_opps": "Recent Simulation Candidates",
    "filter_all": "All",
    "filter_ok": "Sim OK",
    "filter_fail": "Sim Fail",
    "search_placeholder": "Filter token / route / error...",
    "th_base": "Base",
    "th_borrow": "Borrow",
    "th_est_usd": "Est$",
    "th_sim_usd": "Sim$",
    "th_result": "Result / Reason",
    "th_route": "Route",
    "pill_sim_ok": "SIM OK",
    "sec_proc_ctrl": "Process Control",
    "proc_stopped": "STOPPED",
    "proc_running_detail": "RUNNING ({action}, pid={pid})",
    "token_placeholder": "Paste access token (auto-filled from ?token=)",
    "btn_start_scan": "Start Scan (Read-Only)",
    "btn_start_dry": "Start Dry-Run",
    "chk_confirm_live": "I understand live trading risks",
    "btn_start_live": "Start Live",
    "btn_stop_proc": "Stop Process",
    "last_cmd_fmt": "Last cmd: {cmd}",
    "sec_runtime_cfg": "Runtime Config (pyrite.json)",
    "btn_save_cfg": "Save pyrite.json",
    "cfg_security_banner": "<strong>Note:</strong> <code>pyrite.json</code> only saves chain, RPC, and strategy settings. Private keys are never stored here; manage them in the Wallet tab (saved separately to <code>.env</code>).",
    "lbl_chain": "Chain",
    "opt_chain_eth": "eth (Ethereum)",
    "opt_chain_bsc": "bsc (BNB Chain)",
    "opt_chain_base": "base (Base)",
    "lbl_dry_run": "Dry Run Default (mode.dry_run)",
    "opt_dry_true": "true (safe default)",
    "opt_dry_false": "false (allow live when launched)",
    "lbl_min_profit": "Min Profit USD (mode.min_profit_usd)",
    "lbl_target_usd": "Target Profit USD (mode.target_usd)",
    "lbl_read_rpcs": "Read RPC URLs (one per line, optional override)",
    "ph_read_rpcs": "Leave blank to use chains/<chain>.yaml defaults",
    "lbl_ws_rpcs": "WebSocket RPC URLs (one per line, optional)",
    "lbl_ms_enabled": "Flashbots MEV-Share (mev_share.enabled)",
    "opt_enabled": "enabled",
    "opt_disabled": "disabled",
    "lbl_ms_min_swap": "MEV-Share Min Swap USD (mev_share.min_swap_usd)",
    "lbl_solver_mode": "Solver Mode (solver.mode)",
    "opt_solver_cpu": "cpu",
    "opt_solver_gpu": "gpu (CUDA .so)",
    "opt_solver_hybrid": "hybrid",
    "lbl_exec_addr": "Executor Address Override (current chain)",
    "ph_exec_addr": "0x... (optional)",
    "wallet_banner": "<strong>Wallet tip:</strong> Flash-loan arbitrage does not need trading principal. Create a dedicated small-balance wallet with just $5-$20 for gas. Private keys are written straight to local <code>.env</code> (<code>chmod 0600</code>) over localhost only, never placed in <code>pyrite.json</code>, and cannot be viewed again in the browser.",
    "sec_wallet_status": "Current Wallet",
    "lbl_pub_addr": "Wallet Address",
    "ph_pub_addr": "Not set yet (create or import on the right)",
    "lbl_pk_status": "PRIVATE_KEY Status",
    "lbl_fb_status": "FLASHBOTS_AUTH_KEY Status",
    "lbl_env_perm": ".env Permissions (0600)",
    "sec_wallet_actions": "Create or Import Private Key",
    "btn_gen_burner": "Generate New Wallet",
    "btn_replace_burner": "Replace Current Wallet",
    "lbl_key_slot": "Key Slot",
    "opt_slot_pk": "PRIVATE_KEY (Gas & Signing Wallet)",
    "opt_slot_fb": "FLASHBOTS_AUTH_KEY (Flashbots Auth)",
    "lbl_key_hex": "Private Key (hidden while typing, not readable from UI after save)",
    "ph_key_hex": "0x... (64 hex characters)",
    "btn_save_env": "Save to .env",
    "w_none_derived": "(not configured yet)",
    "w_pk_ok": "Configured in .env",
    "w_pk_missing": "Missing",
    "w_fb_ok": "Configured (separate auth key)",
    "w_fb_fallback": "Fallback to PRIVATE_KEY",
    "w_env_ok": "OK (0600)",
    "w_env_warn": "WARNING: check permissions!",
    "w_env_none": ".env not created yet",
    "msg_saved_cfg": "Saved pyrite.json.",
    "msg_save_failed": "Save failed: {err}",
    "msg_started_proc": "Started {action} (pid={pid}). Logs streaming to runtime.log.",
    "msg_start_failed": "Start failed: {err}",
    "msg_stopped_proc": "Process stopped.",
    "msg_stop_failed": "Stop failed: {err}",
    "msg_wallet_saved_no_bin": "(saved in .env; build pyrite binary to display address)",
    "msg_burner_saved": "New wallet saved to .env (0600): {addr}",
    "msg_wallet_gen_failed": "Wallet generate failed: {err}",
    "msg_import_saved": "Saved {key} to .env (0600).",
    "msg_import_failed": "Import failed: {err}"
  },
  "zh": {
    "page_title": "Pyrite | EVM 套利控制台",
    "brand_sub": "EVM 套利控制台",
    "proc_idle": "进程空闲",
    "proc_running_badge": "进程 {action} #{pid}",
    "tab_monitor": "监控总览",
    "tab_topology": "实时星图",
    "tab_config": "配置与启停",
    "tab_wallet": "钱包",
    "topo_title": "实时流动性星图",
    "topo_stats_fmt": "{pools} 个池 | {tokens} 个代币 | {venues} 个 DEX",
    "topo_loading": "正在加载拓扑图...",
    "topo_zoom_reset": "重置视角",
    "topo_follow_on": "自动追踪: 开",
    "topo_follow_off": "自动追踪: 关",
    "topo_legend_hub": "核心枢纽簇",
    "topo_legend_match": "路由匹配池",
    "topo_legend_flow": "仿真通过脉冲",
    "topo_legend_fail": "试探路径 (5s淡出)",
    "topo_pool_selected": "选中流动性池",
    "topo_close": "关闭",
    "topo_pool_pair": "交易对",
    "topo_pool_venue": "交易所",
    "topo_pool_addr": "池地址",
    "topo_copy": "复制",
    "topo_copied": "已复制",
    "topo_explorer": "浏览器查看",
    "topo_route_none": "点击右侧实时候选路由或等待下一区块扫描结果。",
    "topo_route_meta": "区块 {block} | 匹配 {matched} 个池 | 预估 ${est} | 仿真 {sim}",
    "topo_more_hops": "+{n} 跳已在日志折叠",
    "topo_feed_title": "实时路由事件流",
    "topo_feed_badge": "显示 {shown} / 共 {total} 条",
    "topo_f_all": "全部",
    "topo_f_ok": "仿真通过",
    "topo_f_gpu": "GPU",
    "topo_f_ms": "MEV-Share",
    "topo_f_deep": "4+ 跳",
    "topo_search_ph": "筛选代币 / 交易所 / 策略...",
    "topo_feed_empty": "当前日志尾部暂无匹配的候选路由。",
    "topo_src_pools_cache": "数据源: pools_cache_{chain}.json",
    "topo_src_opportunities_fallback": "数据源: opportunities.jsonl (冷启动聚合)",
    "topo_src_empty": "暂未生成池缓存",
    "topo_updated_fmt": "更新于 {ts}",
    "topo_card_sub": "块 {block} | {steps} 跳 | 仿真 {sim}",
    "connecting_sse": "正在连接 SSE...",
    "live_sse": "实时 SSE",
    "live_3s": "每 3 秒刷新",
    "paused_btn": "已暂停",
    "sync_paused": "已暂停",
    "sync_reconnecting": "SSE 重连中...",
    "sync_lost": "连接已断开",
    "btn_refresh": "刷新",
    "vm_local": "本地主机",
    "vm_badge": "负载 {load} | 内存 {used}/{total}MB",
    "kpi_net_pnl": "已实现净盈亏",
    "kpi_net_sub_init": "毛利 $0.00 | Gas $0.00",
    "kpi_net_sub": "毛利 ${gross} | Gas ${gas}",
    "kpi_net_sub_onchain": " | 链上账变 {delta}",
    "kpi_equity": "链上总资产",
    "kpi_equity_sub_init": "原生币 -- | 稳定币 -- | 合约 --",
    "kpi_equity_sub": "{sym} {native} | 稳定币 ${stables} | 合约 ${exec}",
    "waiting_rpc": "等待 RPC 数据",
    "kpi_trades": "已执行成交",
    "kpi_win_rate_init": "胜率 --",
    "kpi_win_rate": "胜率 {rate}% | 回滚 {rev}",
    "no_trades_yet": "暂无已执行交易",
    "no_trades_short": "暂无成交",
    "kpi_block": "实时区块与耗时",
    "kpi_speed_init": "求解 -- us | 整块 -- ms",
    "kpi_speed": "求解 {solve_ms}ms | 整块 {blk_ms}ms | 交易 {txs}",
    "waiting_runtime_log": "等待 runtime.log",
    "kpi_graph": "流动性池图",
    "kpi_quarantine_init": "隔离池 --",
    "kpi_quarantine": "已隔离 {count} 个池子",
    "kpi_opps": "仿真候选机会",
    "kpi_best_init": "最高预估 -- | 仿真 --",
    "kpi_best": "最高预估 ${est} | 仿真 ${sim}",
    "sec_pnl_curve": "累计净盈亏曲线与成交记录",
    "trade_count": "{n} 笔成交",
    "th_time": "时间",
    "th_block": "区块",
    "th_strategy": "策略",
    "th_status": "状态",
    "th_gross": "毛利",
    "th_gas": "Gas",
    "th_net": "净利",
    "th_tx": "交易",
    "sec_top_strats": "高频策略分布 (采样候选)",
    "sec_top_errors": "高频回滚 / 仿真失败原因",
    "no_data": "暂无数据",
    "chart_stats": "最高 ${max} | 最低 ${min} | 当前 ${cur}",
    "sec_eoa_pool": "多 EOA 并发签名池与执行合约",
    "executor_idle": "执行合约 --",
    "executor_lbl": "执行合约",
    "owner_stables_fmt": "Owner 稳定币: ${usd} ({sym} {amt})",
    "exec_resid_fmt": "合约残余: ${usd}",
    "eoa_pool_fmt": "{n} 个 EOA | {native} {sym} (${usd})",
    "th_idx": "#",
    "th_role": "角色",
    "th_address": "地址",
    "th_balance": "余额",
    "th_usd": "美元估值",
    "th_nonce": "Nonce",
    "th_recent_submits": "近期提交",
    "no_eoas": "未配置 EOA (logs/eoas.json)",
    "sec_bundles": "近期 Bundle 提交 (TOB 与内存池回跑)",
    "btn_win_1h": "1小时 (2分)",
    "btn_win_6h": "6小时 (10分)",
    "btn_win_24h": "24小时 (30分)",
    "bundle_count_badge": "窗口内 {win} | 显示 {rows} / 索引 {idx}",
    "lbl_tob": "TOB 抢跑",
    "lbl_backrun": "同块回跑",
    "lbl_opp_blocks": "机会区块",
    "lbl_confirmed_wins": "确认成交",
    "lbl_peak_bucket": "单桶峰值",
    "tip_bundles": "提交数",
    "tip_max_gross": "最大毛利",
    "no_bundles_win": "所选时间窗口内暂无 Bundle 提交",
    "th_type": "类型",
    "th_eoa": "EOA",
    "th_tip": "贿赂",
    "th_est_net": "预估净利",
    "th_latency": "延迟",
    "no_bundles_tail": "日志尾部暂无近期 Bundle 提交",
    "sec_sim_opps": "近期仿真候选路径",
    "filter_all": "全部",
    "filter_ok": "仿真通过",
    "filter_fail": "仿真失败",
    "search_placeholder": "筛选代币 / 路径 / 错误...",
    "th_base": "本位币",
    "th_borrow": "借款量",
    "th_est_usd": "预估$",
    "th_sim_usd": "仿真$",
    "th_result": "结果 / 原因",
    "th_route": "执行路径",
    "pill_sim_ok": "仿真通过",
    "sec_proc_ctrl": "进程控制",
    "proc_stopped": "已停止",
    "proc_running_detail": "运行中 ({action}, pid={pid})",
    "token_placeholder": "输入访问令牌（通过 ?token= 链接打开会自动填入）",
    "btn_start_scan": "启动只读扫描 (Scan)",
    "btn_start_dry": "启动模拟运行 (Dry-Run)",
    "chk_confirm_live": "我已知晓实盘交易风险",
    "btn_start_live": "启动实盘 (Live)",
    "btn_stop_proc": "停止进程",
    "last_cmd_fmt": "最近命令: {cmd}",
    "sec_runtime_cfg": "运行配置 (pyrite.json)",
    "btn_save_cfg": "保存 pyrite.json",
    "cfg_security_banner": "<strong>提示：</strong><code>pyrite.json</code> 只放链、RPC 和策略参数，不会存私钥。私钥请到「钱包」页面设置，会单独保存在本地的 <code>.env</code> 文件里。",
    "lbl_chain": "目标链",
    "opt_chain_eth": "eth (以太坊主网)",
    "opt_chain_bsc": "bsc (币安智能链)",
    "opt_chain_base": "base (Base L2)",
    "lbl_dry_run": "默认模拟开关 (mode.dry_run)",
    "opt_dry_true": "true (安全默认)",
    "opt_dry_false": "false (允许实盘启动)",
    "lbl_min_profit": "最小净利润门槛 USD (mode.min_profit_usd)",
    "lbl_target_usd": "目标参考额 USD (mode.target_usd)",
    "lbl_read_rpcs": "只读 RPC 列表 (每行一个, 可选覆盖)",
    "ph_read_rpcs": "留空则使用 chains/<chain>.yaml 默认节点",
    "lbl_ws_rpcs": "WebSocket RPC 列表 (每行一个, 可选)",
    "lbl_ms_enabled": "Flashbots MEV-Share 开关 (mev_share.enabled)",
    "opt_enabled": "启用",
    "opt_disabled": "禁用",
    "lbl_ms_min_swap": "MEV-Share 最小 Swap 过滤额 USD (mev_share.min_swap_usd)",
    "lbl_solver_mode": "求解器模式 (solver.mode)",
    "opt_solver_cpu": "cpu",
    "opt_solver_gpu": "gpu (CUDA 动态库)",
    "opt_solver_hybrid": "hybrid (CPU+GPU 混合)",
    "lbl_exec_addr": "执行合约地址覆盖 (当前链)",
    "ph_exec_addr": "0x... (可选)",
    "wallet_banner": "<strong>提示：</strong>闪电贷套利不需要在钱包里放本金，建议新建一个专门的小额钱包，只充 $5-$20 付 Gas 就够了。私钥只会在本机直接存进 <code>.env</code>（权限 <code>0600</code>），不会写进 <code>pyrite.json</code>，保存后网页上也不能再次查看或导出。",
    "sec_wallet_status": "当前钱包状态",
    "lbl_pub_addr": "钱包地址",
    "ph_pub_addr": "暂未配置（可在右侧生成或导入）",
    "lbl_pk_status": "PRIVATE_KEY 状态",
    "lbl_fb_status": "FLASHBOTS_AUTH_KEY 状态",
    "lbl_env_perm": ".env 文件权限 (0600)",
    "sec_wallet_actions": "新建或导入私钥",
    "btn_gen_burner": "一键生成新钱包",
    "btn_replace_burner": "重新生成并替换",
    "lbl_key_slot": "保存位置",
    "opt_slot_pk": "PRIVATE_KEY（发交易 / 付 Gas 钱包）",
    "opt_slot_fb": "FLASHBOTS_AUTH_KEY（Flashbots 身份签名）",
    "lbl_key_hex": "私钥（输入时隐藏，保存后无法从网页查看）",
    "ph_key_hex": "0x... (64 位十六进制字符)",
    "btn_save_env": "保存到 .env",
    "w_none_derived": "（暂未配置）",
    "w_pk_ok": "已配置于 .env",
    "w_pk_missing": "未配置",
    "w_fb_ok": "已配置 (独立声誉私钥)",
    "w_fb_fallback": "回退使用 PRIVATE_KEY",
    "w_env_ok": "正常 (0600)",
    "w_env_warn": "警告: 请检查文件权限!",
    "w_env_none": "尚未创建 .env",
    "msg_saved_cfg": "已保存 pyrite.json。",
    "msg_save_failed": "保存失败: {err}",
    "msg_started_proc": "已启动 {action} (pid={pid})，日志正在写入 runtime.log。",
    "msg_start_failed": "启动失败: {err}",
    "msg_stopped_proc": "进程已停止。",
    "msg_stop_failed": "停止失败: {err}",
    "msg_wallet_saved_no_bin": "(已写入 .env；构建 pyrite 二进制后可显示公钥地址)",
    "msg_burner_saved": "新钱包已保存到 .env (0600): {addr}",
    "msg_wallet_gen_failed": "生成钱包失败: {err}",
    "msg_import_saved": "已将 {key} 保存到 .env (0600)。",
    "msg_import_failed": "导入失败: {err}"
  }
};
// I18N_END

let currentLang = 'en';
(function initLang() {
  try {
    const p = new URLSearchParams(window.location.search);
    const ql = p.get('lang');
    if (ql === 'en' || ql === 'zh') {
      currentLang = ql;
      localStorage.setItem('pyrite_lang', ql);
    } else {
      const saved = localStorage.getItem('pyrite_lang');
      if (saved === 'en' || saved === 'zh') currentLang = saved;
    }
  } catch (e) {}
})();

function t(key, vars) {
  const dict = I18N[currentLang] || I18N.en;
  let str = dict[key];
  if (str == null) str = I18N.en[key];
  if (str == null) return key;
  if (vars) {
    for (const [k, v] of Object.entries(vars)) {
      str = str.split(`{${k}}`).join(String(v ?? ''));
    }
  }
  return str;
}

let state = null;
let lastControlData = null;
let streaming = true;
let evtSource = null;
let pollTimer = null;
let oppFilter = 'all';
let bundleWinSec = 21600;
let bundleBucketSec = 600;
let lastBucketStats = [];
let hoveredBucketIdx = -1;

let pendingState = null;
let pendingTick = null;
let rafScheduled = false;

const prevSig = {
  pnlChart: '',
  trades: '',
  strats: '',
  errs: '',
  exec: '',
  eoas: '',
  bundleChart: '',
  bundles: '',
  opps: '',
};

function applyI18n() {
  document.documentElement.lang = currentLang === 'zh' ? 'zh-CN' : 'en';
  document.title = t('page_title');
  const sel = document.getElementById('langSelect');
  if (sel) sel.value = currentLang;

  document.querySelectorAll('[data-i18n]').forEach(el => {
    const k = el.getAttribute('data-i18n');
    if (k) el.textContent = t(k);
  });
  document.querySelectorAll('[data-i18n-placeholder]').forEach(el => {
    const k = el.getAttribute('data-i18n-placeholder');
    if (k) el.setAttribute('placeholder', t(k));
  });
  document.querySelectorAll('[data-i18n-html]').forEach(el => {
    const k = el.getAttribute('data-i18n-html');
    if (k) el.innerHTML = t(k);
  });

  setText('togglePoll', t(streaming ? (window.EventSource ? 'live_sse' : 'live_3s') : 'paused_btn'));
  if (!streaming) {
    setText('syncTime', t('sync_paused'));
  } else if (!state) {
    setText('syncTime', t('connecting_sse'));
  }

  Object.keys(prevSig).forEach(k => { prevSig[k] = ''; });
  if (state) {
    renderAll(state);
  } else {
    setText('tradeCountBadge', t('trade_count', { n: 0 }));
    setText('bundleCountBadge', t('bundle_count_badge', { win: 0, rows: 0, idx: 0 }));
    setText('execBadge', t('executor_idle'));
    setText('ownerStablesBadge', t('owner_stables_fmt', { usd: '--', sym: 'USD', amt: '--' }));
    setText('execResidBadge', t('exec_resid_fmt', { usd: '--' }));
    setText('eoaPoolBadge', t('eoa_pool_fmt', { n: 0, native: '0.0000', sym: 'ETH', usd: '0.00' }));
    renderChart([]);
  }
  if (typeof refreshTopoUi === 'function') {
    refreshTopoUi();
  }
  if (lastControlData) {
    renderControlMeta(lastControlData);
  }
}

function setLang(lang) {
  currentLang = lang === 'zh' ? 'zh' : 'en';
  try { localStorage.setItem('pyrite_lang', currentLang); } catch (e) {}
  applyI18n();
}

function setText(id, val) {
  const el = document.getElementById(id);
  const s = String(val ?? '');
  if (el && el.textContent !== s) el.textContent = s;
}

function setClass(id, cls) {
  const el = document.getElementById(id);
  if (el && el.className !== cls) el.className = cls;
}

function setDotState(mode) {
  const dot = document.getElementById('pulseDot');
  if (!dot) return;
  if (mode === 'live') {
    dot.style.background = 'var(--emerald)';
    dot.style.boxShadow = '0 0 8px var(--emerald)';
  } else if (mode === 'paused') {
    dot.style.background = 'var(--amber)';
    dot.style.boxShadow = '0 0 8px var(--amber)';
  } else {
    dot.style.background = 'var(--rose)';
    dot.style.boxShadow = '0 0 8px var(--rose)';
  }
}

function setHtmlPreserveScroll(tbodyId, html) {
  const tbody = document.getElementById(tbodyId);
  if (!tbody) return;
  const wrap = tbody.closest('.table-wrap');
  const st = wrap ? wrap.scrollTop : 0;
  tbody.innerHTML = html;
  if (wrap && st > 0) wrap.scrollTop = st;
}

function esc(s) {
  return String(s ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
}

function fmtLocal(iso) {
  if (!iso) return '-';
  const norm = String(iso).replace(/(\.\d{3})\d+/, '$1');
  const d = new Date(norm);
  if (isNaN(d.getTime())) return String(iso).slice(0, 19);
  const pad = n => String(n).padStart(2, '0');
  return `${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}

function setFilter(f, btn) {
  oppFilter = f;
  document.querySelectorAll('[data-filter]').forEach(b => b.classList.remove('active'));
  btn.classList.add('active');
  renderOpps(true);
}

function togglePolling() {
  streaming = !streaming;
  const btn = document.getElementById('togglePoll');
  btn.textContent = t(streaming ? (window.EventSource ? 'live_sse' : 'live_3s') : 'paused_btn');
  btn.classList.toggle('active', streaming);
  if (streaming) {
    setDotState('live');
    connectStream();
  } else {
    closeStream();
    setDotState('paused');
    setText('syncTime', t('sync_paused'));
  }
}

function setBundleWin(winSec, bucketSec, btn) {
  bundleWinSec = winSec;
  bundleBucketSec = bucketSec;
  document.querySelectorAll('[data-bwin]').forEach(b => b.classList.remove('active'));
  if (btn) btn.classList.add('active');
  if (state) renderBundleChart(state, true);
}

function fmtHHMM(epochSec) {
  const d = new Date(epochSec * 1000);
  const pad = n => String(n).padStart(2, '0');
  return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

function showBundleTip(idx) {
  hoveredBucketIdx = idx;
  const b = lastBucketStats[idx];
  const tip = document.getElementById('bundleTooltip');
  if (!b || !tip) return;
  const tot = b.tob + b.br;
  tip.innerHTML =
    `<div style="color:#e2e8f0;font-weight:600">${fmtHHMM(b.t)} - ${fmtHHMM(b.t + bundleBucketSec)}</div>` +
    `<div>${esc(t('tip_bundles'))}: <span class="cy">${tot}</span> (${esc(t('lbl_tob'))} <span class="cy">${b.tob}</span> / ${esc(t('lbl_backrun'))} <span style="color:#fbbf24">${b.br}</span>)</div>` +
    `<div>${esc(t('lbl_opp_blocks'))}: <span style="color:#a78bfa">${b.blocks.size}</span> | ${esc(t('tip_max_gross'))}: <span class="pos">$${b.maxGross.toFixed(4)}</span></div>` +
    (b.wins > 0 ? `<div class="pos">${esc(t('lbl_confirmed_wins'))}: ${b.wins}</div>` : '');
  if (idx > lastBucketStats.length / 2) {
    tip.style.left = '44px';
    tip.style.right = 'auto';
  } else {
    tip.style.right = '14px';
    tip.style.left = 'auto';
  }
  tip.style.display = 'block';
}

function hideBundleTip() {
  hoveredBucketIdx = -1;
  const tip = document.getElementById('bundleTooltip');
  if (tip) tip.style.display = 'none';
}

function renderBundleChart(s, forceRedraw) {
  const svgWrap = document.getElementById('bundleSvgWrap');
  const strip = document.getElementById('bundleSummaryStrip');
  if (!svgWrap || !strip) return;

  const nowSec = Number(s.server_epoch || Math.floor(Date.now() / 1000));
  const endBucket = Math.floor(nowSec / bundleBucketSec) * bundleBucketSec;
  const trades = (s.realized_pnl && s.realized_pnl.trades) || [];
  const sig = `${currentLang}:${bundleWinSec}:${bundleBucketSec}:${endBucket}:${s.total_bundles_indexed || 0}:${trades.length}:${(s.bundle_series || []).length}`;
  if (!forceRedraw && prevSig.bundleChart === sig) return;
  prevSig.bundleChart = sig;

  const numBuckets = Math.max(1, Math.floor(bundleWinSec / bundleBucketSec));
  const startSec = endBucket - (numBuckets - 1) * bundleBucketSec;
  const buckets = Array.from({ length: numBuckets }, (_, i) => ({
    t: startSec + i * bundleBucketSec,
    tob: 0,
    br: 0,
    blocks: new Set(),
    maxGross: 0,
    wins: 0,
  }));

  let totTob = 0, totBr = 0, totWins = 0;
  const allBlocks = new Set();
  for (const row of (s.bundle_series || [])) {
    const ep = Number(row[0] || 0);
    const isBr = Number(row[1] || 0) === 1;
    const blk = Number(row[2] || 0);
    const gross = Number(row[3] || 0);
    const idx = Math.floor((ep - startSec) / bundleBucketSec);
    if (idx >= 0 && idx < numBuckets) {
      const b = buckets[idx];
      if (isBr) { b.br += 1; totBr += 1; } else { b.tob += 1; totTob += 1; }
      if (blk > 0) { b.blocks.add(blk); allBlocks.add(blk); }
      if (gross > b.maxGross) b.maxGross = gross;
    }
  }
  for (const tr of trades) {
    if (!String(tr.status || '').includes('CONFIRMED')) continue;
    const norm = String(tr.timestamp || '').replace(/(\.\d{3})\d+/, '$1');
    const ms = Date.parse(norm);
    if (isNaN(ms)) continue;
    const idx = Math.floor((Math.floor(ms / 1000) - startSec) / bundleBucketSec);
    if (idx >= 0 && idx < numBuckets) {
      buckets[idx].wins += 1;
      totWins += 1;
    }
  }

  lastBucketStats = buckets;
  const totBundles = totTob + totBr;
  const peak = buckets.reduce((m, b) => Math.max(m, b.tob + b.br), 0);
  const rows = (s.recent_bundles || []).length;
  const totIdx = Number(s.total_bundles_indexed || 0);
  setText('bundleCountBadge', t('bundle_count_badge', { win: totBundles, rows, idx: totIdx }));
  strip.innerHTML =
    `<span><i class="swatch" style="background:#38bdf8"></i>${esc(t('lbl_tob'))}: <b style="color:#e2e8f0">${totTob}</b></span>` +
    `<span><i class="swatch" style="background:#fbbf24"></i>${esc(t('lbl_backrun'))}: <b style="color:#e2e8f0">${totBr}</b></span>` +
    `<span><i class="swatch" style="background:#a78bfa;border-radius:50%"></i>${esc(t('lbl_opp_blocks'))}: <b style="color:#e2e8f0">${allBlocks.size}</b></span>` +
    `<span><i class="swatch" style="background:#34d399;transform:rotate(45deg)"></i>${esc(t('lbl_confirmed_wins'))}: <b class="pos">${totWins}</b></span>` +
    `<span>${esc(t('lbl_peak_bucket'))}: <b style="color:#e2e8f0">${peak}</b></span>`;

  const W = 920, H = 170, L = 38, R = 14, T = 16, B = 24;
  const pw = W - L - R, ph = H - T - B;
  const maxY = Math.max(peak, 4);
  const midY = Math.ceil(maxY / 2);
  const yOf = v => T + ph - (v / maxY) * ph;
  const slotW = pw / numBuckets;
  const barW = Math.max(4, Math.min(22, slotW * 0.72));

  let barsSvg = '';
  for (let i = 0; i < numBuckets; i++) {
    const b = buckets[i];
    const xSlot = L + i * slotW;
    const xBar = xSlot + (slotW - barW) / 2;
    const cx = xSlot + slotW / 2;
    const tot = b.tob + b.br;
    if (b.tob > 0) {
      const yTop = yOf(b.tob);
      const hBar = Math.max(1.5, T + ph - yTop);
      barsSvg += `<rect x="${xBar.toFixed(1)}" y="${yTop.toFixed(1)}" width="${barW.toFixed(1)}" height="${hBar.toFixed(1)}" rx="2" fill="#38bdf8" fill-opacity="0.86"/>`;
    }
    if (b.br > 0) {
      const yTop = yOf(tot);
      const hBar = Math.max(1.5, yOf(b.tob) - yTop);
      barsSvg += `<rect x="${xBar.toFixed(1)}" y="${yTop.toFixed(1)}" width="${barW.toFixed(1)}" height="${hBar.toFixed(1)}" rx="2" fill="#fbbf24" fill-opacity="0.9"/>`;
    }
    if (tot > 0 && b.blocks.size > 0) {
      const yBlk = yOf(b.blocks.size);
      barsSvg += `<circle cx="${cx.toFixed(1)}" cy="${yBlk.toFixed(1)}" r="2.7" fill="#a78bfa" stroke="#070b12" stroke-width="1"/>`;
    }
    if (b.wins > 0) {
      const yWin = Math.max(8, yOf(tot) - 7);
      barsSvg += `<polygon points="${cx.toFixed(1)},${(yWin - 4.5).toFixed(1)} ${(cx + 4.5).toFixed(1)},${yWin.toFixed(1)} ${cx.toFixed(1)},${(yWin + 4.5).toFixed(1)} ${(cx - 4.5).toFixed(1)},${yWin.toFixed(1)}" fill="#34d399" stroke="#070b12" stroke-width="1"/>`;
    }
    barsSvg += `<rect x="${xSlot.toFixed(1)}" y="${T}" width="${slotW.toFixed(1)}" height="${ph}" fill="transparent" onmousemove="showBundleTip(${i})"/>`;
  }

  const tickCount = Math.min(numBuckets, 7);
  let xTicks = '';
  for (let k = 0; k < tickCount; k++) {
    const i = tickCount === 1 ? 0 : Math.round((k * (numBuckets - 1)) / (tickCount - 1));
    const cx = L + (i + 0.5) * slotW;
    xTicks += `<text x="${cx.toFixed(1)}" y="${H - 6}" text-anchor="middle" fill="#7c8da6" font-size="10.5" font-family="monospace">${fmtHHMM(buckets[i].t)}</text>`;
  }

  const emptyMsg = totBundles === 0
    ? `<text x="${(L + pw / 2).toFixed(1)}" y="${(T + ph / 2).toFixed(1)}" text-anchor="middle" fill="#64748b" font-size="12" font-family="monospace">${esc(t('no_bundles_win'))}</text>`
    : '';

  svgWrap.innerHTML =
    `<svg viewBox="0 0 ${W} ${H}" preserveAspectRatio="none" onmouseleave="hideBundleTip()">` +
      `<line x1="${L}" y1="${yOf(maxY).toFixed(1)}" x2="${W - R}" y2="${yOf(maxY).toFixed(1)}" stroke="rgba(148,163,184,0.14)" stroke-dasharray="3 3"/>` +
      `<line x1="${L}" y1="${yOf(midY).toFixed(1)}" x2="${W - R}" y2="${yOf(midY).toFixed(1)}" stroke="rgba(148,163,184,0.14)" stroke-dasharray="3 3"/>` +
      `<line x1="${L}" y1="${(T + ph).toFixed(1)}" x2="${W - R}" y2="${(T + ph).toFixed(1)}" stroke="rgba(148,163,184,0.28)"/>` +
      `<text x="${L - 6}" y="${(yOf(maxY) + 4).toFixed(1)}" text-anchor="end" fill="#7c8da6" font-size="10.5" font-family="monospace">${maxY}</text>` +
      `<text x="${L - 6}" y="${(yOf(midY) + 4).toFixed(1)}" text-anchor="end" fill="#7c8da6" font-size="10.5" font-family="monospace">${midY}</text>` +
      `<text x="${L - 6}" y="${(T + ph + 4).toFixed(1)}" text-anchor="end" fill="#7c8da6" font-size="10.5" font-family="monospace">0</text>` +
      barsSvg + emptyMsg + xTicks +
    `</svg>`;
  if (hoveredBucketIdx >= 0 && hoveredBucketIdx < lastBucketStats.length) {
    showBundleTip(hoveredBucketIdx);
  }
}

function renderChart(trades) {
  const sig = `${currentLang}:${trades && trades.length ? `${trades.length}:${trades[trades.length - 1].tx_hash || ''}:${trades[trades.length - 1].net_usd || 0}` : 'empty'}`;
  if (prevSig.pnlChart === sig) return;
  prevSig.pnlChart = sig;

  const el = document.getElementById('pnlChart');
  if (!trades || trades.length === 0) {
    el.innerHTML = `<div style="color:var(--muted);padding:50px 0;text-align:center">${esc(t('no_trades_yet'))}</div>`;
    return;
  }
  let cum = 0;
  const pts = [0];
  for (const tr of trades) {
    cum += Number(tr.net_usd || 0);
    pts.push(cum);
  }
  const w = 640, h = 140, pad = 16;
  const minV = Math.min(...pts, 0);
  const maxV = Math.max(...pts, 0.001);
  const span = (maxV - minV) || 1;
  const xStep = pts.length > 1 ? (w - pad * 2) / (pts.length - 1) : 0;
  const coords = pts.map((v, i) => {
    const x = pad + i * xStep;
    const y = h - pad - ((v - minV) / span) * (h - pad * 2);
    return [x, y, v];
  });
  const zeroY = h - pad - ((0 - minV) / span) * (h - pad * 2);
  const poly = coords.map(c => `${c[0].toFixed(1)},${c[1].toFixed(1)}`).join(' ');
  const area = `${coords[0][0].toFixed(1)},${zeroY.toFixed(1)} ${poly} ${coords[coords.length-1][0].toFixed(1)},${zeroY.toFixed(1)}`;
  const last = coords[coords.length - 1];
  const color = last[2] >= 0 ? '#34d399' : '#fb7185';
  const statLabel = t('chart_stats', { max: maxV.toFixed(4), min: minV.toFixed(4), cur: last[2].toFixed(4) });
  el.innerHTML = `
    <svg viewBox="0 0 ${w} ${h}" preserveAspectRatio="none">
      <defs>
        <linearGradient id="g" x1="0" y1="0" x2="0" y2="1">
          <stop offset="0%" stop-color="${color}" stop-opacity="0.32"/>
          <stop offset="100%" stop-color="${color}" stop-opacity="0.0"/>
        </linearGradient>
      </defs>
      <line x1="${pad}" y1="${zeroY.toFixed(1)}" x2="${w-pad}" y2="${zeroY.toFixed(1)}" stroke="rgba(148,163,184,0.28)" stroke-dasharray="4 4"/>
      <polygon points="${area}" fill="url(#g)"/>
      <polyline points="${poly}" fill="none" stroke="${color}" stroke-width="2.4" stroke-linejoin="round" stroke-linecap="round"/>
      ${coords.map(c => `<circle cx="${c[0].toFixed(1)}" cy="${c[1].toFixed(1)}" r="3.2" fill="${c[2]>=0?'#34d399':'#fb7185'}"/>`).join('')}
      <text x="${pad}" y="12" fill="#7c8da6" font-size="11" font-family="monospace">${esc(statLabel)}</text>
    </svg>`;
}

function renderBars(containerId, sigKey, items, isErr) {
  const sig = `${currentLang}:${JSON.stringify(items || [])}`;
  if (prevSig[sigKey] === sig) return;
  prevSig[sigKey] = sig;
  const el = document.getElementById(containerId);
  if (!items || items.length === 0) {
    el.innerHTML = `<div style="color:var(--muted);font-size:12px">${esc(t('no_data'))}</div>`;
    return;
  }
  const maxVal = Math.max(...items.map(x => x[1]), 1);
  el.innerHTML = items.map(([k, v]) => `
    <div class="bar-row">
      <div class="bar-meta"><span class="mono">${esc(k)}</span><span class="mono">${v}</span></div>
      <div class="bar-track"><div class="bar-fill ${isErr?'err':''}" style="width:${(v/maxVal*100).toFixed(1)}%"></div></div>
    </div>
  `).join('');
}

function renderOpps(forceRedraw) {
  if (!state) return;
  const q = (document.getElementById('searchBox').value || '').toLowerCase();
  const recent = state.recent || [];
  const headSig = recent.length ? `${recent[0].timestamp}:${recent[0].block}:${recent.length}:${state.sampled || 0}` : 'empty';
  const sig = `${currentLang}|${oppFilter}|${q}|${headSig}`;
  if (!forceRedraw && prevSig.opps === sig) return;
  prevSig.opps = sig;

  const list = recent.filter(r => {
    if (oppFilter === 'ok' && !r.sim_ok) return false;
    if (oppFilter === 'fail' && r.sim_ok) return false;
    if (!q) return true;
    return `${r.strategy} ${r.base_token} ${r.err_short} ${r.route}`.toLowerCase().includes(q);
  });
  const okPill = `<span class="pill ok">${esc(t('pill_sim_ok'))}</span>`;
  const html = list.map(r => `
    <tr>
      <td class="mono">${esc(fmtLocal(r.timestamp))}</td>
      <td class="mono">${r.block}</td>
      <td class="mono">${esc(r.strategy)}</td>
      <td class="mono">${esc(r.base_token)}</td>
      <td class="mono">${Number(r.borrow_human || 0).toFixed(2)}</td>
      <td class="mono">$${Number(r.est_net_usd || 0).toFixed(2)}</td>
      <td class="mono">${r.sim_net_usd != null ? '$' + Number(r.sim_net_usd).toFixed(4) : '-'}</td>
      <td>${r.sim_ok ? okPill : `<span class="pill fail">${esc(r.err_short)}</span>`}</td>
      <td class="mono" style="color:#93c5fd">${esc(r.route)}</td>
    </tr>
  `).join('');
  setHtmlPreserveScroll('oppsBody', html);
}

function renderTick(tickData) {
  if (!tickData) return;
  if (state) {
    state.server_time = tickData.server_time;
    state.server_epoch = tickData.server_epoch;
    state.latest_scan = tickData.latest_scan;
    state.vm = tickData.vm;
  }
  setText('syncTime', 'SSE ' + fmtLocal(tickData.server_time));
  const vm = tickData.vm || {};
  setText('vmBadge', vm.mem_used_mb != null
    ? t('vm_badge', { load: vm.load_1m, used: vm.mem_used_mb, total: vm.mem_total_mb })
    : t('vm_local'));

  const sc = tickData.latest_scan;
  if (sc) {
    setText('kpiBlock', sc.block);
    setText('kpiSpeed', t('kpi_speed', { solve_ms: (sc.solve_us / 1000).toFixed(1), blk_ms: sc.blk_ms, txs: sc.txs }));
    setText('kpiPools', sc.total_pools.toLocaleString());
  }
}

function chainMeta(s) {
  const m = (s && s.chain_meta) || {};
  const ch = m.chain || (typeof s === 'string' ? s : (s && s.chain)) || 'bsc';
  const tmpl = m.explorer_tx_url || (ch === 'base' ? 'https://basescan.org/tx/{hash}' : (ch === 'eth' ? 'https://etherscan.io/tx/{hash}' : 'https://bscscan.com/tx/{hash}'));
  const expBase = tmpl.replace(/\/tx\/\{hash\}$/, '');
  const phalconChain = m.phalcon_chain || (ch === 'eth' ? 'ethereum' : ch);
  const sym = m.native_symbol || (ch === 'bsc' ? 'BNB' : 'ETH');
  return { ch, expBase, phalconChain, sym };
}

function renderTradesSection(s) {
  const { expBase, phalconChain } = chainMeta(s);
  const pnl = s.realized_pnl || {};
  const trades = pnl.trades || [];
  setText('tradeCountBadge', t('trade_count', { n: trades.length }));

  renderChart(trades);
  const lastT = trades.length ? trades[trades.length - 1] : null;
  const tradesSig = `${currentLang}:${trades.length}:${lastT ? `${lastT.tx_hash || ''}:${lastT.status || ''}` : 'empty'}`;
  if (prevSig.trades !== tradesSig) {
    prevSig.trades = tradesSig;
    const html = [...trades].reverse().map(tr => {
      const ok = String(tr.status || '').includes('CONFIRMED');
      const n = Number(tr.net_usd || 0);
      const hash = tr.tx_hash || '';
      const explorer = `${expBase}/tx/${hash}`;
      const phalcon = `https://app.blocksec.com/explorer/tx/${phalconChain}/${hash}`;
      return `<tr>
        <td class="mono">${esc(fmtLocal(tr.timestamp))}</td>
        <td class="mono">${tr.block_number}</td>
        <td class="mono">${esc(tr.strategy)}</td>
        <td><span class="pill ${ok?'ok':'warn'}">${esc(tr.status)}</span></td>
        <td class="mono">$${Number(tr.gross_usd||0).toFixed(4)}</td>
        <td class="mono">$${Number(tr.gas_usd||0).toFixed(4)}</td>
        <td class="mono ${n>=0?'pos':'neg'}">${n>=0?'+':'-'}$${Math.abs(n).toFixed(4)}</td>
        <td class="mono">
          <a class="tx" href="${explorer}" target="_blank">${esc(hash.slice(0,10))}</a>
          · <a class="tx" href="${phalcon}" target="_blank" style="color:#34d399">Phalcon</a>
        </td>
      </tr>`;
    }).join('') || `<tr><td colspan="8" style="color:var(--muted)">${esc(t('no_trades_yet'))}</td></tr>`;
    setHtmlPreserveScroll('tradesBody', html);
  }
}

function renderAll(s) {
  state = s;
  setText('chainBadge', (s.chain || 'bsc').toUpperCase());
  setText('syncTime', (evtSource ? 'SSE ' : '') + fmtLocal(s.server_time));
  const vm = s.vm || {};
  setText('vmBadge', vm.mem_used_mb != null
    ? t('vm_badge', { load: vm.load_1m, used: vm.mem_used_mb, total: vm.mem_total_mb })
    : t('vm_local'));

  const { ch, expBase, sym } = chainMeta(s);

  const pnl = s.realized_pnl || {};
  const eq = s.onchain_equity || {};

  const net = Number(pnl.cumulative_net_usd ?? 0);
  setText('kpiNet', (net >= 0 ? '+$' : '-$') + Math.abs(net).toFixed(4));
  setClass('kpiNet', 'kpi-val mono ' + (net >= 0 ? 'pos' : 'neg'));
  let netSub = t('kpi_net_sub', {
    gross: Number(pnl.cumulative_gross_usd || 0).toFixed(4),
    gas: Number(pnl.cumulative_gas_usd || 0).toFixed(4),
  });
  if (eq.delta_arb_usd != null) {
    const da = Number(eq.delta_arb_usd);
    netSub += t('kpi_net_sub_onchain', { delta: `${da >= 0 ? '+$' : '-$'}${Math.abs(da).toFixed(4)}` });
  }
  setText('kpiNetSub', netSub);

  const hasEq = eq.total_equity_usd != null && Number(eq.total_equity_usd) > 0;
  setText('kpiEquity', hasEq ? `$${Number(eq.total_equity_usd).toFixed(2)}` : '--');
  setText('kpiEquitySub', hasEq
    ? t('kpi_equity_sub', {
        sym,
        native: Number(eq.eoa_bnb_total || 0).toFixed(5),
        stables: Number(eq.owner_stables_usd || 0).toFixed(4),
        exec: Number(eq.executor_tokens_usd || 0).toFixed(2),
      })
    : t('waiting_rpc'));

  const totTr = Number(pnl.total_trades || 0);
  const winTr = Number(pnl.winning_trades || 0);
  setText('kpiTrades', `${winTr} / ${totTr}`);
  setText('kpiWinRate', totTr > 0
    ? t('kpi_win_rate', { rate: (winTr / totTr * 100).toFixed(1), rev: pnl.reverted_trades || 0 })
    : t('no_trades_short'));

  const sc = s.latest_scan;
  setText('kpiBlock', sc ? sc.block : (s.block_max || '--'));
  setText('kpiSpeed', sc
    ? t('kpi_speed', { solve_ms: (sc.solve_us / 1000).toFixed(1), blk_ms: sc.blk_ms, txs: sc.txs })
    : t('waiting_runtime_log'));

  setText('kpiPools', sc ? sc.total_pools.toLocaleString() : '--');
  setText('kpiQuarantine', t('kpi_quarantine', { count: s.quarantined }));

  setText('kpiOpps', `${s.sim_ok_count} / ${s.sampled}`);
  setText('kpiBest', t('kpi_best', { est: Number(s.best_est_usd || 0).toFixed(2), sim: Number(s.best_sim_usd || 0).toFixed(3) }));

  const execSig = `${currentLang}:${ch}:${s.executor || ''}`;
  if (prevSig.exec !== execSig) {
    prevSig.exec = execSig;
    const execEl = document.getElementById('execBadge');
    if (s.executor) {
      execEl.innerHTML = `${esc(t('executor_lbl'))} <a class="tx" href="${expBase}/address/${esc(s.executor)}" target="_blank">${esc(s.executor.slice(0,10))}...</a>`;
    } else {
      execEl.textContent = t('executor_idle');
    }
  }

  const ot = eq.owner_tokens || {};
  const usdtVal = Number(ot.USDT || ot.USDC || 0);
  const stableSym = ot.USDT != null ? 'USDT' : (ot.USDC != null ? 'USDC' : 'USD');
  setText('ownerStablesBadge', t('owner_stables_fmt', {
    usd: Number(eq.owner_stables_usd || 0).toFixed(4),
    sym: stableSym,
    amt: usdtVal.toFixed(4),
  }));
  const execResid = Number(eq.executor_tokens_usd || 0);
  setText('execResidBadge', t('exec_resid_fmt', { usd: execResid.toFixed(4) }));
  setClass('execResidBadge', 'badge mono' + (execResid > 0.001 ? ' neg' : ''));

  const eoas = s.eoa_accounts || [];
  const totalNative = eoas.reduce((acc, e) => acc + Number(e.balance_native || 0), 0);
  const totalUsd = eoas.reduce((acc, e) => acc + Number(e.balance_usd || 0), 0);
  setText('eoaPoolBadge', t('eoa_pool_fmt', {
    n: eoas.length,
    native: totalNative.toFixed(4),
    sym,
    usd: totalUsd.toFixed(2),
  }));
  const eoaSig = `${currentLang}:${JSON.stringify(eoas)}`;
  if (prevSig.eoas !== eoaSig) {
    prevSig.eoas = eoaSig;
    const html = eoas.map(e => {
      const isPrim = e.role === 'primary';
      return `<tr>
        <td class="mono">${e.index}</td>
        <td><span class="pill ${isPrim?'ok':'warn'}">${esc(e.role.toUpperCase())}</span></td>
        <td class="mono"><a class="tx" href="${expBase}/address/${esc(e.address)}" target="_blank">${esc(e.address)}</a></td>
        <td class="mono">${Number(e.balance_native||0).toFixed(6)} ${sym}</td>
        <td class="mono">$${Number(e.balance_usd||0).toFixed(2)}</td>
        <td class="mono">${e.nonce}</td>
        <td class="mono cy">${e.recent_submits || 0}</td>
      </tr>`;
    }).join('') || `<tr><td colspan="7" style="color:var(--muted)">${esc(t('no_eoas'))}</td></tr>`;
    setHtmlPreserveScroll('eoasBody', html);
  }

  renderBundleChart(s, false);
  const bundles = s.recent_bundles || [];
  const bundlesSig = `${currentLang}:${bundles.length ? `${bundles.length}:${bundles[0].tx_hash || ''}:${bundles[0].block || ''}` : 'empty'}`;
  if (prevSig.bundles !== bundlesSig) {
    prevSig.bundles = bundlesSig;
    const html = bundles.map(b => {
      const isBr = b.kind === 'BACKRUN';
      const h = b.tx_hash || '';
      return `<tr>
        <td class="mono">${esc(fmtLocal(b.timestamp))}</td>
        <td><span class="pill ${isBr?'warn':'ok'}">${esc(b.kind)}</span></td>
        <td class="mono">${esc(b.block)}</td>
        <td class="mono">${esc((b.eoa||'-').slice(0,10))}</td>
        <td class="mono">${esc(b.strategy)}</td>
        <td class="mono">${esc(b.gross_usd)}</td>
        <td class="mono">${esc(b.tip_usd)}</td>
        <td class="mono pos">${esc(b.expected_net_usd)}</td>
        <td class="mono">${esc(b.pre_submit_ms)}ms</td>
        <td class="mono"><a class="tx" href="${expBase}/tx/${esc(h)}" target="_blank">${esc(h.slice(0,10))}</a></td>
      </tr>`;
    }).join('') || `<tr><td colspan="10" style="color:var(--muted)">${esc(t('no_bundles_tail'))}</td></tr>`;
    setHtmlPreserveScroll('bundlesBody', html);
  }

  renderTradesSection(s);

  renderBars('stratBars', 'strats', s.top_strategies, false);
  renderBars('errBars', 'errs', s.top_errors, true);
  renderOpps(false);
  if (typeof onTopoStateUpdate === 'function') {
    onTopoStateUpdate(s);
  }
}

function scheduleFlush() {
  if (rafScheduled) return;
  rafScheduled = true;
  requestAnimationFrame(() => {
    rafScheduled = false;
    if (pendingState !== null) {
      const s = pendingState;
      const tk = pendingTick;
      pendingState = null;
      pendingTick = null;
      if (tk) {
        s.server_time = tk.server_time;
        s.server_epoch = tk.server_epoch;
        s.latest_scan = tk.latest_scan;
        s.vm = tk.vm;
      }
      renderAll(s);
    } else if (pendingTick !== null) {
      const tk = pendingTick;
      pendingTick = null;
      renderTick(tk);
    }
  });
}

async function fetchState(force) {
  try {
    const url = force ? '/api/state?force=1' : '/api/state';
    const res = await fetch(url, { cache: 'no-store' });
    const data = await res.json();
    pendingState = data;
    scheduleFlush();
  } catch (e) {
    setDotState('err');
    setText('syncTime', t('sync_lost'));
  }
}

function closeStream() {
  if (evtSource) {
    evtSource.close();
    evtSource = null;
  }
  if (pollTimer) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

function connectStream() {
  closeStream();
  if (!streaming) return;
  if (window.EventSource) {
    evtSource = new EventSource('/api/stream');
    evtSource.onopen = () => {
      setDotState('live');
      setText('togglePoll', t('live_sse'));
    };
    evtSource.onmessage = (ev) => {
      try {
        const data = JSON.parse(ev.data);
        setDotState('live');
        if (data && data.msg_type === 'tick') {
          pendingTick = data;
        } else {
          pendingState = data;
        }
        scheduleFlush();
      } catch (e) {}
    };
    evtSource.onerror = () => {
      setDotState('paused');
      setText('syncTime', t('sync_reconnecting'));
    };
  } else {
    setText('togglePoll', t('live_3s'));
    fetchState(false);
    pollTimer = setInterval(() => { if (streaming) fetchState(false); }, 3000);
  }
}

window.addEventListener('beforeunload', closeStream);
(function initTokenFromQuery() {
  try {
    const p = new URLSearchParams(window.location.search);
    const tk = p.get('token');
    if (tk) {
      sessionStorage.setItem('pyrite_token', tk);
      p.delete('token');
      const cleanUrl = window.location.pathname + (p.toString() ? '?' + p.toString() : '') + window.location.hash;
      window.history.replaceState({}, '', cleanUrl);
    }
    const saved = sessionStorage.getItem('pyrite_token') || '';
    const inp = document.getElementById('adminTokenInput');
    if (inp && saved) inp.value = saved;
  } catch (e) {}
})();

function saveAdminToken(val) {
  try { sessionStorage.setItem('pyrite_token', val || ''); } catch (e) {}
}

function getAdminToken() {
  const inp = document.getElementById('adminTokenInput');
  if (inp && inp.value) return inp.value.trim();
  try { return (sessionStorage.getItem('pyrite_token') || '').trim(); } catch (e) { return ''; }
}

const TAB_ROUTES = {
  monitor: '/monitor',
  topology: '/topology',
  config: '/config',
  wallet: '/wallet',
};

function tabFromPath(pathname) {
  const clean = String(pathname || '/').replace(/\/+$/, '').toLowerCase();
  if (clean === '/topology') return 'topology';
  if (clean === '/config') return 'config';
  if (clean === '/wallet') return 'wallet';
  return 'monitor';
}

function switchTab(name, updateHistory) {
  const tab = TAB_ROUTES[name] ? name : 'monitor';
  document.querySelectorAll('.tab-panel').forEach(el => el.classList.remove('active'));
  const target = document.getElementById('tab-' + tab);
  if (target) target.classList.add('active');
  document.querySelectorAll('[data-tab]').forEach(btn => {
    btn.classList.toggle('active', btn.getAttribute('data-tab') === tab);
  });
  if (updateHistory !== false) {
    try {
      const targetPath = TAB_ROUTES[tab];
      if (window.location.pathname.replace(/\/+$/, '') !== targetPath) {
        window.history.pushState({ tab }, '', targetPath + window.location.search + window.location.hash);
      }
    } catch (e) {}
  }
  if (tab === 'config' || tab === 'wallet') loadControlState();
  if (tab === 'topology') {
    ensureTopoReady();
  } else {
    stopTopoLoop();
  }
}

window.addEventListener('popstate', () => {
  switchTab(tabFromPath(window.location.pathname), false);
});

const TOPO_HUB_COLORS = ['#38bdf8', '#fbbf24', '#34d399', '#a78bfa', '#fb7185', '#60a5fa', '#f472b6', '#2dd4bf'];
const TOPO_OK_COLORS = ['#34d399', '#22d3ee', '#fbbf24', '#2dd4bf'];
const TOPO_FAIL_COLORS = ['#f59e0b', '#fb7185', '#f97316', '#c084fc'];
const TOPO_FAIL_HOLD_MS = 1800;
const TOPO_OK_HOLD_MS = 2400;
const TOPO_FAIL_TTL_MS = 6800;
const TOPO_OK_TTL_MS = 8400;
const TOPO_MAX_PULSES = 24;

let topoData = null;
let topoLayout = null;
let topoLayoutGen = 0;
let topoLoadedChain = '';
let topoLoadedMtimeMs = 0;
let topoLastScanPools = -1;
let topoAutoFollow = true;
let topoFilter = 'all';
let topoSelectedOpp = null;
let topoSelectedKey = '';
let topoChosenPool = null;
const topoHubSlotsByChain = new Map();
let topoKnownPoolAddrs = new Set();
let topoNewPoolPings = [];
let topoPulses = [];
let topoSeenKeys = new Set();
let topoStreamInitialized = false;
let topoView = { zoom: 1.0, panX: 0.0, panY: 0.0 };
let topoWidth = 1;
let topoHeight = 1;
let topoDpr = 1;
let topoRafId = 0;
let topoFetchInFlight = false;
let topoOffCanvas = null;
let topoDrag = null;
let topoHoverNode = null;
let topoResizeObs = null;

function hash32(str, seed) {
  let h = (2166136261 ^ (seed || 0)) >>> 0;
  for (let i = 0; i < str.length; i++) {
    h ^= str.charCodeAt(i);
    h = Math.imul(h, 16777619) >>> 0;
  }
  return h >>> 0;
}

function hash01(str, seed) {
  return (hash32(str, seed) & 0x00ffffff) / 16777216.0;
}

function pairKey(a, b) {
  return a < b ? `${a}|${b}` : `${b}|${a}`;
}

function normVenue(v) {
  return String(v || '').toLowerCase().replace(/^generic/, '').replace(/bsc$/, '');
}

function parseTopoRoute(opp) {
  if (!opp || !opp.route) return { legs: [], extra: 0 };
  const rawParts = String(opp.route).split(' | ');
  const legs = [];
  let extra = 0;
  for (const rawPart of rawParts) {
    const moreM = rawPart.match(/\(\+(\d+)\s+more(?:,\s*\d+\s+steps)?\)/i);
    if (moreM) extra += Number(moreM[1] || 0);
    const clean = rawPart.replace(/\s*\(\+\d+\s+more(?:,\s*\d+\s+steps)?\)\s*$/i, '').trim();
    const colon = clean.indexOf(':');
    const arrow = clean.indexOf('->', colon + 1);
    if (colon <= 0 || arrow <= colon) continue;
    const venue = clean.slice(0, colon).trim();
    const from = clean.slice(colon + 1, arrow).trim();
    const to = clean.slice(arrow + 2).trim();
    if (venue && from && to) legs.push({ venue, from, to });
  }
  return { legs, extra };
}

function oppKey(opp) {
  if (!opp) return '';
  return `${opp.timestamp || ''}|${opp.block || 0}|${opp.strategy || ''}|${opp.route || ''}`;
}

function topoHubSeat(slot, totalSlots) {
  if (slot === 0) return { x: 0.45, y: 0.50, isPrimary: true };
  const ringCount = Math.max(1, totalSlots - 1);
  const angle = ((slot - 1) / ringCount) * Math.PI * 2 - Math.PI * 0.48;
  return {
    x: 0.46 + Math.cos(angle) * 0.30,
    y: 0.50 + Math.sin(angle) * 0.33,
    isPrimary: false,
  };
}

function buildTopoLayout(data) {
  if (!data || !Array.isArray(data.pools)) return null;
  const tokens = data.tokens || [];
  const venues = data.venues || [];
  const hubs = data.hubs || [];
  const hubCounts = data.hub_counts || [];

  const chainKey = String(data.chain || topoLoadedChain || 'default');
  let slotMap = topoHubSlotsByChain.get(chainKey);
  if (!slotMap) {
    slotMap = new Map();
    topoHubSlotsByChain.set(chainKey, slotMap);
  }

  const activeHubSyms = new Set();
  for (let i = 0; i < hubs.length; i++) {
    activeHubSyms.add(tokens[hubs[i]] || '?');
  }
  for (const oldSym of Array.from(slotMap.keys())) {
    if (!activeHubSyms.has(oldSym)) slotMap.delete(oldSym);
  }
  const usedSlots = new Set();
  for (const seat of slotMap.values()) usedSlots.add(seat.slot);
  const totalSlots = Math.max(8, hubs.length);

  const hubAnchors = new Map();
  const hubColorByIdx = new Map();
  const hubMetaList = [];
  for (let i = 0; i < hubs.length; i++) {
    const tokIdx = hubs[i];
    const sym = tokens[tokIdx] || '?';
    let seat = slotMap.get(sym);
    if (!seat) {
      let slot = 0;
      while (usedSlots.has(slot)) slot++;
      usedSlots.add(slot);
      const pos = topoHubSeat(slot, totalSlots);
      seat = {
        slot,
        x: pos.x,
        y: pos.y,
        isPrimary: pos.isPrimary,
        color: TOPO_HUB_COLORS[slot % TOPO_HUB_COLORS.length],
      };
      slotMap.set(sym, seat);
    }
    const anchor = {
      x: seat.x,
      y: seat.y,
      color: seat.color,
      name: sym,
      count: hubCounts[i] || 0,
      tokIdx,
      rank: seat.slot,
      isPrimary: seat.isPrimary,
    };
    hubAnchors.set(tokIdx, anchor);
    hubColorByIdx.set(tokIdx, seat.color);
    hubMetaList.push(anchor);
  }

  const symToTokIdx = new Map();
  for (let i = 0; i < tokens.length; i++) {
    if (!symToTokIdx.has(tokens[i])) symToTokIdx.set(tokens[i], i);
  }

  const outerSeats = new Map();
  function outerAnchorFor(tokIdx, sym) {
    const sk = sym || String(tokIdx);
    if (outerSeats.has(sk)) return outerSeats.get(sk);
    const u1 = hash01(sk, 11);
    const u2 = hash01(sk, 29);
    const theta = u1 * Math.PI * 2;
    const rad = 0.41 + u2 * 0.06;
    const pt = {
      x: Math.max(0.06, Math.min(0.94, 0.47 + Math.cos(theta) * rad)),
      y: Math.max(0.08, Math.min(0.92, 0.50 + Math.sin(theta) * rad)),
      color: '#64748b',
    };
    outerSeats.set(sk, pt);
    return pt;
  }

  const nodes = [];
  const buckets = new Map();
  const pairVenueMap = new Map();
  const pairMap = new Map();
  const bridgeMap = new Map();
  const gridCells = 48;
  const grid = new Map();

  for (let idx = 0; idx < data.pools.length; idx++) {
    const [addr, id0, id1, vid] = data.pools[idx];
    const symA = tokens[id0] || '?';
    const symB = tokens[id1] || '?';
    const venue = venues[vid] || 'DEX';
    const h0 = hubAnchors.get(id0);
    const h1 = hubAnchors.get(id1);

    let nx;
    let ny;
    let color = '#64748b';
    let groupRank = 99;

    if (h0 && h1) {
      const aH = h0.name <= h1.name ? h0 : h1;
      const bH = aH === h0 ? h1 : h0;
      const u1 = hash01(addr, 103);
      const u2 = hash01(addr, 219);
      const tPos = 0.14 + u1 * 0.72;
      const dx = bH.x - aH.x;
      const dy = bH.y - aH.y;
      const len = Math.hypot(dx, dy) || 1;
      const perp = (u2 - 0.5) * 0.055;
      nx = aH.x + dx * tPos + (-dy / len) * perp;
      ny = aH.y + dy * tPos + (dx / len) * perp;
      const colorH = h0.rank < h1.rank ? h1 : h0;
      color = colorH.color;
      groupRank = colorH.rank;
      const bk = pairKey(aH.name, bH.name);
      if (!bridgeMap.has(bk)) bridgeMap.set(bk, [aH, bH]);
    } else if (h0 || h1) {
      const hub = h0 || h1;
      const u1 = hash01(addr, 307);
      const u2 = hash01(addr, 443);
      const r = (0.024 + Math.sqrt(u1) * 0.095) * (hub.isPrimary ? 1.18 : 0.92);
      const theta = u2 * Math.PI * 2;
      nx = hub.x + Math.cos(theta) * r;
      ny = hub.y + Math.sin(theta) * r;
      color = hub.color;
      groupRank = hub.rank;
    } else {
      const symIsA = symA <= symB;
      const anchor = outerAnchorFor(symIsA ? id0 : id1, symIsA ? symA : symB);
      const u1 = hash01(addr, 559);
      const u2 = hash01(addr, 673);
      nx = anchor.x + (u1 - 0.5) * 0.045;
      ny = anchor.y + (u2 - 0.5) * 0.045;
      color = '#64748b';
    }

    nx = Math.max(0.03, Math.min(0.97, nx));
    ny = Math.max(0.05, Math.min(0.95, ny));

    const node = { idx, addr, symA, symB, venue, x: nx, y: ny, color, groupRank };
    nodes.push(node);

    if (!buckets.has(color)) buckets.set(color, []);
    buckets.get(color).push(node);

    const pk = pairKey(symA, symB);
    const pvk = `${pk}|${venue.toLowerCase()}`;
    if (!pairVenueMap.has(pvk)) pairVenueMap.set(pvk, []);
    pairVenueMap.get(pvk).push(node);
    if (!pairMap.has(pk)) pairMap.set(pk, []);
    pairMap.get(pk).push(node);

    const gx = Math.max(0, Math.min(gridCells - 1, Math.floor(nx * gridCells)));
    const gy = Math.max(0, Math.min(gridCells - 1, Math.floor(ny * gridCells)));
    const gKey = gy * gridCells + gx;
    if (!grid.has(gKey)) grid.set(gKey, []);
    grid.get(gKey).push(node);
  }

  const hubBySymbol = new Map();
  for (const hm of hubMetaList) {
    if (!hubBySymbol.has(hm.name)) hubBySymbol.set(hm.name, hm);
  }

  return {
    tokens,
    venues,
    hubMetaList,
    hubBySymbol,
    symToTokIdx,
    outerAnchorFor,
    nodes,
    buckets,
    pairVenueMap,
    pairMap,
    bridgeMap,
    grid,
    gridCells,
  };
}

function topoToScreen(nx, ny) {
  return {
    x: ((nx - 0.5) * topoView.zoom + 0.5) * topoWidth + topoView.panX,
    y: ((ny - 0.5) * topoView.zoom + 0.5) * topoHeight + topoView.panY,
  };
}

function screenToTopo(sx, sy) {
  return {
    x: ((sx - topoView.panX) / topoWidth - 0.5) / topoView.zoom + 0.5,
    y: ((sy - topoView.panY) / topoHeight - 0.5) / topoView.zoom + 0.5,
  };
}

function drawPillRect(ctx, x, y, w, h, r) {
  const rr = Math.min(r, w * 0.5, h * 0.5);
  ctx.beginPath();
  ctx.moveTo(x + rr, y);
  ctx.lineTo(x + w - rr, y);
  ctx.quadraticCurveTo(x + w, y, x + w, y + rr);
  ctx.lineTo(x + w, y + h - rr);
  ctx.quadraticCurveTo(x + w, y + h, x + w - rr, y + h);
  ctx.lineTo(x + rr, y + h);
  ctx.quadraticCurveTo(x, y + h, x, y + h - rr);
  ctx.lineTo(x, y + rr);
  ctx.quadraticCurveTo(x, y, x + rr, y);
  ctx.closePath();
}

function redrawTopoStatic() {
  if (!topoOffCanvas) topoOffCanvas = document.createElement('canvas');
  topoOffCanvas.width = Math.max(1, Math.round(topoWidth * topoDpr));
  topoOffCanvas.height = Math.max(1, Math.round(topoHeight * topoDpr));
  const octx = topoOffCanvas.getContext('2d');
  octx.setTransform(topoDpr, 0, 0, topoDpr, 0, 0);
  octx.clearRect(0, 0, topoWidth, topoHeight);
  if (!topoLayout) return;

  octx.lineWidth = 0.8;
  octx.strokeStyle = 'rgba(124, 141, 166, 0.14)';
  for (const [aH, bH] of topoLayout.bridgeMap.values()) {
    const pA = topoToScreen(aH.x, aH.y);
    const pB = topoToScreen(bH.x, bH.y);
    octx.beginPath();
    octx.moveTo(pA.x, pA.y);
    octx.lineTo(pB.x, pB.y);
    octx.stroke();
  }

  for (const hub of topoLayout.hubMetaList) {
    const p = topoToScreen(hub.x, hub.y);
    const rad = Math.max(24, Math.min(topoWidth, topoHeight) * 0.14) * Math.sqrt(topoView.zoom);
    const grad = octx.createRadialGradient(p.x, p.y, 0, p.x, p.y, rad);
    grad.addColorStop(0, hub.color + '24');
    grad.addColorStop(1, hub.color + '00');
    octx.fillStyle = grad;
    octx.beginPath();
    octx.arc(p.x, p.y, rad, 0, Math.PI * 2);
    octx.fill();
  }

  const dotSize = Math.max(1.5, Math.min(3.8, 1.8 * Math.sqrt(topoView.zoom)));
  const half = dotSize * 0.5;
  for (const [color, list] of topoLayout.buckets.entries()) {
    octx.fillStyle = color + '7a';
    octx.beginPath();
    for (let i = 0; i < list.length; i++) {
      const p = topoToScreen(list[i].x, list[i].y);
      if (p.x < -6 || p.x > topoWidth + 6 || p.y < -6 || p.y > topoHeight + 6) continue;
      octx.rect(p.x - half, p.y - half, dotSize, dotSize);
    }
    octx.fill();
  }
}

function matchRouteOnLayout(opp) {
  const empty = { arcs: [], matchedPools: [], stationLabels: [], legs: [], extra: 0 };
  if (!topoLayout || !opp) return empty;
  const parsed = parseTopoRoute(opp);
  if (!parsed.legs.length) return empty;

  const matchedPools = [];
  const matchedSet = new Set();
  const anchorPoints = new Map();
  const legEntries = [];

  for (let i = 0; i < parsed.legs.length; i++) {
    const leg = parsed.legs[i];
    const pk = pairKey(leg.from, leg.to);
    const exactKey = `${pk}|${leg.venue.toLowerCase()}`;
    let hits = topoLayout.pairVenueMap.get(exactKey);
    if (!hits || !hits.length) {
      const vNorm = normVenue(leg.venue);
      const candidates = topoLayout.pairMap.get(pk) || [];
      const prefixHits = candidates.filter(n => {
        const nv = normVenue(n.venue);
        return nv === vNorm || (vNorm && nv.includes(vNorm)) || (nv && vNorm.includes(nv));
      });
      hits = prefixHits.length ? prefixHits : candidates.slice(0, 3);
    }
    for (const node of hits) {
      if (!matchedSet.has(node.idx)) {
        matchedSet.add(node.idx);
        matchedPools.push(node);
      }
    }
    legEntries.push({ ...leg, hits });
  }

  function resolveStation(sym) {
    if (anchorPoints.has(sym)) return anchorPoints.get(sym);
    const hub = topoLayout.hubBySymbol.get(sym);
    if (hub) {
      const st = { x: hub.x, y: hub.y, name: sym, isHub: true, color: hub.color };
      anchorPoints.set(sym, st);
      return st;
    }
    let sumX = 0;
    let sumY = 0;
    let count = 0;
    for (const n of matchedPools) {
      if (n.symA === sym || n.symB === sym) {
        sumX += n.x;
        sumY += n.y;
        count++;
      }
    }
    if (count > 0) {
      const st = { x: sumX / count, y: sumY / count, name: sym, isHub: false, color: '#e2e8f0' };
      anchorPoints.set(sym, st);
      return st;
    }
    const tid = topoLayout.symToTokIdx.get(sym);
    const oa = topoLayout.outerAnchorFor(tid != null ? tid : hash32(sym, 99), sym);
    const st = { x: oa.x, y: oa.y, name: sym, isHub: false, color: '#e2e8f0' };
    anchorPoints.set(sym, st);
    return st;
  }

  const arcs = [];
  const seenDir = new Map();
  const arcPalette = opp.sim_ok ? TOPO_OK_COLORS : TOPO_FAIL_COLORS;
  for (let i = 0; i < legEntries.length; i++) {
    const leg = legEntries[i];
    const fromPt = resolveStation(leg.from);
    const toPt = resolveStation(leg.to);
    if (fromPt === toPt) continue;
    const dk = `${leg.from}->${leg.to}`;
    const mult = seenDir.get(dk) || 0;
    seenDir.set(dk, mult + 1);
    const bend = 0.14 + mult * 0.09;
    arcs.push({
      from: fromPt,
      to: toPt,
      bend,
      color: arcPalette[i % arcPalette.length],
    });
  }

  const stationLabels = [];
  for (const st of anchorPoints.values()) {
    if (!st.isHub) stationLabels.push(st);
  }
  return { arcs, matchedPools, stationLabels, legs: parsed.legs, extra: parsed.extra };
}

let cachedMatchedRouteKey = '';
let cachedMatchedRoute = null;

function focusTopoOpp(opp) {
  topoSelectedOpp = opp || null;
  topoSelectedKey = opp ? oppKey(opp) : '';
  cachedMatchedRouteKey = '';
}

function getActiveRouteScene() {
  const key = `${topoLoadedChain}:${topoData ? topoData.pool_count : 0}:${topoSelectedKey}`;
  if (cachedMatchedRoute && cachedMatchedRouteKey === key) return cachedMatchedRoute;
  cachedMatchedRouteKey = key;
  cachedMatchedRoute = matchRouteOnLayout(topoSelectedOpp);
  return cachedMatchedRoute;
}

function routeSigOf(opp) {
  return String((opp && opp.route) || '').trim();
}

function computePulseAlpha(age, ttl, holdMs) {
  if (age < 0 || age >= ttl) return 0;
  if (age < 140) return age / 140;
  const hold = holdMs != null ? holdMs : Math.min(1800, ttl * 0.28);
  if (age <= hold) return 1.0;
  const rem = Math.max(0, 1 - (age - hold) / Math.max(1, ttl - hold));
  return Math.pow(rem, 1.25);
}

function pushTopoPulse(opp, bornMs) {
  if (!opp || !opp.route) return;
  const sig = routeSigOf(opp) || oppKey(opp);
  const existing = topoPulses.find(p => p.sig === sig);
  if (existing) {
    const prevSimOk = existing.simOk;
    existing.bornMs = bornMs;
    existing.opp = opp;
    existing.key = oppKey(opp);
    existing.hits = (existing.hits || 1) + 1;
    existing.simOk = existing.simOk || !!opp.sim_ok;
    existing.holdMs = existing.simOk ? TOPO_OK_HOLD_MS : TOPO_FAIL_HOLD_MS;
    existing.ttlMs = existing.simOk ? TOPO_OK_TTL_MS : TOPO_FAIL_TTL_MS;
    if (existing.simOk !== prevSimOk) existing.layoutGen = -1;
    return;
  }
  const simOk = !!opp.sim_ok;
  topoPulses.push({
    sig,
    key: oppKey(opp),
    opp,
    simOk,
    bornMs,
    holdMs: simOk ? TOPO_OK_HOLD_MS : TOPO_FAIL_HOLD_MS,
    ttlMs: simOk ? TOPO_OK_TTL_MS : TOPO_FAIL_TTL_MS,
    hits: 1,
    bendJitter: (hash01(sig, 71) - 0.5) * 0.14,
    layoutGen: -1,
    scene: null,
  });
  while (topoPulses.length > TOPO_MAX_PULSES) {
    const failIdx = topoPulses.findIndex(p => !p.simOk);
    topoPulses.splice(failIdx >= 0 ? failIdx : 0, 1);
  }
}

function seedDistinctTopoPulses(opps, maxCount, staggerMs) {
  if (!opps || !opps.length) return;
  const picked = [];
  const seenSig = new Set();
  for (const o of opps) {
    const sig = routeSigOf(o) || oppKey(o);
    if (!sig || seenSig.has(sig)) continue;
    seenSig.add(sig);
    picked.push(o);
    if (picked.length >= maxCount) break;
  }
  const now = performance.now();
  picked.reverse();
  for (let i = 0; i < picked.length; i++) {
    pushTopoPulse(picked[i], now - (picked.length - 1 - i) * staggerMs);
  }
}

function enqueueTopoPulses(opps, isVisible) {
  if (!opps || !opps.length) return null;
  if (topoSeenKeys.size > 400) {
    topoSeenKeys = new Set(opps.map(o => oppKey(o)));
  }
  if (!topoStreamInitialized) {
    topoStreamInitialized = true;
    for (const o of opps) topoSeenKeys.add(oppKey(o));
    if (isVisible && topoAutoFollow) {
      seedDistinctTopoPulses(opps, 8, 120);
    }
    return opps[0];
  }
  const fresh = [];
  for (const o of opps) {
    const k = oppKey(o);
    if (!topoSeenKeys.has(k)) {
      topoSeenKeys.add(k);
      fresh.push(o);
    }
  }
  if (!fresh.length) return null;
  if (isVisible && topoAutoFollow) {
    const now = performance.now();
    const batch = fresh.slice(0, 16).reverse();
    for (let i = 0; i < batch.length; i++) {
      pushTopoPulse(batch[i], now + i * 45);
    }
  }
  return fresh[0];
}

function addStationLabels(stationMap, labels, alpha, simOk, hits) {
  for (const st of labels) {
    const prev = stationMap.get(st.name);
    const h = Math.max(1, hits || 1);
    if (!prev) {
      stationMap.set(st.name, { st, alpha, simOk, hits: h });
    } else {
      prev.alpha = Math.max(prev.alpha, alpha);
      if (simOk) prev.simOk = true;
      if (h > prev.hits) prev.hits = h;
    }
  }
}

function drawRouteLayer(ctx, scene, opts) {
  if (!scene || opts.alpha <= 0.01) return;
  const isOk = !!opts.simOk;
  const baseAlpha = opts.alpha;
  const flare = (!opts.isInspect && opts.age >= 0 && opts.age < 420)
    ? (1 + (1 - opts.age / 420) * 0.55)
    : 1.0;

  if (scene.arcs.length) {
    ctx.save();
    ctx.setLineDash([]);
    for (let i = 0; i < scene.arcs.length; i++) {
      const arc = scene.arcs[i];
      const p0 = topoToScreen(arc.from.x, arc.from.y);
      const p1 = topoToScreen(arc.to.x, arc.to.y);
      const bend = arc.bend + (opts.bendJitter || 0);
      const mx = (p0.x + p1.x) * 0.5 - (p1.y - p0.y) * bend;
      const my = (p0.y + p1.y) * 0.5 + (p1.x - p0.x) * bend;

      ctx.beginPath();
      ctx.moveTo(p0.x, p0.y);
      ctx.quadraticCurveTo(mx, my, p1.x, p1.y);
      ctx.globalAlpha = baseAlpha * (isOk ? 0.34 : 0.26);
      ctx.strokeStyle = arc.color;
      ctx.lineWidth = (isOk ? 9.5 : 6.2) * flare;
      ctx.stroke();

      ctx.globalAlpha = baseAlpha * (isOk ? 0.98 : 0.86);
      ctx.lineWidth = (isOk ? 2.35 : 1.65) * flare;
      ctx.stroke();

      if (isOk || opts.isInspect || baseAlpha > 0.18) {
        const sparks = (isOk || opts.isInspect) ? (scene.arcs.length > 8 ? 1 : 2) : 1;
        const rad = (isOk || opts.isInspect) ? 2.8 : 2.1;
        ctx.globalAlpha = baseAlpha * (isOk || opts.isInspect ? 0.98 : 0.82);
        for (let k = 0; k < sparks; k++) {
          const tPos = (opts.sec * 0.55 + i * 0.19 + k * 0.5) % 1.0;
          const u = 1.0 - tPos;
          const sx = u * u * p0.x + 2 * u * tPos * mx + tPos * tPos * p1.x;
          const sy = u * u * p0.y + 2 * u * tPos * my + tPos * tPos * p1.y;
          ctx.fillStyle = '#ffffff';
          ctx.beginPath();
          ctx.arc(sx, sy, rad, 0, Math.PI * 2);
          ctx.fill();
          ctx.strokeStyle = arc.color;
          ctx.lineWidth = (isOk || opts.isInspect) ? 2.0 : 1.5;
          ctx.stroke();
        }
      }
    }
    ctx.restore();
  }

  const poolR = Math.max(3.5, Math.min(6.0, 3.2 * Math.sqrt(topoView.zoom)));
  if (scene.matchedPools.length) {
    ctx.save();
    for (let i = 0; i < scene.matchedPools.length; i++) {
      const p = topoToScreen(scene.matchedPools[i].x, scene.matchedPools[i].y);
      if (p.x < -14 || p.x > topoWidth + 14 || p.y < -14 || p.y > topoHeight + 14) continue;
      ctx.globalAlpha = baseAlpha * (isOk ? 0.96 : 0.80);
      ctx.beginPath();
      ctx.arc(p.x, p.y, poolR, 0, Math.PI * 2);
      ctx.fillStyle = isOk ? 'rgba(251, 191, 36, 0.38)' : 'rgba(251, 113, 133, 0.28)';
      ctx.fill();
      ctx.strokeStyle = isOk ? '#fbbf24' : '#fb7185';
      ctx.lineWidth = isOk ? 1.35 : 1.1;
      ctx.stroke();

      const ringMs = isOk ? 750 : 550;
      if (!opts.isInspect && opts.age >= 0 && opts.age < ringMs) {
        const rp = opts.age / ringMs;
        ctx.globalAlpha = baseAlpha * (1.0 - rp) * (isOk ? 0.72 : 0.55);
        ctx.beginPath();
        ctx.arc(p.x, p.y, poolR + rp * (isOk ? 12.0 : 9.0), 0, Math.PI * 2);
        ctx.strokeStyle = isOk ? '#34d399' : '#fb7185';
        ctx.lineWidth = isOk ? 1.6 : 1.25;
        ctx.stroke();
      }
    }
    ctx.restore();
  }
}

function drawTopoFrame(nowMs) {
  const canvas = document.getElementById('topoCanvas');
  if (!canvas) return;
  const ctx = canvas.getContext('2d');
  ctx.setTransform(topoDpr, 0, 0, topoDpr, 0, 0);
  ctx.clearRect(0, 0, topoWidth, topoHeight);

  if (topoOffCanvas) {
    ctx.drawImage(topoOffCanvas, 0, 0, topoWidth, topoHeight);
  }

  const now = nowMs || performance.now();
  const sec = now / 1000.0;

  if (topoNewPoolPings.length) {
    topoNewPoolPings = topoNewPoolPings.filter(pg => (now - pg.bornMs) < pg.ttlMs);
    ctx.save();
    for (const pg of topoNewPoolPings) {
      const age = now - pg.bornMs;
      const frac = Math.max(0, Math.min(1, age / pg.ttlMs));
      const p = topoToScreen(pg.x, pg.y);
      if (p.x < -12 || p.x > topoWidth + 12 || p.y < -12 || p.y > topoHeight + 12) continue;
      ctx.globalAlpha = Math.pow(1 - frac, 1.4) * 0.85;
      ctx.beginPath();
      ctx.arc(p.x, p.y, 2.5 + frac * 8.5, 0, Math.PI * 2);
      ctx.strokeStyle = '#38bdf8';
      ctx.lineWidth = 1.25;
      ctx.stroke();
    }
    ctx.restore();
  }

  const stationMap = new Map();

  if (!topoAutoFollow) {
    const scene = getActiveRouteScene();
    const simOk = !!(topoSelectedOpp && topoSelectedOpp.sim_ok);
    drawRouteLayer(ctx, scene, { alpha: 1.0, simOk, age: 1000, sec, bendJitter: 0, isInspect: true });
    addStationLabels(stationMap, scene.stationLabels, 1.0, simOk, 1);
  } else {
    topoPulses = topoPulses.filter(p => (now - p.bornMs) < p.ttlMs);
    for (const p of topoPulses) {
      if (p.layoutGen !== topoLayoutGen) {
        p.scene = matchRouteOnLayout(p.opp);
        p.layoutGen = topoLayoutGen;
      }
    }
    for (const passOk of [false, true]) {
      for (const p of topoPulses) {
        if (p.simOk !== passOk || !p.scene) continue;
        const age = now - p.bornMs;
        const alpha = computePulseAlpha(age, p.ttlMs, p.holdMs);
        if (alpha <= 0.015) continue;
        drawRouteLayer(ctx, p.scene, { alpha, simOk: p.simOk, age, sec, bendJitter: p.bendJitter, isInspect: false });
        if (alpha > 0.20) {
          addStationLabels(stationMap, p.scene.stationLabels, alpha, p.simOk, p.hits);
        }
      }
    }
  }

  if (topoChosenPool) {
    const cp = topoToScreen(topoChosenPool.x, topoChosenPool.y);
    ctx.beginPath();
    ctx.arc(cp.x, cp.y, 6.5, 0, Math.PI * 2);
    ctx.fillStyle = 'rgba(56, 189, 248, 0.25)';
    ctx.fill();
    ctx.strokeStyle = '#38bdf8';
    ctx.lineWidth = 1.6;
    ctx.stroke();
  }

  if (topoLayout) {
    ctx.textAlign = 'center';
    ctx.textBaseline = 'middle';
    for (const hub of topoLayout.hubMetaList) {
      const p = topoToScreen(hub.x, hub.y);
      if (p.x < -40 || p.x > topoWidth + 40 || p.y < -30 || p.y > topoHeight + 30) continue;
      ctx.font = '700 10.5px "JetBrains Mono", monospace';
      const labelTxt = `${hub.name} (${hub.count})`;
      const tw = ctx.measureText(labelTxt).width;
      drawPillRect(ctx, p.x - tw * 0.5 - 6, p.y - 9, tw + 12, 18, 6);
      ctx.fillStyle = 'rgba(13, 21, 36, 0.90)';
      ctx.fill();
      ctx.strokeStyle = hub.color + '88';
      ctx.lineWidth = 1.0;
      ctx.stroke();
      ctx.fillStyle = hub.color;
      ctx.fillText(labelTxt, p.x, p.y + 0.5);
    }

    for (const entry of stationMap.values()) {
      const st = entry.st;
      const p = topoToScreen(st.x, st.y);
      if (p.x < -30 || p.x > topoWidth + 30 || p.y < -25 || p.y > topoHeight + 25) continue;
      ctx.save();
      ctx.globalAlpha = Math.max(0.18, Math.min(1.0, entry.alpha));
      ctx.font = '600 10px "JetBrains Mono", monospace';
      const label = entry.hits > 1 ? `${st.name} x${entry.hits}` : st.name;
      const tw = ctx.measureText(label).width;
      drawPillRect(ctx, p.x - tw * 0.5 - 5, p.y - 8, tw + 10, 16, 5);
      ctx.fillStyle = 'rgba(7, 11, 18, 0.92)';
      ctx.fill();
      ctx.strokeStyle = entry.simOk ? 'rgba(52, 211, 153, 0.78)' : 'rgba(251, 113, 133, 0.65)';
      ctx.lineWidth = 1.0;
      ctx.stroke();
      ctx.fillStyle = entry.simOk ? '#34d399' : '#fda4af';
      ctx.fillText(label, p.x, p.y + 0.5);
      ctx.restore();
    }
  }
}

function startTopoLoop() {
  if (topoRafId) return;
  const tick = (ts) => {
    const panel = document.getElementById('tab-topology');
    if (!panel || !panel.classList.contains('active')) {
      topoRafId = 0;
      return;
    }
    drawTopoFrame(ts);
    topoRafId = requestAnimationFrame(tick);
  };
  topoRafId = requestAnimationFrame(tick);
}

function stopTopoLoop() {
  if (topoRafId) {
    cancelAnimationFrame(topoRafId);
    topoRafId = 0;
  }
}

function resizeTopoCanvas() {
  const stage = document.getElementById('topoStage');
  const canvas = document.getElementById('topoCanvas');
  if (!stage || !canvas) return;
  const rect = stage.getBoundingClientRect();
  if (rect.width < 10 || rect.height < 10) return;
  topoWidth = Math.round(rect.width);
  topoHeight = Math.round(rect.height);
  topoDpr = Math.min(2, window.devicePixelRatio || 1);
  canvas.width = Math.round(topoWidth * topoDpr);
  canvas.height = Math.round(topoHeight * topoDpr);
  redrawTopoStatic();
  drawTopoFrame(performance.now());
}

function findNearestPool(sx, sy) {
  if (!topoLayout) return null;
  const np = screenToTopo(sx, sy);
  const gN = topoLayout.gridCells;
  const gx = Math.floor(np.x * gN);
  const gy = Math.floor(np.y * gN);
  const maxDist = 10;
  let best = null;
  let bestD = maxDist;
  for (let dy = -2; dy <= 2; dy++) {
    for (let dx = -2; dx <= 2; dx++) {
      const cx = gx + dx;
      const cy = gy + dy;
      if (cx < 0 || cx >= gN || cy < 0 || cy >= gN) continue;
      const cell = topoLayout.grid.get(cy * gN + cx);
      if (!cell) continue;
      for (const node of cell) {
        const sp = topoToScreen(node.x, node.y);
        const d = Math.hypot(sp.x - sx, sp.y - sy);
        if (d < bestD) {
          bestD = d;
          best = node;
        }
      }
    }
  }
  return best;
}

function showTopoPoolCard(node) {
  topoChosenPool = node;
  const card = document.getElementById('topoPoolCard');
  if (!card) return;
  if (!node) {
    card.setAttribute('hidden', '');
    return;
  }
  card.removeAttribute('hidden');
  setText('tpPair', `${node.symA} / ${node.symB}`);
  setText('tpVenue', node.venue);
  const shortAddr = node.addr.startsWith('0x') && node.addr.length > 14
    ? `${node.addr.slice(0, 8)}...${node.addr.slice(-6)}`
    : node.addr;
  setText('tpAddr', shortAddr);
  setText('tpCopyBtn', t('topo_copy'));
  const expEl = document.getElementById('tpExplorerLink');
  if (expEl) {
    if (node.addr.startsWith('0x') && node.addr.length === 42) {
      const { expBase } = chainMeta(state || { chain: topoLoadedChain || 'eth' });
      expEl.href = `${expBase}/address/${node.addr}`;
      expEl.style.display = 'inline';
    } else {
      expEl.style.display = 'none';
    }
  }
}

function closeTopoPoolCard() {
  showTopoPoolCard(null);
  drawTopoFrame(performance.now());
}

function copyTopoPoolAddr() {
  if (!topoChosenPool) return;
  try {
    navigator.clipboard.writeText(topoChosenPool.addr);
    setText('tpCopyBtn', t('topo_copied'));
  } catch (e) {}
}

function topoZoomBy(mult, anchorX, anchorY) {
  const prevZoom = topoView.zoom;
  const nextZoom = Math.max(0.6, Math.min(8.0, prevZoom * mult));
  if (Math.abs(nextZoom - prevZoom) < 1e-6) return;
  const cx = anchorX != null ? anchorX : topoWidth * 0.5;
  const cy = anchorY != null ? anchorY : topoHeight * 0.5;
  const pt = screenToTopo(cx, cy);
  topoView.zoom = nextZoom;
  const nextPt = topoToScreen(pt.x, pt.y);
  topoView.panX += cx - nextPt.x;
  topoView.panY += cy - nextPt.y;
  redrawTopoStatic();
  drawTopoFrame(performance.now());
}

function topoResetView() {
  topoView = { zoom: 1.0, panX: 0.0, panY: 0.0 };
  redrawTopoStatic();
  drawTopoFrame(performance.now());
}

function toggleTopoFollow() {
  topoAutoFollow = !topoAutoFollow;
  refreshTopoFollowBtn();
  if (topoAutoFollow) {
    topoPulses = [];
    if (state && state.recent && state.recent.length) {
      seedDistinctTopoPulses(state.recent, 6, 90);
      focusTopoOpp(state.recent[0]);
    }
    renderTopoRouteBar();
    renderTopoFeed();
    drawTopoFrame(performance.now());
  }
}

function refreshTopoFollowBtn() {
  const btn = document.getElementById('topoFollowBtn');
  if (!btn) return;
  btn.textContent = t(topoAutoFollow ? 'topo_follow_on' : 'topo_follow_off');
  btn.classList.toggle('active', topoAutoFollow);
}

function setTopoFilter(f, btn) {
  topoFilter = f;
  document.querySelectorAll('[data-tfilter]').forEach(b => b.classList.remove('active'));
  if (btn) btn.classList.add('active');
  renderTopoFeed();
}

function renderTopoRouteBar() {
  const legsWrap = document.getElementById('topoLegsWrap');
  if (!legsWrap) return;
  if (!topoSelectedOpp) {
    setText('topoRouteMeta', t('topo_route_none'));
    legsWrap.innerHTML = '';
    return;
  }
  const scene = getActiveRouteScene();
  const simTxt = topoSelectedOpp.sim_net_usd != null
    ? `$${Number(topoSelectedOpp.sim_net_usd).toFixed(4)}`
    : (topoSelectedOpp.err_short || 'ERR');
  setText('topoRouteMeta', t('topo_route_meta', {
    block: topoSelectedOpp.block || '-',
    matched: scene.matchedPools.length,
    est: Number(topoSelectedOpp.est_net_usd || 0).toFixed(2),
    sim: simTxt,
  }));
  const pills = scene.legs.map((leg, i) =>
    `<span class="topo-hop mono">${i + 1}. ${esc(leg.venue)}: ${esc(leg.from)} &rarr; ${esc(leg.to)}</span>`
  );
  if (scene.extra > 0) {
    pills.push(`<span class="topo-hop mono" style="color:var(--amber)">${esc(t('topo_more_hops', { n: scene.extra }))}</span>`);
  }
  legsWrap.innerHTML = pills.join('');
}

function selectTopoOpp(opp, disableFollow) {
  if (disableFollow) {
    topoAutoFollow = false;
    topoPulses = [];
    refreshTopoFollowBtn();
  }
  focusTopoOpp(opp);
  renderTopoRouteBar();
  renderTopoFeed();
  drawTopoFrame(performance.now());
}

function filterTopoOpps(recent) {
  const q = ((document.getElementById('topoSearchBox') || {}).value || '').trim().toLowerCase();
  return (recent || []).filter(r => {
    if (topoFilter === 'ok' && !r.sim_ok) return false;
    const strat = String(r.strategy || '');
    if (topoFilter === 'gpu' && !strat.includes('GPU')) return false;
    if (topoFilter === 'ms' && !strat.includes('MevShare')) return false;
    if (topoFilter === 'deep') {
      const parsed = parseTopoRoute(r);
      if (parsed.legs.length + parsed.extra < 4) return false;
    }
    if (!q) return true;
    return `${strat} ${r.base_token || ''} ${r.err_short || ''} ${r.route || ''}`.toLowerCase().includes(q);
  });
}

function renderTopoFeed() {
  const listEl = document.getElementById('topoFeedList');
  if (!listEl) return;
  const recent = (state && state.recent) || [];
  const filtered = filterTopoOpps(recent);
  setText('topoFeedBadge', t('topo_feed_badge', { shown: filtered.length, total: recent.length }));
  if (!filtered.length) {
    listEl.innerHTML = `<div style="color:var(--muted);font-size:12px;padding:10px 4px">${esc(t('topo_feed_empty'))}</div>`;
    return;
  }
  listEl.innerHTML = filtered.map((r, idx) => {
    const k = oppKey(r);
    const parsed = parseTopoRoute(r);
    const totalHops = parsed.legs.length + parsed.extra;
    const simStr = r.sim_net_usd != null ? `$${Number(r.sim_net_usd).toFixed(4)}` : (r.err_short || 'FAIL');
    const selectedCls = k === topoSelectedKey ? ' selected' : '';
    return `<button type="button" class="topo-card${selectedCls}" data-opp-idx="${idx}">
      <div class="topo-card-top">
        <span class="mono" style="font-weight:700;font-size:12px">${esc(r.strategy || '-')}</span>
        <span class="pill ${r.sim_ok ? 'ok' : 'fail'}">${esc(r.sim_ok ? t('pill_sim_ok') : (r.err_short || 'FAIL'))}</span>
      </div>
      <div class="topo-card-sub mono">${esc(t('topo_card_sub', { block: r.block || '-', steps: totalHops || '-', sim: simStr }))}</div>
      <div class="topo-card-route mono">${esc(r.route || '-')}</div>
    </button>`;
  }).join('');

  listEl.querySelectorAll('.topo-card').forEach(btn => {
    btn.addEventListener('click', () => {
      const i = Number(btn.getAttribute('data-opp-idx'));
      if (filtered[i]) selectTopoOpp(filtered[i], true);
    });
  });
}

function refreshTopoMetaUi() {
  refreshTopoFollowBtn();
  if (!topoData) {
    setText('topoStatsBadge', t('topo_loading'));
    setText('topoSourceLabel', '-');
    setText('topoUpdatedLabel', '-');
    return;
  }
  setText('topoStatsBadge', t('topo_stats_fmt', {
    pools: Number(topoData.pool_count || 0).toLocaleString(),
    tokens: Number(topoData.token_count || 0).toLocaleString(),
    venues: Number(topoData.venue_count || 0),
  }));
  const srcKey = topoData.source === 'pools_cache'
    ? 'topo_src_pools_cache'
    : (topoData.source === 'opportunities_fallback' ? 'topo_src_opportunities_fallback' : 'topo_src_empty');
  setText('topoSourceLabel', t(srcKey, { chain: topoData.chain || 'eth' }));
  const d = topoData.updated_ms ? new Date(topoData.updated_ms) : null;
  const tsStr = d && !isNaN(d.getTime()) ? fmtLocal(d.toISOString()) : '-';
  setText('topoUpdatedLabel', t('topo_updated_fmt', { ts: tsStr }));
}

function refreshTopoUi() {
  refreshTopoMetaUi();
  renderTopoRouteBar();
  renderTopoFeed();
}

async function fetchTopology(force) {
  if (topoFetchInFlight) return;
  topoFetchInFlight = true;
  try {
    const ch = (state && state.chain) || '';
    const url = ch ? `/api/topology?chain=${encodeURIComponent(ch)}` : '/api/topology';
    const res = await fetch(url, { cache: force ? 'reload' : 'no-store' });
    const data = await res.json();
    const prevChain = topoLoadedChain;
    const chainSwitched = !!(prevChain && prevChain !== (data.chain || ch));
    topoData = data;
    topoLoadedChain = data.chain || ch;
    if (chainSwitched) {
      topoPulses = [];
      topoSeenKeys.clear();
      topoStreamInitialized = false;
      topoKnownPoolAddrs.clear();
      topoNewPoolPings = [];
    }
    topoLoadedMtimeMs = Number((state && state.topology_mtime_ms) || data.updated_ms || 0);
    topoLastScanPools = (state && state.latest_scan && state.latest_scan.total_pools) || data.pool_count || 0;
    const prevChosenAddr = !chainSwitched && topoChosenPool ? topoChosenPool.addr : '';
    topoLayout = buildTopoLayout(data);
    topoLayoutGen += 1;
    cachedMatchedRouteKey = '';
    if (topoLayout && Array.isArray(topoLayout.nodes)) {
      const hadKnown = !chainSwitched && topoKnownPoolAddrs.size > 0;
      const pingNow = performance.now();
      let pingAdded = 0;
      const nextKnown = new Set();
      for (const n of topoLayout.nodes) {
        nextKnown.add(n.addr);
        if (hadKnown && !topoKnownPoolAddrs.has(n.addr) && pingAdded < 60) {
          topoNewPoolPings.push({ x: n.x, y: n.y, bornMs: pingNow, ttlMs: 2600 });
          pingAdded++;
        }
      }
      topoKnownPoolAddrs = nextKnown;
    }
    const keptPool = prevChosenAddr && topoLayout ? topoLayout.nodes.find(n => n.addr === prevChosenAddr) : null;
    if (topoChosenPool || keptPool) showTopoPoolCard(keptPool || null);
    refreshTopoUi();
    if (topoWidth > 1 && topoHeight > 1) {
      redrawTopoStatic();
      drawTopoFrame(performance.now());
    } else {
      resizeTopoCanvas();
    }
  } catch (e) {
  } finally {
    topoFetchInFlight = false;
  }
}

let topoBound = false;
function bindTopoCanvasEvents() {
  if (topoBound) return;
  const canvas = document.getElementById('topoCanvas');
  const stage = document.getElementById('topoStage');
  if (!canvas || !stage) return;
  topoBound = true;

  canvas.addEventListener('pointerdown', (ev) => {
    if (ev.button !== 0) return;
    canvas.setPointerCapture(ev.pointerId);
    topoDrag = { id: ev.pointerId, sx: ev.clientX, sy: ev.clientY, px: topoView.panX, py: topoView.panY, moved: false };
  });
  canvas.addEventListener('pointermove', (ev) => {
    const rect = canvas.getBoundingClientRect();
    const lx = ev.clientX - rect.left;
    const ly = ev.clientY - rect.top;
    if (!topoDrag || topoDrag.id !== ev.pointerId) {
      topoHoverNode = findNearestPool(lx, ly);
      canvas.classList.toggle('clickable', !!topoHoverNode);
      return;
    }
    const dx = ev.clientX - topoDrag.sx;
    const dy = ev.clientY - topoDrag.sy;
    if (Math.hypot(dx, dy) > 4) topoDrag.moved = true;
    if (!topoDrag.moved) return;
    canvas.classList.add('dragging');
    topoView.panX = Math.max(-topoWidth * topoView.zoom, Math.min(topoWidth * topoView.zoom, topoDrag.px + dx));
    topoView.panY = Math.max(-topoHeight * topoView.zoom, Math.min(topoHeight * topoView.zoom, topoDrag.py + dy));
    redrawTopoStatic();
  });
  const endDrag = (ev) => {
    if (!topoDrag || topoDrag.id !== ev.pointerId) return;
    const wasMoved = topoDrag.moved;
    topoDrag = null;
    canvas.classList.remove('dragging');
    if (!wasMoved && ev.type === 'pointerup') {
      const rect = canvas.getBoundingClientRect();
      const hit = findNearestPool(ev.clientX - rect.left, ev.clientY - rect.top);
      showTopoPoolCard(hit === topoChosenPool ? null : hit);
      drawTopoFrame(performance.now());
    }
  };
  canvas.addEventListener('pointerup', endDrag);
  canvas.addEventListener('pointercancel', endDrag);
  canvas.addEventListener('wheel', (ev) => {
    ev.preventDefault();
    const rect = canvas.getBoundingClientRect();
    topoZoomBy(ev.deltaY < 0 ? 1.15 : 0.87, ev.clientX - rect.left, ev.clientY - rect.top);
  }, { passive: false });
  canvas.addEventListener('dblclick', topoResetView);

  if (window.ResizeObserver) {
    topoResizeObs = new ResizeObserver(() => {
      const panel = document.getElementById('tab-topology');
      if (panel && panel.classList.contains('active')) resizeTopoCanvas();
    });
    topoResizeObs.observe(stage);
  }
}

function ensureTopoReady() {
  bindTopoCanvasEvents();
  refreshTopoUi();
  requestAnimationFrame(() => {
    resizeTopoCanvas();
    startTopoLoop();
  });
  const curChain = (state && state.chain) || '';
  if (!topoData || (curChain && topoLoadedChain !== curChain)) {
    fetchTopology(false);
  }
  if (topoAutoFollow && state && state.recent && state.recent.length) {
    if (!topoStreamInitialized) {
      const latest = enqueueTopoPulses(state.recent, true);
      if (latest) focusTopoOpp(latest);
    } else if (!topoPulses.length) {
      seedDistinctTopoPulses(state.recent, 6, 90);
      focusTopoOpp(state.recent[0]);
    }
    renderTopoRouteBar();
    renderTopoFeed();
  }
}

function onTopoStateUpdate(s) {
  if (!s) return;
  const curChain = s.chain || '';
  const totalPools = (s.latest_scan && s.latest_scan.total_pools) || 0;
  const chainChanged = curChain && topoLoadedChain && curChain !== topoLoadedChain;
  const srvMtime = Number(s.topology_mtime_ms || 0);
  const mtimeChanged = srvMtime > 0 && topoLoadedMtimeMs > 0 && srvMtime !== topoLoadedMtimeMs;
  const fallbackDrifted = srvMtime === 0 && topoLastScanPools >= 0 && Math.abs(totalPools - topoLastScanPools) >= 25;
  const panel = document.getElementById('tab-topology');
  const isVisible = !!(panel && panel.classList.contains('active'));

  const latestNew = enqueueTopoPulses(s.recent || [], isVisible);
  if (topoAutoFollow && latestNew) {
    focusTopoOpp(latestNew);
    renderTopoRouteBar();
  }
  renderTopoFeed();

  if (chainChanged || mtimeChanged || fallbackDrifted) {
    if (isVisible) {
      fetchTopology(false);
    } else {
      topoData = null;
    }
  }
}

function setMsg(id, text, isErr) {
  const el = document.getElementById(id);
  if (!el) return;
  el.textContent = text || '';
  el.style.color = isErr ? 'var(--rose)' : 'var(--emerald)';
}

let loadedCfgObj = null;

function renderControlMeta(data) {
  const p = (data && data.process) || {};
  setText('procBadge', p.running ? t('proc_running_badge', { action: p.action.toUpperCase(), pid: p.pid }) : t('proc_idle'));
  setText('procDetailBadge', p.running ? t('proc_running_detail', { action: p.action, pid: p.pid }) : t('proc_stopped'));
  setText('procCmdLine', p.command ? t('last_cmd_fmt', { cmd: p.command }) : '');

  const w = (data && data.wallet) || {};
  document.getElementById('wAddr').value = w.address || t('w_none_derived');
  document.getElementById('wHasPk').value = w.has_private_key ? t('w_pk_ok') : t('w_pk_missing');
  document.getElementById('wHasFb').value = w.has_flashbots_auth_key ? t('w_fb_ok') : t('w_fb_fallback');
  document.getElementById('wEnvMode').value = w.env_exists
    ? (w.env_mode_0600 ? t('w_env_ok') : t('w_env_warn'))
    : t('w_env_none');
}

async function loadControlState() {
  try {
    const res = await fetch('/api/config', { cache: 'no-store' });
    const data = await res.json();
    lastControlData = data;
    loadedCfgObj = data.config || {};
    const c = loadedCfgObj;
    const ch = c.chain || 'eth';
    document.getElementById('cfgChain').value = ch;
    document.getElementById('cfgDryRun').value = String((c.mode && c.mode.dry_run) !== false);
    document.getElementById('cfgMinProfit').value = (c.mode && c.mode.min_profit_usd != null) ? c.mode.min_profit_usd : 0.001;
    document.getElementById('cfgTargetUsd').value = (c.mode && c.mode.target_usd != null) ? c.mode.target_usd : 50.0;
    document.getElementById('cfgReadRpcs').value = ((c.rpcs && c.rpcs.read) || []).join('\n');
    document.getElementById('cfgWsRpcs').value = ((c.rpcs && c.rpcs.ws) || []).join('\n');
    document.getElementById('cfgMsEnabled').value = String((c.mev_share && c.mev_share.enabled) !== false);
    document.getElementById('cfgMsMinSwap').value = (c.mev_share && c.mev_share.min_swap_usd != null) ? c.mev_share.min_swap_usd : 50.0;
    document.getElementById('cfgSolverMode').value = (c.solver && c.solver.mode) || 'cpu';
    document.getElementById('cfgExecAddr').value = (c.executors && c.executors[ch]) || '';
    renderControlMeta(data);
  } catch (e) {}
}

async function apiPost(url, payload) {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json', 'X-Pyrite-Token': getAdminToken() },
    body: JSON.stringify(payload || {})
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
  return body;
}

function _linesToList(id) {
  return (document.getElementById(id).value || '').split(/\r?\n/).map(s => s.trim()).filter(Boolean);
}

async function saveConfig() {
  try {
    const ch = document.getElementById('cfgChain').value;
    const base = loadedCfgObj ? JSON.parse(JSON.stringify(loadedCfgObj)) : {};
    base.chain = ch;
    base.mode = Object.assign({}, base.mode || {}, {
      dry_run: document.getElementById('cfgDryRun').value === 'true',
      min_profit_usd: parseFloat(document.getElementById('cfgMinProfit').value || '0.001'),
      target_usd: parseFloat(document.getElementById('cfgTargetUsd').value || '50')
    });
    base.rpcs = Object.assign({}, base.rpcs || {}, {
      read: _linesToList('cfgReadRpcs'),
      ws: _linesToList('cfgWsRpcs')
    });
    base.mev_share = Object.assign({}, base.mev_share || {}, {
      enabled: document.getElementById('cfgMsEnabled').value === 'true',
      min_swap_usd: parseFloat(document.getElementById('cfgMsMinSwap').value || '50')
    });
    base.solver = Object.assign({}, base.solver || {}, {
      mode: document.getElementById('cfgSolverMode').value
    });
    base.executors = Object.assign({}, base.executors || {});
    base.executors[ch] = document.getElementById('cfgExecAddr').value.trim();

    await apiPost('/api/config', base);
    setMsg('cfgStatusMsg', t('msg_saved_cfg'), false);
    await loadControlState();
    fetchState(true);
  } catch (e) {
    setMsg('cfgStatusMsg', t('msg_save_failed', { err: e.message }), true);
  }
}

async function startProcess(action) {
  try {
    const confirmLive = action === 'live' && document.getElementById('confirmLiveChk').checked;
    const res = await apiPost('/api/process/start', { action, confirm_live: confirmLive });
    setMsg('procStatusMsg', t('msg_started_proc', { action, pid: res.process && res.process.pid }), false);
    await loadControlState();
  } catch (e) {
    setMsg('procStatusMsg', t('msg_start_failed', { err: e.message }), true);
  }
}

async function stopProcess() {
  try {
    await apiPost('/api/process/stop', {});
    setMsg('procStatusMsg', t('msg_stopped_proc'), false);
    await loadControlState();
  } catch (e) {
    setMsg('procStatusMsg', t('msg_stop_failed', { err: e.message }), true);
  }
}

async function genBurnerWallet(force) {
  try {
    const res = await apiPost('/api/wallet/generate', { force: !!force });
    const addr = (res.wallet && res.wallet.address) || t('msg_wallet_saved_no_bin');
    setMsg('walletStatusMsg', t('msg_burner_saved', { addr }), false);
    await loadControlState();
  } catch (e) {
    setMsg('walletStatusMsg', t('msg_wallet_gen_failed', { err: e.message }), true);
  }
}

async function importWalletKey() {
  try {
    const slot = document.getElementById('wImportSlot').value;
    const keyEl = document.getElementById('wImportVal');
    const val = keyEl.value.trim();
    keyEl.value = '';
    const res = await apiPost('/api/wallet/import', { key_name: slot, value: val });
    setMsg('walletStatusMsg', t('msg_import_saved', { key: res.key_name }), false);
    await loadControlState();
  } catch (e) {
    setMsg('walletStatusMsg', t('msg_import_failed', { err: e.message }), true);
  }
}

applyI18n();
switchTab(tabFromPath(window.location.pathname), false);
fetchState(false);
loadControlState();
connectStream();
</script>
</body>
</html>
"""


def main() -> None:
    parser = argparse.ArgumentParser(description="Pyrite (Fool's Gold) Web Control & Log Dashboard")
    parser.add_argument("--logs-dir", default="logs", help="Path to logs directory")
    parser.add_argument("--chain", default="", help="Chain name (e.g. eth/bsc/base) or YAML path; defaults to pyrite.json chain")
    parser.add_argument("--config", default="pyrite.json", help="Path to runtime JSON config")
    parser.add_argument("--max-lines", type=int, default=2000, help="Tail lines to parse")
    parser.add_argument("--since", default="", help="Optional ISO UTC cutoff timestamp (e.g. 2025-01-01T00:00:00)")
    parser.add_argument("--watch", type=int, default=0, help="CLI refresh interval in seconds")
    parser.add_argument("--web", action="store_true", help="Serve interactive HTTP dashboard & control panel")
    parser.add_argument("--host", default="127.0.0.1", help="HTTP bind host (e.g. 0.0.0.0)")
    parser.add_argument("--port", type=int, default=8080, help="HTTP port")
    parser.add_argument("--allow-remote-wallet", action="store_true", help="Allow write-only wallet writes from non-loopback hosts (NOT recommended)")
    args = parser.parse_args()

    repo_root = Path(__file__).resolve().parent.parent
    logs_dir = Path(args.logs_dir)
    config_path = Path(args.config)
    example_path = repo_root / "pyrite.example.json"
    env_path = repo_root / ".env"

    if not args.chain:
        cfg_boot = load_pyrite_config(config_path, example_path)
        args.chain = str(cfg_boot.get("chain") or "eth")

    if args.web:
        broadcaster = StateBroadcaster(logs_dir, args.chain, args.max_lines, since_iso=args.since)
        proc_mgr = ProcessManager(repo_root, config_path, logs_dir)
        admin_token = secrets.token_urlsafe(24)
        # Warm state & EOA RPC cache once synchronously at boot, then serve non-blocking thereafter.
        broadcaster.get_state(force=True, blocking_rpc_if_cold=True)

        class Handler(BaseHTTPRequestHandler):
            def _send_json(self, code: int, obj: Any) -> None:
                payload = json.dumps(obj, ensure_ascii=False).encode("utf-8")
                self.send_response(code)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Cache-Control", "no-store")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def _read_json_body(self) -> dict[str, Any]:
                length = int(self.headers.get("Content-Length", "0") or 0)
                if length <= 0 or length > 512 * 1024:
                    raise ValueError("Invalid request body size")
                raw = self.rfile.read(length)
                obj = json.loads(raw.decode("utf-8"))
                if not isinstance(obj, dict):
                    raise ValueError("JSON body must be an object")
                return obj

            def _require_token(self) -> bool:
                hdr = (self.headers.get("X-Pyrite-Token") or "").strip()
                if not hdr or not secrets.compare_digest(hdr, admin_token):
                    self._send_json(403, {"error": "Invalid or missing X-Pyrite-Token (open Control URL with ?token=...)"})
                    return False
                return True

            def _require_loopback_wallet(self) -> bool:
                if args.allow_remote_wallet:
                    return True
                client_ip = str(self.client_address[0])
                if not is_loopback_host(args.host) or not is_loopback_host(client_ip):
                    self._send_json(
                        403,
                        {"error": "Write-only wallet endpoints require localhost (127.0.0.1); pass --allow-remote-wallet only over SSH tunnel/VPN"},
                    )
                    return False
                return True

            def do_HEAD(self) -> None:
                path = urllib.parse.urlparse(self.path).path
                if path == "/api/stream":
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream; charset=utf-8")
                    self.send_header("Cache-Control", "no-cache, no-store")
                    self.end_headers()
                elif path in ("/api/state", "/api/config", "/api/topology"):
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json; charset=utf-8")
                    self.send_header("Cache-Control", "no-store")
                    self.end_headers()
                else:
                    self.send_response(200)
                    self.send_header("Content-Type", "text/html; charset=utf-8")
                    self.send_header("Cache-Control", "no-cache")
                    self.end_headers()

            def do_GET(self) -> None:
                parsed = urllib.parse.urlparse(self.path)
                path = parsed.path
                if path == "/api/stream":
                    self.close_connection = True
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream; charset=utf-8")
                    self.send_header("Cache-Control", "no-cache, no-store")
                    self.send_header("Connection", "keep-alive")
                    self.send_header("X-Accel-Buffering", "no")
                    self.end_headers()
                    try:
                        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                    except Exception:
                        pass
                    cid, seq, init_frame = broadcaster.register_sse_client()
                    try:
                        self.wfile.write(b"retry: 3000\n" + init_frame)
                        self.wfile.flush()
                        while not broadcaster._stop_event.is_set():
                            seq, frame = broadcaster.wait_next_sse_frame(seq, timeout=10.0)
                            if broadcaster._stop_event.is_set() or _sse_client_closed(self.connection):
                                break
                            self.wfile.write(frame if frame is not None else b": keepalive\n\n")
                            self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError, TimeoutError, OSError):
                        pass
                    finally:
                        broadcaster.unregister_sse_client(cid)
                    return

                if path == "/api/state":
                    force = "force=1" in parsed.query
                    self._send_json(200, broadcaster.get_state(force=force))
                    return

                if path == "/api/topology":
                    qs = urllib.parse.parse_qs(parsed.query)
                    req_chain = (qs.get("chain") or [broadcaster.chain])[0]
                    if not _SAFE_CHAIN_RE.fullmatch(str(req_chain or "").strip().lower()):
                        req_chain = broadcaster.chain
                    self._send_json(200, load_topology(logs_dir, req_chain))
                    return

                if path == "/api/config":
                    cfg = load_pyrite_config(config_path, example_path)
                    bin_path = find_pyrite_binary(repo_root)
                    wallet = read_wallet_status(env_path, logs_dir, bin_path, str(cfg.get("chain") or args.chain))
                    self._send_json(
                        200,
                        {
                            "config": cfg,
                            "wallet": wallet,
                            "process": proc_mgr.status(),
                            "chain_options": ["eth", "bsc", "base"],
                            "token_required": True,
                        },
                    )
                    return

                body = WEB_HTML.encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/html; charset=utf-8")
                self.send_header("Cache-Control", "no-cache")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self) -> None:
                path = urllib.parse.urlparse(self.path).path
                if not self._require_token():
                    return
                try:
                    data = self._read_json_body()
                except Exception as e:
                    self._send_json(400, {"error": str(e)})
                    return

                try:
                    if path == "/api/config":
                        save_pyrite_config(config_path, data)
                        new_chain = str(data.get("chain") or args.chain).strip()
                        if new_chain and new_chain != broadcaster.chain:
                            broadcaster.set_chain(new_chain)
                        self._send_json(200, {"ok": True, "config": load_pyrite_config(config_path, example_path)})
                        return
                    if path == "/api/process/start":
                        cfg = load_pyrite_config(config_path, example_path)
                        ch = str(data.get("chain") or cfg.get("chain") or broadcaster.chain)
                        proc_state = proc_mgr.start(
                            str(data.get("action") or ""),
                            ch,
                            confirm_live=bool(data.get("confirm_live")),
                        )
                        self._send_json(200, {"ok": True, "process": proc_state})
                        return
                    if path == "/api/process/stop":
                        self._send_json(200, {"ok": True, "process": proc_mgr.stop()})
                        return
                    if path == "/api/wallet/generate":
                        if not self._require_loopback_wallet():
                            return
                        cfg = load_pyrite_config(config_path, example_path)
                        ch = str(cfg.get("chain") or broadcaster.chain)
                        res = generate_burner_wallet(
                            env_path,
                            logs_dir,
                            find_pyrite_binary(repo_root),
                            ch,
                            force=bool(data.get("force")),
                        )
                        self._send_json(200, res)
                        return
                    if path == "/api/wallet/import":
                        if not self._require_loopback_wallet():
                            return
                        cfg = load_pyrite_config(config_path, example_path)
                        ch = str(cfg.get("chain") or broadcaster.chain)
                        res = import_wallet_key(
                            env_path,
                            logs_dir,
                            str(data.get("key_name") or ""),
                            str(data.get("value") or ""),
                            find_pyrite_binary(repo_root),
                            ch,
                        )
                        self._send_json(200, res)
                        return
                    self._send_json(404, {"error": f"Unknown endpoint {path}"})
                except ValueError as e:
                    self._send_json(400, {"error": str(e)})
                except RuntimeError as e:
                    self._send_json(409, {"error": str(e)})
                except Exception as e:
                    self._send_json(500, {"error": str(e)})

            def log_message(self, format: str, *a: Any) -> None:
                return

        server = DashboardHTTPServer((args.host, args.port), Handler)
        print(f"Pyrite Read-Only Monitor : http://{args.host}:{args.port} (logs={logs_dir})")
        print(f"Pyrite Control Panel URL : http://{args.host}:{args.port}/?token={admin_token}")
        try:
            server.serve_forever()
        except KeyboardInterrupt:
            pass
        finally:
            proc_mgr.stop()
            broadcaster.shutdown()
            server.server_close()
        return

    if args.watch > 0:
        try:
            while True:
                os.system("clear")
                print(render_cli(load_summary(logs_dir, args.chain, args.max_lines, blocking_rpc_if_cold=True, since_iso=args.since)))
                time.sleep(args.watch)
        except KeyboardInterrupt:
            return
    else:
        print(render_cli(load_summary(logs_dir, args.chain, args.max_lines, blocking_rpc_if_cold=True, since_iso=args.since)))


if __name__ == "__main__":
    main()
