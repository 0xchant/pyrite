//! Optional runtime-loaded CUDA CFMM arbitrage solver (`libcfmm_gpu.so`).
//!
//! Zero CUDA toolkit or NVIDIA driver is required at compile time: the shared
//! library is opened lazily at runtime via `libloading` only when `SOLVER_MODE`
//! is `gpu` or `hybrid`. When unavailable or disabled, routing stays purely on CPU.

use alloy::primitives::Address;
use libloading::Library;
use std::collections::{HashMap, HashSet};
use std::ffi::{c_char, c_int, CStr};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::amm::{
    human_to_u256, metric_edge_haircut, pow10, sqrt_price_x96_to_f64, v3_sqrt_bounds,
    BalancerPoolKind, PoolState,
};
use crate::config::DexKind;
use crate::discovery::MarketState;
use crate::router::{
    compile_dag_steps, estimate_steps_gas, render_mermaid, DirectedFlow, RouteCandidate,
    MAX_DAG_STEPS,
};

pub const MAX_GPU_TOKENS: usize = 256;
pub const MAX_COMPACT_TOKENS: usize = 16;

pub const POOL_UNIV2: i32 = 0;
pub const POOL_CLMM: i32 = 1;
pub const POOL_CURVE_STABLE: i32 = 2;
#[allow(dead_code)]
pub const POOL_PROP_BINS: i32 = 3;
pub const POOL_SOLIDLY_STABLE: i32 = 4;
pub const POOL_BALANCER_V2: i32 = 5;
pub const POOL_METRIC_OMM: i32 = 6;

pub const PROTO_V2: i32 = 0;
pub const PROTO_V3: i32 = 1;
pub const PROTO_CURVE: i32 = 2;
#[allow(dead_code)]
pub const PROTO_PROP_BINS: i32 = 3;
pub const PROTO_SOLIDLY: i32 = 4;
pub const PROTO_BALANCER_V2: i32 = 5;
pub const PROTO_METRIC_OMM: i32 = 6;
pub const PROTO_UNIV4: i32 = 7;
pub const PROTO_INFINITY_CL: i32 = 8;

/// Router execution mode controlled via `SOLVER_MODE=cpu|gpu|hybrid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SolverMode {
    Cpu,
    Gpu,
    Hybrid,
}

/// Parse a `SOLVER_MODE` string, defaulting safely to [`SolverMode::Cpu`].
#[inline]
pub fn parse_solver_mode(raw: &str) -> SolverMode {
    match raw.trim().to_ascii_lowercase().as_str() {
        "gpu" => SolverMode::Gpu,
        "hybrid" => SolverMode::Hybrid,
        _ => SolverMode::Cpu,
    }
}

/// Process-wide solver mode read once from `SOLVER_MODE` (default: `cpu`).
pub fn solver_mode() -> SolverMode {
    static MODE: OnceLock<SolverMode> = OnceLock::new();
    *MODE.get_or_init(|| {
        std::env::var("SOLVER_MODE")
            .map(|v| parse_solver_mode(&v))
            .unwrap_or(SolverMode::Cpu)
    })
}

/// Runtime tuning knobs for GPU solves (`GPU_BUDGET_MS`, `GPU_MAX_GAS_LIMIT`, `GPU_ENABLE_LAYER2`, `GPU_BASE_TX_GAS`, `GPU_GAS_SCALE`).
#[derive(Debug, Clone, Copy)]
pub struct GpuConfig {
    pub budget_ms: i32,
    pub max_gas_limit: f64,
    pub enable_layer2: bool,
    pub base_tx_gas: f64,
    pub gas_scale: f64,
}

impl GpuConfig {
    pub fn from_env(block_time_ms: u64) -> Self {
        let default_budget = if block_time_ms <= 2_000 { 35 } else { 120 };
        let budget_ms = std::env::var("GPU_BUDGET_MS")
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .map(|v| v.clamp(5, 5_000))
            .unwrap_or(default_budget);
        let max_gas_limit = std::env::var("GPU_MAX_GAS_LIMIT")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v.max(250_000.0))
            .unwrap_or(3_200_000.0);
        let enable_layer2 = std::env::var("GPU_ENABLE_LAYER2")
            .ok()
            .map(|s| !matches!(s.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no"))
            .unwrap_or(true);
        let base_tx_gas = std::env::var("GPU_BASE_TX_GAS")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v.clamp(21_000.0, 500_000.0))
            .unwrap_or(65_000.0);
        let gas_scale = std::env::var("GPU_GAS_SCALE")
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
            .map(|v| v.clamp(0.25, 4.0))
            .unwrap_or(1.0);
        Self {
            budget_ms,
            max_gas_limit,
            enable_layer2,
            base_tx_gas,
            gas_scale,
        }
    }
}

/// Exact C ABI mirror of `struct PoolData` (104 bytes) in `cuda/cfmm_solver.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuPoolData {
    pub token0: i32,
    pub token1: i32,
    pub pool_type: i32,
    pub protocol_id: i32,
    pub tick_offset: i32,
    pub tick_count: i32,
    pub cur_seg: i32,
    pub _pad: i32,
    pub gas_units: f64,
    pub fee_ppm: f64,
    pub r0: f64,
    pub r1: f64,
    pub sqrt_p: f64,
    pub liq: f64,
    pub param0: f64,
    pub param1: f64,
    pub param2: f64,
}

impl GpuPoolData {
    /// Bit-exact equality for dirty-pool diffing against resident GPU state.
    #[inline]
    fn bits_eq(&self, other: &Self) -> bool {
        self.token0 == other.token0
            && self.token1 == other.token1
            && self.pool_type == other.pool_type
            && self.protocol_id == other.protocol_id
            && self.tick_offset == other.tick_offset
            && self.tick_count == other.tick_count
            && self.cur_seg == other.cur_seg
            && self.gas_units.to_bits() == other.gas_units.to_bits()
            && self.fee_ppm.to_bits() == other.fee_ppm.to_bits()
            && self.r0.to_bits() == other.r0.to_bits()
            && self.r1.to_bits() == other.r1.to_bits()
            && self.sqrt_p.to_bits() == other.sqrt_p.to_bits()
            && self.liq.to_bits() == other.liq.to_bits()
            && self.param0.to_bits() == other.param0.to_bits()
            && self.param1.to_bits() == other.param1.to_bits()
            && self.param2.to_bits() == other.param2.to_bits()
    }
}

/// Exact C ABI mirror of `struct SolverStep` (32 bytes) in `cuda/cfmm_solver.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuSolverStep {
    pub pool_idx: i32,
    pub dir: i32,
    pub amount_in: f64,
    pub amount_out: f64,
    pub gas_units: f64,
}

/// Exact C ABI mirror of `struct SolveResult` (112 bytes) in `cuda/cfmm_solver.h`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct GpuSolveResult {
    pub num_steps: i32,
    pub max_steps_cap: i32,
    pub borrow_amount: f64,
    pub gross_profit: f64,
    pub gas_cost_base: f64,
    pub net_profit: f64,
    pub total_gas_units: f64,
    pub max_deficit: f64,
    pub layer1_ms: f64,
    pub layer2_ms: f64,
    pub total_ms: f64,
    pub l1_unpruned_gross: f64,
    pub l1_unpruned_gas_cost: f64,
    pub l1_unpruned_net: f64,
    pub l1_unpruned_steps: i32,
    pub _reserved: i32,
}

