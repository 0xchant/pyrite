use alloy::primitives::{aliases::{I24, U24}, uint, Address, Bytes, B256, U256, U512};
use alloy::sol_types::SolValue;
use serde::{Deserialize, Serialize};

use crate::config::DexKind;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct V4PoolKey {
    pub manager: Address,
    pub vault: Address,
    pub pool_id: B256,
    pub currency0: Address,
    pub currency1: Address,
    pub fee: u32,
    pub tick_spacing: i32,
    pub hooks: Address,
    pub parameters: B256,
}

impl V4PoolKey {
    pub fn encode_extra_data(&self, dex_kind: DexKind) -> Bytes {
        match dex_kind {
            DexKind::InfinityCl => (
                self.currency0,
                self.currency1,
                self.hooks,
                self.manager,
                U24::from(self.fee),
                self.parameters,
            )
                .abi_encode_params()
                .into(),
            DexKind::UniV4 => (
                self.currency0,
                self.currency1,
                U24::from(self.fee),
                I24::try_from(self.tick_spacing).unwrap_or_default(),
                self.hooks,
            )
                .abi_encode_params()
                .into(),
            _ => Bytes::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TokenMeta {
    #[allow(dead_code)]
    pub address: Address,
    pub symbol: String,
    pub decimals: u8,
    pub approx_usd: f64,
}

impl TokenMeta {
    #[allow(dead_code)]
    pub fn scale(&self) -> f64 {
        10f64.powi(self.decimals as i32)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricPoolState {
    pub sell_price_x64: u128, // Q64.64 raw token1/token0 when selling token0 (zero_for_one = true)
    pub buy_price_x64: u128,  // Q64.64 raw token1/token0 when buying token0 (zero_for_one = false)
    pub num_bins: u16,        // total discrete bins (max_bin - min_bin + 1), clamped >= 2
    #[serde(default)]
    pub active_bin: i16,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BalancerPoolKind {
    Weighted { w0: f64, w1: f64 },
    Stable {
        amp: f64,
        scale0: f64,
        scale1: f64,
        other_scaled_balances: Vec<f64>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BalancerPoolState {
    pub vault: Address,
    pub pool_id: B256,
    pub kind: BalancerPoolKind,
}

const Q64_F64: f64 = 18446744073709551616.0_f64;

#[inline]
pub(crate) fn metric_edge_haircut(sell: f64, buy: f64, active_bin: i16, zero_for_one: bool) -> f64 {
    let spr = ((buy - sell) / sell).clamp(0.00001, 0.02);
    let base_fee = (spr * 0.5).clamp(0.00003, 0.00015);
    let adverse_bins = if zero_for_one {
        (-active_bin).clamp(0, 20) as f64
    } else {
        active_bin.clamp(0, 20) as f64
    };
    let bin_step = (spr * 0.5).clamp(0.000015, 0.00012);
    (base_fee + adverse_bins * bin_step).min(0.05)
}

#[derive(Debug, Clone)]
pub struct PoolState {
    pub address: Address,
    pub factory_name: String,
    pub dex_kind: DexKind,
    pub token0: Address,
    pub token1: Address,
    pub dec0: u8,
    pub dec1: u8,
    pub fee_num: u16, // out of 10000 for V2 (e.g., 9975); or unused for V3
    pub fee_ppm: u32, // parts per 1e6 (e.g., 2500 = 0.25%, 500 = 0.05%, 100 = 0.01%)
    pub extra_tax_ppm: u32, // off-chain calibrated transfer/tax/unknown-fee friction (never sent on-chain)
    pub stable: bool,
    // V2 / Solidly reserves (raw integer strings / u128 / f64)
    pub r0_raw: U256,
    pub r1_raw: U256,
    // V3 concentrated liquidity state
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub tick_spacing: i32,
    pub allow_multi_tick: bool,
    pub is_hub_0: bool,
    pub is_hub_1: bool,
    pub cached_depth_usd: f64,
    pub last_updated_block: u64,
    pub v4_key: Option<V4PoolKey>,
    pub metric: Option<MetricPoolState>,
    pub balancer: Option<BalancerPoolState>,
}

impl PoolState {
    pub fn other_token(&self, token_in: Address) -> Option<(Address, bool)> {
        if token_in == self.token0 {
            Some((self.token1, true))
        } else if token_in == self.token1 {
            Some((self.token0, false))
        } else {
            None
        }
    }

    pub fn is_active(&self) -> bool {
        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly => !self.r0_raw.is_zero() && !self.r1_raw.is_zero(),
            DexKind::BalancerV2 => self.balancer.is_some() && !self.r0_raw.is_zero() && !self.r1_raw.is_zero(),
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => !self.sqrt_price_x96.is_zero() && self.liquidity > 0,
            DexKind::MetricOmm => {
                self.metric
                    .as_ref()
                    .is_some_and(|m| !m.paused && m.sell_price_x64 > 0 && m.buy_price_x64 >= m.sell_price_x64)
                    && (!self.r0_raw.is_zero() || !self.r1_raw.is_zero())
            }
        }
    }

    /// Convert raw reserves / virtual CL reserves to human-readable f64 pair `(x0_human, x1_human)`.
    pub fn human_reserves(&self) -> (f64, f64) {
        let s0 = pow10(self.dec0);
        let s1 = pow10(self.dec1);
        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly | DexKind::BalancerV2 => {
                (u256_to_f64(self.r0_raw) / s0, u256_to_f64(self.r1_raw) / s1)
            }
            DexKind::MetricOmm => (
                u256_to_f64(self.r0_raw) / 1e18,
                u256_to_f64(self.r1_raw) / 1e18,
            ),
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                let s = sqrt_price_x96_to_f64(self.sqrt_price_x96);
                let l = self.liquidity as f64;
                if s <= 0.0 || l <= 0.0 {
                    return (0.0, 0.0);
                }
                let x0 = (l / s) / s0;
                let x1 = (l * s) / s1;
                (x0, x1)
            }
        }
    }

    /// Exact remaining USD liquidity depth inside current guaranteed active tick (or bounded
    /// multi-tick range on deep hub-to-hub V3 pools), or total reserve USD value for V2 / Solidly.
    pub fn usable_depth_usd(&self, p0_usd: f64, p1_usd: f64) -> f64 {
        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly => self.pricing_depth_usd(p0_usd, p1_usd),
            DexKind::BalancerV2 => {
                if let Some(BalancerPoolState {
                    kind: BalancerPoolKind::Weighted { w0, w1 },
                    ..
                }) = &self.balancer
                {
                    let (x0, x1) = self.human_reserves();
                    let usd0 = if p0_usd > 0.0 { x0 * p0_usd } else { 0.0 };
                    let usd1 = if p1_usd > 0.0 { x1 * p1_usd } else { 0.0 };
                    if usd0 > 0.0 && usd1 > 0.0 {
                        usd0 + usd1
                    } else if usd0 > 0.0 && *w0 > 1e-9 {
                        usd0 * (w0 + w1) / w0
                    } else if usd1 > 0.0 && *w1 > 1e-9 {
                        usd1 * (w0 + w1) / w1
                    } else {
                        0.0
                    }
                } else {
                    self.pricing_depth_usd(p0_usd, p1_usd)
                }
            }
            DexKind::MetricOmm => {
                let (x0, x1) = self.human_reserves();
                let usd0 = if p0_usd > 0.0 { x0 * p0_usd } else { 0.0 };
                let usd1 = if p1_usd > 0.0 { x1 * p1_usd } else { 0.0 };
                if p0_usd > 0.0 && p1_usd > 0.0 {
                    usd0 + usd1
                } else {
                    usd0.max(usd1)
                }
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                if !self.is_active() {
                    return 0.0;
                }
                let s = sqrt_price_x96_to_f64(self.sqrt_price_x96);
                let (s_lo, s_hi) =
                    v3_sqrt_bounds(s, self.tick, self.tick_spacing, self.allow_multi_tick);
                let l = self.liquidity as f64;
                if s <= 0.0 || l <= 0.0 {
                    return 0.0;
                }
                let s0 = pow10(self.dec0);
                let s1 = pow10(self.dec1);
                let cap0 = if s > s_lo { l * (1.0 / s_lo - 1.0 / s) / s0 } else { 0.0 };
                let cap1 = if s_hi > s { l * (s_hi - s) / s1 } else { 0.0 };
                let u0 = if p0_usd > 0.0 { cap0 * p0_usd } else { 0.0 };
                let u1 = if p1_usd > 0.0 { cap1 * p1_usd } else { 0.0 };
                if u0 > 0.0 && u1 > 0.0 {
                    u0 + u1
                } else {
                    2.0 * u0.max(u1)
                }
            }
        }
    }

    pub fn refresh_depth(&mut self, p0_usd: f64, p1_usd: f64) {
        self.cached_depth_usd = self.usable_depth_usd(p0_usd, p1_usd);
    }

    /// Maximum single-swap directional USD input inside current reserve / V3 tick bounds.
    pub fn max_input_usd(&self, token_in: Address, p_in_usd: f64) -> f64 {
        let Some((_, zero_for_one)) = self.other_token(token_in) else {
            return 0.0;
        };
        if !self.is_active() {
            return 0.0;
        }
        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly => {
                let (x0, x1) = self.human_reserves();
                let r_in = if zero_for_one { x0 } else { x1 };
                let in_cap = if p_in_usd > 0.0 {
                    r_in * p_in_usd * 3.5
                } else {
                    0.0
                };
                in_cap.max(self.cached_depth_usd * 0.85)
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                let s = sqrt_price_x96_to_f64(self.sqrt_price_x96);
                let l = self.liquidity as f64;
                if s <= 0.0 || l <= 0.0 {
                    return 0.0;
                }
                if p_in_usd <= 0.0 {
                    return self.cached_depth_usd * 0.85;
                }
                let (s_lo, s_hi) =
                    v3_sqrt_bounds(s, self.tick, self.tick_spacing, self.allow_multi_tick);
                let cap_in_h = if zero_for_one {
                    if s > s_lo {
                        l * (1.0 / s_lo - 1.0 / s) / pow10(self.dec0)
                    } else {
                        0.0
                    }
                } else if s_hi > s {
                    l * (s_hi - s) / pow10(self.dec1)
                } else {
                    0.0
                };
                cap_in_h * p_in_usd * 0.98
            }
            DexKind::MetricOmm => {
                let Some(ref m) = self.metric else {
                    return 0.0;
                };
                if m.paused {
                    return 0.0;
                }
                if p_in_usd <= 0.0 {
                    return self.cached_depth_usd * 0.5;
                }
                let (x0, x1) = self.human_reserves();
                let out_avail_h = if zero_for_one { x1 } else { x0 };
                if out_avail_h <= 1e-12 || m.sell_price_x64 == 0 || m.buy_price_x64 < m.sell_price_x64 {
                    return 0.0;
                }
                let rate_in_per_out = if zero_for_one {
                    let sell_human = (m.sell_price_x64 as f64) / Q64_F64;
                    1.0 / sell_human
                } else {
                    (m.buy_price_x64 as f64) / Q64_F64
                };
                if !rate_in_per_out.is_finite() || rate_in_per_out <= 0.0 {
                    return 0.0;
                }
                out_avail_h * 0.80 * rate_in_per_out * p_in_usd
            }
            DexKind::BalancerV2 => {
                let Some(ref bal) = self.balancer else {
                    return 0.0;
                };
                let (x0, x1) = self.human_reserves();
                let (r_in, r_out) = if zero_for_one { (x0, x1) } else { (x1, x0) };
                match &bal.kind {
                    BalancerPoolKind::Weighted { .. } => {
                        if p_in_usd > 0.0 {
                            r_in * p_in_usd * 0.29
                        } else {
                            self.cached_depth_usd * 0.20
                        }
                    }
                    BalancerPoolKind::Stable { .. } => {
                        let avail = r_in.min(r_out * 1.1) * 0.90;
                        if p_in_usd > 0.0 {
                            avail * p_in_usd
                        } else {
                            self.cached_depth_usd * 0.45
                        }
                    }
                }
            }
        }
    }

    /// Virtual band USD depth used for USD token price inference even near tick boundaries.
    #[allow(dead_code)]
    pub fn pricing_depth_usd(&self, p0_usd: f64, p1_usd: f64) -> f64 {
        let (x0, x1) = self.human_reserves();
        let usd0 = if p0_usd > 0.0 { x0 * p0_usd } else { 0.0 };
        let usd1 = if p1_usd > 0.0 { x1 * p1_usd } else { 0.0 };
        let total = if usd0 > 0.0 && usd1 > 0.0 {
            usd0 + usd1
        } else {
            2.0 * usd0.max(usd1)
        };
        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly | DexKind::MetricOmm | DexKind::BalancerV2 => total,
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                let band_frac = ((self.tick_spacing.max(1) as f64) * 0.0002).clamp(0.0002, 0.03);
                total * band_frac
            }
        }
    }

    /// Real USD liquidity on the `known_token` side ONLY:
    /// - V2 / Solidly: `2.0 * real_reserve(known_token) * known_usd`
    /// - V3: `2.0 * actual_in_tick_reserve(known_token) * known_usd` (`L*(1/s - 1/s_hi)` or `L*(s - s_lo)`),
    ///   preventing extreme-tick virtual `L * sqrtP` or unverified `est_p_other` from spoofing depth.
    pub fn known_side_depth_usd(&self, known_token: Address, known_usd: f64) -> f64 {
        if known_usd <= 0.0 || !self.is_active() {
            return 0.0;
        }
        let is_t0 = if known_token == self.token0 {
            true
        } else if known_token == self.token1 {
            false
        } else {
            return 0.0;
        };
        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly | DexKind::MetricOmm => {
                let (x0, x1) = self.human_reserves();
                2.0 * (if is_t0 { x0 } else { x1 }) * known_usd
            }
            DexKind::BalancerV2 => {
                let (x0, x1) = self.human_reserves();
                let x_known = if is_t0 { x0 } else { x1 };
                if let Some(BalancerPoolState {
                    kind: BalancerPoolKind::Weighted { w0, w1 },
                    ..
                }) = &self.balancer
                {
                    let (w_known, w_other) = if is_t0 { (*w0, *w1) } else { (*w1, *w0) };
                    if w_known > 1e-9 && w_other > 1e-9 {
                        return x_known * known_usd * (w_known + w_other) / w_known;
                    }
                }
                2.0 * x_known * known_usd
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                let s = sqrt_price_x96_to_f64(self.sqrt_price_x96);
                let (s_lo, s_hi) = v3_sqrt_bounds(s, self.tick, self.tick_spacing, false);
                let l = self.liquidity as f64;
                if s <= 0.0 || l <= 0.0 {
                    return 0.0;
                }
                let amt_known = if is_t0 {
                    if s_hi > s {
                        l * (1.0 / s - 1.0 / s_hi) / pow10(self.dec0)
                    } else {
                        0.0
                    }
                } else if s > s_lo {
                    l * (s - s_lo) / pow10(self.dec1)
                } else {
                    0.0
                };
                2.0 * amt_known.max(0.0) * known_usd
            }
        }
    }

