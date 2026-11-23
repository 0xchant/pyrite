#!/usr/bin/env python3
"""Standalone CPU vs CUDA CFMM solver parity tests and benchmark suite.

Uses only the Python standard library and dynamically loads `libcfmm_gpu.so`.
Zero RPC keys, mainnet secrets, or third-party packages required.
"""
from __future__ import annotations

import argparse
from collections import defaultdict
import ctypes
import json
import math
import os
from pathlib import Path
import random
import sys
import time
from typing import Any

MAX_TOKENS = 256
MAX_L2_CANDS = 256
MAX_CLMM_SEGS_PER_POOL = 64

POOL_UNIV2 = 0
POOL_CLMM = 1
POOL_CURVE_STABLE = 2
POOL_PROP_BINS = 3
POOL_SOLIDLY_STABLE = 4
POOL_BALANCER_V2 = 5
POOL_METRIC_OMM = 6

PROTO_V2 = 0
PROTO_V3 = 1
PROTO_CURVE = 2
PROTO_PROP_BINS = 3
PROTO_SOLIDLY = 4
PROTO_BALANCER_V2 = 5
PROTO_METRIC_OMM = 6
PROTO_UNIV4 = 7
PROTO_INFINITY_CL = 8


class PoolData(ctypes.Structure):
    _fields_ = [
        ("token0", ctypes.c_int32),
        ("token1", ctypes.c_int32),
        ("pool_type", ctypes.c_int32),
        ("protocol_id", ctypes.c_int32),
        ("tick_offset", ctypes.c_int32),
        ("tick_count", ctypes.c_int32),
        ("cur_seg", ctypes.c_int32),
        ("_pad", ctypes.c_int32),
        ("gas_units", ctypes.c_double),
        ("fee_ppm", ctypes.c_double),
        ("r0", ctypes.c_double),
        ("r1", ctypes.c_double),
        ("sqrt_p", ctypes.c_double),
        ("liq", ctypes.c_double),
        ("param0", ctypes.c_double),
        ("param1", ctypes.c_double),
        ("param2", ctypes.c_double),
    ]


assert ctypes.sizeof(PoolData) == 104, f"Expected 104B PoolData, got {ctypes.sizeof(PoolData)}"


class SolverStep(ctypes.Structure):
    _fields_ = [
        ("pool_idx", ctypes.c_int32),
        ("dir", ctypes.c_int32),
        ("amount_in", ctypes.c_double),
        ("amount_out", ctypes.c_double),
        ("gas_units", ctypes.c_double),
    ]


class SolveResult(ctypes.Structure):
    _fields_ = [
        ("num_steps", ctypes.c_int32),
        ("max_steps_cap", ctypes.c_int32),
        ("borrow_amount", ctypes.c_double),
        ("gross_profit", ctypes.c_double),
        ("gas_cost_base", ctypes.c_double),
        ("net_profit", ctypes.c_double),
        ("total_gas_units", ctypes.c_double),
        ("max_deficit", ctypes.c_double),
        ("layer1_ms", ctypes.c_double),
        ("layer2_ms", ctypes.c_double),
        ("total_ms", ctypes.c_double),
        ("l1_unpruned_gross", ctypes.c_double),
        ("l1_unpruned_gas_cost", ctypes.c_double),
        ("l1_unpruned_net", ctypes.c_double),
        ("l1_unpruned_steps", ctypes.c_int32),
        ("_reserved", ctypes.c_int32),
    ]


def resolve_lib_path(explicit: str | None = None) -> Path:
    """Locate `libcfmm_gpu.so` from CLI flag, env var, or local build directories."""
    if explicit:
        p = Path(explicit).expanduser()
        if p.is_file():
            return p
        print(f"ERROR: Specified library not found: {p}", file=sys.stderr)
        sys.exit(1)

    candidates: list[Path] = []
    env_lib = os.environ.get("CFMM_GPU_LIB", "").strip()
    if env_lib:
        candidates.append(Path(env_lib).expanduser())
    here = Path(__file__).resolve().parent
    candidates.extend([
        here / "libcfmm_gpu.so",
        Path("./cuda/libcfmm_gpu.so"),
        Path("./libcfmm_gpu.so"),
    ])

    for cand in candidates:
        if cand.is_file():
            return cand

    print(
        "ERROR: libcfmm_gpu.so not found.\n"
        "Build it on a CUDA host by specifying CUDA_ARCH explicitly, for example:\n"
        "  make -C cuda CUDA_ARCH=sm_89\n"
        "Then rerun with: python3 cuda/bench.py (or pass --lib /path/to/libcfmm_gpu.so).",
        file=sys.stderr,
    )
    sys.exit(1)


def load_lib(so_path: str | Path) -> ctypes.CDLL:
    """Load `libcfmm_gpu.so` and bind C solver signatures."""
    lib = ctypes.CDLL(str(so_path))
    lib.cfmm_get_device_info.argtypes = [
        ctypes.c_char_p, ctypes.c_int,
        ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
        ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_int),
    ]
    lib.cfmm_get_device_info.restype = ctypes.c_int

    lib.cfmm_quote_single.argtypes = [
        ctypes.POINTER(PoolData), ctypes.POINTER(ctypes.c_double), ctypes.POINTER(ctypes.c_double),
        ctypes.c_int32, ctypes.c_double,
    ]
    lib.cfmm_quote_single.restype = ctypes.c_double

    lib.cfmm_init_market.argtypes = [
        ctypes.POINTER(PoolData), ctypes.c_int32,
        ctypes.POINTER(ctypes.c_double), ctypes.POINTER(ctypes.c_double),
        ctypes.c_int32, ctypes.c_int32,
    ]
    lib.cfmm_init_market.restype = ctypes.c_int

    lib.cfmm_update_dirty_pools.argtypes = [
        ctypes.POINTER(ctypes.c_int32), ctypes.POINTER(PoolData), ctypes.c_int32,
    ]
    lib.cfmm_update_dirty_pools.restype = ctypes.c_int

    lib.cfmm_solve_resident.argtypes = [
        ctypes.c_int32, ctypes.c_double, ctypes.c_double, ctypes.c_double,
        ctypes.c_int32, ctypes.c_int32,
        ctypes.POINTER(SolverStep), ctypes.c_int32, ctypes.POINTER(SolveResult),
    ]
    lib.cfmm_solve_resident.restype = ctypes.c_int

    lib.cfmm_solve_gpu.argtypes = [
        ctypes.POINTER(PoolData), ctypes.c_int32,
        ctypes.POINTER(ctypes.c_double), ctypes.POINTER(ctypes.c_double),
        ctypes.c_int32, ctypes.c_int32, ctypes.c_int32,
        ctypes.c_double, ctypes.c_double, ctypes.c_double,
        ctypes.c_int32, ctypes.c_int32,
        ctypes.POINTER(SolverStep), ctypes.c_int32, ctypes.POINTER(SolveResult),
    ]
    lib.cfmm_solve_gpu.restype = ctypes.c_int

    lib.cfmm_solve_cpu_greedy_2_3hop.argtypes = [
        ctypes.POINTER(PoolData), ctypes.c_int32,
        ctypes.POINTER(ctypes.c_double), ctypes.POINTER(ctypes.c_double),
        ctypes.c_int32, ctypes.c_int32,
        ctypes.c_double, ctypes.c_double, ctypes.c_double,
        ctypes.POINTER(SolverStep), ctypes.c_int32, ctypes.POINTER(SolveResult),
    ]
    lib.cfmm_solve_cpu_greedy_2_3hop.restype = ctypes.c_int

    lib.cfmm_solve_cpu_dual_single.argtypes = [
        ctypes.POINTER(PoolData), ctypes.c_int32,
        ctypes.POINTER(ctypes.c_double), ctypes.POINTER(ctypes.c_double),
        ctypes.c_int32, ctypes.c_int32,
        ctypes.c_double, ctypes.c_double, ctypes.c_double,
        ctypes.c_int32, ctypes.c_int32,
        ctypes.POINTER(SolverStep), ctypes.c_int32, ctypes.POINTER(SolveResult),
    ]
    lib.cfmm_solve_cpu_dual_single.restype = ctypes.c_int

    lib.cfmm_benchmark_um_sync.argtypes = [
        ctypes.c_int32, ctypes.c_int32, ctypes.c_int32, ctypes.POINTER(ctypes.c_double),
    ]
    lib.cfmm_benchmark_um_sync.restype = ctypes.c_int
    return lib