type FnGetDeviceInfo =
    unsafe extern "C" fn(*mut c_char, c_int, *mut c_int, *mut c_int, *mut c_int, *mut c_int) -> c_int;
type FnInitMarket =
    unsafe extern "C" fn(*const GpuPoolData, i32, *const f64, *const f64, i32, i32) -> c_int;
type FnUpdateDirtyPools = unsafe extern "C" fn(*const i32, *const GpuPoolData, i32) -> c_int;
type FnSolveResident = unsafe extern "C" fn(
    i32,
    f64,
    f64,
    f64,
    i32,
    i32,
    *mut GpuSolverStep,
    i32,
    *mut GpuSolveResult,
) -> c_int;
type FnQuoteSingle = unsafe extern "C" fn(*const GpuPoolData, *const f64, *const f64, i32, f64) -> f64;

/// Dynamically loaded CUDA library handle and resolved C ABI entrypoints.
pub struct GpuRuntime {
    _lib: Library,
    get_device_info: FnGetDeviceInfo,
    init_market: FnInitMarket,
    update_dirty_pools: FnUpdateDirtyPools,
    solve_resident: FnSolveResident,
    #[allow(dead_code)]
    quote_single: FnQuoteSingle,
    pub library_path: String,
    pub device_name: String,
    pub sm_count: i32,
    pub cap_major: i32,
    pub cap_minor: i32,
    pub managed_memory: bool,
}

// `GpuSession` serializes all access to the process-global CUDA workspace via a `Mutex`.
unsafe impl Send for GpuRuntime {}
unsafe impl Sync for GpuRuntime {}

impl GpuRuntime {
    /// Load `libcfmm_gpu.so` from an explicit filesystem path or dynamic linker name.
    pub fn load_from_path(path: impl AsRef<Path>) -> Option<Self> {
        let p = path.as_ref();
        unsafe {
            let lib = Library::new(p).ok()?;
            let get_device_info: FnGetDeviceInfo = *lib.get(b"cfmm_get_device_info\0").ok()?;
            let init_market: FnInitMarket = *lib.get(b"cfmm_init_market\0").ok()?;
            let update_dirty_pools: FnUpdateDirtyPools =
                *lib.get(b"cfmm_update_dirty_pools\0").ok()?;
            let solve_resident: FnSolveResident = *lib.get(b"cfmm_solve_resident\0").ok()?;
            let quote_single: FnQuoteSingle = *lib.get(b"cfmm_quote_single\0").ok()?;

            let mut name_buf = [0 as c_char; 128];
            let (mut sm, mut maj, mut min, mut managed) = (0, 0, 0, 0);
            let rc = get_device_info(
                name_buf.as_mut_ptr(),
                name_buf.len() as c_int,
                &mut sm,
                &mut maj,
                &mut min,
                &mut managed,
            );
            if rc != 0 {
                return None;
            }
            let device_name = CStr::from_ptr(name_buf.as_ptr())
                .to_string_lossy()
                .into_owned();
            Some(Self {
                _lib: lib,
                get_device_info,
                init_market,
                update_dirty_pools,
                solve_resident,
                quote_single,
                library_path: p.display().to_string(),
                device_name,
                sm_count: sm,
                cap_major: maj,
                cap_minor: min,
                managed_memory: managed != 0,
            })
        }
    }

    fn discover_and_load() -> Option<Self> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Ok(explicit) = std::env::var("CFMM_GPU_LIB") {
            let trimmed = explicit.trim();
            if !trimmed.is_empty() {
                candidates.push(PathBuf::from(trimmed));
            }
        }
        candidates.push(PathBuf::from("cuda/libcfmm_gpu.so"));
        candidates.push(PathBuf::from("libcfmm_gpu.so"));
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                candidates.push(dir.join("cuda").join("libcfmm_gpu.so"));
                candidates.push(dir.join("libcfmm_gpu.so"));
            }
        }
        candidates.push(PathBuf::from("libcfmm_gpu.so"));

        let mut tried: Vec<PathBuf> = Vec::new();
        for c in candidates {
            if tried.contains(&c) {
                continue;
            }
            // Prefer probing existing files for relative/explicit paths before dynamic linker lookup
            let is_bare_soname = c.components().count() == 1;
            if !is_bare_soname && !c.is_file() {
                tried.push(c);
                continue;
            }
            if let Some(rt) = Self::load_from_path(&c) {
                let _ = rt.get_device_info;
                tracing::info!(
                    lib = %rt.library_path,
                    gpu = %rt.device_name,
                    sm = rt.sm_count,
                    cc = format!("{}.{}", rt.cap_major, rt.cap_minor),
                    managed = rt.managed_memory,
                    "loaded optional CUDA CFMM solver runtime"
                );
                return Some(rt);
            }
            tried.push(c);
        }

        tracing::warn!(
            mode = ?solver_mode(),
            "libcfmm_gpu.so unavailable; falling back to CPU router"
        );
        None
    }
}

#[derive(Default)]
struct GpuSessionCache {
    pool_addrs: Vec<Address>,
    token_addrs: Vec<Address>,
    last_pools: Vec<GpuPoolData>,
    last_tick_sp: Vec<f64>,
    last_tick_liq: Vec<f64>,
}

struct GpuBackend {
    runtime: GpuRuntime,
    session: Mutex<GpuSessionCache>,
}

fn gpu_backend() -> Option<&'static GpuBackend> {
    static BACKEND: OnceLock<Option<GpuBackend>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            GpuRuntime::discover_and_load().map(|runtime| GpuBackend {
                runtime,
                session: Mutex::new(GpuSessionCache::default()),
            })
        })
        .as_ref()
}

/// Returns `true` when `libcfmm_gpu.so` is loaded and ready for solving.
pub fn is_available() -> bool {
    gpu_backend().is_some()
}

#[inline]
fn pool_swap_gas_units(kind: DexKind) -> f64 {
    match kind {
        DexKind::UniV2 => 52_000.0,
        DexKind::Solidly => 82_000.0,
        DexKind::UniV3 | DexKind::PancakeV3 => 92_000.0,
        DexKind::InfinityCl | DexKind::UniV4 => 125_000.0,
        DexKind::MetricOmm => 135_000.0,
        DexKind::BalancerV2 => 115_000.0,
    }
}

/// Convert a Rust [`PoolState`] into CUDA [`GpuPoolData`] plus optional CSR tick bounds.
///
/// Unsupported or future protocol variants (and 3+-asset / rate-scaled Balancer Stable pools)
/// return `None` cleanly so CPU routers continue handling them.
#[allow(dead_code)]
pub fn pool_state_to_gpu(
    pool: &PoolState,
    token0_idx: i32,
    token1_idx: i32,
    tick_sp: &mut Vec<f64>,
    tick_liq: &mut Vec<f64>,
) -> Option<GpuPoolData> {
    pool_state_to_gpu_with_gas_scale(pool, token0_idx, token1_idx, 1.0, tick_sp, tick_liq)
}