    /// Spot exchange rate `token_out / token_in` in human units *including fee*.
    /// Returns 0.0 if pool is empty or token_in not in pool.
    pub fn marginal_rate_human(&self, token_in: Address) -> f64 {
        let Some((_, zero_for_one)) = self.other_token(token_in) else {
            return 0.0;
        };
        if !self.is_active() {
            return 0.0;
        }
        let total_ppm = self.fee_ppm.saturating_add(self.extra_tax_ppm).min(999_999);
        let gamma = 1.0 - (total_ppm as f64) / 1_000_000.0;
        let dec_scale = pow10(self.dec0) / pow10(self.dec1); // dec0 / dec1 ratio

        match self.dex_kind {
            DexKind::UniV2 => {
                let r0 = u256_to_f64(self.r0_raw);
                let r1 = u256_to_f64(self.r1_raw);
                if r0 <= 0.0 || r1 <= 0.0 {
                    return 0.0;
                }
                let raw_spot = if zero_for_one { r1 / r0 } else { r0 / r1 };
                let human_spot = if zero_for_one {
                    raw_spot * dec_scale
                } else {
                    raw_spot / dec_scale
                };
                human_spot * gamma
            }
            DexKind::Solidly => {
                let (x0, x1) = self.human_reserves();
                if x0 <= 0.0 || x1 <= 0.0 {
                    return 0.0;
                }
                if !self.stable {
                    let spot = if zero_for_one { x1 / x0 } else { x0 / x1 };
                    spot * gamma
                } else {
                    // Derivative of x^3*y + x*y^3 = k:
                    // dy/dx = -(3x^2 y + y^3) / (x^3 + 3x y^2)
                    let num = if zero_for_one {
                        3.0 * x0 * x0 * x1 + x1 * x1 * x1
                    } else {
                        3.0 * x1 * x1 * x0 + x0 * x0 * x0
                    };
                    let den = if zero_for_one {
                        x0 * x0 * x0 + 3.0 * x0 * x1 * x1
                    } else {
                        x1 * x1 * x1 + 3.0 * x1 * x0 * x0
                    };
                    if den <= 0.0 {
                        0.0
                    } else {
                        (num / den) * gamma
                    }
                }
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                let s = sqrt_price_x96_to_f64(self.sqrt_price_x96);
                if s <= 0.0 || self.liquidity == 0 {
                    return 0.0;
                }
                let p_raw = s * s; // token1_raw / token0_raw
                let p_human = p_raw * dec_scale; // token1_human / token0_human
                let rate = if zero_for_one { p_human } else { 1.0 / p_human };
                rate * gamma
            }
            DexKind::MetricOmm => {
                let Some(ref m) = self.metric else {
                    return 0.0;
                };
                if m.paused || m.sell_price_x64 == 0 || m.buy_price_x64 < m.sell_price_x64 {
                    return 0.0;
                }
                let sell_human = (m.sell_price_x64 as f64) / Q64_F64;
                let buy_human = (m.buy_price_x64 as f64) / Q64_F64;
                let haircut = metric_edge_haircut(sell_human, buy_human, m.active_bin, zero_for_one);
                let tax_mult = 1.0 - (self.extra_tax_ppm.min(999_999) as f64) / 1_000_000.0;
                if zero_for_one {
                    if self.r1_raw.is_zero() {
                        return 0.0;
                    }
                    sell_human * (1.0 - haircut) * tax_mult
                } else {
                    if self.r0_raw.is_zero() {
                        return 0.0;
                    }
                    (1.0 / buy_human) * (1.0 - haircut) * tax_mult
                }
            }
            DexKind::BalancerV2 => {
                let Some(ref bal) = self.balancer else {
                    return 0.0;
                };
                let (x0, x1) = self.human_reserves();
                if x0 <= 1e-12 || x1 <= 1e-12 {
                    return 0.0;
                }
                match &bal.kind {
                    BalancerPoolKind::Weighted { w0, w1 } => {
                        let (r_in, w_in, r_out, w_out) = if zero_for_one {
                            (x0, *w0, x1, *w1)
                        } else {
                            (x1, *w1, x0, *w0)
                        };
                        if w_in <= 1e-9 || w_out <= 1e-9 {
                            return 0.0;
                        }
                        ((r_out / w_out) / (r_in / w_in)) * gamma
                    }
                    BalancerPoolKind::Stable {
                        amp,
                        scale0,
                        scale1,
                        other_scaled_balances,
                    } => {
                        let rate = balancer_stable_marginal_rate(
                            *amp,
                            x0 * *scale0,
                            x1 * *scale1,
                            *scale0,
                            *scale1,
                            other_scaled_balances,
                            zero_for_one,
                        );
                        rate * gamma
                    }
                }
            }
        }
    }