def stable_get_D_py(x0: float, x1: float, A: float) -> float:
    S = x0 + x1
    if S <= 0.0 or x0 <= 0.0 or x1 <= 0.0:
        return 0.0
    D = S
    Ann = 4.0 * max(A, 1e-6)
    for _ in range(12):
        D_p = (D * D / (2.0 * x0)) * (D / (2.0 * x1))
        D_prev = D
        num = (Ann * S + 2.0 * D_p) * D
        den = (Ann - 1.0) * D + 3.0 * D_p
        if den <= 0.0:
            break
        D = num / den
        if abs(D - D_prev) <= 1e-13 * max(1.0, D):
            break
    return D


def stable_get_y_py(x_new: float, D: float, A: float) -> float:
    if x_new <= 0.0 or D <= 0.0:
        return 0.0
    Ann = 4.0 * max(A, 1e-6)
    B = x_new + D / Ann - D
    C = (D * D / (4.0 * Ann * x_new)) * D
    root = math.sqrt(max(0.0, B * B + 4.0 * C))
    if B >= 0.0:
        den = B + root
        return (2.0 * C / den) if den > 0.0 else 0.0
    return 0.5 * (-B + root)


def solidly_stable_out_py(x0: float, y0: float, dx_eff: float) -> float:
    if x0 <= 0.0 or y0 <= 0.0 or dx_eff <= 0.0:
        return 0.0
    S = x0 + y0
    xn = x0 / S
    yn = y0 / S
    d = dx_eff / S
    k = xn * xn * xn * yn + xn * yn * yn * yn
    x1 = xn + d
    x1_3 = x1 * x1 * x1
    if x1_3 <= 0.0:
        return 0.0
    y = min(yn, k / x1_3)
    for _ in range(8):
        f = x1 * y * y * y + x1_3 * y - k
        df = 3.0 * x1 * y * y + x1_3
        if df <= 0.0:
            break
        next_y = y - f / df
        if abs(next_y - y) <= 1e-13 * max(1e-18, y):
            y = next_y
            break
        y = max(1e-18, next_y)
    return max(0.0, (yn - y) * S)


def quote_pool_py(p: PoolData, tick_sp: list[float], tick_liq: list[float], direction: int, dx: float) -> float:
    """Exact fp64 reference quoter used to replay solver steps sequentially."""
    if dx <= 0.0:
        return 0.0
    gamma = 1.0 - p.fee_ppm * 1e-6
    if not (1e-6 < gamma <= 1.0):
        gamma = 0.997

    if p.pool_type == POOL_UNIV2:
        rin, rout = (p.r0, p.r1) if direction == 0 else (p.r1, p.r0)
        if rin <= 0.0 or rout <= 0.0:
            return 0.0
        dx_eff = gamma * dx
        return (rout * dx_eff) / (rin + dx_eff)

    if p.pool_type == POOL_CLMM:
        n = p.tick_count
        if n <= 0 or n > MAX_CLMM_SEGS_PER_POOL or p.sqrt_p <= 0.0:
            return 0.0
        k = max(0, min(n - 1, p.cur_seg))
        off = p.tick_offset
        s = p.sqrt_p
        x_rem = gamma * dx
        out_acc = 0.0
        if direction == 0:
            for seg in range(k, -1, -1):
                if x_rem <= 0.0:
                    break
                L = tick_liq[off + seg]
                sa = tick_sp[off + seg]
                if L <= 0.0 or sa <= 0.0 or s <= sa:
                    s = sa
                    continue
                max_x = L * (1.0 / sa - 1.0 / s)
                if x_rem >= max_x:
                    out_acc += L * (s - sa)
                    x_rem -= max_x
                    s = sa
                else:
                    s_next = (L * s) / (L + s * x_rem)
                    out_acc += L * (s - s_next)
                    x_rem = 0.0
                    break
        else:
            for seg in range(k, n):
                if x_rem <= 0.0:
                    break
                L = tick_liq[off + seg]
                sb = tick_sp[off + seg + 1]
                if L <= 0.0 or sb <= 0.0 or s >= sb:
                    s = sb
                    continue
                max_y = L * (sb - s)
                if x_rem >= max_y:
                    out_acc += L * (1.0 / s - 1.0 / sb)
                    x_rem -= max_y
                    s = sb
                else:
                    s_next = s + x_rem / L
                    out_acc += L * (1.0 / s - 1.0 / s_next)
                    x_rem = 0.0
                    break
        return out_acc

    if p.pool_type == POOL_CURVE_STABLE:
        rin, rout = (p.r0, p.r1) if direction == 0 else (p.r1, p.r0)
        if rin <= 0.0 or rout <= 0.0:
            return 0.0
        A = p.param0
        D = p.liq if p.liq > 0.0 else stable_get_D_py(p.r0, p.r1, A)
        y_new = stable_get_y_py(rin + gamma * dx, D, A)
        return (rout - y_new) if (0.0 < y_new < rout) else 0.0

    if p.pool_type == POOL_PROP_BINS:
        n_bins = p.tick_count if direction == 0 else p.cur_seg
        off = p.tick_offset + (0 if direction == 0 else p.tick_count)
        if n_bins <= 0:
            return 0.0
        rem = gamma * dx
        out_acc = 0.0
        for b in range(n_bins):
            if rem <= 0.0:
                break
            rate = tick_sp[off + b]
            cap = tick_liq[off + b]
            if rate <= 0.0 or cap <= 0.0:
                continue
            take = min(rem, cap)
            out_acc += take * rate
            rem -= take
        return out_acc

    if p.pool_type == POOL_SOLIDLY_STABLE:
        rin, rout = (p.r0, p.r1) if direction == 0 else (p.r1, p.r0)
        return solidly_stable_out_py(rin, rout, gamma * dx)

    if p.pool_type == POOL_BALANCER_V2:
        rin, rout = (p.r0, p.r1) if direction == 0 else (p.r1, p.r0)
        w_in, w_out = (p.liq, p.param0) if direction == 0 else (p.param0, p.liq)
        if rin <= 0.0 or rout <= 0.0 or w_in <= 0.0 or w_out <= 0.0:
            return 0.0
        alpha = w_in / w_out
        return -rout * math.expm1(-alpha * math.log1p((gamma * dx) / rin))

    if p.pool_type == POOL_METRIC_OMM:
        out_avail = p.r1 if direction == 0 else p.r0
        top_rate = p.sqrt_p if direction == 0 else ((1.0 / p.liq) if p.liq > 0.0 else 0.0)
        haircut = p.param1 if direction == 0 else p.param2
        max_walk = p.param0
        if out_avail <= 1e-12 or top_rate <= 0.0:
            return 0.0
        out_no_slip = dx * top_rate
        if out_no_slip >= out_avail * 0.80:
            return 0.0
        frac = out_no_slip / out_avail
        c_mult = (1.0 - haircut) * gamma
        return max(0.0, out_no_slip * (1.0 - 0.5 * max_walk * frac) * c_mult)

    return 0.0