/// Convert a [`PoolState`] to dense [`GpuPoolData`] with an execution gas multiplier.
pub fn pool_state_to_gpu_with_gas_scale(
    pool: &PoolState,
    token0_idx: i32,
    token1_idx: i32,
    gas_scale: f64,
    tick_sp: &mut Vec<f64>,
    tick_liq: &mut Vec<f64>,
) -> Option<GpuPoolData> {
    if !pool.is_active() || token0_idx < 0 || token1_idx < 0 || token0_idx == token1_idx {
        return None;
    }

    let scale = if gas_scale.is_finite() && gas_scale > 0.0 {
        gas_scale.clamp(0.25, 4.0)
    } else {
        1.0
    };
    let gas_units = pool_swap_gas_units(pool.dex_kind) * scale;
    let tax_mult = 1.0 - (pool.extra_tax_ppm.min(999_999) as f64) / 1_000_000.0;
    let combined_fee_ppm = pool.fee_ppm.saturating_add(pool.extra_tax_ppm).min(999_999) as f64;

    match pool.dex_kind {
        DexKind::UniV2 => {
            let (r0, r1) = pool.human_reserves();
            if r0 <= 1e-12 || r1 <= 1e-12 || !r0.is_finite() || !r1.is_finite() {
                return None;
            }
            let gamma = (tax_mult * (pool.fee_num as f64) / 10_000.0).clamp(1e-6, 1.0);
            let fee_ppm = ((1.0 - gamma) * 1_000_000.0).clamp(0.0, 999_999.0);
            Some(GpuPoolData {
                token0: token0_idx,
                token1: token1_idx,
                pool_type: POOL_UNIV2,
                protocol_id: PROTO_V2,
                gas_units,
                fee_ppm,
                r0,
                r1,
                ..Default::default()
            })
        }
        DexKind::Solidly => {
            let (r0, r1) = pool.human_reserves();
            if r0 <= 1e-12 || r1 <= 1e-12 || !r0.is_finite() || !r1.is_finite() {
                return None;
            }
            Some(GpuPoolData {
                token0: token0_idx,
                token1: token1_idx,
                pool_type: if pool.stable {
                    POOL_SOLIDLY_STABLE
                } else {
                    POOL_UNIV2
                },
                protocol_id: PROTO_SOLIDLY,
                gas_units,
                fee_ppm: combined_fee_ppm,
                r0,
                r1,
                ..Default::default()
            })
        }
        DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
            let s_raw = sqrt_price_x96_to_f64(pool.sqrt_price_x96);
            let l_raw = pool.liquidity as f64;
            if s_raw <= 0.0 || l_raw <= 0.0 || !s_raw.is_finite() || !l_raw.is_finite() {
                return None;
            }
            let (s_lo_raw, s_hi_raw) =
                v3_sqrt_bounds(s_raw, pool.tick, pool.tick_spacing, pool.allow_multi_tick);
            let scale0 = pow10(pool.dec0);
            let scale1 = pow10(pool.dec1);
            let dec_sqrt = (scale0 / scale1).sqrt();
            let liq_scale = (scale0 * scale1).sqrt();
            if !dec_sqrt.is_finite() || !liq_scale.is_finite() || liq_scale <= 0.0 {
                return None;
            }
            let sqrt_p_h = s_raw * dec_sqrt;
            let s_lo_h = s_lo_raw * dec_sqrt;
            let s_hi_h = s_hi_raw * dec_sqrt;
            let liq_h = l_raw / liq_scale;
            if !sqrt_p_h.is_finite()
                || sqrt_p_h <= 0.0
                || !s_lo_h.is_finite()
                || !s_hi_h.is_finite()
                || s_lo_h <= 0.0
                || s_hi_h <= s_lo_h
                || !liq_h.is_finite()
                || liq_h <= 0.0
            {
                return None;
            }
            let protocol_id = match pool.dex_kind {
                DexKind::UniV4 => PROTO_UNIV4,
                DexKind::InfinityCl => PROTO_INFINITY_CL,
                _ => PROTO_V3,
            };
            let tick_offset = i32::try_from(tick_sp.len()).ok()?;
            tick_sp.push(s_lo_h);
            tick_sp.push(s_hi_h);
            tick_liq.push(liq_h);
            tick_liq.push(0.0);

            Some(GpuPoolData {
                token0: token0_idx,
                token1: token1_idx,
                pool_type: POOL_CLMM,
                protocol_id,
                tick_offset,
                tick_count: 1,
                cur_seg: 0,
                gas_units,
                fee_ppm: combined_fee_ppm,
                r0: liq_h / sqrt_p_h,
                r1: liq_h * sqrt_p_h,
                sqrt_p: sqrt_p_h,
                liq: liq_h,
                ..Default::default()
            })
        }
        DexKind::MetricOmm => {
            let m = pool.metric.as_ref()?;
            if m.paused || m.sell_price_x64 == 0 || m.buy_price_x64 < m.sell_price_x64 {
                return None;
            }
            let (r0, r1) = pool.human_reserves();
            if r0 <= 1e-12 && r1 <= 1e-12 {
                return None;
            }
            const Q64_F64: f64 = 18446744073709551616.0_f64;
            let sell_h = (m.sell_price_x64 as f64) / Q64_F64;
            let buy_h = (m.buy_price_x64 as f64) / Q64_F64;
            if sell_h <= 0.0 || !sell_h.is_finite() || !buy_h.is_finite() || buy_h < sell_h {
                return None;
            }
            let spr_walk = ((buy_h - sell_h) / sell_h).clamp(0.00005, 0.02);
            let half_bins = (m.num_bins.max(2) as f64) * 0.5;
            let max_walk = (spr_walk * half_bins * 0.5).clamp(0.0005, 0.05);
            let hc0 = metric_edge_haircut(sell_h, buy_h, m.active_bin, true);
            let hc1 = metric_edge_haircut(sell_h, buy_h, m.active_bin, false);
            // `src/amm.rs` applies `haircut + extra_tax_ppm` (never `pool.fee_ppm`) on MetricOmm.
            let extra_ppm = pool.extra_tax_ppm.min(999_999) as f64;
            Some(GpuPoolData {
                token0: token0_idx,
                token1: token1_idx,
                pool_type: POOL_METRIC_OMM,
                protocol_id: PROTO_METRIC_OMM,
                gas_units,
                fee_ppm: extra_ppm,
                r0,
                r1,
                sqrt_p: sell_h,
                liq: buy_h,
                param0: max_walk,
                param1: hc0,
                param2: hc1,
                ..Default::default()
            })
        }
        DexKind::BalancerV2 => {
            let bal = pool.balancer.as_ref()?;
            let (r0, r1) = pool.human_reserves();
            if r0 <= 1e-12 || r1 <= 1e-12 || !r0.is_finite() || !r1.is_finite() {
                return None;
            }
            match &bal.kind {
                BalancerPoolKind::Weighted { w0, w1 } if *w0 > 1e-9 && *w1 > 1e-9 => {
                    Some(GpuPoolData {
                        token0: token0_idx,
                        token1: token1_idx,
                        pool_type: POOL_BALANCER_V2,
                        protocol_id: PROTO_BALANCER_V2,
                        gas_units,
                        fee_ppm: combined_fee_ppm,
                        r0,
                        r1,
                        liq: *w0,
                        param0: *w1,
                        ..Default::default()
                    })
                }
                BalancerPoolKind::Stable {
                    amp,
                    scale0,
                    scale1,
                    other_scaled_balances,
                } if other_scaled_balances.is_empty()
                    && (*scale0 - 1.0).abs() < 1e-9
                    && (*scale1 - 1.0).abs() < 1e-9
                    && *amp > 0.0 =>
                {
                    Some(GpuPoolData {
                        token0: token0_idx,
                        token1: token1_idx,
                        pool_type: POOL_CURVE_STABLE,
                        protocol_id: PROTO_CURVE,
                        gas_units,
                        fee_ppm: combined_fee_ppm,
                        r0,
                        r1,
                        param0: *amp,
                        ..Default::default()
                    })
                }
                // Composable/multi-asset or rate-scaled Balancer Stable pools remain CPU-only.
                _ => None,
            }
        }
    }
}