    pub fn to_residual(&self) -> ResidualPool {
        self.residual_with_multi_tick(self.allow_multi_tick)
    }

    pub fn residual_with_multi_tick(&self, allow_multi_tick: bool) -> ResidualPool {
        let s = sqrt_price_x96_to_f64(self.sqrt_price_x96);
        let (s_lo, s_hi) = if matches!(self.dex_kind, DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4) {
            v3_sqrt_bounds(s, self.tick, self.tick_spacing, allow_multi_tick)
        } else {
            (0.0, 0.0)
        };
        let (metric_sell, metric_buy, metric_num_bins, metric_active_bin, metric_paused) = self
            .metric
            .as_ref()
            .map(|m| {
                (
                    (m.sell_price_x64 as f64) / Q64_F64,
                    (m.buy_price_x64 as f64) / Q64_F64,
                    m.num_bins.max(2),
                    m.active_bin,
                    m.paused,
                )
            })
            .unwrap_or((0.0, 0.0, 2, 0, false));
        let balancer = self
            .balancer
            .as_ref()
            .map(|b| std::sync::Arc::new(b.kind.clone()));
        ResidualPool {
            address: self.address,
            dex_kind: self.dex_kind,
            token0: self.token0,
            token1: self.token1,
            dec0: self.dec0,
            dec1: self.dec1,
            scale0: pow10(self.dec0),
            scale1: pow10(self.dec1),
            fee_num: self.fee_num,
            fee_ppm: self.fee_ppm,
            extra_tax_ppm: self.extra_tax_ppm,
            stable: self.stable,
            r0_raw: u256_to_f64(self.r0_raw),
            r1_raw: u256_to_f64(self.r1_raw),
            sqrt_p: s,
            s_lo,
            s_hi,
            liquidity: self.liquidity as f64,
            tick: self.tick,
            tick_spacing: self.tick_spacing,
            allow_multi_tick,
            metric_sell,
            metric_buy,
            metric_num_bins,
            metric_active_bin,
            metric_paused,
            balancer,
        }
    }

    /// Gross input (including the swap fee) and final Q96 price for a full V3 exact-output fill
    /// inside the current liquidity interval. Amount deltas and input fees round up as in V3.
    pub fn quote_v3_exact_output(
        &self,
        zero_for_one: bool,
        amount_out: U256,
        price_limit: U256,
    ) -> Option<(U256, U256)> {
        if !matches!(
            self.dex_kind,
            DexKind::PancakeV3 | DexKind::UniV3 | DexKind::InfinityCl | DexKind::UniV4
        )
            || !self.is_active()
            || amount_out.is_zero()
            || self.fee_ppm >= 1_000_000
        {
            return None;
        }
        let sqrt = U512::from(self.sqrt_price_x96);
        let liquidity = U512::from(self.liquidity);
        let q96 = U512::ONE << 96_usize;
        let liquidity_x96 = liquidity * q96;
        let out = U512::from(amount_out);
        let next = if zero_for_one {
            sqrt.checked_sub((out * q96).div_ceil(liquidity))?
        } else {
            let den = liquidity_x96.checked_sub(out * sqrt)?;
            if den.is_zero() {
                return None;
            }
            (liquidity_x96 * sqrt).div_ceil(den)
        };
        let next_x96 = U256::checked_from_limbs_slice(next.as_limbs())?;
        const MIN_SQRT_RATIO: U256 = uint!(4295128739_U256);
        const MAX_SQRT_RATIO: U256 = uint!(1461446703485210103287273052203988822378723970342_U256);
        if next_x96 <= MIN_SQRT_RATIO || next_x96 >= MAX_SQRT_RATIO {
            return None;
        }
        if !price_limit.is_zero()
            && (price_limit <= MIN_SQRT_RATIO
                || price_limit >= MAX_SQRT_RATIO
                || (zero_for_one && next_x96 < price_limit)
                || (!zero_for_one && next_x96 > price_limit))
        {
            return None;
        }
        let (lo, hi) = v3_sqrt_bounds(
            sqrt_price_x96_to_f64(self.sqrt_price_x96),
            self.tick,
            self.tick_spacing,
            false,
        );
        let next_s = sqrt_price_x96_to_f64(next_x96);
        if (zero_for_one && next_s < lo) || (!zero_for_one && next_s > hi) {
            return None;
        }
        let net_in = if zero_for_one {
            (liquidity_x96 * (sqrt - next)).div_ceil(sqrt * next)
        } else {
            (liquidity * (next - sqrt)).div_ceil(q96)
        };
        let gross_in = (net_in * U512::from(1_000_000)).div_ceil(U512::from(1_000_000 - self.fee_ppm));
        Some((U256::checked_from_limbs_slice(gross_in.as_limbs())?, next_x96))
    }