class MarketBuilder:
    """In-memory multi-protocol CFMM market serializer for ctypes solver FFI."""

    def __init__(self) -> None:
        self.pools: list[PoolData] = []
        self.tick_sp: list[float] = []
        self.tick_liq: list[float] = []

    def add_v2(self, t0: int, t1: int, r0: float, r1: float, fee_ppm: float = 3000.0, gas: float = 52000.0, proto: int = PROTO_V2) -> int:
        p = PoolData()
        p.token0, p.token1, p.pool_type, p.protocol_id = t0, t1, POOL_UNIV2, proto
        p.gas_units, p.fee_ppm, p.r0, p.r1 = gas, fee_ppm, r0, r1
        self.pools.append(p)
        return len(self.pools) - 1

    def add_clmm(self, t0: int, t1: int, sqrt_p: float, sp_bounds: list[float], seg_liqs: list[float], cur_seg: int, fee_ppm: float = 500.0, gas: float = 92000.0, proto: int = PROTO_V3) -> int:
        n = len(seg_liqs)
        assert 1 <= n <= MAX_CLMM_SEGS_PER_POOL and len(sp_bounds) == n + 1
        off = len(self.tick_sp)
        self.tick_sp.extend(sp_bounds)
        self.tick_liq.extend(seg_liqs)
        self.tick_liq.append(0.0)
        p = PoolData()
        p.token0, p.token1, p.pool_type, p.protocol_id = t0, t1, POOL_CLMM, proto
        p.tick_offset, p.tick_count, p.cur_seg = off, n, cur_seg
        p.gas_units, p.fee_ppm, p.sqrt_p, p.liq = gas, fee_ppm, sqrt_p, seg_liqs[cur_seg]
        self.pools.append(p)
        return len(self.pools) - 1

    def add_curve(self, t0: int, t1: int, r0: float, r1: float, amp: float = 100.0, fee_ppm: float = 400.0, gas: float = 115000.0, proto: int = PROTO_CURVE) -> int:
        p = PoolData()
        p.token0, p.token1, p.pool_type, p.protocol_id = t0, t1, POOL_CURVE_STABLE, proto
        p.gas_units, p.fee_ppm, p.r0, p.r1, p.param0 = gas, fee_ppm, r0, r1, amp
        p.liq = stable_get_D_py(r0, r1, amp)
        self.pools.append(p)
        return len(self.pools) - 1

    def add_prop_bins(self, t0: int, t1: int, bid_rates: list[float], bid_caps0: list[float], ask_rates: list[float], ask_caps1: list[float], fee_ppm: float = 0.0, gas: float = 85000.0, proto: int = PROTO_PROP_BINS) -> int:
        off = len(self.tick_sp)
        self.tick_sp.extend(bid_rates)
        self.tick_sp.extend(ask_rates)
        self.tick_liq.extend(bid_caps0)
        self.tick_liq.extend(ask_caps1)
        p = PoolData()
        p.token0, p.token1, p.pool_type, p.protocol_id = t0, t1, POOL_PROP_BINS, proto
        p.tick_offset, p.tick_count, p.cur_seg = off, len(bid_rates), len(ask_rates)
        p.gas_units, p.fee_ppm = gas, fee_ppm
        self.pools.append(p)
        return len(self.pools) - 1

    def add_solidly_stable(self, t0: int, t1: int, r0: float, r1: float, fee_ppm: float = 500.0, gas: float = 82000.0, proto: int = PROTO_SOLIDLY) -> int:
        p = PoolData()
        p.token0, p.token1, p.pool_type, p.protocol_id = t0, t1, POOL_SOLIDLY_STABLE, proto
        p.gas_units, p.fee_ppm, p.r0, p.r1 = gas, fee_ppm, r0, r1
        self.pools.append(p)
        return len(self.pools) - 1

    def add_balancer_v2(self, t0: int, t1: int, r0: float, r1: float, w0: float, w1: float, fee_ppm: float = 3000.0, gas: float = 130000.0, proto: int = PROTO_BALANCER_V2) -> int:
        p = PoolData()
        p.token0, p.token1, p.pool_type, p.protocol_id = t0, t1, POOL_BALANCER_V2, proto
        p.gas_units, p.fee_ppm, p.r0, p.r1 = gas, fee_ppm, r0, r1
        p.liq, p.param0 = w0, w1
        self.pools.append(p)
        return len(self.pools) - 1

    def add_metric_omm(self, t0: int, t1: int, r0: float, r1: float, sell_human: float, buy_human: float, num_bins: int = 40, active_bin: int = 0, extra_tax_ppm: float = 0.0, gas: float = 135000.0, proto: int = PROTO_METRIC_OMM) -> int:
        spr_w = min(0.02, max(0.00005, (buy_human - sell_human) / sell_human))
        half_bins = max(num_bins, 2) * 0.5
        max_walk = min(0.05, max(0.0005, spr_w * half_bins * 0.5))

        def _hc(zfo: bool) -> float:
            spr = min(0.02, max(0.00001, (buy_human - sell_human) / sell_human))
            base_fee = min(0.00015, max(0.00003, spr * 0.5))
            adv = max(0, min(20, -active_bin if zfo else active_bin))
            bin_step = min(0.00012, max(0.000015, spr * 0.5))
            return min(0.05, base_fee + adv * bin_step)

        p = PoolData()
        p.token0, p.token1, p.pool_type, p.protocol_id = t0, t1, POOL_METRIC_OMM, proto
        p.gas_units, p.fee_ppm, p.r0, p.r1 = gas, extra_tax_ppm, r0, r1
        p.sqrt_p, p.liq = sell_human, buy_human
        p.param0, p.param1, p.param2 = max_walk, _hc(True), _hc(False)
        self.pools.append(p)
        return len(self.pools) - 1

    def to_c_arrays(self):
        p_arr = (PoolData * len(self.pools))(*self.pools)
        n_t = max(1, len(self.tick_sp))
        sp_data = self.tick_sp if self.tick_sp else [0.0]
        lq_data = self.tick_liq if self.tick_liq else [0.0]
        sp_arr = (ctypes.c_double * n_t)(*sp_data)
        lq_arr = (ctypes.c_double * n_t)(*lq_data)
        return p_arr, len(self.pools), sp_arr, lq_arr, len(self.tick_sp)