/// Prepared dense GPU market batch bounded by [`MAX_GPU_TOKENS`] with deterministic ordering.
struct GpuMarketBatch<'a> {
    selected_pools: Vec<&'a PoolState>,
    pool_addrs: Vec<Address>,
    token_addrs: Vec<Address>,
    token_indices: HashMap<Address, i32>,
    gpu_pools: Vec<GpuPoolData>,
    tick_sp: Vec<f64>,
    tick_liq: Vec<f64>,
}

/// Build a `<= MAX_GPU_TOKENS` resident market batch prioritizing base anchors, hub pairs, and dirty pools.
#[cfg_attr(not(test), allow(dead_code))]
fn build_gpu_market_batch<'a>(
    market: &MarketState,
    routable_pools: &[&'a PoolState],
    dirty_pools: Option<&HashSet<Address>>,
) -> GpuMarketBatch<'a> {
    build_gpu_market_batch_with_gas_scale(market, routable_pools, dirty_pools, 1.0)
}

/// Build a dense [`GpuMarketBatch`] applying `gas_scale` to per-pool swap gas units.
fn build_gpu_market_batch_with_gas_scale<'a>(
    market: &MarketState,
    routable_pools: &[&'a PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    gas_scale: f64,
) -> GpuMarketBatch<'a> {
    let mut ranked: Vec<&'a PoolState> = routable_pools
        .iter()
        .copied()
        .filter(|p| is_pool_gpu_eligible(p))
        .collect();

    ranked.sort_by(|a, b| {
        let a_dirty = dirty_pools.is_some_and(|dp| dp.contains(&a.address));
        let b_dirty = dirty_pools.is_some_and(|dp| dp.contains(&b.address));
        let a_hub_pair = a.is_hub_0 && a.is_hub_1;
        let b_hub_pair = b.is_hub_0 && b.is_hub_1;
        b_hub_pair
            .cmp(&a_hub_pair)
            .then_with(|| b_dirty.cmp(&a_dirty))
            .then_with(|| {
                b.cached_depth_usd
                    .partial_cmp(&a.cached_depth_usd)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.address.cmp(&b.address))
    });

    let mut token_indices: HashMap<Address, i32> = HashMap::with_capacity(MAX_GPU_TOKENS);
    let mut token_addrs: Vec<Address> = Vec::with_capacity(MAX_GPU_TOKENS);
    for anchor in market.base_anchors() {
        if token_addrs.len() >= MAX_GPU_TOKENS {
            break;
        }
        if !token_indices.contains_key(&anchor) {
            let id = token_addrs.len() as i32;
            token_indices.insert(anchor, id);
            token_addrs.push(anchor);
        }
    }
    for hub in &market.chain.hub_tokens {
        if token_addrs.len() >= MAX_GPU_TOKENS {
            break;
        }
        if !token_indices.contains_key(&hub.address) {
            let id = token_addrs.len() as i32;
            token_indices.insert(hub.address, id);
            token_addrs.push(hub.address);
        }
    }

    let mut chosen: Vec<&'a PoolState> = Vec::with_capacity(ranked.len());
    for p in ranked {
        if p.token0 == p.token1 {
            continue;
        }
        let need0 = usize::from(!token_indices.contains_key(&p.token0));
        let need1 = usize::from(!token_indices.contains_key(&p.token1));
        if token_addrs.len() + need0 + need1 > MAX_GPU_TOKENS {
            continue;
        }
        if need0 == 1 {
            let id = token_addrs.len() as i32;
            token_indices.insert(p.token0, id);
            token_addrs.push(p.token0);
        }
        if need1 == 1 {
            let id = token_addrs.len() as i32;
            token_indices.insert(p.token1, id);
            token_addrs.push(p.token1);
        }
        chosen.push(p);
    }

    // Sort chosen pools stably by address so identical topology yields identical indices
    // across blocks and enables `cfmm_update_dirty_pools` instead of re-initializing CUDA state.
    chosen.sort_by_key(|p| p.address);

    let mut selected_pools: Vec<&'a PoolState> = Vec::with_capacity(chosen.len());
    let mut pool_addrs: Vec<Address> = Vec::with_capacity(chosen.len());
    let mut gpu_pools: Vec<GpuPoolData> = Vec::with_capacity(chosen.len());
    let mut tick_sp: Vec<f64> = Vec::with_capacity(chosen.len() * 2);
    let mut tick_liq: Vec<f64> = Vec::with_capacity(chosen.len() * 2);

    for p in chosen {
        let Some(&t0) = token_indices.get(&p.token0) else {
            continue;
        };
        let Some(&t1) = token_indices.get(&p.token1) else {
            continue;
        };
        if let Some(gp) =
            pool_state_to_gpu_with_gas_scale(p, t0, t1, gas_scale, &mut tick_sp, &mut tick_liq)
        {
            selected_pools.push(p);
            pool_addrs.push(p.address);
            gpu_pools.push(gp);
        }
    }

    GpuMarketBatch {
        selected_pools,
        pool_addrs,
        token_addrs,
        token_indices,
        gpu_pools,
        tick_sp,
        tick_liq,
    }
}

#[inline]
fn is_pool_gpu_eligible(pool: &PoolState) -> bool {
    if !pool.is_active() {
        return false;
    }
    match pool.dex_kind {
        DexKind::BalancerV2 => match pool.balancer.as_ref().map(|b| &b.kind) {
            Some(BalancerPoolKind::Weighted { w0, w1 }) => *w0 > 1e-9 && *w1 > 1e-9,
            Some(BalancerPoolKind::Stable {
                amp,
                scale0,
                scale1,
                other_scaled_balances,
            }) => {
                other_scaled_balances.is_empty()
                    && (*scale0 - 1.0).abs() < 1e-9
                    && (*scale1 - 1.0).abs() < 1e-9
                    && *amp > 0.0
            }
            None => false,
        },
        _ => true,
    }
}