    /// Apply a virtual swap in-place to project post-victim pool reserves / sqrtPriceX96 for mempool backruns.
    pub fn apply_virtual_swap(
        &mut self,
        token_in: Address,
        amount_in_human: f64,
        p0_usd: f64,
        p1_usd: f64,
    ) -> f64 {
        let Some((_, zero_for_one)) = self.other_token(token_in) else {
            return 0.0;
        };
        if amount_in_human <= 0.0 || !self.is_active() {
            return 0.0;
        }
        let mut res =
            self.residual_with_multi_tick(self.allow_multi_tick || self.cached_depth_usd >= 1_000.0);
        let out_h = res.apply_swap_human(zero_for_one, amount_in_human);
        if out_h <= 0.0 {
            return 0.0;
        }
        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly | DexKind::BalancerV2 => {
                self.r0_raw = U256::from(res.r0_raw.max(1.0) as u128);
                self.r1_raw = U256::from(res.r1_raw.max(1.0) as u128);
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                let two_96 = 79228162514264337593543950336.0_f64;
                self.sqrt_price_x96 = human_to_u256(res.sqrt_p * two_96, 0);
            }
            DexKind::MetricOmm => {
                self.r0_raw = U256::from(res.r0_raw.max(0.0) as u128);
                self.r1_raw = U256::from(res.r1_raw.max(0.0) as u128);
                if let Some(ref mut m) = self.metric {
                    m.sell_price_x64 = (res.metric_sell * Q64_F64).round().max(1.0) as u128;
                    m.buy_price_x64 = (res.metric_buy * Q64_F64)
                        .round()
                        .max(m.sell_price_x64 as f64) as u128;
                }
            }
        }
        self.refresh_depth(p0_usd, p1_usd);
        out_h
    }
}

/// Fast residual pool snapshot mutated during path / DAG flow optimization.
#[derive(Debug, Clone)]
pub struct ResidualPool {
    #[allow(dead_code)]
    pub address: Address,
    pub dex_kind: DexKind,
    #[allow(dead_code)]
    pub token0: Address,
    #[allow(dead_code)]
    pub token1: Address,
    #[allow(dead_code)]
    pub dec0: u8,
    #[allow(dead_code)]
    pub dec1: u8,
    pub scale0: f64,
    pub scale1: f64,
    pub fee_num: u16,
    pub fee_ppm: u32,
    pub extra_tax_ppm: u32,
    pub stable: bool,
    pub r0_raw: f64,
    pub r1_raw: f64,
    pub sqrt_p: f64,
    pub s_lo: f64,
    pub s_hi: f64,
    pub liquidity: f64,
    #[allow(dead_code)]
    pub tick: i32,
    #[allow(dead_code)]
    pub tick_spacing: i32,
    #[allow(dead_code)]
    pub allow_multi_tick: bool,
    pub metric_sell: f64,
    pub metric_buy: f64,
    pub metric_num_bins: u16,
    pub metric_active_bin: i16,
    pub metric_paused: bool,
    pub balancer: Option<std::sync::Arc<BalancerPoolKind>>,
}

impl ResidualPool {
    #[inline]
    pub fn gamma(&self) -> f64 {
        let total_ppm = self.fee_ppm.saturating_add(self.extra_tax_ppm).min(999_999);
        1.0 - (total_ppm as f64) / 1_000_000.0
    }

    fn metric_quote_and_walk(&self, zero_for_one: bool, amount_in_human: f64) -> Option<(f64, f64, f64)> {
        if self.metric_paused || self.metric_sell <= 0.0 || self.metric_buy < self.metric_sell {
            return None;
        }
        let out_avail_h = if zero_for_one {
            self.r1_raw / 1e18
        } else {
            self.r0_raw / 1e18
        };
        if out_avail_h <= 1e-12 {
            return None;
        }
        let top_rate = if zero_for_one {
            self.metric_sell
        } else {
            1.0 / self.metric_buy
        };
        let out_no_slip = amount_in_human * top_rate;
        if out_no_slip >= out_avail_h * 0.80 {
            return None;
        }
        let frac = out_no_slip / out_avail_h;
        let spr = ((self.metric_buy - self.metric_sell) / self.metric_sell).clamp(0.00005, 0.02);
        let half_bins = (self.metric_num_bins.max(2) as f64) * 0.5;
        let max_walk = (spr * half_bins * 0.5).clamp(0.0005, 0.05);
        let haircut = metric_edge_haircut(self.metric_sell, self.metric_buy, self.metric_active_bin, zero_for_one);
        let tax_mult = 1.0 - (self.extra_tax_ppm.min(999_999) as f64) / 1_000_000.0;
        let out_h = (out_no_slip * (1.0 - 0.5 * max_walk * frac) * (1.0 - haircut) * tax_mult).max(0.0);
        Some((out_h, frac, max_walk))
    }

    /// Current instantaneous marginal rate (human tokenOut per human tokenIn) at current residual state.
    #[allow(dead_code)]
    pub fn marginal_rate_human(&self, zero_for_one: bool) -> f64 {
        let gamma = self.gamma();
        let dec_scale = self.scale0 / self.scale1;
        match self.dex_kind {
            DexKind::UniV2 => {
                if self.r0_raw <= 1.0 || self.r1_raw <= 1.0 {
                    return 0.0;
                }
                let raw_spot = if zero_for_one {
                    self.r1_raw / self.r0_raw
                } else {
                    self.r0_raw / self.r1_raw
                };
                let h = if zero_for_one {
                    raw_spot * dec_scale
                } else {
                    raw_spot / dec_scale
                };
                h * gamma
            }
            DexKind::Solidly => {
                let x0 = self.r0_raw / self.scale0;
                let x1 = self.r1_raw / self.scale1;
                if x0 <= 1e-12 || x1 <= 1e-12 {
                    return 0.0;
                }
                if !self.stable {
                    let spot = if zero_for_one { x1 / x0 } else { x0 / x1 };
                    spot * gamma
                } else {
                    let (a, b) = if zero_for_one { (x0, x1) } else { (x1, x0) };
                    let num = 3.0 * a * a * b + b * b * b;
                    let den = a * a * a + 3.0 * a * b * b;
                    if den <= 0.0 {
                        0.0
                    } else {
                        (num / den) * gamma
                    }
                }
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                if self.sqrt_p <= 0.0 || self.liquidity <= 0.0 {
                    return 0.0;
                }
                let p_raw = self.sqrt_p * self.sqrt_p;
                let p_human = p_raw * dec_scale;
                let rate = if zero_for_one { p_human } else { 1.0 / p_human };
                rate * gamma
            }
            DexKind::MetricOmm => {
                if self.metric_paused || self.metric_sell <= 0.0 || self.metric_buy < self.metric_sell {
                    return 0.0;
                }
                let haircut = metric_edge_haircut(self.metric_sell, self.metric_buy, self.metric_active_bin, zero_for_one);
                let tax_mult = 1.0 - (self.extra_tax_ppm.min(999_999) as f64) / 1_000_000.0;
                if zero_for_one {
                    if self.r1_raw <= 1.0 {
                        return 0.0;
                    }
                    self.metric_sell * (1.0 - haircut) * tax_mult
                } else {
                    if self.r0_raw <= 1.0 {
                        return 0.0;
                    }
                    (1.0 / self.metric_buy) * (1.0 - haircut) * tax_mult
                }
            }
            DexKind::BalancerV2 => {
                let Some(bal) = self.balancer.as_deref() else {
                    return 0.0;
                };
                let x0 = self.r0_raw / self.scale0;
                let x1 = self.r1_raw / self.scale1;
                if x0 <= 1e-12 || x1 <= 1e-12 {
                    return 0.0;
                }
                match bal {
                    BalancerPoolKind::Weighted { w0, w1 } => {
                        let (r_in, w_in, r_out, w_out) = if zero_for_one {
                            (x0, *w0, x1, *w1)
                        } else {
                            (x1, *w1, x0, *w0)
                        };
                        if w_in <= 1e-9 || w_out <= 1e-9 {
                            return 0.0;
                        }
                        ((r_out / w_out) / (r_in / w_in)) * gamma
                    }
                    BalancerPoolKind::Stable {
                        amp,
                        scale0,
                        scale1,
                        other_scaled_balances,
                    } => {
                        let rate = balancer_stable_marginal_rate(
                            *amp,
                            x0 * *scale0,
                            x1 * *scale1,
                            *scale0,
                            *scale1,
                            other_scaled_balances,
                            zero_for_one,
                        );
                        rate * gamma
                    }
                }
            }
        }
    }