def verify_solution_py(
    market: MarketBuilder,
    base_tok: int,
    gas_price_base: float,
    base_tx_gas: float,
    max_gas_limit: float,
    steps: list[SolverStep],
    res: SolveResult,
) -> dict[str, Any]:
    """Sequential causal verifier enforcing zero token deficit and exact gas accounting."""
    bal: dict[int, float] = defaultdict(float)
    bal[base_tok] = res.borrow_amount
    max_deficit = 0.0
    total_gas = base_tx_gas if res.num_steps > 0 else 0.0
    used_pools: set[int] = set()

    for idx in range(res.num_steps):
        st = steps[idx]
        p = market.pools[st.pool_idx]
        if st.pool_idx in used_pools:
            raise AssertionError(f"Duplicate pool reused at step {idx}: pool={st.pool_idx}")
        used_pools.add(st.pool_idx)
        tin, tout = (p.token0, p.token1) if st.dir == 0 else (p.token1, p.token0)
        if st.amount_in > bal[tin]:
            max_deficit = max(max_deficit, st.amount_in - bal[tin])
        bal[tin] -= st.amount_in
        py_out = quote_pool_py(p, market.tick_sp, market.tick_liq, st.dir, st.amount_in)
        step_err = abs(py_out - st.amount_out) / max(1e-12, abs(py_out), abs(st.amount_out))
        if step_err > 1e-6:
            raise AssertionError(f"Step {idx} quote mismatch: py={py_out:.12g} c={st.amount_out:.12g}")
        bal[tout] += py_out
        total_gas += p.gas_units

    py_gross = bal[base_tok] - res.borrow_amount
    py_gas_cost = total_gas * gas_price_base
    py_net = py_gross - py_gas_cost
    gross_rel_err = abs(py_gross - res.gross_profit) / max(abs(res.gross_profit), 1e-12)
    net_rel_err = abs(py_net - res.net_profit) / max(abs(res.net_profit), 1e-12)
    gas_limit_ok = (max_gas_limit <= 0.0) or (total_gas <= max_gas_limit + 1e-6)
    gas_match_ok = abs(total_gas - res.total_gas_units) <= 1e-2
    ok = (
        (max_deficit <= 1e-9)
        and (gross_rel_err <= 1e-4)
        and (net_rel_err <= 1e-4)
        and gas_limit_ok
        and gas_match_ok
    )
    return {
        "ok": ok,
        "max_deficit": max_deficit,
        "py_gross": py_gross,
        "py_gas_cost": py_gas_cost,
        "py_net": py_net,
        "total_gas": total_gas,
        "gas_limit_ok": gas_limit_ok,
        "gross_rel_err": gross_rel_err,
        "net_rel_err": net_rel_err,
    }


def solve_gpu(
    lib: ctypes.CDLL,
    market: MarketBuilder,
    n_tokens: int,
    base_tok: int,
    gas_price_base: float,
    base_tx_gas: float = 0.0,
    max_gas_limit: float = 0.0,
    budget_ms: int = 500,
    enable_layer2: int = 1,
    max_steps: int = 1024,
):
    p_arr, n_p, sp_arr, lq_arr, n_te = market.to_c_arrays()
    steps = (SolverStep * max_steps)()
    res = SolveResult()
    rc = lib.cfmm_solve_gpu(
        p_arr, n_p, sp_arr, lq_arr, n_te, n_tokens, base_tok,
        gas_price_base, base_tx_gas, max_gas_limit, budget_ms, enable_layer2,
        steps, max_steps, ctypes.byref(res),
    )
    assert rc == 0, f"cfmm_solve_gpu failed rc={rc}"
    sl = list(steps[:res.num_steps])
    v = verify_solution_py(market, base_tok, gas_price_base, base_tx_gas, max_gas_limit, sl, res)
    return res, sl, v


def solve_cpu_greedy(
    lib: ctypes.CDLL,
    market: MarketBuilder,
    n_tokens: int,
    base_tok: int,
    gas_price_base: float,
    base_tx_gas: float = 0.0,
    max_gas_limit: float = 0.0,
    max_steps: int = 16,
):
    p_arr, n_p, sp_arr, lq_arr, _ = market.to_c_arrays()
    steps = (SolverStep * max_steps)()
    res = SolveResult()
    rc = lib.cfmm_solve_cpu_greedy_2_3hop(
        p_arr, n_p, sp_arr, lq_arr, n_tokens, base_tok,
        gas_price_base, base_tx_gas, max_gas_limit,
        steps, max_steps, ctypes.byref(res),
    )
    assert rc == 0, f"cfmm_solve_cpu_greedy_2_3hop failed rc={rc}"
    sl = list(steps[:res.num_steps])
    v = verify_solution_py(market, base_tok, gas_price_base, base_tx_gas, max_gas_limit, sl, res)
    return res, sl, v


def solve_cpu_dual(
    lib: ctypes.CDLL,
    market: MarketBuilder,
    n_tokens: int,
    base_tok: int,
    gas_price_base: float,
    base_tx_gas: float = 0.0,
    max_gas_limit: float = 0.0,
    num_restarts: int = 32,
    num_prune_masks: int = 16,
    max_steps: int = 1024,
):
    p_arr, n_p, sp_arr, lq_arr, _ = market.to_c_arrays()
    steps = (SolverStep * max_steps)()
    res = SolveResult()
    rc = lib.cfmm_solve_cpu_dual_single(
        p_arr, n_p, sp_arr, lq_arr, n_tokens, base_tok,
        gas_price_base, base_tx_gas, max_gas_limit,
        num_restarts, num_prune_masks, steps, max_steps, ctypes.byref(res),
    )
    assert rc == 0, f"cfmm_solve_cpu_dual_single failed rc={rc}"
    sl = list(steps[:res.num_steps])
    v = verify_solution_py(market, base_tok, gas_price_base, base_tx_gas, max_gas_limit, sl, res)
    return res, sl, v


def fmt_res(tag: str, res: SolveResult, ver: dict[str, Any], base_usd: float) -> dict[str, Any]:
    return {
        "tag": tag,
        "steps": int(res.num_steps),
        "borrow_base": float(res.borrow_amount),
        "gross_usd": float(res.gross_profit * base_usd),
        "gas_usd": float(res.gas_cost_base * base_usd),
        "net_usd": float(res.net_profit * base_usd),
        "total_gas": float(res.total_gas_units),
        "layer1_ms": float(res.layer1_ms),
        "layer2_ms": float(res.layer2_ms),
        "total_ms": float(res.total_ms),
        "l1_unpruned_steps": int(res.l1_unpruned_steps),
        "l1_unpruned_gross_usd": float(res.l1_unpruned_gross * base_usd),
        "l1_unpruned_gas_usd": float(res.l1_unpruned_gas_cost * base_usd),
        "l1_unpruned_net_usd": float(res.l1_unpruned_net * base_usd),
        "verified": bool(ver["ok"]),
        "max_deficit": float(ver["max_deficit"]),
        "net_rel_err": float(ver["net_rel_err"]),
    }