/// Convert raw CUDA solver steps into a validated [`RouteCandidate`] ready for [`crate::router::verify_and_refine_candidate`].
pub fn gpu_steps_to_candidate(
    market: &MarketState,
    selected_pools: &[&PoolState],
    base_token: Address,
    res: &GpuSolveResult,
    steps: &[GpuSolverStep],
    min_profit_usd: f64,
) -> Option<RouteCandidate> {
    if steps.len() < 2
        || steps.len() > MAX_DAG_STEPS
        || !res.borrow_amount.is_finite()
        || res.borrow_amount <= 0.0
        || !res.gross_profit.is_finite()
        || !res.max_deficit.is_finite()
        || res.max_deficit > 1e-6
    {
        return None;
    }

    let base_usd = market.token_usd(base_token);
    if base_usd <= 0.0 || !base_usd.is_finite() {
        return None;
    }

    let mut flows: Vec<DirectedFlow> = Vec::with_capacity(steps.len());
    let mut used_pools: Vec<Address> = Vec::with_capacity(steps.len());

    for s in steps {
        let p_idx = usize::try_from(s.pool_idx).ok()?;
        let pool = *selected_pools.get(p_idx)?;
        if !s.amount_in.is_finite()
            || !s.amount_out.is_finite()
            || s.amount_in <= 0.0
            || s.amount_out <= 0.0
        {
            return None;
        }
        let (token_in, token_out) = match s.dir {
            0 => (pool.token0, pool.token1),
            1 => (pool.token1, pool.token0),
            _ => return None,
        };
        used_pools.push(pool.address);
        flows.push(DirectedFlow {
            pool: pool.address,
            label: format!(
                "{}:{}->{}",
                pool.factory_name,
                market.token_symbol(token_in),
                market.token_symbol(token_out)
            ),
            token_in,
            token_out,
            dex_kind: pool.dex_kind,
            fee_num: pool.fee_num,
            flow_in_human: s.amount_in,
            flow_out_human: s.amount_out,
        });
    }

    let virtual_exec = Address::repeat_byte(0xEE);
    let dag_steps = compile_dag_steps(&flows, base_token, virtual_exec)?;
    if dag_steps.len() > MAX_DAG_STEPS {
        return None;
    }
    let mut unique_tokens: HashSet<Address> = HashSet::with_capacity(MAX_COMPACT_TOKENS + 1);
    unique_tokens.insert(base_token);
    for step in &dag_steps {
        unique_tokens.insert(step.tokenIn);
        unique_tokens.insert(step.tokenOut);
    }
    if unique_tokens.len() > MAX_COMPACT_TOKENS {
        return None;
    }
    let est_gas = estimate_steps_gas(&dag_steps);

    let base_decimals = market.token_decimals(base_token);
    let borrow_u256 = human_to_u256(res.borrow_amount, base_decimals);
    if borrow_u256.is_zero() || u128::try_from(borrow_u256).is_err() {
        return None;
    }
    let flash = market.select_flash_source(base_token, borrow_u256, &used_pools)?;

    // Root flash swap (`execution_flash_source`) pays 0 external flash fee when steps[0] qualifies.
    let qualifies_root_flash = dag_steps.len() >= 2
        && dag_steps[0].tokenIn == base_token
        && dag_steps[0].shareBps == 10_000
        && !dag_steps[1..].iter().any(|s| s.pool == dag_steps[0].pool)
        && ((dag_steps[0].dexKind == DexKind::MetricOmm.as_u8())
            || ((dag_steps[0].dexKind == DexKind::UniV3.as_u8()
                || dag_steps[0].dexKind == DexKind::PancakeV3.as_u8()
                || (dag_steps[0].dexKind == DexKind::UniV2.as_u8()
                    && dag_steps[0].recipient == virtual_exec))
                && market.pools.get(&dag_steps[0].pool).is_some_and(|p| {
                    market.chain.factory_supports_flash(p.factory_name.as_str())
                })));

    let flash_fee_h = if qualifies_root_flash {
        0.0
    } else {
        res.borrow_amount * (flash.fee_bps / 10_000.0)
    };
    let gross_h = res.gross_profit - flash_fee_h;
    if gross_h <= 0.0 || !gross_h.is_finite() {
        return None;
    }
    let gross_raw = human_to_u256(gross_h, base_decimals);
    if u128::try_from(gross_raw).is_err() {
        return None;
    }

    let gross_usd = gross_h * base_usd;
    let gas_usd = market.estimate_gas_cost_usd(est_gas, 0);
    let net_usd = gross_usd - gas_usd;
    if net_usd < min_profit_usd {
        return None;
    }

    let mermaid = render_mermaid(market, base_token, &flows);
    Some(RouteCandidate {
        strategy: format!("GPU_CFMM({}steps)", dag_steps.len()),
        base_token,
        borrow_human: res.borrow_amount,
        est_gross_profit_human: gross_h,
        est_gross_profit_usd: gross_usd,
        est_net_profit_usd: net_usd,
        flash_mode: flash.flash_mode,
        flash_source: flash.pool,
        flash_fee_bps: flash.fee_bps,
        flows,
        mermaid,
    })
}