    /// Pure quote in human units (does not mutate self).
    /// Returns 0.0 if pool is empty or CL price move exceeds precomputed sqrt bounds.
    pub fn quote_human(&self, zero_for_one: bool, amount_in_human: f64) -> f64 {
        if amount_in_human <= 0.0 {
            return 0.0;
        }
        let (s_in, s_out) = if zero_for_one {
            (self.scale0, self.scale1)
        } else {
            (self.scale1, self.scale0)
        };
        let amt_in_raw = amount_in_human * s_in;

        match self.dex_kind {
            DexKind::UniV2 => {
                let (r_in, r_out) = if zero_for_one {
                    (self.r0_raw, self.r1_raw)
                } else {
                    (self.r1_raw, self.r0_raw)
                };
                if r_in <= 1.0 || r_out <= 1.0 {
                    return 0.0;
                }
                let amt_after_tax = if self.extra_tax_ppm == 0 {
                    amt_in_raw
                } else {
                    amt_in_raw * (1.0 - (self.extra_tax_ppm.min(999_999) as f64) / 1_000_000.0)
                };
                let amt_eff = amt_after_tax * (self.fee_num as f64);
                let out_raw = (amt_eff * r_out) / (r_in * 10000.0 + amt_eff);
                out_raw / s_out
            }
            DexKind::Solidly => {
                let x0 = self.r0_raw / self.scale0;
                let x1 = self.r1_raw / self.scale1;
                if x0 <= 1e-12 || x1 <= 1e-12 {
                    return 0.0;
                }
                let eff_in_h = amount_in_human * self.gamma();
                if !self.stable {
                    let (r_in, r_out) = if zero_for_one { (x0, x1) } else { (x1, x0) };
                    (eff_in_h * r_out) / (r_in + eff_in_h)
                } else {
                    let (x_in, y_out) = if zero_for_one { (x0, x1) } else { (x1, x0) };
                    solidly_stable_out(x_in, y_out, eff_in_h)
                }
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                if self.sqrt_p <= 0.0 || self.liquidity <= 0.0 {
                    return 0.0;
                }
                let gamma_v3 = 1.0 - (self.fee_ppm.min(950_000) as f64) / 1_000_000.0;
                let eff_raw = amt_in_raw * gamma_v3;
                let l = self.liquidity;
                let s = self.sqrt_p;
                let (s_lo, s_hi) = (self.s_lo, self.s_hi);
                let out_raw = if zero_for_one {
                    if s <= s_lo {
                        return 0.0;
                    }
                    let s_next = (l * s) / (l + eff_raw * s);
                    if s_next < s_lo {
                        return 0.0;
                    }
                    l * (s - s_next)
                } else {
                    if s >= s_hi {
                        return 0.0;
                    }
                    let s_next = s + eff_raw / l;
                    if s_next > s_hi {
                        return 0.0;
                    }
                    l * (1.0 / s - 1.0 / s_next)
                };
                let tax_mult = if self.extra_tax_ppm == 0 {
                    1.0
                } else {
                    1.0 - (self.extra_tax_ppm.min(999_999) as f64) / 1_000_000.0
                };
                ((out_raw * tax_mult) / s_out).max(0.0)
            }
            DexKind::MetricOmm => self
                .metric_quote_and_walk(zero_for_one, amount_in_human)
                .map_or(0.0, |(out_h, _, _)| out_h),
            DexKind::BalancerV2 => {
                let Some(bal) = self.balancer.as_deref() else {
                    return 0.0;
                };
                let x0 = self.r0_raw / self.scale0;
                let x1 = self.r1_raw / self.scale1;
                if x0 <= 1e-12 || x1 <= 1e-12 {
                    return 0.0;
                }
                let eff_in_h = amount_in_human * self.gamma();
                match bal {
                    BalancerPoolKind::Weighted { w0, w1 } => {
                        let (r_in, w_in, r_out, w_out) = if zero_for_one {
                            (x0, *w0, x1, *w1)
                        } else {
                            (x1, *w1, x0, *w0)
                        };
                        if amount_in_human >= r_in * 0.299 {
                            return 0.0;
                        }
                        balancer_weighted_out(r_in, w_in, r_out, w_out, eff_in_h)
                    }
                    BalancerPoolKind::Stable {
                        amp,
                        scale0,
                        scale1,
                        other_scaled_balances,
                    } => balancer_stable_out(
                        *amp,
                        x0 * *scale0,
                        x1 * *scale1,
                        *scale0,
                        *scale1,
                        other_scaled_balances,
                        zero_for_one,
                        eff_in_h,
                    ),
                }
            }
        }
    }

    /// Quote and update residual reserves/sqrtPrice in-place.
    pub fn apply_swap_human(&mut self, zero_for_one: bool, amount_in_human: f64) -> f64 {
        let metric_walk = if self.dex_kind == DexKind::MetricOmm {
            self.metric_quote_and_walk(zero_for_one, amount_in_human)
        } else {
            None
        };
        let out_h = if let Some((o, _, _)) = metric_walk {
            o
        } else {
            self.quote_human(zero_for_one, amount_in_human)
        };
        if out_h <= 0.0 {
            return 0.0;
        }
        let (s_in, s_out) = if zero_for_one {
            (self.scale0, self.scale1)
        } else {
            (self.scale1, self.scale0)
        };
        let in_raw = amount_in_human * s_in;
        let out_raw = out_h * s_out;

        match self.dex_kind {
            DexKind::UniV2 | DexKind::Solidly | DexKind::BalancerV2 => {
                if zero_for_one {
                    self.r0_raw += in_raw;
                    self.r1_raw = (self.r1_raw - out_raw).max(1.0);
                } else {
                    self.r1_raw += in_raw;
                    self.r0_raw = (self.r0_raw - out_raw).max(1.0);
                }
            }
            DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                let gamma_v3 = 1.0 - (self.fee_ppm.min(950_000) as f64) / 1_000_000.0;
                let eff_raw = in_raw * gamma_v3;
                if zero_for_one {
                    self.sqrt_p = (self.liquidity * self.sqrt_p) / (self.liquidity + eff_raw * self.sqrt_p);
                } else {
                    self.sqrt_p += eff_raw / self.liquidity;
                }
            }
            DexKind::MetricOmm => {
                let in_1e18 = amount_in_human * 1e18;
                let out_1e18 = out_h * 1e18;
                if zero_for_one {
                    self.r0_raw += in_1e18;
                    self.r1_raw = (self.r1_raw - out_1e18).max(0.0);
                    if let Some((_, frac, max_walk)) = metric_walk {
                        let shift = (1.0 - max_walk * frac).clamp(0.5, 1.0);
                        self.metric_sell *= shift;
                        self.metric_buy *= shift;
                    }
                } else {
                    self.r1_raw += in_1e18;
                    self.r0_raw = (self.r0_raw - out_1e18).max(0.0);
                    if let Some((_, frac, max_walk)) = metric_walk {
                        let shift = (1.0 + max_walk * frac).clamp(1.0, 2.0);
                        self.metric_sell *= shift;
                        self.metric_buy *= shift;
                    }
                }
            }
        }
        out_h
    }
}

/// Exact integer UniV2 constant-product formula matching Solidity `DagArbExecutor`.
#[allow(dead_code)]
pub fn univ2_amount_out_u256(amount_in: U256, r_in: U256, r_out: U256, fee_num: u16) -> U256 {
    if amount_in.is_zero() || r_in.is_zero() || r_out.is_zero() {
        return U256::ZERO;
    }
    let amt_fee = amount_in * U256::from(fee_num);
    let num = amt_fee * r_out;
    let den = r_in * U256::from(10_000u64) + amt_fee;
    if den.is_zero() {
        U256::ZERO
    } else {
        num / den
    }
}