def c_quote_pool(lib: ctypes.CDLL, mb: MarketBuilder, idx: int, direction: int, dx: float) -> float:
    p_arr, _, sp_arr, lq_arr, _ = mb.to_c_arrays()
    return float(lib.cfmm_quote_single(ctypes.byref(p_arr[idx]), sp_arr, lq_arr, direction, dx))


def build_synthetic_stress_market(
    seed: int,
    n_pools: int,
    n_tokens: int,
    shock_bps: float = 32.0,
    num_shocked_toks: int = 12,
) -> tuple[MarketBuilder, int, int, float, float]:
    """7-protocol coupled heterogeneous stress market generator (stdlib random only)."""
    rng = random.Random(seed)
    base_usd = 1.0
    gas_price_base = 2.2e-6

    prices = [1.0]
    for t in range(1, n_tokens):
        if t <= 4:
            prices.append(rng.choice([1.0, 3100.0, 90000.0, 3600.0, 620.0]))
        else:
            prices.append(10.0 ** rng.uniform(-0.3, 3.2))

    mb = MarketBuilder()
    core_count = int(n_pools * 0.65)
    for k in range(core_count):
        if k < n_tokens - 1:
            t0, t1 = 0, k + 1
        else:
            t0 = rng.randint(0, min(6, n_tokens - 1)) if rng.random() < 0.6 else rng.randint(0, n_tokens - 1)
            t1 = rng.randint(0, n_tokens - 1)
            if t1 == t0:
                t1 = (t0 + 1) % n_tokens
        if t0 > t1:
            t0, t1 = t1, t0

        fair_p10 = prices[t0] / prices[t1]
        skew = 1.0
        if t1 <= num_shocked_toks and rng.random() < 0.55:
            skew = math.exp(rng.uniform(-shock_bps, shock_bps) * 1e-4)
        p10 = fair_p10 * skew

        tvl_usd = 10.0 ** rng.uniform(4.2, 6.2)
        r0 = (0.5 * tvl_usd) / prices[t0]
        r1 = r0 * p10

        roll = rng.random()
        is_pegged = 0.80 <= fair_p10 <= 1.25

        if roll < 0.25:
            mb.add_v2(t0, t1, r0, r1, fee_ppm=rng.choice([2500.0, 3000.0]), gas=52000.0)
        elif roll < 0.60:
            n_seg = rng.randint(6, 16)
            mid_s = math.sqrt(p10)
            bounds = [mid_s * (0.98 ** (n_seg // 2 - i)) for i in range(n_seg + 1)]
            cur_seg = n_seg // 2
            s_cur = 0.5 * (bounds[cur_seg] + bounds[cur_seg + 1])
            s_cur = max(bounds[cur_seg] * 1.0001, min(bounds[cur_seg + 1] * 0.9999, s_cur))
            l_base = (0.5 * tvl_usd / prices[t1]) / (s_cur * 0.08)
            seg_liqs = [l_base * rng.uniform(0.6, 1.5) for _ in range(n_seg)]
            proto = PROTO_UNIV4 if rng.random() < 0.30 else PROTO_V3
            gas_u = 125000.0 if proto == PROTO_UNIV4 else 92000.0
            mb.add_clmm(t0, t1, s_cur, bounds, seg_liqs, cur_seg, fee_ppm=rng.choice([100.0, 500.0, 3000.0]), gas=gas_u, proto=proto)
        elif roll < 0.72 and is_pegged:
            mb.add_curve(t0, t1, r0, r1, amp=rng.uniform(20.0, 200.0), fee_ppm=400.0, gas=115000.0)
        elif roll < 0.80 and is_pegged:
            mb.add_solidly_stable(t0, t1, r0, r1, fee_ppm=200.0, gas=82000.0)
        elif roll < 0.90:
            w0 = rng.choice([0.5, 0.8, 0.6, 0.2])
            w1 = 1.0 - w0
            r0_b = (w0 * tvl_usd) / prices[t0]
            r1_b = r0_b * (w1 / w0) * p10
            mb.add_balancer_v2(t0, t1, r0_b, r1_b, w0, w1, fee_ppm=rng.choice([1000.0, 3000.0]), gas=130000.0)
        elif roll < 0.95:
            sell_h = p10 * (1.0 - rng.uniform(2e-4, 5e-4))
            buy_h = p10 * (1.0 + rng.uniform(2e-4, 5e-4))
            mb.add_metric_omm(t0, t1, r0 * 0.4, r1 * 0.4, sell_h, buy_h, num_bins=rng.randint(24, 60), active_bin=rng.randint(-2, 2), gas=135000.0)
        else:
            b_rates = [p10 * (1.0 - 0.0002 - 0.0008 * b) for b in range(4)]
            a_rates = [(1.0 / p10) * (1.0 - 0.0002 - 0.0008 * b) for b in range(4)]
            cap0 = [0.06 * r0, 0.08 * r0, 0.12 * r0, 0.20 * r0]
            cap1 = [0.06 * r1, 0.08 * r1, 0.12 * r1, 0.20 * r1]
            mb.add_prop_bins(t0, t1, b_rates, cap0, a_rates, cap1, fee_ppm=0.0, gas=85000.0)

    inj_count = int(n_pools * 0.10)
    for _ in range(inj_count):
        t0 = 0 if rng.random() < 0.45 else rng.randint(1, min(num_shocked_toks, n_tokens - 1))
        t1 = rng.randint(1, n_tokens - 1)
        if t1 == t0:
            t1 = (t0 + 1) % n_tokens
        if t0 > t1:
            t0, t1 = t1, t0
        fair_p10 = prices[t0] / prices[t1]
        shift_dir = rng.choice([-1.0, 1.0])
        oracle_p10 = fair_p10 * math.exp(shift_dir * rng.uniform(0.0020, 0.0065))
        cap_usd = 10.0 ** rng.uniform(4.2, 5.2)
        if rng.random() < 0.5:
            r0_m = (0.5 * cap_usd) / prices[t0]
            r1_m = (0.5 * cap_usd) / prices[t1]
            mb.add_metric_omm(t0, t1, r0_m, r1_m, oracle_p10 * 0.9998, oracle_p10 * 1.0002, num_bins=36, active_bin=0, gas=135000.0)
        else:
            b_rates = [oracle_p10 * (1.0 - 0.0002 - 0.0008 * b) for b in range(4)]
            a_rates = [(1.0 / oracle_p10) * (1.0 - 0.0002 - 0.0008 * b) for b in range(4)]
            b_cap = [(f * cap_usd) / prices[t0] for f in (0.12, 0.16, 0.22, 0.30)]
            a_cap = [(f * cap_usd) / prices[t1] for f in (0.12, 0.16, 0.22, 0.30)]
            mb.add_prop_bins(t0, t1, b_rates, b_cap, a_rates, a_cap, fee_ppm=0.0, gas=85000.0)

    while len(mb.pools) < n_pools:
        t0 = rng.randint(0, n_tokens - 1)
        t1 = rng.randint(0, n_tokens - 1)
        if t1 == t0:
            t1 = (t0 + 1) % n_tokens
        if t0 > t1:
            t0, t1 = t1, t0
        fair_p10 = prices[t0] / prices[t1]
        p10 = fair_p10 * math.exp(rng.uniform(-0.008, 0.008))
        dust_usd = rng.uniform(8.0, 90.0)
        r0 = (0.5 * dust_usd) / prices[t0]
        r1 = r0 * p10
        mb.add_v2(t0, t1, r0, r1, fee_ppm=3000.0, gas=55000.0)

    return mb, n_tokens, 0, gas_price_base, base_usd


def run_self_tests(lib: ctypes.CDLL) -> list[dict[str, Any]]:
    print("=== SELF-TESTS (6/6 Strict fp64 Parity & Pruning Tests) ===")
    results: list[dict[str, Any]] = []

    # 1. UniV2 2-pool closed-form exact parity
    mb1 = MarketBuilder()
    mb1.add_v2(0, 1, 100_000.0, 100_000.0, fee_ppm=3000.0, gas=52000.0)
    mb1.add_v2(0, 1, 100_000.0, 120_000.0, fee_ppm=3000.0, gas=52000.0)
    res1, _, v1 = solve_gpu(lib, mb1, 2, 0, gas_price_base=1e-6, base_tx_gas=0.0, max_gas_limit=0.0, budget_ms=100, enable_layer2=1)
    assert v1["ok"] and res1.net_profit > 100.0
    g = 0.997
    dx_opt = (math.sqrt(g * g * 100000 * 120000 * 100000 * 100000) - 100000 * 100000) / (g * (100000 + g * 120000))
    dy1 = quote_pool_py(mb1.pools[1], mb1.tick_sp, mb1.tick_liq, 0, dx_opt)
    dy2 = quote_pool_py(mb1.pools[0], mb1.tick_sp, mb1.tick_liq, 1, dy1)
    net_exact = (dy2 - dx_opt) - 2 * 52000.0 * 1e-6
    rel1 = abs(res1.net_profit - net_exact) / net_exact
    assert rel1 < 1e-6
    print(f"  [Test 1] UniV2 2-pool parity: GPU=${res1.net_profit:.4f} vs Exact=${net_exact:.4f} (rel_err={rel1:.2e}) PASS")
    results.append({"name": "univ2_2pool_parity", "gpu_net": res1.net_profit, "exact_net": net_exact, "rel_err": rel1, "passed": True})

    # 2. Solidly Stable (x^3 y + x y^3 = k) quote parity + arbitrage cycle
    mb2 = MarketBuilder()
    mb2.add_solidly_stable(0, 1, 1_000_000.0, 1_000_000.0, fee_ppm=200.0, gas=82000.0)
    mb2.add_v2(0, 1, 100_000.0, 102_500.0, fee_ppm=3000.0, gas=52000.0)
    for dx_t in (1.0, 1000.0, 50000.0, 250000.0):
        cq = c_quote_pool(lib, mb2, 0, 0, dx_t)
        pq = quote_pool_py(mb2.pools[0], mb2.tick_sp, mb2.tick_liq, 0, dx_t)
        re = abs(cq - pq) / max(1e-12, pq)
        assert re < 1e-12, f"Solidly quote err {re}"
    res2, _, v2 = solve_gpu(lib, mb2, 2, 0, gas_price_base=1e-6, base_tx_gas=0.0, max_gas_limit=0.0, budget_ms=100, enable_layer2=1)
    assert v2["ok"] and res2.net_profit > 10.0
    print(f"  [Test 2] Solidly Stable (x^3y+xy^3=k) quote err<1e-12 & arb net=${res2.net_profit:.4f} PASS")
    results.append({"name": "solidly_stable_parity", "net": res2.net_profit, "passed": True})

    # 3. Balancer V2 Weighted (80/20 & 50/50) quote parity + arb cycle
    mb3 = MarketBuilder()
    mb3.add_balancer_v2(0, 1, 80_000.0, 21_500.0, 0.80, 0.20, fee_ppm=3000.0, gas=130000.0)
    mb3.add_balancer_v2(0, 1, 50_000.0, 50_000.0, 0.50, 0.50, fee_ppm=1000.0, gas=130000.0)
    for idx_b in (0, 1):
        for dx_t in (5.0, 500.0, 10000.0):
            cq = c_quote_pool(lib, mb3, idx_b, 0, dx_t)
            pq = quote_pool_py(mb3.pools[idx_b], mb3.tick_sp, mb3.tick_liq, 0, dx_t)
            re = abs(cq - pq) / max(1e-12, pq)
            assert re < 1e-12, f"BalV2 quote err {re}"
    res3, _, v3 = solve_gpu(lib, mb3, 2, 0, gas_price_base=1e-6, base_tx_gas=0.0, max_gas_limit=0.0, budget_ms=100, enable_layer2=1)
    assert v3["ok"] and res3.net_profit > 20.0
    print(f"  [Test 3] Balancer V2 Weighted (80/20 & 50/50) parity err<1e-12 & arb net=${res3.net_profit:.4f} PASS")
    results.append({"name": "balancer_v2_weighted_parity", "net": res3.net_profit, "passed": True})

    # 4. MetricOmm PropAMM parity
    mb4 = MarketBuilder()
    mb4.add_metric_omm(0, 1, 100.0, 55_000.0, sell_human=548.0, buy_human=550.0, num_bins=40, active_bin=0, extra_tax_ppm=0.0)
    mb4.add_metric_omm(0, 1, 100.0, 55_000.0, sell_human=548.0, buy_human=550.0, num_bins=40, active_bin=-2, extra_tax_ppm=0.0)
    q_zero = c_quote_pool(lib, mb4, 0, 0, 1.0)
    q_skew = c_quote_pool(lib, mb4, 1, 0, 1.0)
    pq_zero = quote_pool_py(mb4.pools[0], mb4.tick_sp, mb4.tick_liq, 0, 1.0)
    assert abs(q_zero - pq_zero) / pq_zero < 1e-12
    assert q_skew < q_zero and q_zero < 548.0
    assert c_quote_pool(lib, mb4, 0, 0, 100.0) == 0.0
    mb4_c = MarketBuilder()
    mb4_c.add_metric_omm(0, 1, 100.0, 55_000.0, sell_human=548.0, buy_human=550.0, num_bins=40, active_bin=0, gas=135000.0)
    mb4_c.add_v2(0, 1, 100.0, 56_000.0, fee_ppm=500.0, gas=52000.0)
    res4, _, v4 = solve_gpu(lib, mb4_c, 2, 0, gas_price_base=1e-10, base_tx_gas=0.0, max_gas_limit=0.0, budget_ms=100, enable_layer2=1)
    assert v4["ok"] and res4.net_profit > 0.005
    print(f"  [Test 4] MetricOmm PropAMM parity: q_zero={q_zero:.4f}>q_skew={q_skew:.4f}, arb net={res4.net_profit:.4f} base PASS")
    results.append({"name": "metric_omm_parity", "q_zero": q_zero, "q_skew": q_skew, "net_base": res4.net_profit, "passed": True})

    # 5. 32-Tick Multi-Tick CLMM crossing >= 12 tick boundaries
    mb5 = MarketBuilder()
    n_seg = 32
    bounds = [0.70 * (1.02 ** i) for i in range(n_seg + 1)]
    liqs = [12_000.0 + 1500.0 * (i % 5) for i in range(n_seg)]
    s_cur = 0.5 * (bounds[16] + bounds[17])
    mb5.add_clmm(0, 1, s_cur, bounds, liqs, cur_seg=16, fee_ppm=500.0, gas=92000.0)
    mb5.add_v2(0, 1, 800_000.0, 800_000.0 * (s_cur * s_cur * 1.65), fee_ppm=300.0, gas=52000.0)
    res5, st5, v5 = solve_gpu(lib, mb5, 2, 0, gas_price_base=1e-6, base_tx_gas=0.0, max_gas_limit=0.0, budget_ms=120, enable_layer2=1)
    assert v5["ok"] and res5.net_profit > 100.0
    clmm_step = next(s for s in st5 if s.pool_idx == 0)
    dx_eff = clmm_step.amount_in * 0.9995
    crossed = 0
    if clmm_step.dir == 1:
        rem = dx_eff
        cur_s = s_cur
        for seg in range(16, n_seg):
            max_y = liqs[seg] * (bounds[seg + 1] - cur_s)
            if rem >= max_y:
                rem -= max_y
                cur_s = bounds[seg + 1]
                crossed += 1
            else:
                break
    assert crossed >= 12, f"Expected >=12 ticks crossed, got {crossed}"
    print(f"  [Test 5] 32-Tick CLMM crossed {crossed} tick segments, net=${res5.net_profit:.2f} (rel_err={v5['net_rel_err']:.2e}) PASS")
    results.append({"name": "clmm_multitick_parity", "ticks_crossed": crossed, "net": res5.net_profit, "rel_err": v5["net_rel_err"], "passed": True})

    # 6. 105-pool gas pruning & hard max_gas_limit check
    mb6, nt6, bt6, gp6, _ = build_synthetic_stress_market(seed=777, n_pools=105, n_tokens=10, shock_bps=35.0)
    g6_unlim, _, vg6_u = solve_gpu(lib, mb6, nt6, bt6, gp6, base_tx_gas=65000.0, max_gas_limit=0.0, budget_ms=250, enable_layer2=1)
    g6_cap, _, vg6_c = solve_gpu(lib, mb6, nt6, bt6, gp6, base_tx_gas=65000.0, max_gas_limit=500_000.0, budget_ms=250, enable_layer2=1)
    cg6, _, vcg6 = solve_cpu_greedy(lib, mb6, nt6, bt6, gp6, base_tx_gas=65000.0, max_gas_limit=0.0)
    assert vg6_u["ok"] and vg6_c["ok"] and vcg6["ok"]
    assert g6_unlim.net_profit >= cg6.net_profit * 0.999
    assert g6_cap.total_gas_units <= 500_000.0 + 1e-6 and g6_cap.net_profit > 0.0
    print(f"  [Test 6] Gas pruning & hard 500k cap: Greedy=${cg6.net_profit:.2f}, GPU_unlim=${g6_unlim.net_profit:.2f}, GPU_500kCap=${g6_cap.net_profit:.2f} (gas={g6_cap.total_gas_units:,.0f}) PASS")
    results.append({"name": "gas_pruning_500k_cap", "greedy_net": cg6.net_profit, "gpu_unlim_net": g6_unlim.net_profit, "gpu_500k_net": g6_cap.net_profit, "gpu_500k_gas": g6_cap.total_gas_units, "passed": True})
    return results


def run_bench_a(lib: ctypes.CDLL, sizes: list[tuple[int, int]]) -> list[dict[str, Any]]:
    print("\n=== BENCHMARK A: Resident Unified Memory Incremental Sync & Solve ===")
    rows = []
    for n_pools, dirty in sizes:
        avg_sync_ms = ctypes.c_double(0.0)
        rc = lib.cfmm_benchmark_um_sync(n_pools, dirty, 200, ctypes.byref(avg_sync_ms))
        assert rc == 0

        res_upd_ms = 0.0
        res_solve_50_ms = 0.0
        res_solve_100_ms = 0.0
        res_net = 0.0
        if n_pools >= 500:
            ntok = min(32, max(12, n_pools // 100))
            mb, nt, bt, gp, _ = build_synthetic_stress_market(seed=90 + n_pools, n_pools=n_pools, n_tokens=ntok)
            p_arr, n_p, sp_arr, lq_arr, n_te = mb.to_c_arrays()
            assert lib.cfmm_init_market(p_arr, n_p, sp_arr, lq_arr, n_te, nt) == 0

            d_idxs = [i for i, p in enumerate(mb.pools) if p.pool_type in (POOL_UNIV2, POOL_BALANCER_V2)][:dirty]
            nd = len(d_idxs)
            idx_arr = (ctypes.c_int32 * nd)(*d_idxs)
            upd_pools = []
            for di in d_idxs:
                p_copy = PoolData()
                ctypes.memmove(ctypes.byref(p_copy), ctypes.byref(mb.pools[di]), ctypes.sizeof(PoolData))
                p_copy.r0 *= 1.0035
                upd_pools.append(p_copy)
                mb.pools[di].r0 *= 1.0035
            d_arr = (PoolData * nd)(*upd_pools)

            t0 = time.perf_counter()
            for _ in range(20):
                assert lib.cfmm_update_dirty_pools(idx_arr, d_arr, nd) == 0
            res_upd_ms = (time.perf_counter() - t0) * 1000.0 / 20.0

            steps = (SolverStep * 1024)()
            sr50 = SolveResult()
            assert lib.cfmm_solve_resident(bt, gp, 65000.0, 16_500_000.0, 50, 1, steps, 1024, ctypes.byref(sr50)) == 0
            v50 = verify_solution_py(mb, bt, gp, 65000.0, 16_500_000.0, list(steps[:sr50.num_steps]), sr50)
            assert v50["ok"]
            res_solve_50_ms = float(sr50.total_ms)
            res_net = float(sr50.net_profit)

            sr100 = SolveResult()
            assert lib.cfmm_solve_resident(bt, gp, 65000.0, 16_500_000.0, 100, 1, steps, 1024, ctypes.byref(sr100)) == 0
            v100 = verify_solution_py(mb, bt, gp, 65000.0, 16_500_000.0, list(steps[:sr100.num_steps]), sr100)
            assert v100["ok"]
            res_solve_100_ms = float(sr100.total_ms)

        rows.append({
            "n_pools": n_pools,
            "dirty_pools": dirty,
            "um_micro_ms": float(avg_sync_ms.value),
            "resident_update_ms": res_upd_ms,
            "resident_solve_50ms": res_solve_50_ms,
            "resident_solve_100ms": res_solve_100_ms,
            "resident_net_usd": res_net,
        })
        print(
            f"  M={n_pools:4d} (dirty={dirty:2d}): UM_micro={avg_sync_ms.value:.4f} ms | "
            f"dirty_upd={res_upd_ms:.4f} ms | solve@50={res_solve_50_ms:.2f} ms | solve@100={res_solve_100_ms:.2f} ms (net=${res_net:.2f})"
        )
    return rows


def run_bench_c(
    lib: ctypes.CDLL,
    specs: list[tuple[int, int, int]],
    budgets: list[int],
) -> list[dict[str, Any]]:
    print("\n=== BENCHMARK C: 7-Protocol CPU vs CUDA Scaling Suite ===")
    rows = []
    ref_bud = str(500 if 500 in budgets else budgets[-1])
    for npools, ntoks, seed in specs:
        mb, nt, bt, gp, base_usd = build_synthetic_stress_market(seed, npools, ntoks, shock_bps=32.0)
        cg, _, vcg = solve_cpu_greedy(lib, mb, nt, bt, gp, base_tx_gas=65000.0)
        cd, _, vcd = solve_cpu_dual(lib, mb, nt, bt, gp, base_tx_gas=65000.0, num_restarts=32, num_prune_masks=24)
        assert vcg["ok"] and vcd["ok"]

        anytime = {}
        for bud in budgets:
            gs, _, vg = solve_gpu(lib, mb, nt, bt, gp, base_tx_gas=65000.0, budget_ms=bud, enable_layer2=1)
            assert vg["ok"], f"Verifier failed N={npools} bud={bud}"
            anytime[str(bud)] = fmt_res(f"gpu_{bud}ms", gs, vg, base_usd)

        gref = anytime[ref_bud]
        cpu_ms_per_r = cd.layer1_ms / 32.0
        gpu_ms_per_r = gref["layer1_ms"] / 288.0
        speedup_x = cpu_ms_per_r / max(1e-6, gpu_ms_per_r)

        rows.append({
            "n_pools": npools,
            "n_tokens": ntoks,
            "cpu_greedy": fmt_res("cg", cg, vcg, base_usd),
            "cpu_dual_32r": fmt_res("cd", cd, vcd, base_usd),
            "gpu_anytime": anytime,
            "l1_throughput_speedup_per_restart": round(speedup_x, 2),
        })
        budget_parts = " | ".join(
            f"@{b}=${anytime[str(b)]['net_usd']:8.2f} ({anytime[str(b)]['total_ms']:5.1f}ms, {anytime[str(b)]['steps']:3d}s)"
            for b in budgets
        )
        print(
            f"  N={npools:5d} ({ntoks:2d}t): Greedy=${cg.net_profit:8.2f} ({cg.total_ms:5.1f}ms) | "
            f"CPU_Dual=${cd.net_profit:8.2f} ({cd.total_ms:5.1f}ms) | {budget_parts} | L1-x={speedup_x:.1f}x"
        )
    return rows


def run_bench_d(lib: ctypes.CDLL) -> list[dict[str, Any]]:
    print("\n=== BENCHMARK D: ETH Gas Limit Mega-Packing Suite (5,000 pools, 128 tokens) ===")
    mb, nt, bt, gp, base_usd = build_synthetic_stress_market(
        seed=2026, n_pools=5000, n_tokens=128, shock_bps=48.0, num_shocked_toks=115
    )
    caps = [
        ("2M_TxCap", 2_000_000.0),
        ("16.5M_EIP7825", 16_500_000.0),
        ("30M_BlockCap", 30_000_000.0),
        ("Unlimited", 0.0),
    ]
    rows = []
    for label, cap in caps:
        gs, _, vg = solve_gpu(
            lib, mb, nt, bt, gp,
            base_tx_gas=65000.0, max_gas_limit=cap, budget_ms=500, enable_layer2=1, max_steps=1024,
        )
        assert vg["ok"] and vg["gas_limit_ok"], f"Bench D failed on {label}: {vg}"
        pct = (100.0 * gs.total_gas_units / cap) if cap > 0 else 0.0
        rows.append({
            "label": label,
            "gas_limit": cap,
            "pct_used": round(pct, 1),
            **fmt_res(label, gs, vg, base_usd),
        })
        cap_str = f"{cap:,.0f}" if cap > 0 else "Unlimited"
        pct_str = f"{pct:.1f}%" if cap > 0 else "-"
        print(
            f"  {label:14s} (cap={cap_str:>10s}): net=${gs.net_profit:9.2f} | "
            f"gas={gs.total_gas_units:10,.0f} ({pct_str:>5s}) | steps={gs.num_steps:3d} | "
            f"{gs.total_ms:6.1f} ms | def={vg['max_deficit']:.1e}"
        )
    return rows


def main() -> None:
    parser = argparse.ArgumentParser(description="Pyrite CPU vs CUDA CFMM solver parity tests and benchmark suite")
    parser.add_argument("--lib", help="Path to libcfmm_gpu.so (defaults to $CFMM_GPU_LIB or ./cuda/libcfmm_gpu.so)")
    parser.add_argument("--self-test-only", action="store_true", help="Run only the 6 fp64 parity unit tests")
    parser.add_argument("--quick", action="store_true", help="Run a shorter subset of sizes and budgets")
    parser.add_argument("--json", metavar="PATH", help="Write full benchmark results as JSON to PATH")
    args = parser.parse_args()

    so_path = resolve_lib_path(args.lib)
    lib = load_lib(so_path)

    name_buf = ctypes.create_string_buffer(256)
    sm = ctypes.c_int()
    cmaj = ctypes.c_int()
    cmin = ctypes.c_int()
    man = ctypes.c_int()
    lib.cfmm_get_device_info(name_buf, 256, ctypes.byref(sm), ctypes.byref(cmaj), ctypes.byref(cmin), ctypes.byref(man))
    dev_info = {
        "name": name_buf.value.decode(errors="replace"),
        "compute_cap": f"{cmaj.value}.{cmin.value}",
        "sm_count": sm.value,
        "managed_memory": bool(man.value),
    }
    print(f"GPU Device: {dev_info['name']} (CC {dev_info['compute_cap']}, {dev_info['sm_count']} SMs, Unified Memory={dev_info['managed_memory']})")

    self_tests = run_self_tests(lib)
    report: dict[str, Any] = {
        "device": dev_info,
        "self_tests": self_tests,
    }

    if not args.self_test_only:
        if args.quick:
            a_sizes = [(50, 10), (500, 50), (2000, 50)]
            c_specs = [(300, 12, 101), (1000, 18, 202)]
            c_budgets = [50, 100, 500]
            report["bench_a_resident"] = run_bench_a(lib, a_sizes)
            report["bench_c_scale"] = run_bench_c(lib, c_specs, c_budgets)
        else:
            a_sizes = [(50, 10), (500, 50), (2000, 50), (5000, 50)]
            c_specs = [(300, 12, 101), (1000, 18, 202), (2500, 24, 303), (5000, 32, 404)]
            c_budgets = [50, 100, 500, 2000]
            report["bench_a_resident"] = run_bench_a(lib, a_sizes)
            report["bench_c_scale"] = run_bench_c(lib, c_specs, c_budgets)
            report["bench_d_eth_gas_packing"] = run_bench_d(lib)

    if args.json:
        out_path = Path(args.json).expanduser()
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(report, indent=2) + "\n")
        print(f"\nSaved benchmark JSON to {out_path}")


if __name__ == "__main__":
    main()