/// Solve multi-protocol CFMM arbitrage on GPU across up to 3 base anchors.
///
/// Returns an empty vector cleanly whenever `libcfmm_gpu.so` is not available
/// or `market` lacks GPU-eligible pools.
pub fn solve_gpu(
    market: &MarketState,
    routable_pools: &[&PoolState],
    dirty_pools: Option<&HashSet<Address>>,
    min_profit_usd: f64,
) -> Vec<RouteCandidate> {
    if routable_pools.len() < 2 {
        return Vec::new();
    }
    let Some(backend) = gpu_backend() else {
        return Vec::new();
    };
    if market.native_usd <= 0.0 || !market.native_usd.is_finite() {
        return Vec::new();
    }

    let cfg = GpuConfig::from_env(market.chain.block_time_ms);
    let batch =
        build_gpu_market_batch_with_gas_scale(market, routable_pools, dirty_pools, cfg.gas_scale);
    if batch.gpu_pools.len() < 2 || batch.token_addrs.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();

    let Ok(mut cache) = backend.session.lock() else {
        tracing::warn!("GPU session mutex poisoned; skipping GPU solve");
        return Vec::new();
    };

    // Synchronize resident GPU memory via incremental dirty diff or full market init.
    let topology_same = cache.pool_addrs == batch.pool_addrs
        && cache.token_addrs == batch.token_addrs
        && cache.last_tick_sp == batch.tick_sp
        && cache.last_tick_liq == batch.tick_liq
        && cache.last_pools.len() == batch.gpu_pools.len();

    let sync_ok = if topology_same {
        let mut dirty_indices: Vec<i32> = Vec::new();
        let mut dirty_vals: Vec<GpuPoolData> = Vec::new();
        for (idx, (prev, cur)) in cache
            .last_pools
            .iter()
            .zip(batch.gpu_pools.iter())
            .enumerate()
        {
            if !prev.bits_eq(cur) {
                dirty_indices.push(idx as i32);
                dirty_vals.push(*cur);
            }
        }
        if dirty_indices.is_empty() {
            true
        } else {
            let rc = unsafe {
                (backend.runtime.update_dirty_pools)(
                    dirty_indices.as_ptr(),
                    dirty_vals.as_ptr(),
                    dirty_indices.len() as i32,
                )
            };
            if rc == 0 {
                cache.last_pools = batch.gpu_pools.clone();
                true
            } else {
                false
            }
        }
    } else {
        false
    };

    if !sync_ok {
        let rc = unsafe {
            (backend.runtime.init_market)(
                batch.gpu_pools.as_ptr(),
                batch.gpu_pools.len() as i32,
                if batch.tick_sp.is_empty() {
                    std::ptr::null()
                } else {
                    batch.tick_sp.as_ptr()
                },
                if batch.tick_liq.is_empty() {
                    std::ptr::null()
                } else {
                    batch.tick_liq.as_ptr()
                },
                batch.tick_sp.len() as i32,
                batch.token_addrs.len() as i32,
            )
        };
        if rc != 0 {
            tracing::warn!(rc, "cfmm_init_market failed; skipping GPU solve");
            *cache = GpuSessionCache::default();
            return Vec::new();
        }
        cache.pool_addrs = batch.pool_addrs;
        cache.token_addrs = batch.token_addrs;
        cache.last_pools = batch.gpu_pools;
        cache.last_tick_sp = batch.tick_sp;
        cache.last_tick_liq = batch.tick_liq;
    }

    let mut steps_buf = [GpuSolverStep::default(); MAX_DAG_STEPS];
    for base_token in market.base_anchors().into_iter().take(3) {
        let Some(&base_tok_idx) = batch.token_indices.get(&base_token) else {
            continue;
        };
        let base_usd = market.token_usd(base_token);
        if base_usd <= 0.0 || !base_usd.is_finite() {
            continue;
        }
        let gas_price_base =
            ((market.gas_price_wei as f64) / 1e18) * market.native_usd / base_usd;
        if !gas_price_base.is_finite() || gas_price_base < 0.0 {
            continue;
        }

        let mut res = GpuSolveResult::default();
        let rc = unsafe {
            (backend.runtime.solve_resident)(
                base_tok_idx,
                gas_price_base,
                cfg.base_tx_gas,
                cfg.max_gas_limit,
                cfg.budget_ms,
                i32::from(cfg.enable_layer2),
                steps_buf.as_mut_ptr(),
                MAX_DAG_STEPS as i32,
                &mut res,
            )
        };
        if rc != 0 || res.num_steps < 2 {
            continue;
        }
        let n_steps = (res.num_steps as usize).min(MAX_DAG_STEPS);
        if let Some(cand) = gpu_steps_to_candidate(
            market,
            &batch.selected_pools,
            base_token,
            &res,
            &steps_buf[..n_steps],
            min_profit_usd,
        ) {
            out.push(cand);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::amm::{human_to_u256, BalancerPoolState, MetricPoolState, V4PoolKey};
    use crate::config::load_chain_config;
    use alloy::primitives::{B256, U256};

    #[test]
    fn test_gpu_ffi_struct_layout_matches_c_header() {
        assert_eq!(std::mem::size_of::<GpuPoolData>(), 104);
        assert_eq!(std::mem::align_of::<GpuPoolData>(), 8);
        assert_eq!(std::mem::size_of::<GpuSolverStep>(), 32);
        assert_eq!(std::mem::align_of::<GpuSolverStep>(), 8);
        assert_eq!(std::mem::size_of::<GpuSolveResult>(), 112);
        assert_eq!(std::mem::align_of::<GpuSolveResult>(), 8);
    }

    #[test]
    fn test_solver_mode_parsing() {
        assert_eq!(parse_solver_mode("cpu"), SolverMode::Cpu);
        assert_eq!(parse_solver_mode("GPU"), SolverMode::Gpu);
        assert_eq!(parse_solver_mode(" Hybrid "), SolverMode::Hybrid);
        assert_eq!(parse_solver_mode(""), SolverMode::Cpu);
        assert_eq!(parse_solver_mode("unknown"), SolverMode::Cpu);
    }

    #[test]
    fn test_missing_library_graceful_fallback() {
        assert!(GpuRuntime::load_from_path("/nonexistent/path/to/libcfmm_gpu.so").is_none());
    }

    #[test]
    fn test_pool_conversion_all_supported_protocols() {
        let t0 = Address::repeat_byte(0x11);
        let t1 = Address::repeat_byte(0x22);
        let mut tick_sp = Vec::new();
        let mut tick_liq = Vec::new();

        // 1. UniV2
        let v2 = PoolState {
            address: Address::repeat_byte(0xa1),
            factory_name: "UniswapV2".into(),
            dex_kind: DexKind::UniV2,
            token0: t0,
            token1: t1,
            dec0: 18,
            dec1: 6,
            fee_num: 9970,
            fee_ppm: 3000,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: human_to_u256(100.0, 18),
            r1_raw: human_to_u256(300_000.0, 6),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 600_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        let gp_v2 = pool_state_to_gpu(&v2, 0, 1, &mut tick_sp, &mut tick_liq).unwrap();
        assert_eq!(gp_v2.pool_type, POOL_UNIV2);
        assert_eq!(gp_v2.protocol_id, PROTO_V2);
        assert!((gp_v2.r0 - 100.0).abs() < 1e-9);
        assert!((gp_v2.r1 - 300_000.0).abs() < 1e-6);
        assert!((gp_v2.fee_ppm - 3000.0).abs() < 1e-6);

        // 2. Solidly Stable
        let mut sol = v2.clone();
        sol.address = Address::repeat_byte(0xa2);
        sol.dex_kind = DexKind::Solidly;
        sol.stable = true;
        sol.fee_ppm = 400;
        let gp_sol = pool_state_to_gpu(&sol, 0, 1, &mut tick_sp, &mut tick_liq).unwrap();
        assert_eq!(gp_sol.pool_type, POOL_SOLIDLY_STABLE);
        assert_eq!(gp_sol.protocol_id, PROTO_SOLIDLY);
        assert!((gp_sol.fee_ppm - 400.0).abs() < 1e-6);

        // 3. UniV3 / UniV4 mixed 18/6 decimals CLMM scaling parity against ResidualPool
        let tick = -196_250; // ~3000 USDC/WETH in raw 18/6 scale
        let s_raw = 1.0001_f64.powf((tick as f64 + 0.5) * 0.5);
        let sqrt_p_x96 = human_to_u256(s_raw * 79228162514264337593543950336.0_f64, 0);
        let v4 = PoolState {
            address: Address::repeat_byte(0xa3),
            factory_name: "UniswapV4".into(),
            dex_kind: DexKind::UniV4,
            token0: t0,
            token1: t1,
            dec0: 18,
            dec1: 6,
            fee_num: 0,
            fee_ppm: 500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::ZERO,
            r1_raw: U256::ZERO,
            sqrt_price_x96: sqrt_p_x96,
            liquidity: 5_000_000_000_000_000_000u128,
            tick,
            tick_spacing: 10,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 100_000.0,
            last_updated_block: 1,
            v4_key: Some(V4PoolKey {
                manager: Address::repeat_byte(0x01),
                vault: Address::repeat_byte(0x02),
                pool_id: B256::repeat_byte(0x03),
                currency0: t0,
                currency1: t1,
                fee: 500,
                tick_spacing: 10,
                hooks: Address::ZERO,
                parameters: B256::ZERO,
            }),
            metric: None,
            balancer: None,
        };
        let gp_v4 = pool_state_to_gpu(&v4, 0, 1, &mut tick_sp, &mut tick_liq).unwrap();
        assert_eq!(gp_v4.pool_type, POOL_CLMM);
        assert_eq!(gp_v4.protocol_id, PROTO_UNIV4);
        assert_eq!(gp_v4.tick_count, 1);
        let off = gp_v4.tick_offset as usize;
        let sa = tick_sp[off];
        let sb = tick_sp[off + 1];
        let l_h = tick_liq[off];
        assert!(sa < gp_v4.sqrt_p && gp_v4.sqrt_p < sb);
        // Verify exact CLMM swap quote parity with ResidualPool::quote_human
        let dx_h = 0.01;
        let rust_out = v4.to_residual().quote_human(true, dx_h);
        let gamma = 1.0 - gp_v4.fee_ppm * 1e-6;
        let s_next = (l_h * gp_v4.sqrt_p) / (l_h + gp_v4.sqrt_p * (gamma * dx_h));
        let gpu_out = l_h * (gp_v4.sqrt_p - s_next);
        assert!(
            (rust_out - gpu_out).abs() / rust_out < 1e-9,
            "rust_out={rust_out}, gpu_out={gpu_out}"
        );

        // 4. MetricOmm (does not double-count `fee_ppm`, preserves `extra_tax_ppm`)
        const Q64_F64: f64 = 18446744073709551616.0_f64;
        let metric = PoolState {
            address: Address::repeat_byte(0xa4),
            factory_name: "MetricOmm".into(),
            dex_kind: DexKind::MetricOmm,
            token0: t0,
            token1: t1,
            dec0: 18,
            dec1: 6,
            fee_num: 10_000,
            fee_ppm: 99, // intentionally ignored for MetricOmm just like src/amm.rs
            extra_tax_ppm: 10,
            stable: false,
            r0_raw: human_to_u256(100.0, 18),
            r1_raw: human_to_u256(300_000.0, 18),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 1,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 600_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: Some(MetricPoolState {
                sell_price_x64: (2998.0 * Q64_F64) as u128,
                buy_price_x64: (3002.0 * Q64_F64) as u128,
                num_bins: 40,
                active_bin: -1,
                paused: false,
            }),
            balancer: None,
        };
        let gp_met = pool_state_to_gpu(&metric, 0, 1, &mut tick_sp, &mut tick_liq).unwrap();
        assert_eq!(gp_met.pool_type, POOL_METRIC_OMM);
        assert_eq!(gp_met.protocol_id, PROTO_METRIC_OMM);
        assert!((gp_met.fee_ppm - 10.0).abs() < 1e-9);
        assert!(gp_met.param1 > gp_met.param2); // adverse active_bin=-1 gives higher haircut on dir=0

        // 5. BalancerV2 Weighted (supported) vs 3-coin rate-scaled Stable (skipped cleanly)
        let mut bal_w = v2.clone();
        bal_w.address = Address::repeat_byte(0xa5);
        bal_w.dex_kind = DexKind::BalancerV2;
        bal_w.dec1 = 18;
        bal_w.r1_raw = human_to_u256(300_000.0, 18);
        bal_w.balancer = Some(BalancerPoolState {
            vault: Address::repeat_byte(0xba),
            pool_id: B256::repeat_byte(0x01),
            kind: BalancerPoolKind::Weighted { w0: 0.8, w1: 0.2 },
        });
        let gp_bal = pool_state_to_gpu(&bal_w, 0, 1, &mut tick_sp, &mut tick_liq).unwrap();
        assert_eq!(gp_bal.pool_type, POOL_BALANCER_V2);
        assert_eq!(gp_bal.protocol_id, PROTO_BALANCER_V2);
        assert!((gp_bal.liq - 0.8).abs() < 1e-12);
        assert!((gp_bal.param0 - 0.2).abs() < 1e-12);

        let mut bal_3 = bal_w;
        bal_3.address = Address::repeat_byte(0xa6);
        bal_3.balancer = Some(BalancerPoolState {
            vault: Address::repeat_byte(0xba),
            pool_id: B256::repeat_byte(0x02),
            kind: BalancerPoolKind::Stable {
                amp: 500.0,
                scale0: 1.18,
                scale1: 1.0,
                other_scaled_balances: vec![10_000.0],
            },
        });
        assert!(pool_state_to_gpu(&bal_3, 0, 1, &mut tick_sp, &mut tick_liq).is_none());
    }

    #[test]
    fn test_gpu_batch_respects_max_tokens_cap() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let market = MarketState::new(cfg);

        let mut pools = Vec::with_capacity(300);
        for i in 0..300u16 {
            let mut b = [0u8; 20];
            b[0] = 0x50;
            b[18] = (i >> 8) as u8;
            b[19] = (i & 0xff) as u8;
            let tok = Address::from(b);
            pools.push(PoolState {
                address: Address::from({
                    let mut p = [0u8; 20];
                    p[0] = 0x90;
                    p[18] = (i >> 8) as u8;
                    p[19] = (i & 0xff) as u8;
                    p
                }),
                factory_name: "PancakeV2".into(),
                dex_kind: DexKind::UniV2,
                token0: wbnb,
                token1: tok,
                dec0: 18,
                dec1: 18,
                fee_num: 9975,
                fee_ppm: 2500,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: human_to_u256(100.0, 18),
                r1_raw: human_to_u256(10_000.0, 18),
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 0,
                allow_multi_tick: false,
                is_hub_0: true,
                is_hub_1: false,
                cached_depth_usd: 120_000.0 - (i as f64),
                last_updated_block: 1,
                v4_key: None,
                metric: None,
                balancer: None,
            });
        }
        let refs: Vec<&PoolState> = pools.iter().collect();
        let batch = build_gpu_market_batch(&market, &refs, None);
        assert!(batch.token_addrs.len() <= MAX_GPU_TOKENS);
        assert!(batch.token_indices.contains_key(&wbnb));
        assert!(batch.gpu_pools.len() < 300);
    }

    #[test]
    fn test_gpu_steps_to_candidate_and_compile_dag() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let usdt = cfg.usd_anchor;
        let tok = Address::repeat_byte(0x77);
        let mut market = MarketState::new(cfg);
        for fp in &mut market.flash_pools {
            fp.1 = human_to_u256(1_000_000.0, 18);
        }

        let mk_pool = |byte: u8, t0: Address, t1: Address| PoolState {
            address: Address::repeat_byte(byte),
            factory_name: "PancakeV2".into(),
            dex_kind: DexKind::UniV2,
            token0: t0,
            token1: t1,
            dec0: 18,
            dec1: 18,
            fee_num: 9975,
            fee_ppm: 2500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: human_to_u256(10_000.0, 18),
            r1_raw: human_to_u256(10_000.0, 18),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 100_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        };

        let p0 = mk_pool(0x01, wbnb, tok);
        let p1 = mk_pool(0x02, tok, usdt);
        let p2 = mk_pool(0x03, usdt, wbnb);
        let selected = [&p0, &p1, &p2];

        let steps = [
            GpuSolverStep {
                pool_idx: 0,
                dir: 0,
                amount_in: 1.0,
                amount_out: 100.0,
                gas_units: 52_000.0,
            },
            GpuSolverStep {
                pool_idx: 1,
                dir: 0,
                amount_in: 100.0,
                amount_out: 620.0,
                gas_units: 52_000.0,
            },
            GpuSolverStep {
                pool_idx: 2,
                dir: 0,
                amount_in: 620.0,
                amount_out: 1.03,
                gas_units: 52_000.0,
            },
        ];
        let res = GpuSolveResult {
            num_steps: 3,
            max_steps_cap: 32,
            borrow_amount: 1.0,
            gross_profit: 0.03,
            gas_cost_base: 0.001,
            net_profit: 0.029,
            total_gas_units: 221_000.0,
            max_deficit: 0.0,
            ..Default::default()
        };

        let cand = gpu_steps_to_candidate(&market, &selected, wbnb, &res, &steps, 0.01)
            .expect("valid GPU steps must convert to RouteCandidate");
        assert_eq!(cand.base_token, wbnb);
        assert_eq!(cand.strategy, "GPU_CFMM(3steps)");
        assert!(cand.est_net_profit_usd > 1.0);
        let dag = compile_dag_steps(&cand.flows, wbnb, Address::repeat_byte(0xEE))
            .expect("converted flows must compile to DagStep[]");
        assert_eq!(dag.len(), 3);
    }

    #[test]
    fn test_gpu_steps_enforces_v2_compact_token_and_u128_limits() {
        let cfg = load_chain_config("bsc").unwrap();
        let wbnb = cfg.wrapped_native;
        let mut market = MarketState::new(cfg);
        for fp in &mut market.flash_pools {
            fp.1 = U256::MAX >> 10;
        }

        let mk_pool = |idx: usize, t0: Address, t1: Address| PoolState {
            address: Address::from([idx as u8 + 1; 20]),
            factory_name: "PancakeV2".into(),
            dex_kind: DexKind::UniV2,
            token0: t0,
            token1: t1,
            dec0: 18,
            dec1: 18,
            fee_num: 9975,
            fee_ppm: 2500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: human_to_u256(10_000.0, 18),
            r1_raw: human_to_u256(10_000.0, 18),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 100_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        };

        let make_cycle = |n_tokens: usize| {
            let mut toks = Vec::with_capacity(n_tokens);
            toks.push(wbnb);
            for i in 1..n_tokens {
                let mut bytes = [0u8; 20];
                bytes[0] = 0xA0;
                bytes[19] = i as u8;
                toks.push(Address::from(bytes));
            }
            let mut pools = Vec::with_capacity(n_tokens);
            let mut steps = Vec::with_capacity(n_tokens);
            for i in 0..n_tokens {
                pools.push(mk_pool(i, toks[i], toks[(i + 1) % n_tokens]));
                steps.push(GpuSolverStep {
                    pool_idx: i as i32,
                    dir: 0,
                    amount_in: 1.0,
                    amount_out: if i + 1 == n_tokens { 1.05 } else { 1.0 },
                    gas_units: 45_000.0,
                });
            }
            let res = GpuSolveResult {
                num_steps: n_tokens as i32,
                max_steps_cap: MAX_DAG_STEPS as i32,
                borrow_amount: 1.0,
                gross_profit: 0.05,
                gas_cost_base: 0.001,
                net_profit: 0.049,
                total_gas_units: 65_000.0 + (n_tokens as f64) * 45_000.0,
                max_deficit: 0.0,
                ..Default::default()
            };
            (pools, steps, res)
        };

        // 16 unique tokens fits V2 DagArbExecutor transient slot range 32..47.
        let (pools_16, steps_16, res_16) = make_cycle(16);
        let refs_16: Vec<&PoolState> = pools_16.iter().collect();
        assert!(
            gpu_steps_to_candidate(&market, &refs_16, wbnb, &res_16, &steps_16, 0.01).is_some(),
            "16-token cycle must be accepted"
        );

        // 17 unique tokens violates V2 DagArbExecutor <= 16-token limit even though steps <= 32.
        let (pools_17, steps_17, res_17) = make_cycle(17);
        let refs_17: Vec<&PoolState> = pools_17.iter().collect();
        assert!(
            gpu_steps_to_candidate(&market, &refs_17, wbnb, &res_17, &steps_17, 0.01).is_none(),
            "17-token cycle must be rejected"
        );

        // Borrow and gross profit exceeding u128::MAX (~3.4028e38 wei = 3.4028e20 ether) must reject.
        let (pools_3, steps_3, mut res_big_borrow) = make_cycle(3);
        let refs_3: Vec<&PoolState> = pools_3.iter().collect();
        res_big_borrow.borrow_amount = 4.0e20;
        res_big_borrow.gross_profit = 1.0;
        assert!(
            gpu_steps_to_candidate(&market, &refs_3, wbnb, &res_big_borrow, &steps_3, 0.01)
                .is_none(),
            "borrow exceeding u128::MAX must be rejected"
        );

        let mut res_big_gross = make_cycle(3).2;
        res_big_gross.borrow_amount = 1.0;
        res_big_gross.gross_profit = 4.0e20;
        assert!(
            gpu_steps_to_candidate(&market, &refs_3, wbnb, &res_big_gross, &steps_3, 0.01)
                .is_none(),
            "gross profit exceeding u128::MAX must be rejected"
        );

        // Verify gas scaling and clamping in GpuConfig + pool serialization.
        let gcfg = GpuConfig::from_env(12_000);
        assert!((21_000.0..=500_000.0).contains(&gcfg.base_tx_gas));
        assert!((0.25..=4.0).contains(&gcfg.gas_scale));
        let mut tick_sp = Vec::new();
        let mut tick_liq = Vec::new();
        let gp_scaled =
            pool_state_to_gpu_with_gas_scale(&pools_3[0], 0, 1, 0.85, &mut tick_sp, &mut tick_liq)
                .expect("pool converts with custom gas scale");
        let base_gas = pool_swap_gas_units(DexKind::UniV2);
        assert!((gp_scaled.gas_units - base_gas * 0.85).abs() < 1e-6);
        let gp_clamped =
            pool_state_to_gpu_with_gas_scale(&pools_3[0], 0, 1, 10.0, &mut tick_sp, &mut tick_liq)
                .expect("pool converts with clamped gas scale");
        assert!((gp_clamped.gas_units - base_gas * 4.0).abs() < 1e-6);
    }
}