/// BalancerV2 WeightedMath exact out given in. Enforces Vault 30% max-in guard.
fn balancer_weighted_out(b_in: f64, w_in: f64, b_out: f64, w_out: f64, dx_eff_human: f64) -> f64 {
    if b_in <= 1e-12 || b_out <= 1e-12 || w_in <= 1e-9 || w_out <= 1e-9 || dx_eff_human <= 0.0 {
        return 0.0;
    }
    if dx_eff_human >= b_in * 0.30 {
        return 0.0;
    }
    let ratio = b_in / (b_in + dx_eff_human);
    let out = b_out * (1.0 - ratio.powf(w_in / w_out));
    if out.is_finite() && out > 0.0 && out < b_out * 0.999 {
        out
    } else {
        0.0
    }
}

/// Curve / Balancer V2 StableMath invariant D across `n` scaled balances (`Ann = A * n`).
fn balancer_stable_d(amp: f64, balances: &[f64]) -> f64 {
    let n = balances.len() as f64;
    if amp <= 0.0 || n < 2.0 || balances.iter().any(|&b| b <= 1e-12 || !b.is_finite()) {
        return 0.0;
    }
    let sum: f64 = balances.iter().sum();
    if sum <= 0.0 {
        return 0.0;
    }
    let ann = amp * n;
    let mut d = sum;
    for _ in 0..64 {
        let mut d_p = d;
        for &b in balances {
            d_p = (d_p * d) / (b * n);
        }
        let prev_d = d;
        let num = (ann * sum + d_p * n) * d;
        let den = (ann - 1.0) * d + (n + 1.0) * d_p;
        if den <= 0.0 || !num.is_finite() {
            return 0.0;
        }
        d = num / den;
        if (d - prev_d).abs() <= d * 1e-13 + 1e-12 {
            return d;
        }
    }
    d
}

fn balancer_stable_marginal_rate(
    amp: f64,
    x0_scaled: f64,
    x1_scaled: f64,
    scale0: f64,
    scale1: f64,
    other_scaled: &[f64],
    zero_for_one: bool,
) -> f64 {
    if scale0 <= 0.0 || scale1 <= 0.0 || x0_scaled <= 1e-12 || x1_scaled <= 1e-12 {
        return 0.0;
    }
    let mut bals = [0.0_f64; 8];
    let n = (2 + other_scaled.len()).min(8);
    bals[0] = x0_scaled;
    bals[1] = x1_scaled;
    for (i, &b) in other_scaled.iter().take(6).enumerate() {
        if b <= 1e-12 {
            return 0.0;
        }
        bals[2 + i] = b;
    }
    let d = balancer_stable_d(amp, &bals[..n]);
    if d <= 0.0 {
        return 0.0;
    }
    let n_f = n as f64;
    let ann = amp * n_f;
    let mut d_p = d;
    for &b in &bals[..n] {
        d_p = (d_p * d) / (b * n_f);
    }
    let (x_in, x_out, s_in, s_out) = if zero_for_one {
        (x0_scaled, x1_scaled, scale0, scale1)
    } else {
        (x1_scaled, x0_scaled, scale1, scale0)
    };
    let deriv_scaled = (ann + d_p / x_in) / (ann + d_p / x_out);
    let rate = deriv_scaled * (s_in / s_out);
    if rate.is_finite() && rate > 0.0 {
        rate
    } else {
        0.0
    }
}

fn balancer_stable_out(
    amp: f64,
    x0_scaled: f64,
    x1_scaled: f64,
    scale0: f64,
    scale1: f64,
    other_scaled: &[f64],
    zero_for_one: bool,
    dx_eff_human: f64,
) -> f64 {
    if dx_eff_human <= 0.0 || scale0 <= 0.0 || scale1 <= 0.0 || x0_scaled <= 1e-12 || x1_scaled <= 1e-12 {
        return 0.0;
    }
    let mut bals = [0.0_f64; 8];
    let n = (2 + other_scaled.len()).min(8);
    bals[0] = x0_scaled;
    bals[1] = x1_scaled;
    for (i, &b) in other_scaled.iter().take(6).enumerate() {
        if b <= 1e-12 {
            return 0.0;
        }
        bals[2 + i] = b;
    }
    let d = balancer_stable_d(amp, &bals[..n]);
    if d <= 0.0 {
        return 0.0;
    }
    let (in_idx, out_idx, s_in, s_out) = if zero_for_one {
        (0usize, 1usize, scale0, scale1)
    } else {
        (1usize, 0usize, scale1, scale0)
    };
    let y_old = bals[out_idx];
    bals[in_idx] += dx_eff_human * s_in;

    let n_f = n as f64;
    let ann = amp * n_f;
    let mut sum = 0.0_f64;
    let mut c = d;
    for (idx, &b) in bals[..n].iter().enumerate() {
        if idx == out_idx {
            continue;
        }
        sum += b;
        c = (c * d) / (b * n_f);
    }
    c = (c * d) / (ann * n_f);
    let b_quad = sum + d / ann;
    let mut y = d;
    for _ in 0..64 {
        let prev_y = y;
        let den = 2.0 * y + b_quad - d;
        if den <= 0.0 {
            return 0.0;
        }
        y = (y * y + c) / den;
        if (y - prev_y).abs() <= y * 1e-13 + 1e-12 {
            break;
        }
    }
    let dy_scaled = y_old - y;
    if !dy_scaled.is_finite() || dy_scaled <= 0.0 || dy_scaled >= y_old * 0.999 {
        return 0.0;
    }
    dy_scaled / s_out
}

/// Solidly stable curve `x^3 * y + y^3 * x = k` solver via Newton-Raphson.
pub fn solidly_stable_out(x0: f64, y0: f64, dx_eff: f64) -> f64 {
    if x0 <= 0.0 || y0 <= 0.0 || dx_eff <= 0.0 {
        return 0.0;
    }
    let k = x0 * x0 * x0 * y0 + x0 * y0 * y0 * y0;
    let x1 = x0 + dx_eff;
    let x1_3 = x1 * x1 * x1;
    let mut y = y0;
    for _ in 0..8 {
        let f = x1 * y * y * y + x1_3 * y - k;
        let df = 3.0 * x1 * y * y + x1_3;
        if df <= 0.0 {
            return 0.0;
        }
        let next_y = y - f / df;
        if (next_y - y).abs() <= 1e-12 * y.max(1.0) {
            y = next_y;
            break;
        }
        y = next_y.max(1e-18);
    }
    (y0 - y).max(0.0)
}

#[inline]
pub fn pow10(dec: u8) -> f64 {
    const TABLE: [f64; 25] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
        1e16, 1e17, 1e18, 1e19, 1e20, 1e21, 1e22, 1e23, 1e24,
    ];
    TABLE.get(dec as usize).copied().unwrap_or_else(|| 10f64.powi(dec as i32))
}

#[inline]
pub fn v3_sqrt_bounds(
    s: f64,
    tick: i32,
    tick_spacing: i32,
    allow_multi_tick: bool,
) -> (f64, f64) {
    let ts = tick_spacing.max(1);
    let tick_lo = tick.div_euclid(ts) * ts;
    let tick_hi = tick_lo + ts;
    let s_tick_lo = 1.0001_f64.powf((tick_lo as f64) * 0.5);
    let s_tick_hi = 1.0001_f64.powf((tick_hi as f64) * 0.5);
    let tight_lo = s_tick_lo * 1.000003;
    let tight_hi = s_tick_hi * 0.999997;
    if allow_multi_tick && ts <= 60 && s > 0.0 {
        let extra = if ts == 1 {
            260
        } else if ts <= 10 {
            240
        } else {
            180
        };
        let lo = if s > s_tick_lo * (1.0 + 1e-9) {
            1.0001_f64.powf(((tick_lo - extra) as f64) * 0.5)
        } else {
            tight_lo
        };
        let hi = if s < s_tick_hi * (1.0 - 1e-9) {
            1.0001_f64.powf(((tick_hi + extra) as f64) * 0.5)
        } else {
            tight_hi
        };
        (lo, hi)
    } else {
        (tight_lo, tight_hi)
    }
}

#[inline]
pub fn sqrt_price_x96_to_f64(sq: U256) -> f64 {
    if sq.is_zero() {
        return 0.0;
    }
    let two_96 = 79228162514264337593543950336.0_f64; // 2^96
    u256_to_f64(sq) / two_96
}

#[inline]
pub fn u256_to_f64(v: U256) -> f64 {
    if v.is_zero() {
        return 0.0;
    }
    // Convert top 128/256 bits safely via byte representation
    let limbs = v.as_limbs();
    let mut acc = 0.0_f64;
    let mut mult = 1.0_f64;
    let step = 18446744073709551616.0_f64; // 2^64
    for &limb in limbs {
        acc += (limb as f64) * mult;
        mult *= step;
    }
    acc
}

pub fn human_to_u256(amount: f64, decimals: u8) -> U256 {
    if amount <= 0.0 || !amount.is_finite() {
        return U256::ZERO;
    }
    let scale = pow10(decimals);
    let raw = (amount * scale).floor();
    if raw <= 0.0 || !raw.is_finite() {
        return U256::ZERO;
    }
    if raw < (u128::MAX as f64) {
        U256::from(raw as u128)
    } else {
        // Divide cleanly into high/low if ever > 2^128
        let s = format!("{:.0}", raw);
        U256::from_str_radix(&s, 10).unwrap_or(U256::ZERO)
    }
}

pub fn u256_to_human(val: U256, decimals: u8) -> f64 {
    u256_to_f64(val) / pow10(decimals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_univ2_exact_u256_vs_residual_f64() {
        let t0 = Address::repeat_byte(0x11);
        let t1 = Address::repeat_byte(0x22);
        let r0 = U256::from(1_000_000u128) * U256::from(10u128.pow(18));
        let r1 = U256::from(2_000_000u128) * U256::from(10u128.pow(18));
        let pool = PoolState {
            address: Address::repeat_byte(0xaa),
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
            r0_raw: r0,
            r1_raw: r1,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: false,
            is_hub_1: false,
            cached_depth_usd: 3_000_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        let amt_in = U256::from(1_000u128) * U256::from(10u128.pow(18));
        let exact = u256_to_human(univ2_amount_out_u256(amt_in, r0, r1, 9975), 18);
        let fast = pool.to_residual().quote_human(true, 1000.0);
        assert!((exact - fast).abs() < 1e-9, "exact={exact}, fast={fast}");
    }

    #[test]
    fn test_solidly_stable_quote() {
        let out = solidly_stable_out(1_000_000.0, 1_000_000.0, 10_000.0 * 0.9995);
        // Stable invariant keeps slippage minimal on 1% trade
        assert!(out > 9990.0 && out < 9996.0, "stable out={out}");
    }

    #[test]
    fn test_known_side_depth_prevents_v3_spoofing() {
        // Pool where token0=USDT (dec=18), token1=WBNB (dec=18), extreme high tick with tiny real USDT in-tick reserve
        let usdt = Address::repeat_byte(0x01);
        let wbnb = Address::repeat_byte(0x02);
        // tick = 200_000 -> price ratio ~ 1.0001^200_000 ~ 4.8e8; sqrtP ~ 22,000
        let tick = 200_000;
        let sqrt_p = 1.0001_f64.powf((tick as f64) * 0.5);
        let sqrt_p_x96 = U256::from((sqrt_p * 79228162514264337593543950336.0_f64) as u128);
        let pool = PoolState {
            address: Address::repeat_byte(0xbb),
            factory_name: "PancakeV3".into(),
            dex_kind: DexKind::PancakeV3,
            token0: usdt,
            token1: wbnb,
            dec0: 18,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::ZERO,
            r1_raw: U256::ZERO,
            sqrt_price_x96: sqrt_p_x96,
            liquidity: 10u128.pow(18),
            tick,
            tick_spacing: 10,
            allow_multi_tick: false,
            is_hub_0: false,
            is_hub_1: false,
            cached_depth_usd: 0.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        let real_usdt_side_depth = pool.known_side_depth_usd(usdt, 1.0);
        assert!(real_usdt_side_depth < 1.0, "spoofed depth={real_usdt_side_depth}");
    }

    #[test]
    fn test_v3_parked_at_tick_boundary_blocks_phantom_multitick() {
        let cake = Address::repeat_byte(0x11);
        let usdc = Address::repeat_byte(0x22);
        let tick = 6600;
        let sqrt_p = 1.0001_f64.powf((tick as f64) * 0.5);
        let sqrt_p_x96 = U256::from((sqrt_p * 79228162514264337593543950336.0_f64) as u128);

        for ts in [200, 50] {
            let pool = PoolState {
                address: Address::repeat_byte(0x79),
                factory_name: "PancakeV3".into(),
                dex_kind: DexKind::PancakeV3,
                token0: cake,
                token1: usdc,
                dec0: 18,
                dec1: 18,
                fee_num: 0,
                fee_ppm: if ts == 200 { 10000 } else { 2500 },
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: U256::ZERO,
                r1_raw: U256::ZERO,
                sqrt_price_x96: sqrt_p_x96,
                liquidity: 322_519_155_349_519_951_062u128,
                tick,
                tick_spacing: ts,
                allow_multi_tick: true,
                is_hub_0: true,
                is_hub_1: true,
                cached_depth_usd: 1000.0,
                last_updated_block: 1_000_000,
                v4_key: None,
                metric: None,
                balancer: None,
            };
            assert_eq!(pool.to_residual().quote_human(true, 1.0), 0.0, "ts={ts} zfo must be 0.0");
            assert_eq!(pool.max_input_usd(cake, 1.63), 0.0, "ts={ts} max_in_usd zfo must be 0.0");
            assert!(
                pool.to_residual().quote_human(false, 1.0) > 0.0,
                "ts={ts} upward tick interior must still quote"
            );
        }
    }

    #[test]
    fn test_v2_dislocated_pool_max_input_allows_full_rebalance() {
        let t0 = Address::repeat_byte(0x33);
        let wbnb = Address::repeat_byte(0xbb);
        // Drained token0 side ($120 = 200 tokens @ $0.60) vs bloated WBNB side ($3,000 = 5 WBNB @ $600)
        let r0 = U256::from(200u128) * U256::from(10u128.pow(18));
        let r1 = U256::from(5u128) * U256::from(10u128.pow(18));
        let mut pool = PoolState {
            address: Address::repeat_byte(0xcc),
            factory_name: "PancakeV1".into(),
            dex_kind: DexKind::UniV2,
            token0: t0,
            token1: wbnb,
            dec0: 18,
            dec1: 18,
            fee_num: 9980,
            fee_ppm: 2000,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: r0,
            r1_raw: r1,
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: false,
            is_hub_1: true,
            cached_depth_usd: 3_120.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        pool.refresh_depth(0.60, 600.0);
        let max_in = pool.max_input_usd(t0, 0.60);
        assert!(
            max_in > 2_400.0,
            "dislocated V2 max_input_usd must exceed $2400 (got {max_in})"
        );
    }

    #[test]
    fn test_metric_omm_unsorted_quote_spread_and_diminishing_returns() {
        // Unsorted WBNB/USDT Metric pool: token0 = 0xbb4c (WBNB) > token1 = 0x55d3 (USDT)
        let wbnb = Address::repeat_byte(0xbb);
        let usdt = Address::repeat_byte(0x55);
        let sell_q64 = (548.0 * Q64_F64) as u128;
        let buy_q64 = (550.0 * Q64_F64) as u128;
        let pool = PoolState {
            address: Address::repeat_byte(0xf3),
            factory_name: "MetricOmm".into(),
            dex_kind: DexKind::MetricOmm,
            token0: wbnb,
            token1: usdt,
            dec0: 18,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 36,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: human_to_u256(100.0, 18),
            r1_raw: human_to_u256(55_000.0, 18),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 115_000.0,
            last_updated_block: 1,
            v4_key: None,
            metric: Some(MetricPoolState {
                sell_price_x64: sell_q64,
                buy_price_x64: buy_q64,
                num_bins: 40,
                active_bin: 0,
                paused: false,
            }),
            balancer: None,
        };

        // Spot sell WBNB -> USDT ~ 548 (with <= 2bps conservative haircut); buy WBNB w/ USDT ~ 1 / 550
        let r_sell = pool.marginal_rate_human(wbnb);
        let r_buy = pool.marginal_rate_human(usdt);
        assert!(r_sell < 548.0 && r_sell > 548.0 * 0.9998, "r_sell={r_sell}");
        assert!(r_buy < 1.0 / 550.0 && r_buy > (1.0 / 550.0) * 0.9998, "r_buy={r_buy}");
        assert!(r_sell * r_buy < 1.0, "spread must prevent zero-profit loop");

        // Successive tranches see monotonically diminishing marginal output and cap at 80% reserve
        let mut res = pool.to_residual();
        let out1 = res.apply_swap_human(true, 10.0);
        let out2 = res.apply_swap_human(true, 10.0);
        assert!(out1 > 5_400.0 && out1 < 5_480.0, "out1={out1}");
        assert!(out2 > 0.0 && out2 < out1, "diminishing return failed: out1={out1}, out2={out2}");
        assert_eq!(res.quote_human(true, 100.0), 0.0, "must block >80% bin reserve drain");

        // Adverse active_bin = -2 lowers zfo=true spot rate and quote relative to active_bin = 0
        let mut skewed_pool = pool.clone();
        skewed_pool.metric.as_mut().unwrap().active_bin = -2;
        let r_sell_skew = skewed_pool.marginal_rate_human(wbnb);
        let q_zero = pool.to_residual().quote_human(true, 1.0);
        let q_skew = skewed_pool.to_residual().quote_human(true, 1.0);
        assert!(r_sell_skew < r_sell, "expected skewed rate {r_sell_skew} < {r_sell}");
        assert!(q_skew < q_zero, "expected skewed quote {q_skew} < {q_zero}");

        // Paused pool is inactive and quotes 0.0
        let mut paused_pool = pool;
        paused_pool.metric.as_mut().unwrap().paused = true;
        assert!(!paused_pool.is_active());
        assert_eq!(paused_pool.marginal_rate_human(wbnb), 0.0);
        assert_eq!(paused_pool.to_residual().quote_human(true, 1.0), 0.0);
    }

    #[test]
    fn test_balancer_v2_weighted_80_20_math_and_max_in_guard() {
        let bal = Address::repeat_byte(0xba);
        let weth = Address::repeat_byte(0xc0);
        // 80/20 BAL/WETH pool: 1,000,000 BAL (80%) and 1,000 WETH (20%), 1% fee (10_000 ppm)
        let pool = PoolState {
            address: Address::repeat_byte(0x01),
            factory_name: "BalancerV2".into(),
            dex_kind: DexKind::BalancerV2,
            token0: bal,
            token1: weth,
            dec0: 18,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 10_000,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: human_to_u256(1_000_000.0, 18),
            r1_raw: human_to_u256(1_000.0, 18),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: false,
            is_hub_1: true,
            cached_depth_usd: 0.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: Some(BalancerPoolState {
                vault: Address::repeat_byte(0xbb),
                pool_id: B256::repeat_byte(0x11),
                kind: BalancerPoolKind::Weighted { w0: 0.8, w1: 0.2 },
            }),
        };

        // Spot WETH -> BAL: (x0 / w0) / (x1 / w1) * (1 - 0.01) = (1e6/0.8)/(1e3/0.2)*0.99 = 250 * 0.99 = 247.5
        let rate_weth_to_bal = pool.marginal_rate_human(weth);
        assert!((rate_weth_to_bal - 247.5).abs() < 1e-6, "rate={rate_weth_to_bal}");

        // Quote 10 WETH -> BAL: exp = w1/w0 = 0.25; eff = 9.9 WETH
        let out = pool.to_residual().quote_human(false, 10.0);
        let expected = 1_000_000.0 * (1.0 - (1000.0_f64 / 1009.9_f64).powf(0.25));
        assert!((out - expected).abs() < 1e-6, "out={out}, expected={expected}");

        // Known-side WETH (20% weight) depth @ $3000/WETH -> total value = 1000*3000 / 0.2 = $15M
        let depth = pool.known_side_depth_usd(weth, 3000.0);
        assert!((depth - 15_000_000.0).abs() < 1.0, "depth={depth}");

        // MaxInRatio 30% guard blocks >= 300 WETH in single swap
        assert_eq!(pool.to_residual().quote_human(false, 310.0), 0.0);
    }

    #[test]
    fn test_balancer_v2_composable_stable_3pool_and_rate_scaling() {
        let wsteth = Address::repeat_byte(0x7f);
        let weth = Address::repeat_byte(0xc0);
        // wstETH/WETH ComposableStable (amp=500, wstETH rate scale = 1.18, 3rd asset scaled bal = 10,000)
        let pool = PoolState {
            address: Address::repeat_byte(0x02),
            factory_name: "BalancerV2".into(),
            dex_kind: DexKind::BalancerV2,
            token0: wsteth,
            token1: weth,
            dec0: 18,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 400, // 4 bps
            extra_tax_ppm: 0,
            stable: true,
            r0_raw: human_to_u256(10_000.0, 18),
            r1_raw: human_to_u256(11_800.0, 18), // perfectly balanced: 10000*1.18 == 11800*1.0
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 0.0,
            last_updated_block: 1,
            v4_key: None,
            metric: None,
            balancer: Some(BalancerPoolState {
                vault: Address::repeat_byte(0xbb),
                pool_id: B256::repeat_byte(0x22),
                kind: BalancerPoolKind::Stable {
                    amp: 500.0,
                    scale0: 1.18,
                    scale1: 1.0,
                    other_scaled_balances: vec![10_000.0],
                },
            }),
        };

        // At balanced scaled reserves (11800, 11800, 10000), selling 1 wstETH yields ~1.18 * 0.9996 WETH
        let rate = pool.marginal_rate_human(wsteth);
        assert!(
            (rate - 1.18 * 0.9996).abs() < 1e-3,
            "balanced rate={rate}, expected ~{}",
            1.18 * 0.9996
        );
        let out = pool.to_residual().quote_human(true, 100.0);
        assert!(out > 117.8 && out < 118.0, "stable out for 100 wstETH = {out}");
    }

    #[test]
    fn test_metric_omm_mixed_decimals_18_6_eth_scaling() {
        let link = Address::repeat_byte(0x51);
        let usdc = Address::repeat_byte(0xa0);
        // ETH LINK/USDC Metric pool: token0=LINK(dec=18), token1=USDC(dec=6);
        // slot 1 reserves are normalized to 1e18 and Q64.64 prices are in human units (USDC/LINK).
        let sell_q64 = (8.75592 * Q64_F64) as u128;
        let buy_q64 = (8.77289 * Q64_F64) as u128;
        let pool = PoolState {
            address: Address::repeat_byte(0xda),
            factory_name: "Metric".into(),
            dex_kind: DexKind::MetricOmm,
            token0: link,
            token1: usdc,
            dec0: 18,
            dec1: 6,
            fee_num: 10_000,
            fee_ppm: 0,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: human_to_u256(6_706.94, 18),
            r1_raw: human_to_u256(99_465.67, 18),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 1,
            allow_multi_tick: false,
            is_hub_0: false,
            is_hub_1: true,
            cached_depth_usd: 0.0,
            last_updated_block: 1,
            v4_key: None,
            metric: Some(MetricPoolState {
                sell_price_x64: sell_q64,
                buy_price_x64: buy_q64,
                num_bins: 40,
                active_bin: 0,
                paused: false,
            }),
            balancer: None,
        };

        let (h0, h1) = pool.human_reserves();
        assert!((h0 - 6_706.94).abs() < 1e-3, "h0={h0}");
        assert!((h1 - 99_465.67).abs() < 1e-3, "h1={h1}");

        let r_link = pool.marginal_rate_human(link);
        let r_usdc = pool.marginal_rate_human(usdc);
        assert!(r_link > 8.70 && r_link < 8.76, "r_link={r_link}");
        assert!(r_usdc > 0.113 && r_usdc < 0.115, "r_usdc={r_usdc}");

        let depth = pool.usable_depth_usd(8.76, 1.0);
        assert!(depth > 150_000.0 && depth < 165_000.0, "depth={depth}");

        let mut res = pool.to_residual();
        let out_usdc = res.apply_swap_human(true, 100.0);
        assert!(out_usdc > 865.0 && out_usdc < 876.0, "out_usdc={out_usdc}");
        let rem_usdc_h = res.r1_raw / 1e18;
        assert!(
            rem_usdc_h > 98_580.0 && rem_usdc_h < 98_610.0,
            "rem_usdc_h={rem_usdc_h}"
        );
    }
}
