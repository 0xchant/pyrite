#include "cfmm_solver.h"
#include <cuda_runtime.h>
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <vector>

#define CUDA_CHECK(call)                                                       \
    do {                                                                       \
        cudaError_t _e = (call);                                               \
        if (_e != cudaSuccess) {                                               \
            std::fprintf(stderr, "CUDA error %s:%d: %s\n", __FILE__, __LINE__, \
                         cudaGetErrorString(_e));                              \
            return -1;                                                         \
        }                                                                      \
    } while (0)

__host__ __device__ inline double pool_gamma(const PoolData& p) {
    double g = 1.0 - p.fee_ppm * 1e-6;
    return (g > 1e-6 && g <= 1.0) ? g : 0.997;
}

__host__ __device__ inline double stable_get_D(double x0, double x1, double A) {
    double S = x0 + x1;
    if (S <= 0.0 || x0 <= 0.0 || x1 <= 0.0) return 0.0;
    double D = S;
    double Ann = 4.0 * fmax(A, 1e-6);
    for (int it = 0; it < 12; ++it) {
        double D_p = (D * D / (2.0 * x0)) * (D / (2.0 * x1));
        double D_prev = D;
        double num = (Ann * S + 2.0 * D_p) * D;
        double den = (Ann - 1.0) * D + 3.0 * D_p;
        if (den <= 0.0) break;
        D = num / den;
        if (fabs(D - D_prev) <= 1e-13 * fmax(1.0, D)) break;
    }
    return D;
}

__host__ __device__ inline double stable_get_y(double x_new, double D, double A) {
    if (x_new <= 0.0 || D <= 0.0) return 0.0;
    double Ann = 4.0 * fmax(A, 1e-6);
    double B = x_new + D / Ann - D;
    double C = (D * D / (4.0 * Ann * x_new)) * D;
    double disc = B * B + 4.0 * C;
    double root = sqrt(fmax(0.0, disc));
    if (B >= 0.0) {
        double den = B + root;
        return (den > 0.0) ? (2.0 * C / den) : 0.0;
    }
    return 0.5 * (-B + root);
}

__host__ __device__ inline double stable_neg_dydx(double x, double y, double D, double A) {
    if (x <= 1e-18 || y <= 1e-18 || D <= 0.0) return 0.0;
    double d3_4 = 0.25 * D * D * D;
    double fx = 4.0 * A + d3_4 / (x * x * y);
    double fy = 4.0 * A + d3_4 / (x * y * y);
    return (fy > 0.0) ? (fx / fy) : 0.0;
}

__host__ __device__ inline double solidly_stable_quote(double x0, double y0, double dx_eff) {
    if (x0 <= 0.0 || y0 <= 0.0 || dx_eff <= 0.0) return 0.0;
    double S = x0 + y0;
    double xn = x0 / S;
    double yn = y0 / S;
    double d = dx_eff / S;
    double k = xn * xn * xn * yn + xn * yn * yn * yn;
    double x1 = xn + d;
    double x1_3 = x1 * x1 * x1;
    if (x1_3 <= 0.0) return 0.0;
    double y = fmin(yn, k / x1_3);
    for (int it = 0; it < 8; ++it) {
        double f = x1 * y * y * y + x1_3 * y - k;
        double df = 3.0 * x1 * y * y + x1_3;
        if (df <= 0.0) break;
        double next_y = y - f / df;
        if (fabs(next_y - y) <= 1e-13 * fmax(1e-18, y)) {
            y = next_y;
            break;
        }
        y = fmax(1e-18, next_y);
    }
    return fmax(0.0, (yn - y) * S);
}

__host__ __device__ inline double solidly_stable_spot(double x0, double y0) {
    if (x0 <= 1e-18 || y0 <= 1e-18) return 0.0;
    double S = x0 + y0;
    double x = x0 / S;
    double y = y0 / S;
    double den = x * x * x + 3.0 * x * y * y;
    if (den <= 0.0) return 0.0;
    return (3.0 * x * x * y + y * y * y) / den;
}

__host__ __device__ inline void pool_tokens(const PoolData& p, int dir, int& tok_in, int& tok_out) {
    if (dir == 0) {
        tok_in = p.token0;
        tok_out = p.token1;
    } else {
        tok_in = p.token1;
        tok_out = p.token0;
    }
}

__host__ __device__ inline double pool_quote(
    const PoolData& p,
    const double* __restrict__ tick_sp,
    const double* __restrict__ tick_liq,
    int dir,
    double dx
) {
    if (dx <= 0.0) return 0.0;
    double gamma = pool_gamma(p);

    if (p.pool_type == POOL_UNIV2) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        if (rin <= 0.0 || rout <= 0.0) return 0.0;
        double dx_eff = gamma * dx;
        return (rout * dx_eff) / (rin + dx_eff);
    }

    if (p.pool_type == POOL_CLMM) {
        int n = p.tick_count;
        if (n <= 0 || n > MAX_CLMM_SEGS_PER_POOL || p.sqrt_p <= 0.0 || !tick_sp || !tick_liq) return 0.0;
        int k = p.cur_seg;
        if (k < 0) k = 0;
        if (k >= n) k = n - 1;
        const double* sp = tick_sp + p.tick_offset;
        const double* tl = tick_liq + p.tick_offset;

        double s = p.sqrt_p;
        double x_rem = gamma * dx;
        double out_acc = 0.0;

        if (dir == 0) {
            for (int seg = k; seg >= 0 && x_rem > 0.0; --seg) {
                double L = tl[seg];
                double sa = sp[seg];
                if (L <= 0.0 || sa <= 0.0 || s <= sa) {
                    s = sa;
                    continue;
                }
                double max_x = L * (1.0 / sa - 1.0 / s);
                if (x_rem >= max_x) {
                    out_acc += L * (s - sa);
                    x_rem -= max_x;
                    s = sa;
                } else {
                    double s_next = (L * s) / (L + s * x_rem);
                    out_acc += L * (s - s_next);
                    x_rem = 0.0;
                    break;
                }
            }
        } else {
            for (int seg = k; seg < n && x_rem > 0.0; ++seg) {
                double L = tl[seg];
                double sb = sp[seg + 1];
                if (L <= 0.0 || sb <= 0.0 || s >= sb) {
                    s = sb;
                    continue;
                }
                double max_y = L * (sb - s);
                if (x_rem >= max_y) {
                    out_acc += L * (1.0 / s - 1.0 / sb);
                    x_rem -= max_y;
                    s = sb;
                } else {
                    double s_next = s + x_rem / L;
                    out_acc += L * (1.0 / s - 1.0 / s_next);
                    x_rem = 0.0;
                    break;
                }
            }
        }
        return out_acc;
    }

    if (p.pool_type == POOL_CURVE_STABLE) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        if (rin <= 0.0 || rout <= 0.0) return 0.0;
        double A = p.param0;
        double D = (p.liq > 0.0) ? p.liq : stable_get_D(p.r0, p.r1, A);
        double y_new = stable_get_y(rin + gamma * dx, D, A);
        return (y_new < rout && y_new > 0.0) ? (rout - y_new) : 0.0;
    }

    if (p.pool_type == POOL_PROP_BINS) {
        if (!tick_sp || !tick_liq) return 0.0;
        int n_bins = (dir == 0) ? p.tick_count : p.cur_seg;
        int off = p.tick_offset + ((dir == 0) ? 0 : p.tick_count);
        if (n_bins <= 0) return 0.0;
        double rem = gamma * dx;
        double out_acc = 0.0;
        for (int b = 0; b < n_bins && rem > 0.0; ++b) {
            double rate = tick_sp[off + b];
            double cap = tick_liq[off + b];
            if (rate <= 0.0 || cap <= 0.0) continue;
            double take = (rem < cap) ? rem : cap;
            out_acc += take * rate;
            rem -= take;
        }
        return out_acc;
    }

    if (p.pool_type == POOL_SOLIDLY_STABLE) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        return solidly_stable_quote(rin, rout, gamma * dx);
    }

    if (p.pool_type == POOL_BALANCER_V2) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        double w_in = (dir == 0) ? p.liq : p.param0;
        double w_out = (dir == 0) ? p.param0 : p.liq;
        if (rin <= 0.0 || rout <= 0.0 || w_in <= 0.0 || w_out <= 0.0) return 0.0;
        double alpha = w_in / w_out;
        double dx_eff = gamma * dx;
        return -rout * expm1(-alpha * log1p(dx_eff / rin));
    }

    if (p.pool_type == POOL_METRIC_OMM) {
        double out_avail = (dir == 0) ? p.r1 : p.r0;
        double top_rate = (dir == 0) ? p.sqrt_p : ((p.liq > 0.0) ? (1.0 / p.liq) : 0.0);
        double haircut = (dir == 0) ? p.param1 : p.param2;
        double max_walk = p.param0;
        if (out_avail <= 1e-12 || top_rate <= 0.0) return 0.0;
        double out_no_slip = dx * top_rate;
        if (out_no_slip >= out_avail * 0.80) return 0.0;
        double frac = out_no_slip / out_avail;
        double c_mult = (1.0 - haircut) * gamma;
        return fmax(0.0, out_no_slip * (1.0 - 0.5 * max_walk * frac) * c_mult);
    }

    return 0.0;
}

__host__ __device__ inline double pool_spot_rate(
    const PoolData& p,
    const double* __restrict__ tick_sp,
    const double* __restrict__ tick_liq,
    int dir
) {
    double gamma = pool_gamma(p);
    if (p.pool_type == POOL_UNIV2) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        if (rin <= 0.0 || rout <= 0.0) return 0.0;
        return gamma * (rout / rin);
    }
    if (p.pool_type == POOL_CLMM) {
        if (p.sqrt_p <= 0.0 || p.liq <= 0.0 || p.tick_count <= 0) return 0.0;
        double p01 = p.sqrt_p * p.sqrt_p;
        return (dir == 0) ? (gamma * p01) : (gamma / p01);
    }
    if (p.pool_type == POOL_CURVE_STABLE) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        double A = p.param0;
        double D = (p.liq > 0.0) ? p.liq : stable_get_D(p.r0, p.r1, A);
        return gamma * stable_neg_dydx(rin, rout, D, A);
    }
    if (p.pool_type == POOL_PROP_BINS) {
        if (!tick_sp || !tick_liq) return 0.0;
        int n_bins = (dir == 0) ? p.tick_count : p.cur_seg;
        int off = p.tick_offset + ((dir == 0) ? 0 : p.tick_count);
        if (n_bins <= 0 || tick_liq[off] <= 0.0) return 0.0;
        return gamma * tick_sp[off];
    }
    if (p.pool_type == POOL_SOLIDLY_STABLE) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        return gamma * solidly_stable_spot(rin, rout);
    }
    if (p.pool_type == POOL_BALANCER_V2) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        double w_in = (dir == 0) ? p.liq : p.param0;
        double w_out = (dir == 0) ? p.param0 : p.liq;
        if (rin <= 0.0 || rout <= 0.0 || w_in <= 0.0 || w_out <= 0.0) return 0.0;
        return gamma * (w_in / w_out) * (rout / rin);
    }
    if (p.pool_type == POOL_METRIC_OMM) {
        double out_avail = (dir == 0) ? p.r1 : p.r0;
        double top_rate = (dir == 0) ? p.sqrt_p : ((p.liq > 0.0) ? (1.0 / p.liq) : 0.0);
        double haircut = (dir == 0) ? p.param1 : p.param2;
        if (out_avail <= 1e-12 || top_rate <= 0.0) return 0.0;
        return top_rate * (1.0 - haircut) * gamma;
    }
    return 0.0;
}

__host__ __device__ inline void pool_opt_trade(
    const PoolData& p,
    const double* __restrict__ tick_sp,
    const double* __restrict__ tick_liq,
    double v0,
    double v1,
    int& best_dir,
    double& best_dx,
    double& best_dy
) {
    best_dir = 0;
    best_dx = 0.0;
    best_dy = 0.0;
    if (v0 <= 0.0 || v1 <= 0.0) return;

    double gamma = pool_gamma(p);
    double s0 = pool_spot_rate(p, tick_sp, tick_liq, 0);
    double s1 = pool_spot_rate(p, tick_sp, tick_liq, 1);
    double m0 = (v1 * s0) / v0;
    double m1 = (v0 * s1) / v1;
    if (m0 <= 1.0 && m1 <= 1.0) return;

    int dir = (m0 >= m1) ? 0 : 1;
    double v_in = (dir == 0) ? v0 : v1;
    double v_out = (dir == 0) ? v1 : v0;
    double dx = 0.0;

    if (p.pool_type == POOL_UNIV2) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        double root = sqrt((gamma * rout * v_out * rin) / v_in);
        if (root > rin) dx = (root - rin) / gamma;
    } else if (p.pool_type == POOL_CLMM) {
        int n = p.tick_count;
        int k = p.cur_seg;
        double s = p.sqrt_p;
        if (n > 0 && n <= MAX_CLMM_SEGS_PER_POOL && k >= 0 && k < n && s > 0.0 && tick_sp && tick_liq) {
            const double* sp = tick_sp + p.tick_offset;
            const double* tl = tick_liq + p.tick_offset;
            double x_eff = 0.0;
            if (dir == 0) {
                double s_target = sqrt(v0 / (gamma * v1));
                for (int seg = k; seg >= 0 && s > s_target; --seg) {
                    double L = tl[seg];
                    double sa = sp[seg];
                    if (L <= 0.0 || s <= sa) {
                        s = sa;
                        continue;
                    }
                    double s_next = (s_target > sa) ? s_target : sa;
                    x_eff += L * (1.0 / s_next - 1.0 / s);
                    s = s_next;
                    if (s_next == s_target) break;
                }
            } else {
                double s_target = sqrt((gamma * v0) / v1);
                for (int seg = k; seg < n && s < s_target; ++seg) {
                    double L = tl[seg];
                    double sb = sp[seg + 1];
                    if (L <= 0.0 || s >= sb) {
                        s = sb;
                        continue;
                    }
                    double s_next = (s_target < sb) ? s_target : sb;
                    x_eff += L * (s_next - s);
                    s = s_next;
                    if (s_next == s_target) break;
                }
            }
            dx = x_eff / gamma;
        }
    } else if (p.pool_type == POOL_CURVE_STABLE) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        double A = p.param0;
        double D = (p.liq > 0.0) ? p.liq : stable_get_D(p.r0, p.r1, A);
        if (D > 0.0) {
            double target_slope = v_in / (gamma * v_out);
            double lo = 0.0;
            double hi = fmax(rout * 0.05, 1e-6);
            double max_hi = rin * 24.0 + D * 4.0;
            double f_lo = stable_neg_dydx(rin, rout, D, A) - target_slope;
            double f_hi = f_lo;
            for (int w = 0; w < 10; ++w) {
                double xw = rin + gamma * hi;
                double yw = stable_get_y(xw, D, A);
                f_hi = stable_neg_dydx(xw, yw, D, A) - target_slope;
                if (f_hi <= 0.0 || hi >= max_hi) break;
                lo = hi;
                f_lo = f_hi;
                hi *= 2.0;
            }
            for (int b = 0; b < 16; ++b) {
                if (hi - lo <= 1e-12 * fmax(1.0, hi)) break;
                double mid = 0.5 * (lo + hi);
                double denom_s = f_lo - f_hi;
                if (denom_s > 1e-18) {
                    double sec = lo + f_lo * (hi - lo) / denom_s;
                    double pad = 0.08 * (hi - lo);
                    if (sec > lo + pad && sec < hi - pad) mid = sec;
                }
                double xm = rin + gamma * mid;
                double ym = stable_get_y(xm, D, A);
                double fm = stable_neg_dydx(xm, ym, D, A) - target_slope;
                if (fm > 0.0) {
                    lo = mid;
                    f_lo = fm;
                } else {
                    hi = mid;
                    f_hi = fm;
                }
            }
            dx = 0.5 * (lo + hi);
        }
    } else if (p.pool_type == POOL_PROP_BINS) {
        if (tick_sp && tick_liq) {
            int n_bins = (dir == 0) ? p.tick_count : p.cur_seg;
            int off = p.tick_offset + ((dir == 0) ? 0 : p.tick_count);
            double target = v_in / (gamma * v_out);
            double x_eff = 0.0;
            const double bw = 5e-4;
            for (int b = 0; b < n_bins; ++b) {
                double rate = tick_sp[off + b];
                double cap = tick_liq[off + b];
                if (rate <= 0.0 || cap <= 0.0) continue;
                double lo_r = rate * (1.0 - bw);
                double hi_r = rate * (1.0 + bw);
                if (target <= lo_r) {
                    x_eff += cap;
                } else if (target < hi_r) {
                    x_eff += cap * ((hi_r - target) / (hi_r - lo_r));
                    break;
                } else {
                    break;
                }
            }
            dx = x_eff / gamma;
        }
    } else if (p.pool_type == POOL_SOLIDLY_STABLE) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        if (rin > 1e-18 && rout > 1e-18) {
            double T = v_in / (gamma * v_out);
            double q = cbrt((T - 1.0) / (T + 1.0));
            if (q > 0.9999999999) q = 0.9999999999;
            if (q < -0.9999999999) q = -0.9999999999;
            double r_star = (1.0 + q) / (1.0 - q);
            double r0 = rout / rin;
            double num_k = r0 * (1.0 + r0 * r0);
            double den_k = r_star * (1.0 + r_star * r_star);
            if (den_k > 0.0 && num_k > den_k) {
                double x_star = rin * sqrt(sqrt(num_k / den_k));
                dx = fmax(0.0, (x_star - rin) / gamma);
            }
        }
    } else if (p.pool_type == POOL_BALANCER_V2) {
        double rin = (dir == 0) ? p.r0 : p.r1;
        double rout = (dir == 0) ? p.r1 : p.r0;
        double w_in = (dir == 0) ? p.liq : p.param0;
        double w_out = (dir == 0) ? p.param0 : p.liq;
        if (rin > 0.0 && rout > 0.0 && w_in > 0.0 && w_out > 0.0) {
            double alpha = w_in / w_out;
            double R = (gamma * alpha * rout * v_out) / (rin * v_in);
            if (R > 1.0) {
                dx = (rin / gamma) * expm1(log(R) / (alpha + 1.0));
            }
        }
    } else if (p.pool_type == POOL_METRIC_OMM) {
        double out_avail = (dir == 0) ? p.r1 : p.r0;
        double top_rate = (dir == 0) ? p.sqrt_p : ((p.liq > 0.0) ? (1.0 / p.liq) : 0.0);
        double haircut = (dir == 0) ? p.param1 : p.param2;
        double max_walk = p.param0;
        if (out_avail > 1e-12 && top_rate > 0.0 && max_walk > 1e-12) {
            double spot = top_rate * (1.0 - haircut) * gamma;
            double T = v_in / v_out;
            if (spot > T) {
                double frac = (1.0 - T / spot) / max_walk;
                if (frac > 0.7999) frac = 0.7999;
                if (frac > 0.0) dx = (frac * out_avail) / top_rate;
            }
        }
    }

    if (dx > 1e-13) {
        double dy = pool_quote(p, tick_sp, tick_liq, dir, dx);
        if (dy > 0.0) {
            best_dir = dir;
            best_dx = dx;
            best_dy = dy;
        }
    }
}

struct AcyclicScratch {
    double* from_flow; // [n_tokens]
    double* to_flow;   // [n_tokens]
    double* score;     // [n_tokens]
    int32_t* i0;       // [n_tokens] dist_from / tok_rank
    int32_t* i1;       // [n_tokens] dist_to / is_active
    uint8_t* u0;       // [n_tokens] rf
    uint8_t* u1;       // [n_tokens] rt
};

struct HostAcyclicScratch {
    double from_flow[MAX_TOKENS];
    double to_flow[MAX_TOKENS];
    double score[MAX_TOKENS];
    int32_t i0[MAX_TOKENS];
    int32_t i1[MAX_TOKENS];
    uint8_t u0[MAX_TOKENS];
    uint8_t u1[MAX_TOKENS];
    double bal[MAX_TOKENS];
    double sum_out_plan[MAX_TOKENS];
};

// Zero-spill Value-Weighted FAS Acyclic Orientation usable identically on GPU (shared scratch) & Host
__host__ __device__ inline int build_acyclic_rank_compact(
    const PoolData* __restrict__ pools,
    const double* __restrict__ nu_val,
    const int32_t* __restrict__ edge_pool,
    const int32_t* __restrict__ edge_dir,
    const double* __restrict__ edge_dx,
    int K,
    int n_tokens,
    int base_tok,
    uint8_t* __restrict__ edge_keep,
    int32_t* __restrict__ tok_order,
    const AcyclicScratch& sc
) {
    int32_t* dist_from = sc.i0;
    int32_t* dist_to = sc.i1;
    double* from_flow = sc.from_flow;
    double* to_flow = sc.to_flow;
    double* score = sc.score;
    uint8_t* rf = sc.u0;
    uint8_t* rt = sc.u1;

    for (int t = 0; t < n_tokens; ++t) {
        dist_from[t] = (t == base_tok) ? 0 : 999;
        dist_to[t] = (t == base_tok) ? 0 : 999;
        from_flow[t] = 0.0;
        to_flow[t] = 0.0;
        rf[t] = 0;
        rt[t] = 0;
    }

    for (int e = 0; e < K; ++e) {
        edge_keep[e] = (edge_dx[e] > 1e-12) ? 1 : 0;
        if (!edge_keep[e]) continue;
        int tin, tout;
        pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
        if (tin == base_tok && tout != base_tok) {
            from_flow[tout] += (nu_val ? nu_val[tin] * edge_dx[e] : edge_dx[e]);
        }
        if (tout == base_tok && tin != base_tok) {
            to_flow[tin] += (nu_val ? nu_val[tin] * edge_dx[e] : edge_dx[e]);
        }
    }

    for (int it = 0; it < n_tokens; ++it) {
        bool ch = false;
        for (int e = 0; e < K; ++e) {
            if (!edge_keep[e]) continue;
            int tin, tout;
            pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
            if (dist_from[tin] + 1 < dist_from[tout] && tout != base_tok) {
                dist_from[tout] = dist_from[tin] + 1;
                ch = true;
            }
            if (dist_to[tout] + 1 < dist_to[tin] && tin != base_tok) {
                dist_to[tin] = dist_to[tout] + 1;
                ch = true;
            }
        }
        if (!ch) break;
    }

    int m_act = 0;
    for (int e = 0; e < K; ++e) {
        if (!edge_keep[e]) continue;
        int tin, tout;
        pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
        if (dist_from[tin] >= 999 || dist_to[tout] >= 999) {
            edge_keep[e] = 0;
        } else {
            if (tin != base_tok) rf[tin] = 1;
            if (tout != base_tok) rf[tout] = 1;
        }
    }

    for (int t = 0; t < n_tokens; ++t) {
        if (t == base_tok || !rf[t]) {
            score[t] = 1e9;
            continue;
        }
        double df = (double)dist_from[t];
        double dt = (double)dist_to[t];
        double ratio = (to_flow[t] + 1e-9) / (from_flow[t] + 1e-9);
        score[t] = (df - dt) + 0.45 * tanh(log(ratio));
        tok_order[m_act++] = t;
    }

    // Insertion sort active intermediates by ascending (score[t], t)
    for (int i = 1; i < m_act; ++i) {
        int cur = tok_order[i];
        int j = i - 1;
        while (j >= 0 && (score[tok_order[j]] > score[cur] || (score[tok_order[j]] == score[cur] && tok_order[j] > cur))) {
            tok_order[j + 1] = tok_order[j];
            --j;
        }
        tok_order[j + 1] = cur;
    }

    int32_t* tok_rank = dist_from; // safe overwrite after scores computed
    for (int t = 0; t < n_tokens; ++t) tok_rank[t] = 9999;
    tok_rank[base_tok] = -1;
    for (int i = 0; i < m_act; ++i) tok_rank[tok_order[i]] = i;

    // 3-pass value-weighted FAS adjacent-swap hill climb over active intermediates
    if (m_act >= 2 && m_act <= 96) {
        for (int pass = 0; pass < 3; ++pass) {
            bool swapped = false;
            for (int i = 0; i < m_act - 1; ++i) {
                int u = tok_order[i];
                int v = tok_order[i + 1];
                double w_uv = 0.0;
                double w_vu = 0.0;
                for (int e = 0; e < K; ++e) {
                    if (!edge_keep[e]) continue;
                    int tin, tout;
                    pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
                    if (tin == u && tout == v) {
                        w_uv += (nu_val ? nu_val[tin] * edge_dx[e] : edge_dx[e]);
                    } else if (tin == v && tout == u) {
                        w_vu += (nu_val ? nu_val[tin] * edge_dx[e] : edge_dx[e]);
                    }
                }
                if (w_vu > w_uv * 1.0001) {
                    tok_order[i] = v;
                    tok_order[i + 1] = u;
                    tok_rank[v] = i;
                    tok_rank[u] = i + 1;
                    swapped = true;
                }
            }
            if (!swapped) break;
        }
    }

    for (int e = 0; e < K; ++e) {
        if (!edge_keep[e]) continue;
        int tin, tout;
        pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
        if (tin != base_tok && tout != base_tok && tok_rank[tin] >= tok_rank[tout]) {
            edge_keep[e] = 0;
        }
    }

    for (int t = 0; t < n_tokens; ++t) {
        rf[t] = (t == base_tok) ? 1 : 0;
        rt[t] = (t == base_tok) ? 1 : 0;
    }
    for (int pass = 0; pass < 4; ++pass) {
        for (int e = 0; e < K; ++e) {
            if (!edge_keep[e]) continue;
            int tin, tout;
            pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
            if (tin == base_tok && tout != base_tok) rf[tout] = 1;
            if (tout == base_tok && tin != base_tok) rt[tin] = 1;
        }
        for (int ord = 0; ord < m_act; ++ord) {
            int t = tok_order[ord];
            if (!rf[t]) continue;
            for (int e = 0; e < K; ++e) {
                if (!edge_keep[e]) continue;
                int tin, tout;
                pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
                if (tin == t && tout != base_tok) rf[tout] = 1;
            }
        }
        for (int ord = m_act - 1; ord >= 0; --ord) {
            int t = tok_order[ord];
            if (!rt[t]) continue;
            for (int e = 0; e < K; ++e) {
                if (!edge_keep[e]) continue;
                int tin, tout;
                pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
                if (tout == t && tin != base_tok) rt[tin] = 1;
            }
        }
        bool dropped = false;
        for (int e = 0; e < K; ++e) {
            if (!edge_keep[e]) continue;
            int tin, tout;
            pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
            if (!rf[tin] || !rt[tout]) {
                edge_keep[e] = 0;
                dropped = true;
            }
        }
        if (!dropped) break;
    }
    return m_act;
}

__host__ __device__ inline double replay_acyclic_sharebps(
    const PoolData* __restrict__ pools,
    const double* __restrict__ tick_sp,
    const double* __restrict__ tick_liq,
    const int32_t* __restrict__ edge_pool,
    const int32_t* __restrict__ edge_dir,
    const double* __restrict__ edge_dx,
    const uint8_t* __restrict__ edge_keep,
    int K,
    int n_tokens,
    int base_tok,
    const int32_t* __restrict__ tok_order,
    int m_act,
    double scale,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    double* __restrict__ bal,
    double* __restrict__ sum_out_plan,
    double& out_borrow,
    double& out_gross,
    double& out_gas_cost,
    double& out_total_gas,
    int32_t& out_steps_cnt
) {
    for (int t = 0; t < n_tokens; ++t) {
        bal[t] = 0.0;
        sum_out_plan[t] = 0.0;
    }
    double borrow = 0.0;
    int last_base_e = -1;
    for (int e = 0; e < K; ++e) {
        if (!edge_keep[e]) continue;
        int tin, tout;
        pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
        if (tin == base_tok) {
            double ra = edge_dx[e] * scale;
            borrow += ra;
            if (ra > 1e-12) last_base_e = e;
        } else {
            sum_out_plan[tin] += edge_dx[e];
        }
    }
    if (borrow <= 1e-12) {
        out_borrow = 0.0; out_gross = 0.0; out_gas_cost = 0.0; out_total_gas = 0.0; out_steps_cnt = 0;
        return 0.0;
    }
    double rem_base = borrow;
    bal[base_tok] = borrow;
    double tot_gas = base_tx_gas;
    int32_t steps_cnt = 0;

    for (int e = 0; e < K && rem_base > 1e-12; ++e) {
        if (!edge_keep[e]) continue;
        int tin, tout;
        pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
        if (tin == base_tok) {
            double amt = (e == last_base_e) ? rem_base : fmin(rem_base, edge_dx[e] * scale);
            if (amt > 1e-12) {
                double dy = pool_quote(pools[edge_pool[e]], tick_sp, tick_liq, edge_dir[e], amt);
                rem_base -= amt;
                bal[base_tok] = rem_base;
                bal[tout] += dy;
                tot_gas += pools[edge_pool[e]].gas_units;
                ++steps_cnt;
            }
        }
    }

    for (int ord = 0; ord < m_act; ++ord) {
        int t = tok_order[ord];
        double avail = bal[t] * (1.0 - 1e-11);
        double denom = sum_out_plan[t];
        if (avail <= 1e-12 || denom <= 1e-12) continue;
        int last_e = -1;
        for (int e = 0; e < K; ++e) {
            if (!edge_keep[e]) continue;
            int tin, tout;
            pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
            if (tin == t) last_e = e;
        }
        double rem = avail;
        for (int e = 0; e < K && rem > 1e-13; ++e) {
            if (!edge_keep[e]) continue;
            int tin, tout;
            pool_tokens(pools[edge_pool[e]], edge_dir[e], tin, tout);
            if (tin != t) continue;
            double amt = (e == last_e) ? rem : fmin(rem, avail * (edge_dx[e] / denom));
            if (amt > 1e-13) {
                double dy = pool_quote(pools[edge_pool[e]], tick_sp, tick_liq, edge_dir[e], amt);
                rem -= amt;
                bal[tout] += dy;
                tot_gas += pools[edge_pool[e]].gas_units;
                ++steps_cnt;
            }
        }
        bal[t] = 0.0;
    }

    if (steps_cnt == 0) tot_gas = 0.0;
    if (max_gas_limit > 0.0 && tot_gas > max_gas_limit + 1e-6) {
        out_borrow = 0.0; out_gross = 0.0; out_gas_cost = 0.0; out_total_gas = 0.0; out_steps_cnt = 0;
        return -1e18;
    }

    out_borrow = borrow;
    out_gross = bal[base_tok] - borrow;
    out_total_gas = tot_gas;
    out_gas_cost = tot_gas * gas_price_base;
    out_steps_cnt = steps_cnt;
    return out_gross - out_gas_cost;
}

static double project_and_schedule_host(
    const PoolData* pools,
    const double* tick_sp,
    const double* tick_liq,
    int n_pools,
    int n_tokens,
    int base_tok,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    const double* nu_val,
    const double* in_dx,
    const int32_t* in_dir,
    const uint8_t* mask,
    std::vector<SolverStep>& out_steps,
    double& out_borrow,
    double& out_gross,
    double& out_gas_cost,
    double& out_total_gas,
    bool allow_negative_net,
    bool do_polish
) {
    out_steps.clear();
    out_borrow = 0.0; out_gross = 0.0; out_gas_cost = 0.0; out_total_gas = 0.0;

    std::vector<int32_t> epool;
    std::vector<int32_t> edir;
    std::vector<double> edx;
    epool.reserve(256);
    edir.reserve(256);
    edx.reserve(256);
    for (int i = 0; i < n_pools; ++i) {
        if (mask && !mask[i]) continue;
        if (in_dx[i] <= 1e-12) continue;
        epool.push_back(i);
        edir.push_back(in_dir[i]);
        edx.push_back(in_dx[i]);
    }
    int K = (int)epool.size();
    if (K == 0) return 0.0;

    HostAcyclicScratch hs;
    AcyclicScratch sc{hs.from_flow, hs.to_flow, hs.score, hs.i0, hs.i1, hs.u0, hs.u1};
    std::vector<uint8_t> ekeep(K, 0);
    int32_t tok_order[MAX_TOKENS];
    int m_act = build_acyclic_rank_compact(
        pools, nu_val, epool.data(), edir.data(), edx.data(), K, n_tokens, base_tok, ekeep.data(), tok_order, sc
    );

    auto emit_for_scale = [&](double scale, std::vector<SolverStep>& st_out,
                              double& ob, double& og, double& ogc, double& otg) -> double {
        st_out.clear();
        int32_t scnt = 0;
        double net = replay_acyclic_sharebps(
            pools, tick_sp, tick_liq, epool.data(), edir.data(), edx.data(), ekeep.data(), K,
            n_tokens, base_tok, tok_order, m_act, scale, gas_price_base, base_tx_gas, max_gas_limit,
            hs.bal, hs.sum_out_plan, ob, og, ogc, otg, scnt
        );
        if (ob <= 1e-12 || (max_gas_limit > 0.0 && otg > max_gas_limit + 1e-6)) return -1e18;
        if (!allow_negative_net && net <= 0.0) return net;

        for (int t = 0; t < n_tokens; ++t) {
            hs.bal[t] = 0.0;
            hs.sum_out_plan[t] = 0.0;
        }
        double rem_base = ob;
        hs.bal[base_tok] = ob;
        int last_base_e = -1;
        for (int e = 0; e < K; ++e) {
            if (!ekeep[e]) continue;
            int tin, tout;
            pool_tokens(pools[epool[e]], edir[e], tin, tout);
            if (tin != base_tok) hs.sum_out_plan[tin] += edx[e];
            else if (edx[e] * scale > 1e-12) last_base_e = e;
        }
        for (int e = 0; e < K && rem_base > 1e-12; ++e) {
            if (!ekeep[e]) continue;
            int tin, tout;
            pool_tokens(pools[epool[e]], edir[e], tin, tout);
            if (tin == base_tok) {
                double amt = (e == last_base_e) ? rem_base : fmin(rem_base, edx[e] * scale);
                if (amt > 1e-12) {
                    double dy = pool_quote(pools[epool[e]], tick_sp, tick_liq, edir[e], amt);
                    rem_base -= amt;
                    hs.bal[base_tok] = rem_base;
                    hs.bal[tout] += dy;
                    st_out.push_back({epool[e], edir[e], amt, dy, pools[epool[e]].gas_units});
                }
            }
        }
        for (int ord = 0; ord < m_act; ++ord) {
            int t = tok_order[ord];
            double avail = hs.bal[t] * (1.0 - 1e-11);
            double denom = hs.sum_out_plan[t];
            if (avail <= 1e-12 || denom <= 1e-12) continue;
            int last_e = -1;
            for (int e = 0; e < K; ++e) {
                if (!ekeep[e]) continue;
                int tin, tout;
                pool_tokens(pools[epool[e]], edir[e], tin, tout);
                if (tin == t) last_e = e;
            }
            double rem = avail;
            for (int e = 0; e < K && rem > 1e-13; ++e) {
                if (!ekeep[e]) continue;
                int tin, tout;
                pool_tokens(pools[epool[e]], edir[e], tin, tout);
                if (tin != t) continue;
                double amt = (e == last_e) ? rem : fmin(rem, avail * (edx[e] / denom));
                if (amt > 1e-13) {
                    double dy = pool_quote(pools[epool[e]], tick_sp, tick_liq, edir[e], amt);
                    rem -= amt;
                    hs.bal[tout] += dy;
                    st_out.push_back({epool[e], edir[e], amt, dy, pools[epool[e]].gas_units});
                }
            }
            hs.bal[t] = 0.0;
        }
        return net;
    };

    double best_net = allow_negative_net ? -1e17 : 0.0;
    double best_scale = 1.0;
    double sb = 0.0, sg = 0.0, sgc = 0.0, stg = 0.0;
    int32_t scnt = 0;
    if (!do_polish) {
        double sn = replay_acyclic_sharebps(
            pools, tick_sp, tick_liq, epool.data(), edir.data(), edx.data(), ekeep.data(), K,
            n_tokens, base_tok, tok_order, m_act, 1.0, gas_price_base, base_tx_gas, max_gas_limit,
            hs.bal, hs.sum_out_plan, sb, sg, sgc, stg, scnt
        );
        if (sn > best_net) {
            best_net = emit_for_scale(1.0, out_steps, out_borrow, out_gross, out_gas_cost, out_total_gas);
        }
    } else {
        const double coarse[11] = {0.06, 0.15, 0.30, 0.55, 0.80, 1.0, 1.25, 1.6, 2.2, 3.2, 5.0};
        for (int si = 0; si < 11; ++si) {
            double sn = replay_acyclic_sharebps(
                pools, tick_sp, tick_liq, epool.data(), edir.data(), edx.data(), ekeep.data(), K,
                n_tokens, base_tok, tok_order, m_act, coarse[si], gas_price_base, base_tx_gas, max_gas_limit,
                hs.bal, hs.sum_out_plan, sb, sg, sgc, stg, scnt
            );
            if (sn > best_net) {
                best_net = sn;
                best_scale = coarse[si];
            }
        }
        if (best_net > (allow_negative_net ? -1e17 : 0.0)) {
            double lo = best_scale * 0.62;
            double hi = best_scale * 1.48;
            for (int gi = 0; gi < 10; ++gi) {
                double m1 = lo + 0.381966011 * (hi - lo);
                double m2 = lo + 0.618033989 * (hi - lo);
                double n1 = replay_acyclic_sharebps(
                    pools, tick_sp, tick_liq, epool.data(), edir.data(), edx.data(), ekeep.data(), K,
                    n_tokens, base_tok, tok_order, m_act, m1, gas_price_base, base_tx_gas, max_gas_limit,
                    hs.bal, hs.sum_out_plan, sb, sg, sgc, stg, scnt
                );
                double n2 = replay_acyclic_sharebps(
                    pools, tick_sp, tick_liq, epool.data(), edir.data(), edx.data(), ekeep.data(), K,
                    n_tokens, base_tok, tok_order, m_act, m2, gas_price_base, base_tx_gas, max_gas_limit,
                    hs.bal, hs.sum_out_plan, sb, sg, sgc, stg, scnt
                );
                if (n1 > best_net) { best_net = n1; best_scale = m1; }
                if (n2 > best_net) { best_net = n2; best_scale = m2; }
                if (n1 >= n2) hi = m2; else lo = m1;
            }
            best_net = emit_for_scale(best_scale, out_steps, out_borrow, out_gross, out_gas_cost, out_total_gas);
        }
    }

    if (!allow_negative_net && best_net <= 0.0) {
        out_steps.clear();
        out_borrow = 0.0; out_gross = 0.0; out_gas_cost = 0.0; out_total_gas = 0.0;
        return 0.0;
    }
    return best_net;
}

__host__ __device__ inline double det_jitter(int blk, int tok, int salt) {
    uint32_t h = ((uint32_t)blk * 73856093u) ^ ((uint32_t)tok * 19349663u) ^ ((uint32_t)salt * 83492791u + 0x9e3779b9u);
    h ^= h >> 16;
    h *= 0x85ebca6bu;
    h ^= h >> 13;
    h *= 0xc2b2ae35u;
    h ^= h >> 16;
    return ((double)(h & 0xFFFFu) / 32767.5) - 1.0;
}

__global__ void kernel_layer1_dual_opt(
    const PoolData* __restrict__ pools,
    const double* __restrict__ tick_sp,
    const double* __restrict__ tick_liq,
    int n_pools,
    int n_tokens,
    int base_tok,
    const double* __restrict__ init_log_nu,
    int num_iters,
    double* __restrict__ out_dx,
    int32_t* __restrict__ out_dir,
    double* __restrict__ out_log_nu,
    double* __restrict__ out_dual_gap
) {
    int blk = blockIdx.x;
    int tid = threadIdx.x;
    int bdim = blockDim.x;
    int lane = tid & 31;
    int n_reg = (n_tokens < 8) ? n_tokens : 8;

    __shared__ double sh_log_nu[MAX_TOKENS];
    __shared__ double sh_best_log_nu[MAX_TOKENS];
    __shared__ double sh_nu[MAX_TOKENS];
    __shared__ double sh_psi[MAX_TOKENS];
    __shared__ double sh_turn[MAX_TOKENS];
    __shared__ double sh_adam_m[MAX_TOKENS];
    __shared__ double sh_adam_v[MAX_TOKENS];
    __shared__ double sh_best_gap;
    __shared__ int32_t sh_take_best;

    const double lr_table[8] = {0.0005, 0.0015, 0.004, 0.01, 0.025, 0.05, 0.002, 0.008};
    double lr0 = lr_table[blk & 7];
    const double jitter_table[8] = {0.0, 0.0003, 0.001, 0.003, 0.007, 0.015, 0.03, 0.005};
    double jitter_amp = (blk < 8) ? 0.0 : jitter_table[(blk >> 3) & 7];

    for (int t = tid; t < n_tokens; t += bdim) {
        double u = init_log_nu[t];
        if (t != base_tok && blk >= 8) {
            u += jitter_amp * det_jitter(blk, t, 17);
        }
        if (t == base_tok) u = 0.0;
        sh_log_nu[t] = u;
        sh_best_log_nu[t] = u;
        sh_nu[t] = exp(u);
        sh_adam_m[t] = 0.0;
        sh_adam_v[t] = 0.0;
    }
    if (tid == 0) {
        sh_best_gap = 1e9;
        sh_take_best = 0;
    }
    __syncthreads();

    double reg_psi[8];
    double reg_turn[8];

    for (int it = 0; it < num_iters; ++it) {
        for (int t = tid; t < n_tokens; t += bdim) {
            sh_psi[t] = 0.0;
            sh_turn[t] = 0.0;
        }
        #pragma unroll
        for (int k = 0; k < 8; ++k) {
            reg_psi[k] = 0.0;
            reg_turn[k] = 0.0;
        }
        __syncthreads();

        for (int i = tid; i < n_pools; i += bdim) {
            const PoolData p = pools[i];
            if (p.token0 < 0 || p.token1 < 0 || p.token0 >= n_tokens || p.token1 >= n_tokens) continue;
            double v0 = sh_nu[p.token0];
            double v1 = sh_nu[p.token1];
            int dir = 0;
            double dx = 0.0, dy = 0.0;
            pool_opt_trade(p, tick_sp, tick_liq, v0, v1, dir, dx, dy);
            if (dx > 1e-13 && dy > 0.0) {
                int tin = (dir == 0) ? p.token0 : p.token1;
                int tout = (dir == 0) ? p.token1 : p.token0;
                double vin = (dir == 0) ? v0 : v1;
                double vout = (dir == 0) ? v1 : v0;
                if (tin < 8) {
                    reg_psi[tin] -= dx;
                    reg_turn[tin] += vin * dx;
                } else {
                    atomicAdd(&sh_psi[tin], -dx);
                    atomicAdd(&sh_turn[tin], vin * dx);
                }
                if (tout < 8) {
                    reg_psi[tout] += dy;
                    reg_turn[tout] += vout * dy;
                } else {
                    atomicAdd(&sh_psi[tout], dy);
                    atomicAdd(&sh_turn[tout], vout * dy);
                }
            }
        }

        // Unconditional warp-shuffle reduction for hot tokens [0..n_reg-1]
        for (int k = 0; k < n_reg; ++k) {
            double sp = reg_psi[k];
            double st = reg_turn[k];
            for (int off = 16; off > 0; off >>= 1) {
                sp += __shfl_down_sync(0xffffffffu, sp, off);
                st += __shfl_down_sync(0xffffffffu, st, off);
            }
            if (lane == 0) {
                if (sp != 0.0) atomicAdd(&sh_psi[k], sp);
                if (st != 0.0) atomicAdd(&sh_turn[k], st);
            }
        }
        __syncthreads();

        bool check_gap = (it >= num_iters / 2) && (((it & 3) == 3) || (it == num_iters - 1));
        if (check_gap) {
            if (tid == 0) {
                double tot_abs_psi = 0.0, tot_turn = 0.0;
                for (int t = 0; t < n_tokens; ++t) {
                    if (t == base_tok) continue;
                    tot_abs_psi += fabs(sh_nu[t] * sh_psi[t]);
                    tot_turn += sh_turn[t];
                }
                double gap = (tot_turn > 1e-5) ? (tot_abs_psi / tot_turn) : 1e9;
                if (gap <= sh_best_gap) {
                    sh_best_gap = gap;
                    sh_take_best = 1;
                } else {
                    sh_take_best = 0;
                }
            }
            __syncthreads();
            if (sh_take_best) {
                for (int t = tid; t < n_tokens; t += bdim) {
                    sh_best_log_nu[t] = sh_log_nu[t];
                }
            }
        }

        for (int t = tid; t < n_tokens; t += bdim) {
            if (t == base_tok) continue;
            double v = sh_nu[t];
            double grad = (v * sh_psi[t]) / (sh_turn[t] + 1e-12);
            if (grad > 1.0) grad = 1.0;
            if (grad < -1.0) grad = -1.0;

            double m = 0.85 * sh_adam_m[t] + 0.15 * grad;
            double vv = 0.96 * sh_adam_v[t] + 0.04 * grad * grad;
            sh_adam_m[t] = m;
            sh_adam_v[t] = vv;

            double prog = (double)it / (double)num_iters;
            double lr = lr0 * (0.05 + 0.95 * (1.0 - prog));
            double step = -lr * m / (sqrt(vv) + 1e-4);
            if (step > 0.05) step = 0.05;
            if (step < -0.05) step = -0.05;

            double u_new = sh_log_nu[t] + step;
            double u_base = init_log_nu[t];
            if (u_new > u_base + 2.5) u_new = u_base + 2.5;
            if (u_new < u_base - 2.5) u_new = u_base - 2.5;
            sh_log_nu[t] = u_new;
            sh_nu[t] = exp(u_new);
        }
        __syncthreads();
    }

    if (sh_best_gap < 1e8) {
        for (int t = tid; t < n_tokens; t += bdim) {
            double bu = sh_best_log_nu[t];
            sh_log_nu[t] = bu;
            sh_nu[t] = exp(bu);
        }
        __syncthreads();
    }

    for (int i = tid; i < n_pools; i += bdim) {
        const PoolData p = pools[i];
        int dir = 0;
        double dx = 0.0, dy = 0.0;
        if (p.token0 >= 0 && p.token1 >= 0 && p.token0 < n_tokens && p.token1 < n_tokens) {
            pool_opt_trade(p, tick_sp, tick_liq, sh_nu[p.token0], sh_nu[p.token1], dir, dx, dy);
        }
        out_dx[(size_t)blk * (size_t)n_pools + i] = dx;
        out_dir[(size_t)blk * (size_t)n_pools + i] = dir;
    }
    for (int t = tid; t < n_tokens; t += bdim) {
        out_log_nu[(size_t)blk * MAX_TOKENS + t] = sh_log_nu[t];
    }
    if (tid == 0) {
        out_dual_gap[blk] = sh_best_gap;
    }
}

__global__ void kernel_layer2_gas_pruning(
    const PoolData* __restrict__ pools,
    const double* __restrict__ tick_sp,
    const double* __restrict__ tick_liq,
    const int32_t* __restrict__ cand_pools,
    const uint8_t* __restrict__ masks,
    int n_cands,
    int n_tokens,
    int base_tok,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    const double* __restrict__ init_log_nu,
    const double* __restrict__ warm_log_nu,
    int num_iters,
    double* __restrict__ out_dx,
    int32_t* __restrict__ out_dir,
    double* __restrict__ out_net,
    double* __restrict__ out_gross,
    double* __restrict__ out_gas
) {
    int blk = blockIdx.x;
    int tid = threadIdx.x;
    int bdim = blockDim.x;
    int lane = tid & 31;
    int n_reg = (n_tokens < 8) ? n_tokens : 8;

    __shared__ double sh_log_nu[MAX_TOKENS];
    __shared__ double sh_nu[MAX_TOKENS];
    __shared__ double sh_psi[MAX_TOKENS];
    __shared__ double sh_turn[MAX_TOKENS];
    __shared__ double sh_m[MAX_TOKENS];
    __shared__ double sh_v[MAX_TOKENS];
    __shared__ double sh_dx[MAX_L2_CANDS];
    __shared__ int32_t sh_dir[MAX_L2_CANDS];
    __shared__ int32_t sh_epool[MAX_L2_CANDS];
    __shared__ int32_t sh_tok_order[MAX_TOKENS];
    __shared__ int32_t sh_i0[MAX_TOKENS];
    __shared__ int32_t sh_i1[MAX_TOKENS];
    __shared__ uint8_t sh_ekeep[MAX_L2_CANDS];
    __shared__ uint8_t sh_u0[MAX_TOKENS];
    __shared__ uint8_t sh_u1[MAX_TOKENS];

    const double l2_lr_table[4] = {0.001, 0.004, 0.012, 0.03};
    double l2_lr0 = l2_lr_table[blk & 3];
    const double* start_nu_ptr = (warm_log_nu && (blk & 4)) ? warm_log_nu : init_log_nu;
    for (int t = tid; t < n_tokens; t += bdim) {
        double u = start_nu_ptr[t];
        if (t != base_tok && (blk & 8)) {
            u += 0.0008 * det_jitter(blk, t, 41);
        }
        if (t == base_tok) u = 0.0;
        sh_log_nu[t] = u;
        sh_nu[t] = exp(u);
        sh_m[t] = 0.0;
        sh_v[t] = 0.0;
    }
    __syncthreads();

    const uint8_t* mask = masks + (size_t)blk * (size_t)n_cands;
    double reg_psi[8];
    double reg_turn[8];

    for (int it = 0; it < num_iters; ++it) {
        for (int t = tid; t < n_tokens; t += bdim) {
            sh_psi[t] = 0.0;
            sh_turn[t] = 0.0;
        }
        #pragma unroll
        for (int k = 0; k < 8; ++k) {
            reg_psi[k] = 0.0;
            reg_turn[k] = 0.0;
        }
        __syncthreads();

        for (int c = tid; c < n_cands; c += bdim) {
            if (!mask[c]) continue;
            const PoolData p = pools[cand_pools[c]];
            double v0 = sh_nu[p.token0];
            double v1 = sh_nu[p.token1];
            int dir = 0;
            double dx = 0.0, dy = 0.0;
            pool_opt_trade(p, tick_sp, tick_liq, v0, v1, dir, dx, dy);
            if (dx > 1e-13 && dy > 0.0) {
                int tin = (dir == 0) ? p.token0 : p.token1;
                int tout = (dir == 0) ? p.token1 : p.token0;
                double vin = (dir == 0) ? v0 : v1;
                double vout = (dir == 0) ? v1 : v0;
                if (tin < 8) {
                    reg_psi[tin] -= dx;
                    reg_turn[tin] += vin * dx;
                } else {
                    atomicAdd(&sh_psi[tin], -dx);
                    atomicAdd(&sh_turn[tin], vin * dx);
                }
                if (tout < 8) {
                    reg_psi[tout] += dy;
                    reg_turn[tout] += vout * dy;
                } else {
                    atomicAdd(&sh_psi[tout], dy);
                    atomicAdd(&sh_turn[tout], vout * dy);
                }
            }
        }

        for (int k = 0; k < n_reg; ++k) {
            double sp = reg_psi[k];
            double st = reg_turn[k];
            for (int off = 16; off > 0; off >>= 1) {
                sp += __shfl_down_sync(0xffffffffu, sp, off);
                st += __shfl_down_sync(0xffffffffu, st, off);
            }
            if (lane == 0) {
                if (sp != 0.0) atomicAdd(&sh_psi[k], sp);
                if (st != 0.0) atomicAdd(&sh_turn[k], st);
            }
        }
        __syncthreads();

        for (int t = tid; t < n_tokens; t += bdim) {
            if (t == base_tok) continue;
            double v = sh_nu[t];
            double grad = (v * sh_psi[t]) / (sh_turn[t] + 1e-12);
            if (grad > 1.0) grad = 1.0;
            if (grad < -1.0) grad = -1.0;
            sh_m[t] = 0.85 * sh_m[t] + 0.15 * grad;
            sh_v[t] = 0.96 * sh_v[t] + 0.04 * grad * grad;
            double prog = (double)it / (double)num_iters;
            double lr = l2_lr0 * (0.05 + 0.95 * (1.0 - prog));
            double step = -lr * sh_m[t] / (sqrt(sh_v[t]) + 1e-4);
            if (step > 0.05) step = 0.05;
            if (step < -0.05) step = -0.05;
            double u_new = sh_log_nu[t] + step;
            double u_base = init_log_nu[t];
            if (u_new > u_base + 2.5) u_new = u_base + 2.5;
            if (u_new < u_base - 2.5) u_new = u_base - 2.5;
            sh_log_nu[t] = u_new;
            sh_nu[t] = exp(u_new);
        }
        __syncthreads();
    }

    for (int c = tid; c < n_cands; c += bdim) {
        sh_dx[c] = 0.0;
        sh_dir[c] = 0;
        sh_epool[c] = cand_pools[c];
        if (mask[c]) {
            const PoolData p = pools[cand_pools[c]];
            int dir = 0;
            double dx = 0.0, dy = 0.0;
            pool_opt_trade(p, tick_sp, tick_liq, sh_nu[p.token0], sh_nu[p.token1], dir, dx, dy);
            if (dx > 1e-12 && dy > 0.0) {
                sh_dx[c] = dx;
                sh_dir[c] = dir;
            }
        }
        out_dx[(size_t)blk * (size_t)n_cands + c] = sh_dx[c];
        out_dir[(size_t)blk * (size_t)n_cands + c] = sh_dir[c];
    }
    __syncthreads();

    // Thread 0 evaluates zero-spill FAS acyclic shareBps + 8-scale sweep + 3-step golden zoom
    if (tid == 0) {
        AcyclicScratch sc{sh_m, sh_v, sh_log_nu, sh_i0, sh_i1, sh_u0, sh_u1};
        int m_act = build_acyclic_rank_compact(
            pools, sh_nu, sh_epool, sh_dir, sh_dx, n_cands, n_tokens, base_tok, sh_ekeep, sh_tok_order, sc
        );
        const double scales[8] = {0.08, 0.20, 0.45, 0.75, 1.0, 1.4, 2.2, 3.5};
        double bnet = 0.0, bgross = 0.0, bgcost = 0.0, bscale = 1.0;
        for (int si = 0; si < 8; ++si) {
            double sb = 0.0, sg = 0.0, sgc = 0.0, stg = 0.0;
            int32_t scnt = 0;
            double net = replay_acyclic_sharebps(
                pools, tick_sp, tick_liq, sh_epool, sh_dir, sh_dx, sh_ekeep, n_cands,
                n_tokens, base_tok, sh_tok_order, m_act, scales[si], gas_price_base, base_tx_gas, max_gas_limit,
                sh_psi, sh_turn, sb, sg, sgc, stg, scnt
            );
            if (net > bnet) {
                bnet = net; bgross = sg; bgcost = sgc; bscale = scales[si];
            }
        }
        if (bnet > 0.0) {
            double lo = bscale * 0.65;
            double hi = bscale * 1.45;
            for (int gi = 0; gi < 3; ++gi) {
                double m1 = lo + 0.381966011 * (hi - lo);
                double m2 = lo + 0.618033989 * (hi - lo);
                double sb = 0.0, sg = 0.0, sgc = 0.0, stg = 0.0;
                int32_t scnt = 0;
                double n1 = replay_acyclic_sharebps(
                    pools, tick_sp, tick_liq, sh_epool, sh_dir, sh_dx, sh_ekeep, n_cands,
                    n_tokens, base_tok, sh_tok_order, m_act, m1, gas_price_base, base_tx_gas, max_gas_limit,
                    sh_psi, sh_turn, sb, sg, sgc, stg, scnt
                );
                if (n1 > bnet) { bnet = n1; bgross = sg; bgcost = sgc; bscale = m1; }
                double n2 = replay_acyclic_sharebps(
                    pools, tick_sp, tick_liq, sh_epool, sh_dir, sh_dx, sh_ekeep, n_cands,
                    n_tokens, base_tok, sh_tok_order, m_act, m2, gas_price_base, base_tx_gas, max_gas_limit,
                    sh_psi, sh_turn, sb, sg, sgc, stg, scnt
                );
                if (n2 > bnet) { bnet = n2; bgross = sg; bgcost = sgc; bscale = m2; }
                if (n1 >= n2) hi = m2; else lo = m1;
            }
        }
        if (bscale != 1.0 && bnet > 0.0) {
            for (int c = 0; c < n_cands; ++c) {
                out_dx[(size_t)blk * (size_t)n_cands + c] = sh_dx[c] * bscale;
            }
        }
        out_gross[blk] = bgross;
        out_gas[blk] = bgcost;
        out_net[blk] = bnet;
    }
}

struct GpuWorkspace {
    int cap_pools = 0;
    int cap_ticks = 0;
    int cap_l1_blocks = 0;
    int cap_l2_blocks = 0;

    int cur_n_pools = 0;
    int cur_n_ticks = 0;
    int cur_n_tokens = 0;
    int cached_base_tok = -1;
    bool init_nu_valid = false;

    std::vector<int32_t> gpu_to_orig;
    std::vector<int32_t> orig_to_gpu;

    PoolData* d_pools = nullptr;
    double* d_tick_sp = nullptr;
    double* d_tick_liq = nullptr;
    double* d_init_log_nu = nullptr;
    double* d_l1_dx = nullptr;
    int32_t* d_l1_dir = nullptr;
    double* d_l1_log_nu = nullptr;
    double* d_l1_gap = nullptr;

    int32_t* d_cand_pools = nullptr;
    uint8_t* d_l2_masks = nullptr;
    double* d_l2_dx = nullptr;
    int32_t* d_l2_dir = nullptr;
    double* d_l2_net = nullptr;
    double* d_l2_gross = nullptr;
    double* d_l2_gas = nullptr;

    double* d_pol_warm_nu = nullptr;
    uint8_t* d_pol_masks = nullptr;
    double* d_pol_dx = nullptr;
    int32_t* d_pol_dir = nullptr;
    double* d_pol_net = nullptr;
    double* d_pol_gross = nullptr;
    double* d_pol_gas = nullptr;

    void free_all() {
        if (d_pools) cudaFree(d_pools);
        if (d_tick_sp) cudaFree(d_tick_sp);
        if (d_tick_liq) cudaFree(d_tick_liq);
        if (d_init_log_nu) cudaFree(d_init_log_nu);
        if (d_l1_dx) cudaFree(d_l1_dx);
        if (d_l1_dir) cudaFree(d_l1_dir);
        if (d_l1_log_nu) cudaFree(d_l1_log_nu);
        if (d_l1_gap) cudaFree(d_l1_gap);
        if (d_cand_pools) cudaFree(d_cand_pools);
        if (d_l2_masks) cudaFree(d_l2_masks);
        if (d_l2_dx) cudaFree(d_l2_dx);
        if (d_l2_dir) cudaFree(d_l2_dir);
        if (d_l2_net) cudaFree(d_l2_net);
        if (d_l2_gross) cudaFree(d_l2_gross);
        if (d_l2_gas) cudaFree(d_l2_gas);
        if (d_pol_warm_nu) cudaFree(d_pol_warm_nu);
        if (d_pol_masks) cudaFree(d_pol_masks);
        if (d_pol_dx) cudaFree(d_pol_dx);
        if (d_pol_dir) cudaFree(d_pol_dir);
        if (d_pol_net) cudaFree(d_pol_net);
        if (d_pol_gross) cudaFree(d_pol_gross);
        if (d_pol_gas) cudaFree(d_pol_gas);
        gpu_to_orig.clear();
        orig_to_gpu.clear();
        *this = GpuWorkspace{};
    }

    int ensure(int n_pools, int n_ticks, int l1_blocks, int l2_blocks) {
        int need_t = std::max(n_ticks, 64);
        if (n_pools <= cap_pools && need_t <= cap_ticks && l1_blocks <= cap_l1_blocks && l2_blocks <= cap_l2_blocks) {
            return 0;
        }
        int new_p = std::max(cap_pools, std::max(n_pools, 64));
        int new_t = std::max(cap_ticks, need_t);
        int new_l1 = std::max(cap_l1_blocks, std::max(l1_blocks, 64));
        int new_l2 = std::max(cap_l2_blocks, std::max(l2_blocks, 128));
        free_all();
        cap_pools = new_p;
        cap_ticks = new_t;
        cap_l1_blocks = new_l1;
        cap_l2_blocks = new_l2;

        CUDA_CHECK(cudaMallocManaged(&d_pools, (size_t)cap_pools * sizeof(PoolData)));
        CUDA_CHECK(cudaMallocManaged(&d_tick_sp, (size_t)cap_ticks * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_tick_liq, (size_t)cap_ticks * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_init_log_nu, MAX_TOKENS * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_l1_dx, (size_t)cap_l1_blocks * (size_t)cap_pools * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_l1_dir, (size_t)cap_l1_blocks * (size_t)cap_pools * sizeof(int32_t)));
        CUDA_CHECK(cudaMallocManaged(&d_l1_log_nu, (size_t)cap_l1_blocks * MAX_TOKENS * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_l1_gap, (size_t)cap_l1_blocks * sizeof(double)));

        CUDA_CHECK(cudaMallocManaged(&d_cand_pools, MAX_L2_CANDS * sizeof(int32_t)));
        CUDA_CHECK(cudaMallocManaged(&d_l2_masks, (size_t)cap_l2_blocks * MAX_L2_CANDS * sizeof(uint8_t)));
        CUDA_CHECK(cudaMallocManaged(&d_l2_dx, (size_t)cap_l2_blocks * MAX_L2_CANDS * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_l2_dir, (size_t)cap_l2_blocks * MAX_L2_CANDS * sizeof(int32_t)));
        CUDA_CHECK(cudaMallocManaged(&d_l2_net, (size_t)cap_l2_blocks * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_l2_gross, (size_t)cap_l2_blocks * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_l2_gas, (size_t)cap_l2_blocks * sizeof(double)));

        CUDA_CHECK(cudaMallocManaged(&d_pol_warm_nu, MAX_TOKENS * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_pol_masks, (size_t)128 * MAX_L2_CANDS * sizeof(uint8_t)));
        CUDA_CHECK(cudaMallocManaged(&d_pol_dx, (size_t)128 * MAX_L2_CANDS * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_pol_dir, (size_t)128 * MAX_L2_CANDS * sizeof(int32_t)));
        CUDA_CHECK(cudaMallocManaged(&d_pol_net, (size_t)128 * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_pol_gross, (size_t)128 * sizeof(double)));
        CUDA_CHECK(cudaMallocManaged(&d_pol_gas, (size_t)128 * sizeof(double)));
        return 0;
    }
};

static GpuWorkspace g_ws;

static void compute_init_log_nu(
    const PoolData* pools,
    const double* tick_sp,
    const double* tick_liq,
    int n_pools,
    int n_tokens,
    int base_tok,
    double* out_log_nu,
    int consensus_iters = 10
) {
    std::vector<uint8_t> known(n_tokens, 0);
    std::vector<double> best_weight(n_tokens, 0.0);
    for (int t = 0; t < n_tokens; ++t) out_log_nu[t] = 0.0;
    known[base_tok] = 1;
    best_weight[base_tok] = 1e30;

    for (int pass = 0; pass < n_tokens; ++pass) {
        bool ch = false;
        for (int i = 0; i < n_pools; ++i) {
            const PoolData& p = pools[i];
            if (p.token0 < 0 || p.token1 < 0 || p.token0 >= n_tokens || p.token1 >= n_tokens) continue;
            double r01 = pool_spot_rate(p, tick_sp, tick_liq, 0);
            double r10 = pool_spot_rate(p, tick_sp, tick_liq, 1);
            if (r01 <= 0.0 && r10 <= 0.0) continue;
            double price10 = (r01 > 0.0 && r10 > 0.0) ? sqrt(r01 / r10) : (r01 > 0.0 ? r01 : 1.0 / r10);
            if (price10 <= 0.0) continue;
            double lp = log(price10);
            double w = 1.0;
            if (p.pool_type == POOL_UNIV2 || p.pool_type == POOL_CURVE_STABLE ||
                p.pool_type == POOL_SOLIDLY_STABLE || p.pool_type == POOL_BALANCER_V2 ||
                p.pool_type == POOL_METRIC_OMM) {
                w = 1.0 + sqrt(std::max(0.0, p.r0 * p.r1));
            } else if (p.pool_type == POOL_CLMM) {
                w = 1.0 + p.liq;
            } else if (p.pool_type == POOL_PROP_BINS && tick_liq) {
                double bcap = (p.tick_count > 0) ? tick_liq[p.tick_offset] : 0.0;
                double acap = (p.cur_seg > 0) ? tick_liq[p.tick_offset + p.tick_count] : 0.0;
                w = 1.0 + sqrt(std::max(1.0, (bcap + 1.0) * (acap + 1.0)));
            }

            if (known[p.token1] && (!known[p.token0] || w > best_weight[p.token0] * 10.0)) {
                out_log_nu[p.token0] = out_log_nu[p.token1] + lp;
                known[p.token0] = 1;
                best_weight[p.token0] = w;
                ch = true;
            }
            if (known[p.token0] && (!known[p.token1] || w > best_weight[p.token1] * 10.0)) {
                out_log_nu[p.token1] = out_log_nu[p.token0] - lp;
                known[p.token1] = 1;
                best_weight[p.token1] = w;
                ch = true;
            }
        }
        if (!ch) break;
    }

    std::vector<double> sum_w_u(n_tokens, 0.0), sum_w(n_tokens, 0.0);
    for (int it = 0; it < consensus_iters; ++it) {
        std::fill(sum_w_u.begin(), sum_w_u.end(), 0.0);
        std::fill(sum_w.begin(), sum_w.end(), 0.0);
        for (int i = 0; i < n_pools; ++i) {
            const PoolData& p = pools[i];
            if (p.token0 < 0 || p.token1 < 0 || p.token0 >= n_tokens || p.token1 >= n_tokens) continue;
            if (!known[p.token0] || !known[p.token1]) continue;
            double r01 = pool_spot_rate(p, tick_sp, tick_liq, 0);
            double r10 = pool_spot_rate(p, tick_sp, tick_liq, 1);
            if (r01 <= 0.0 || r10 <= 0.0) continue;
            double lp = log(sqrt(r01 / r10));
            sum_w_u[p.token0] += (out_log_nu[p.token1] + lp);
            sum_w[p.token0] += 1.0;
            sum_w_u[p.token1] += (out_log_nu[p.token0] - lp);
            sum_w[p.token1] += 1.0;
        }
        for (int t = 0; t < n_tokens; ++t) {
            if (t == base_tok || sum_w[t] <= 0.0) continue;
            out_log_nu[t] = 0.5 * out_log_nu[t] + 0.5 * (sum_w_u[t] / sum_w[t]);
        }
    }
}

static int load_market_internal(
    const PoolData* pools,
    int32_t n_pools,
    const double* tick_sp,
    const double* tick_liq,
    int32_t n_tick_entries,
    int32_t n_tokens,
    int l1_blocks,
    int l2_blocks
) {
    if (!pools || n_pools <= 0 || n_tokens <= 0 || n_tokens > MAX_TOKENS) return -1;
    if (g_ws.ensure(n_pools, n_tick_entries, l1_blocks, l2_blocks) != 0) return -1;

    if (n_tick_entries > 0 && tick_sp && tick_liq) {
        std::memcpy(g_ws.d_tick_sp, tick_sp, (size_t)n_tick_entries * sizeof(double));
        std::memcpy(g_ws.d_tick_liq, tick_liq, (size_t)n_tick_entries * sizeof(double));
    }

    // Sort pools by (pool_type, protocol_id, token0, token1, orig_idx) for warp coherence
    g_ws.gpu_to_orig.resize(n_pools);
    g_ws.orig_to_gpu.resize(n_pools);
    for (int i = 0; i < n_pools; ++i) g_ws.gpu_to_orig[i] = i;
    std::sort(g_ws.gpu_to_orig.begin(), g_ws.gpu_to_orig.end(), [&](int a, int b) {
        const PoolData& pa = pools[a];
        const PoolData& pb = pools[b];
        if (pa.pool_type != pb.pool_type) return pa.pool_type < pb.pool_type;
        if (pa.protocol_id != pb.protocol_id) return pa.protocol_id < pb.protocol_id;
        if (pa.token0 != pb.token0) return pa.token0 < pb.token0;
        if (pa.token1 != pb.token1) return pa.token1 < pb.token1;
        return a < b;
    });
    for (int g = 0; g < n_pools; ++g) {
        int orig = g_ws.gpu_to_orig[g];
        g_ws.orig_to_gpu[orig] = g;
        PoolData p = pools[orig];
        if (p.pool_type == POOL_CURVE_STABLE && p.liq <= 0.0) {
            p.liq = stable_get_D(p.r0, p.r1, p.param0);
        }
        g_ws.d_pools[g] = p;
    }
    g_ws.cur_n_pools = n_pools;
    g_ws.cur_n_ticks = n_tick_entries;
    g_ws.cur_n_tokens = n_tokens;
    g_ws.cached_base_tok = -1;
    g_ws.init_nu_valid = false;
    return 0;
}

static int count_route_tokens(const PoolData* pools, const std::vector<SolverStep>& steps, int base_tok) {
    bool seen[MAX_TOKENS];
    std::memset(seen, 0, sizeof(seen));
    int cnt = 0;
    if (base_tok >= 0 && base_tok < MAX_TOKENS) {
        seen[base_tok] = true;
        cnt = 1;
    }
    for (const auto& s : steps) {
        if (s.pool_idx < 0) continue;
        const PoolData& p = pools[s.pool_idx];
        if (p.token0 >= 0 && p.token0 < MAX_TOKENS && !seen[p.token0]) {
            seen[p.token0] = true;
            ++cnt;
        }
        if (p.token1 >= 0 && p.token1 < MAX_TOKENS && !seen[p.token1]) {
            seen[p.token1] = true;
            ++cnt;
        }
    }
    return cnt;
}

static int solve_resident_internal(
    int32_t base_tok,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    int32_t budget_ms,
    int32_t enable_layer2,
    SolverStep* out_steps,
    int32_t max_steps_cap,
    SolveResult* out_result,
    std::chrono::steady_clock::time_point t_start
) {
    int n_pools = g_ws.cur_n_pools;
    int n_tokens = g_ws.cur_n_tokens;
    if (n_pools <= 0 || base_tok < 0 || base_tok >= n_tokens || !out_steps || !out_result) return -1;

    // V2 compact binary DagArbExecutor encodes tokens into 16 transient storage slots (32..47).
    // Offline stress runs with max_steps_cap > 32 keep the full MAX_TOKENS search space.
    const int max_route_tokens = (max_steps_cap <= 32) ? 16 : MAX_TOKENS;
    auto route_within_caps = [&](const std::vector<SolverStep>& steps, double tgas) -> bool {
        if ((int)steps.size() > max_steps_cap) return false;
        if (max_gas_limit > 0.0 && tgas > max_gas_limit + 1e-6) return false;
        if (max_route_tokens < MAX_TOKENS &&
            count_route_tokens(g_ws.d_pools, steps, base_tok) > max_route_tokens) {
            return false;
        }
        return true;
    };

    int l1_blocks = 128;
    int l1_iters = 140;
    int l2_blocks = 256;
    int l2_iters = 90;
    int polish_blocks = 128;
    if (budget_ms <= 50) {
        l1_blocks = 96;  l1_iters = 85;  l2_blocks = 128; l2_iters = 60; polish_blocks = 64;
    } else if (budget_ms <= 120) {
        l1_blocks = 192; l1_iters = 125; l2_blocks = 256; l2_iters = 85; polish_blocks = 96;
    } else if (budget_ms <= 500) {
        l1_blocks = 288; l1_iters = 165; l2_blocks = 384; l2_iters = 100; polish_blocks = 128;
    } else {
        l1_blocks = 384; l1_iters = 220; l2_blocks = 512; l2_iters = 120; polish_blocks = 128;
    }
    if (g_ws.ensure(n_pools, g_ws.cur_n_ticks, l1_blocks, l2_blocks) != 0) return -1;

    if (!g_ws.init_nu_valid || g_ws.cached_base_tok != base_tok) {
        compute_init_log_nu(g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, n_tokens, base_tok, g_ws.d_init_log_nu);
        g_ws.cached_base_tok = base_tok;
        g_ws.init_nu_valid = true;
    }

    auto t_l1_start = std::chrono::steady_clock::now();
    int l1_threads = (n_pools >= 256 || n_tokens > 128) ? 256 : 128;
    kernel_layer1_dual_opt<<<l1_blocks, l1_threads>>>(
        g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, n_tokens, base_tok, g_ws.d_init_log_nu, l1_iters,
        g_ws.d_l1_dx, g_ws.d_l1_dir, g_ws.d_l1_log_nu, g_ws.d_l1_gap
    );
    CUDA_CHECK(cudaGetLastError());
    CUDA_CHECK(cudaDeviceSynchronize());

    std::vector<int> blk_order(l1_blocks);
    for (int b = 0; b < l1_blocks; ++b) blk_order[b] = b;
    std::sort(blk_order.begin(), blk_order.end(), [&](int a, int b) {
        return g_ws.d_l1_gap[a] < g_ws.d_l1_gap[b];
    });

    std::vector<int> l1_eval_blks;
    std::vector<uint8_t> seen_blk(l1_blocks, 0);
    for (int i = 0; i < std::min(6, l1_blocks); ++i) {
        int b = blk_order[i];
        if (!seen_blk[b]) { l1_eval_blks.push_back(b); seen_blk[b] = 1; }
    }
    for (int bucket = 0; bucket < 8; ++bucket) {
        for (int i = 0; i < l1_blocks; ++i) {
            int b = blk_order[i];
            if ((b & 7) == bucket && !seen_blk[b]) {
                l1_eval_blks.push_back(b);
                seen_blk[b] = 1;
                break;
            }
        }
    }

    double raw_l1_gross = 0.0, raw_l1_gcost = 0.0, raw_l1_net = 0.0;
    int32_t raw_l1_steps = 0;
    std::vector<SolverStep> tmp_steps;
    double nu_eval[MAX_TOKENS];
    {
        int rb0 = blk_order[0];
        const double* lu0 = g_ws.d_l1_log_nu + (size_t)rb0 * MAX_TOKENS;
        for (int t = 0; t < n_tokens; ++t) nu_eval[t] = exp(lu0[t]);
        nu_eval[base_tok] = 1.0;
        double sb = 0.0, stg = 0.0;
        raw_l1_net = project_and_schedule_host(
            g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, n_tokens, base_tok,
            gas_price_base, base_tx_gas, 0.0, nu_eval,
            g_ws.d_l1_dx + (size_t)rb0 * (size_t)n_pools,
            g_ws.d_l1_dir + (size_t)rb0 * (size_t)n_pools,
            nullptr, tmp_steps, sb, raw_l1_gross, raw_l1_gcost, stg, true, false
        );
        raw_l1_steps = (int32_t)tmp_steps.size();
    }

    double best_l1_net = -1e18;
    double best_l1_gross = 0.0, best_l1_gcost = 0.0, best_l1_borrow = 0.0, best_l1_tgas = 0.0;
    int best_l1_blk = blk_order[0];
    std::vector<SolverStep> best_l1_steps;

    for (size_t bi = 0; bi < l1_eval_blks.size(); ++bi) {
        int b = l1_eval_blks[bi];
        const double* lub = g_ws.d_l1_log_nu + (size_t)b * MAX_TOKENS;
        for (int t = 0; t < n_tokens; ++t) nu_eval[t] = exp(lub[t]);
        nu_eval[base_tok] = 1.0;
        double sb = 0.0, sg = 0.0, sgc = 0.0, stg = 0.0;
        double net = project_and_schedule_host(
            g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, n_tokens, base_tok,
            gas_price_base, base_tx_gas, max_gas_limit, nu_eval,
            g_ws.d_l1_dx + (size_t)b * (size_t)n_pools,
            g_ws.d_l1_dir + (size_t)b * (size_t)n_pools,
            nullptr, tmp_steps, sb, sg, sgc, stg, false, false
        );
        if ((net > best_l1_net || (bi == 0 && best_l1_net < -1e17)) && (int)tmp_steps.size() <= max_steps_cap) {
            best_l1_net = net;
            best_l1_blk = b;
            best_l1_gross = sg;
            best_l1_gcost = sgc;
            best_l1_borrow = sb;
            best_l1_tgas = stg;
            best_l1_steps = tmp_steps;
        }
    }
    if (best_l1_net < 0.0) best_l1_net = 0.0;
    auto t_l1_end = std::chrono::steady_clock::now();
    double l1_ms = std::chrono::duration<double, std::milli>(t_l1_end - t_l1_start).count();

    bool l1_ok = best_l1_net > 0.0 && route_within_caps(best_l1_steps, best_l1_tgas);
    double best_net = l1_ok ? best_l1_net : 0.0;
    double best_gross = l1_ok ? best_l1_gross : 0.0;
    double best_gcost = l1_ok ? best_l1_gcost : 0.0;
    double best_borrow = l1_ok ? best_l1_borrow : 0.0;
    double best_tgas = l1_ok ? best_l1_tgas : 0.0;
    std::vector<SolverStep> best_steps = l1_ok ? best_l1_steps : std::vector<SolverStep>{};

    double l2_ms = 0.0;
    if (enable_layer2) {
        auto t_l2_start = std::chrono::steady_clock::now();

        struct CandScore {
            int pool_idx;
            double value_flow;
            double surplus_per_gas;
            double surplus_ratio;
        };
        std::vector<CandScore> scored;
        scored.reserve(n_pools);

        double nu_tops[8][MAX_TOKENS];
        int n_fusion = std::min(8, (int)l1_eval_blks.size());
        for (int r = 0; r < n_fusion; ++r) {
            const double* lur = g_ws.d_l1_log_nu + (size_t)l1_eval_blks[r] * MAX_TOKENS;
            for (int t = 0; t < n_tokens; ++t) nu_tops[r][t] = exp(lur[t]);
            nu_tops[r][base_tok] = 1.0;
        }
        double nu_best_arr[MAX_TOKENS];
        const double* lub_best = g_ws.d_l1_log_nu + (size_t)best_l1_blk * MAX_TOKENS;
        for (int t = 0; t < n_tokens; ++t) nu_best_arr[t] = exp(lub_best[t]);
        nu_best_arr[base_tok] = 1.0;
        const double* nu_l1 = nu_best_arr;
        std::vector<uint8_t> cand_added(n_pools, 0);
        double init_nu[MAX_TOKENS];
        for (int t = 0; t < n_tokens; ++t) init_nu[t] = exp(g_ws.d_init_log_nu[t]);
        init_nu[base_tok] = 1.0;

        for (int i = 0; i < n_pools; ++i) {
            const PoolData& p = g_ws.d_pools[i];
            if (p.token0 < 0 || p.token1 < 0 || p.token0 >= n_tokens || p.token1 >= n_tokens) continue;
            double best_vf = 0.0;
            double max_dx = 0.0;
            for (int r = 0; r < n_fusion; ++r) {
                int dir = 0;
                double dx = 0.0, dy = 0.0;
                pool_opt_trade(p, g_ws.d_tick_sp, g_ws.d_tick_liq, nu_tops[r][p.token0], nu_tops[r][p.token1], dir, dx, dy);
                if (dx > 1e-12 && dy > 0.0) {
                    int tin = (dir == 0) ? p.token0 : p.token1;
                    int tout = (dir == 0) ? p.token1 : p.token0;
                    double vf = nu_tops[r][tout] * dy - nu_tops[r][tin] * dx;
                    if (vf > best_vf) best_vf = vf;
                    if (dx > max_dx) max_dx = dx;
                }
                int rb = l1_eval_blks[r];
                double rdx = g_ws.d_l1_dx[(size_t)rb * (size_t)n_pools + i];
                if (rdx > 1e-12) {
                    int rdir = g_ws.d_l1_dir[(size_t)rb * (size_t)n_pools + i];
                    double rdy = pool_quote(p, g_ws.d_tick_sp, g_ws.d_tick_liq, rdir, rdx);
                    int rtin = (rdir == 0) ? p.token0 : p.token1;
                    int rtout = (rdir == 0) ? p.token1 : p.token0;
                    double vf = nu_tops[r][rtout] * rdy - nu_tops[r][rtin] * rdx;
                    if (vf > best_vf) best_vf = vf;
                    if (rdx > max_dx) max_dx = rdx;
                }
            }
            if (max_dx > 1e-12 || best_vf > 0.0) {
                double gu = std::max(1.0, p.gas_units);
                double gcost_i = std::max(1e-12, gu * gas_price_base);
                scored.push_back({i, best_vf, best_vf / gu, best_vf / gcost_i});
                cand_added[i] = 1;
            }
        }

        // Seed best bid & best ask per unordered (token0, token1) pair + top spread/depth pools
        {
            struct PairExt { int i0 = -1; double s0 = -1e18; int i1 = -1; double s1 = -1e18; };
            std::vector<std::pair<uint64_t, PairExt>> pmap;
            for (int i = 0; i < n_pools; ++i) {
                const PoolData& p = g_ws.d_pools[i];
                if (p.token0 < 0 || p.token1 < 0 || p.token0 >= n_tokens || p.token1 >= n_tokens) continue;
                int a = std::min(p.token0, p.token1), b = std::max(p.token0, p.token1);
                uint64_t key = ((uint64_t)(uint32_t)a << 32) | (uint32_t)b;
                double r_ab = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, (p.token0 == a) ? 0 : 1);
                double r_ba = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, (p.token0 == a) ? 1 : 0);
                double sc_ab = r_ab * (nu_l1[b] / std::max(1e-18, nu_l1[a]));
                double sc_ba = r_ba * (nu_l1[a] / std::max(1e-18, nu_l1[b]));
                int pos = -1;
                for (size_t k = 0; k < pmap.size(); ++k) if (pmap[k].first == key) { pos = (int)k; break; }
                if (pos < 0) { pmap.push_back({key, {}}); pos = (int)pmap.size() - 1; }
                if (sc_ab > pmap[pos].second.s0) { pmap[pos].second.s0 = sc_ab; pmap[pos].second.i0 = i; }
                if (sc_ba > pmap[pos].second.s1) { pmap[pos].second.s1 = sc_ba; pmap[pos].second.i1 = i; }
            }
            for (const auto& kv : pmap) {
                const auto& ex = kv.second;
                if (ex.s0 * ex.s1 > 0.992) {
                    for (int idx : {ex.i0, ex.i1}) {
                        if (idx >= 0 && !cand_added[idx] && (int)scored.size() < MAX_L2_CANDS) {
                            const PoolData& p = g_ws.d_pools[idx];
                            double gu = std::max(1.0, p.gas_units);
                            double vf = std::max(1e-8, (ex.s0 * ex.s1 - 0.99) * 1e-2);
                            scored.push_back({idx, vf, vf / gu, vf / std::max(1e-12, gu * gas_price_base)});
                            cand_added[idx] = 1;
                        }
                    }
                }
            }
        }
        std::vector<std::pair<double, int>> spread_rank;
        spread_rank.reserve(n_pools);
        for (int i = 0; i < n_pools; ++i) {
            if (cand_added[i]) continue;
            const PoolData& p = g_ws.d_pools[i];
            if (p.token0 < 0 || p.token1 < 0 || p.token0 >= n_tokens || p.token1 >= n_tokens) continue;
            double sr0 = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, 0) * (init_nu[p.token1] / init_nu[p.token0]) - 1.0;
            double sr1 = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, 1) * (init_nu[p.token0] / init_nu[p.token1]) - 1.0;
            double sp = std::max(sr0, sr1);
            if (sp > -0.004) {
                double depth = 1.0;
                if (p.pool_type == POOL_CLMM) {
                    depth = p.liq * sqrt(init_nu[p.token0] * init_nu[p.token1]);
                } else if (p.pool_type == POOL_PROP_BINS && g_ws.d_tick_liq) {
                    double lc0 = (p.tick_count > 0) ? g_ws.d_tick_liq[p.tick_offset] : 0.0;
                    double lc1 = (p.cur_seg > 0) ? g_ws.d_tick_liq[p.tick_offset + p.tick_count] : 0.0;
                    depth = lc0 * init_nu[p.token0] + lc1 * init_nu[p.token1];
                } else {
                    depth = p.r0 * init_nu[p.token0] + p.r1 * init_nu[p.token1];
                }
                spread_rank.push_back({(sp + 0.005) * sqrt(std::max(1.0, depth)), i});
            }
        }
        std::sort(spread_rank.begin(), spread_rank.end(),
                  [](const auto& a, const auto& b) { return a.first > b.first; });
        for (int j = 0; j < (int)spread_rank.size() && (int)scored.size() < MAX_L2_CANDS; ++j) {
            int idx = spread_rank[j].second;
            double vf = spread_rank[j].first * 1e-4;
            double gu = std::max(1.0, g_ws.d_pools[idx].gas_units);
            scored.push_back({idx, vf, vf / gu, vf / std::max(1e-12, gu * gas_price_base)});
        }

        std::sort(scored.begin(), scored.end(), [](const CandScore& a, const CandScore& b) {
            return a.value_flow > b.value_flow;
        });
        int n_cands = std::min((int)scored.size(), MAX_L2_CANDS);

        if (n_cands >= 2) {
            for (int c = 0; c < n_cands; ++c) g_ws.d_cand_pools[c] = scored[c].pool_idx;
            std::vector<int> ratio_order(n_cands);
            for (int c = 0; c < n_cands; ++c) ratio_order[c] = c;
            std::sort(ratio_order.begin(), ratio_order.end(), [&](int a, int b) {
                return scored[a].surplus_per_gas > scored[b].surplus_per_gas;
            });

            std::memset(g_ws.d_l2_masks, 0, (size_t)l2_blocks * (size_t)n_cands);
            const int ks[] = {2, 3, 4, 5, 6, 8, 10, 12, 14, 16, 20, 24, 28, 32, 40, 48, 64, 80, 96, 128, 160, 192, 256};
            const int num_ks = (int)(sizeof(ks) / sizeof(ks[0]));

            std::vector<double> tok_imp(n_tokens, 0.0);
            for (int c = 0; c < n_cands; ++c) {
                const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                tok_imp[p.token0] += scored[c].value_flow;
                tok_imp[p.token1] += scored[c].value_flow;
            }
            std::vector<int> hub_toks;
            hub_toks.reserve(n_tokens);
            for (int t = 0; t < n_tokens; ++t) if (t != base_tok && tok_imp[t] > 0.0) hub_toks.push_back(t);
            std::sort(hub_toks.begin(), hub_toks.end(), [&](int a, int b) { return tok_imp[a] > tok_imp[b]; });

            // Exact net-profit ranked minimal 2/3-hop cycles + verified incumbent fallback
            struct MinCycle { double prod; double net; double gross; double tgas; double dx; int n; int cs[3]; int dirs[3]; };
            std::vector<MinCycle> min_cycles;
            {
                struct DE { int to; int c; int dir; double r; double cap_b; };
                std::vector<std::vector<std::vector<DE>>> cadj(n_tokens, std::vector<std::vector<DE>>(n_tokens));
                auto edge_cap_base = [&](const PoolData& p, int dir) -> double {
                    double s2b = init_nu[(dir == 0) ? p.token0 : p.token1];
                    if (p.pool_type == POOL_CLMM) return std::max(1e-6, p.liq * p.sqrt_p * 0.02 * init_nu[p.token1]);
                    if (p.pool_type == POOL_PROP_BINS && g_ws.d_tick_liq) {
                        int off = p.tick_offset + ((dir == 0) ? 0 : p.tick_count);
                        return std::max(1e-6, g_ws.d_tick_liq[off] * s2b);
                    }
                    return std::max(1e-6, ((dir == 0) ? p.r0 : p.r1) * s2b * 0.5);
                };
                for (int c = 0; c < n_cands; ++c) {
                    const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                    double r0 = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, 0);
                    double r1 = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, 1);
                    if (r0 > 0.0) cadj[p.token0][p.token1].push_back({p.token1, c, 0, r0, edge_cap_base(p, 0)});
                    if (r1 > 0.0) cadj[p.token1][p.token0].push_back({p.token0, c, 1, r1, edge_cap_base(p, 1)});
                }
                for (int u = 0; u < n_tokens; ++u) {
                    for (int v = 0; v < n_tokens; ++v) {
                        auto& vec = cadj[u][v];
                        if (vec.size() > 3) {
                            std::sort(vec.begin(), vec.end(), [](const DE& a, const DE& b) { return a.r > b.r; });
                            vec.resize(3);
                        }
                    }
                }
                for (int u = 0; u < n_tokens; ++u) {
                    if (u == base_tok) continue;
                    for (const auto& e0 : cadj[base_tok][u]) {
                        for (const auto& e1 : cadj[u][base_tok]) {
                            if (e0.c == e1.c) continue;
                            double pr = e0.r * e1.r;
                            if (pr > 1.00001) min_cycles.push_back({pr, -1e18, 0.0, 0.0, 0.0, 2, {e0.c, e1.c, -1}, {e0.dir, e1.dir, 0}});
                        }
                        for (int v = 0; v < n_tokens; ++v) {
                            if (v == base_tok || v == u) continue;
                            for (const auto& e1 : cadj[u][v]) {
                                if (e1.c == e0.c) continue;
                                double r01 = e0.r * e1.r;
                                for (const auto& e2 : cadj[v][base_tok]) {
                                    if (e2.c == e0.c || e2.c == e1.c) continue;
                                    double pr = r01 * e2.r;
                                    if (pr > 1.00001) min_cycles.push_back({pr, -1e18, 0.0, 0.0, 0.0, 3, {e0.c, e1.c, e2.c}, {e0.dir, e1.dir, e2.dir}});
                                }
                            }
                        }
                    }
                }
                if (min_cycles.size() > 512) {
                    std::sort(min_cycles.begin(), min_cycles.end(), [](const MinCycle& a, const MinCycle& b) {
                        return a.prod > b.prod;
                    });
                    min_cycles.resize(512);
                }
                for (auto& mc : min_cycles) {
                    double tgas = base_tx_gas;
                    double min_ref = 1e18;
                    for (int j = 0; j < mc.n; ++j) {
                        const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[mc.cs[j]]];
                        tgas += p.gas_units;
                        double cp = edge_cap_base(p, mc.dirs[j]);
                        if (cp < min_ref) min_ref = cp;
                    }
                    mc.tgas = tgas;
                    if (max_gas_limit > 0.0 && tgas > max_gas_limit + 1e-6) { mc.net = -1e18; continue; }
                    double hi = std::max(1e-4, std::min(min_ref * 2.0, 1e8));
                    double lo = hi * 1e-6;
                    double gc = tgas * gas_price_base;
                    auto cyc_net = [&](double x) -> double {
                        double amt = x;
                        for (int j = 0; j < mc.n; ++j) {
                            amt = pool_quote(g_ws.d_pools[g_ws.d_cand_pools[mc.cs[j]]], g_ws.d_tick_sp, g_ws.d_tick_liq, mc.dirs[j], amt);
                        }
                        return amt - x - gc;
                    };
                    double bnet = -1e18, bx = lo;
                    for (int g = 0; g <= 36; ++g) {
                        double x = lo * pow(hi / lo, (double)g / 36.0);
                        double v = cyc_net(x);
                        if (v > bnet) { bnet = v; bx = x; }
                    }
                    if (bnet > 0.0) {
                        double a = bx * 0.25, b = bx * 4.0;
                        for (int it = 0; it < 22; ++it) {
                            double m1 = a + 0.381966011 * (b - a);
                            double m2 = a + 0.618033989 * (b - a);
                            double v1 = cyc_net(m1), v2 = cyc_net(m2);
                            if (v1 > bnet) { bnet = v1; bx = m1; }
                            if (v2 > bnet) { bnet = v2; bx = m2; }
                            if (v1 >= v2) b = m2; else a = m1;
                        }
                        mc.dx = bx;
                        mc.net = bnet;
                        mc.gross = bnet + gc;
                    } else {
                        mc.net = -1e18;
                    }
                }
                std::sort(min_cycles.begin(), min_cycles.end(), [](const MinCycle& a, const MinCycle& b) {
                    bool pa = a.net > 0.0, pb = b.net > 0.0;
                    if (pa != pb) return pa;
                    return pa ? (a.net > b.net) : (a.prod > b.prod);
                });
                if (!min_cycles.empty() && min_cycles[0].net > best_net && min_cycles[0].n <= max_steps_cap &&
                    (max_gas_limit <= 0.0 || min_cycles[0].tgas <= max_gas_limit + 1e-6)) {
                    const MinCycle& w = min_cycles[0];
                    std::vector<SolverStep> csteps;
                    double amt = w.dx;
                    for (int j = 0; j < w.n; ++j) {
                        int pi = g_ws.d_cand_pools[w.cs[j]];
                        double out = pool_quote(g_ws.d_pools[pi], g_ws.d_tick_sp, g_ws.d_tick_liq, w.dirs[j], amt);
                        csteps.push_back({pi, w.dirs[j], amt, out, g_ws.d_pools[pi].gas_units});
                        amt = out;
                    }
                    if (route_within_caps(csteps, w.tgas)) {
                        best_borrow = w.dx;
                        best_gross = amt - w.dx;
                        best_tgas = w.tgas;
                        best_gcost = w.tgas * gas_price_base;
                        best_net = best_gross - best_gcost;
                        best_steps = std::move(csteps);
                    }
                }
                if (min_cycles.size() > 16) min_cycles.resize(16);
            }

            const double abs_caps[11] = {
                450000.0, 900000.0, 1800000.0, 3100000.0, 5000000.0, 8000000.0,
                12000000.0, 16400000.0, 22000000.0, 26500000.0, 29900000.0
            };

            for (int b = 0; b < l2_blocks; ++b) {
                uint8_t* m = g_ws.d_l2_masks + (size_t)b * (size_t)n_cands;
                if (b < num_ks) {
                    int lim = std::min(ks[b], n_cands);
                    for (int c = 0; c < lim; ++c) m[c] = 1;
                } else if (b < 2 * num_ks) {
                    int lim = std::min(ks[b - num_ks], n_cands);
                    for (int r = 0; r < lim; ++r) m[ratio_order[r]] = 1;
                } else if (b < 2 * num_ks + 32) {
                    int kb = b - 2 * num_ks;
                    if (kb < 11 || (max_gas_limit > 0.0 && kb < 22)) {
                        double avail_gas = (max_gas_limit > 0.0) ? std::max(0.0, max_gas_limit - base_tx_gas) : 1e12;
                        double cap = (kb < 11) ? std::min(avail_gas, abs_caps[kb])
                                               : avail_gas * std::max(0.05, 1.0 - 0.08 * (double)(kb - 11));
                        bool by_ratio = (kb < 16);
                        double acc = 0.0;
                        int cnt = 0;
                        for (int idx = 0; idx < n_cands; ++idx) {
                            int c = by_ratio ? ratio_order[idx] : idx;
                            double g_i = g_ws.d_pools[g_ws.d_cand_pools[c]].gas_units;
                            if (acc + g_i <= cap) {
                                m[c] = 1;
                                acc += g_i;
                                ++cnt;
                            }
                        }
                        if (cnt == 0 && n_cands > 0) m[ratio_order[0]] = 1;
                    } else {
                        int tb = (max_gas_limit > 0.0) ? (kb - 22) * 3 : (kb - 11) * 2;
                        double tau = 0.04 * (double)tb;
                        for (int c = 0; c < n_cands; ++c) {
                            if (scored[c].surplus_ratio >= tau) m[c] = 1;
                        }
                    }
                } else if (b < 2 * num_ks + 32 + 64) {
                    int off_p = b - (2 * num_ks + 32);
                    if (!hub_toks.empty()) {
                        int n_h = std::min(12, (int)hub_toks.size());
                        if (off_p < 32 && !min_cycles.empty()) {
                            // 32 blocks: top positive-spot minimal 2/3-hop cycles tested across 3+ LRs
                            const MinCycle& mc = min_cycles[(off_p / 3) % (int)min_cycles.size()];
                            for (int j = 0; j < mc.n; ++j) m[mc.cs[j]] = 1;
                        } else if (off_p < 32) {
                            int t1 = hub_toks[off_p % hub_toks.size()];
                            int kept = 0;
                            for (int c = 0; c < n_cands && kept < 16; ++c) {
                                const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                                if ((p.token0 == base_tok && p.token1 == t1) || (p.token1 == base_tok && p.token0 == t1)) {
                                    m[c] = 1;
                                    ++kept;
                                }
                            }
                        } else {
                            int idx2 = off_p - 32;
                            int i1 = (idx2 / n_h) % n_h;
                            int i2 = (i1 + 1 + (idx2 % std::max(1, n_h - 1))) % n_h;
                            int t1 = hub_toks[i1], t2 = hub_toks[i2];
                            int kept = 0;
                            for (int c = 0; c < n_cands && kept < 24; ++c) {
                                const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                                if ((p.token0 == base_tok || p.token0 == t1 || p.token0 == t2) &&
                                    (p.token1 == base_tok || p.token1 == t1 || p.token1 == t2)) {
                                    m[c] = 1;
                                    ++kept;
                                }
                            }
                        }
                    }
                } else if (b < 2 * num_ks + 32 + 64 + 64) {
                    int idx_p = b - (2 * num_ks + 32 + 64);
                    if (max_route_tokens < MAX_TOKENS && idx_p < 32) {
                        static const int tok_budgets[7] = {4, 6, 8, 10, 12, 14, 16};
                        int slot = idx_p >> 1;
                        int tok_lim = std::min(tok_budgets[slot % 7], max_route_tokens);
                        bool by_ratio = ((idx_p & 1) == 0);
                        int start = ((slot / 7) * 3) % n_cands;
                        bool tok_used[MAX_TOKENS];
                        std::memset(tok_used, 0, (size_t)n_tokens * sizeof(bool));
                        tok_used[base_tok] = true;
                        int n_tok_used = 1;
                        for (int j = 0; j < n_cands; ++j) {
                            int ord = (start + j) % n_cands;
                            int c = by_ratio ? ratio_order[ord] : ord;
                            const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                            if (p.token0 < 0 || p.token0 >= n_tokens || p.token1 < 0 || p.token1 >= n_tokens) continue;
                            int add = (!tok_used[p.token0] ? 1 : 0) +
                                      (p.token1 != p.token0 && !tok_used[p.token1] ? 1 : 0);
                            if (n_tok_used + add <= tok_lim) {
                                m[c] = 1;
                                if (!tok_used[p.token0]) { tok_used[p.token0] = true; ++n_tok_used; }
                                if (!tok_used[p.token1]) { tok_used[p.token1] = true; ++n_tok_used; }
                            }
                        }
                    } else {
                        int base_lim = std::min(16 + (idx_p / 16) * 16, n_cands);
                        int drop_c = idx_p % std::max(1, base_lim);
                        for (int c = 0; c < base_lim; ++c) {
                            if (c != drop_c) m[ratio_order[c]] = 1;
                        }
                    }
                } else {
                    int target_k = 4 + ((b * 13) % std::min(160, n_cands));
                    int kept = 0;
                    for (int c = 0; c < n_cands && kept < target_k; ++c) {
                        double p_keep = (c < 12) ? 0.80 : (c < 64 ? 0.50 : 0.30);
                        double rnd = 0.5 * (det_jitter(b, c, 91) + 1.0);
                        if (rnd < p_keep) {
                            m[ratio_order[c]] = 1;
                            ++kept;
                        }
                    }
                    if (kept < 2) {
                        if (n_cands > 0) m[ratio_order[0]] = 1;
                        if (n_cands > 1) m[ratio_order[1]] = 1;
                    }
                }
                if (max_route_tokens < MAX_TOKENS) {
                    bool b_tok[MAX_TOKENS];
                    std::memset(b_tok, 0, (size_t)n_tokens * sizeof(bool));
                    b_tok[base_tok] = true;
                    int b_tok_cnt = 1;
                    bool by_vf = (b < num_ks);
                    for (int r = 0; r < n_cands; ++r) {
                        int c = by_vf ? r : ratio_order[r];
                        if (!m[c]) continue;
                        const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                        if (p.token0 < 0 || p.token0 >= n_tokens || p.token1 < 0 || p.token1 >= n_tokens) {
                            m[c] = 0;
                            continue;
                        }
                        int add = (!b_tok[p.token0] ? 1 : 0) +
                                  (p.token1 != p.token0 && !b_tok[p.token1] ? 1 : 0);
                        if (b_tok_cnt + add <= max_route_tokens) {
                            if (!b_tok[p.token0]) { b_tok[p.token0] = true; ++b_tok_cnt; }
                            if (!b_tok[p.token1]) { b_tok[p.token1] = true; ++b_tok_cnt; }
                        } else {
                            m[c] = 0;
                        }
                    }
                }
            }

            int l2_threads = (n_tokens > 128) ? 256 : 128;
            const double* l1_best_nu_ptr = g_ws.d_l1_log_nu + (size_t)best_l1_blk * MAX_TOKENS;
            kernel_layer2_gas_pruning<<<l2_blocks, l2_threads>>>(
                g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq,
                g_ws.d_cand_pools, g_ws.d_l2_masks, n_cands, n_tokens, base_tok,
                gas_price_base, base_tx_gas, max_gas_limit, g_ws.d_init_log_nu, l1_best_nu_ptr, l2_iters,
                g_ws.d_l2_dx, g_ws.d_l2_dir, g_ws.d_l2_net, g_ws.d_l2_gross, g_ws.d_l2_gas
            );
            CUDA_CHECK(cudaGetLastError());
            CUDA_CHECK(cudaDeviceSynchronize());

            std::vector<int> l2_order(l2_blocks);
            for (int b = 0; b < l2_blocks; ++b) l2_order[b] = b;
            std::sort(l2_order.begin(), l2_order.end(), [&](int a, int b) {
                return g_ws.d_l2_net[a] > g_ws.d_l2_net[b];
            });

            std::vector<double> full_dx(n_pools, 0.0);
            std::vector<int32_t> full_dir(n_pools, 0);
            std::vector<uint8_t> full_mask(n_pools, 0);

            int top_l2 = std::min(12, l2_blocks);
            int best_l2_b = -1;
            for (int bi = 0; bi < top_l2; ++bi) {
                int b = l2_order[bi];
                if (g_ws.d_l2_net[b] <= 0.0 && best_net > 0.0) break;
                if (bi >= 4 && g_ws.d_l2_net[b] <= best_net * 0.90) continue;
                std::fill(full_dx.begin(), full_dx.end(), 0.0);
                std::fill(full_dir.begin(), full_dir.end(), 0);
                std::fill(full_mask.begin(), full_mask.end(), 0);
                for (int c = 0; c < n_cands; ++c) {
                    int pi = g_ws.d_cand_pools[c];
                    full_mask[pi] = g_ws.d_l2_masks[(size_t)b * (size_t)n_cands + c];
                    full_dx[pi] = g_ws.d_l2_dx[(size_t)b * (size_t)n_cands + c];
                    full_dir[pi] = g_ws.d_l2_dir[(size_t)b * (size_t)n_cands + c];
                }
                double sb = 0.0, sg = 0.0, sgc = 0.0, stg = 0.0;
                double net = project_and_schedule_host(
                    g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, n_tokens, base_tok,
                    gas_price_base, base_tx_gas, max_gas_limit, nu_l1,
                    full_dx.data(), full_dir.data(), full_mask.data(),
                    tmp_steps, sb, sg, sgc, stg, false, true
                );
                if (net > best_net && route_within_caps(tmp_steps, stg)) {
                    best_net = net;
                    best_l2_b = b;
                    best_gross = sg; best_gcost = sgc; best_borrow = sb; best_tgas = stg; best_steps = tmp_steps;
                }
            }

            // Stage-2B Parallel GPU Polish Pass: subgraph-recentered warm nu + LR-decoupled drops, prefix trims & adds
            int pol_seed_b = (best_l2_b >= 0) ? best_l2_b : l2_order[0];
            if ((g_ws.d_l2_net[pol_seed_b] > 0.0 || !best_steps.empty()) && polish_blocks > 0) {
                std::vector<int> pool_to_cand(n_pools, -1);
                for (int c = 0; c < n_cands; ++c) pool_to_cand[g_ws.d_cand_pools[c]] = c;
                std::vector<uint8_t> exec_mask(n_cands, 0);
                int exec_cnt = 0;
                for (const auto& st : best_steps) {
                    int c = (st.pool_idx >= 0 && st.pool_idx < n_pools) ? pool_to_cand[st.pool_idx] : -1;
                    if (c >= 0 && !exec_mask[c]) {
                        exec_mask[c] = 1;
                        ++exec_cnt;
                    }
                }
                const uint8_t* raw_seed_mask = (exec_cnt >= 2) ? exec_mask.data() : (g_ws.d_l2_masks + (size_t)pol_seed_b * (size_t)n_cands);
                std::vector<uint8_t> clamped_seed(raw_seed_mask, raw_seed_mask + n_cands);
                bool seed_tok[MAX_TOKENS];
                std::memset(seed_tok, 0, (size_t)n_tokens * sizeof(bool));
                seed_tok[base_tok] = true;
                int seed_tok_cnt = 1;
                if (max_route_tokens < MAX_TOKENS) {
                    std::fill(clamped_seed.begin(), clamped_seed.end(), 0);
                    for (int r = 0; r < n_cands; ++r) {
                        int c = ratio_order[r];
                        if (!raw_seed_mask[c]) continue;
                        const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                        if (p.token0 < 0 || p.token0 >= n_tokens || p.token1 < 0 || p.token1 >= n_tokens) continue;
                        int add = (!seed_tok[p.token0] ? 1 : 0) +
                                  (p.token1 != p.token0 && !seed_tok[p.token1] ? 1 : 0);
                        if (seed_tok_cnt + add <= max_route_tokens) {
                            clamped_seed[c] = 1;
                            if (!seed_tok[p.token0]) { seed_tok[p.token0] = true; ++seed_tok_cnt; }
                            if (!seed_tok[p.token1]) { seed_tok[p.token1] = true; ++seed_tok_cnt; }
                        }
                    }
                } else {
                    for (int c = 0; c < n_cands; ++c) {
                        if (!clamped_seed[c]) continue;
                        const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                        if (p.token0 >= 0 && p.token0 < n_tokens && !seed_tok[p.token0]) { seed_tok[p.token0] = true; ++seed_tok_cnt; }
                        if (p.token1 >= 0 && p.token1 < n_tokens && !seed_tok[p.token1]) { seed_tok[p.token1] = true; ++seed_tok_cnt; }
                    }
                }
                const uint8_t* seed_mask = clamped_seed.data();
                std::vector<int> active_c, inactive_c;
                std::vector<PoolData> sub_pol;
                active_c.reserve(n_cands);
                inactive_c.reserve(n_cands);
                for (int r = n_cands - 1; r >= 0; --r) {
                    int c = ratio_order[r];
                    if (seed_mask[c]) {
                        active_c.push_back(c);
                        sub_pol.push_back(g_ws.d_pools[g_ws.d_cand_pools[c]]);
                    }
                }
                for (int r = 0; r < n_cands; ++r) {
                    int c = ratio_order[r];
                    if (!seed_mask[c]) inactive_c.push_back(c);
                }
                if (!sub_pol.empty()) {
                    compute_init_log_nu(sub_pol.data(), g_ws.d_tick_sp, g_ws.d_tick_liq, (int)sub_pol.size(), n_tokens, base_tok, g_ws.d_pol_warm_nu, 6);
                } else {
                    std::memcpy(g_ws.d_pol_warm_nu, g_ws.d_init_log_nu, (size_t)n_tokens * sizeof(double));
                }
                std::memset(g_ws.d_pol_masks, 0, (size_t)polish_blocks * (size_t)n_cands);
                int half_p = polish_blocks / 2;
                auto try_add_cand = [&](int c, bool* tok_seen, int& tok_cnt) -> bool {
                    const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                    if (p.token0 < 0 || p.token0 >= n_tokens || p.token1 < 0 || p.token1 >= n_tokens) return false;
                    int add = (!tok_seen[p.token0] ? 1 : 0) +
                              (p.token1 != p.token0 && !tok_seen[p.token1] ? 1 : 0);
                    if (max_route_tokens >= MAX_TOKENS || tok_cnt + add <= max_route_tokens) {
                        return true;
                    }
                    return false;
                };
                for (int pb = 0; pb < polish_blocks; ++pb) {
                    uint8_t* pm = g_ws.d_pol_masks + (size_t)pb * (size_t)n_cands;
                    std::memcpy(pm, seed_mask, (size_t)n_cands);
                    if (pb < 4) {
                        // Keep exact seed_mask across all 4 LR buckets (pb & 3)
                        continue;
                    }
                    if (pb < half_p && active_c.size() >= 3) {
                        // Decoupled LR vs dropped edge: every active edge tests all 4 LR buckets (pb & 3)
                        int drop_slot = (pb - 4) >> 2;
                        if (drop_slot < (int)active_c.size()) {
                            pm[active_c[drop_slot]] = 0;
                        } else {
                            int trim_k = std::max(2, ((int)active_c.size() * (8 - ((drop_slot - (int)active_c.size()) % 6))) / 10);
                            int to_drop = (int)active_c.size() - trim_k;
                            for (int d = 0; d < to_drop; ++d) pm[active_c[d]] = 0;
                        }
                    } else if (!inactive_c.empty()) {
                        int add_idx = (pb >= half_p) ? (pb - half_p) : (pb - 4);
                        int add_slot = add_idx >> 2;
                        bool tok_seen[MAX_TOKENS];
                        std::memcpy(tok_seen, seed_tok, (size_t)n_tokens * sizeof(bool));
                        int tok_cnt = seed_tok_cnt;
                        auto commit_cand = [&](int c) -> bool {
                            if (!try_add_cand(c, tok_seen, tok_cnt)) return false;
                            const PoolData& p = g_ws.d_pools[g_ws.d_cand_pools[c]];
                            pm[c] = 1;
                            if (!tok_seen[p.token0]) { tok_seen[p.token0] = true; ++tok_cnt; }
                            if (!tok_seen[p.token1]) { tok_seen[p.token1] = true; ++tok_cnt; }
                            return true;
                        };
                        if (add_slot >= 0 && add_slot < (int)inactive_c.size()) {
                            commit_cand(inactive_c[add_slot]);
                        } else if (add_slot >= 0) {
                            int n_extra = std::min((int)inactive_c.size(), 2 + (add_slot % 6));
                            int added = 0;
                            for (size_t a = 0; a < inactive_c.size() && added < n_extra; ++a) {
                                if (commit_cand(inactive_c[a])) ++added;
                            }
                        }
                    }
                }
                kernel_layer2_gas_pruning<<<polish_blocks, l2_threads>>>(
                    g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq,
                    g_ws.d_cand_pools, g_ws.d_pol_masks, n_cands, n_tokens, base_tok,
                    gas_price_base, base_tx_gas, max_gas_limit, g_ws.d_pol_warm_nu, l1_best_nu_ptr, std::min(85, l2_iters),
                    g_ws.d_pol_dx, g_ws.d_pol_dir, g_ws.d_pol_net, g_ws.d_pol_gross, g_ws.d_pol_gas
                );
                CUDA_CHECK(cudaGetLastError());
                CUDA_CHECK(cudaDeviceSynchronize());

                std::vector<int> pol_order(polish_blocks);
                for (int b = 0; b < polish_blocks; ++b) pol_order[b] = b;
                std::sort(pol_order.begin(), pol_order.end(), [&](int a, int b) {
                    return g_ws.d_pol_net[a] > g_ws.d_pol_net[b];
                });
                int top_pol = std::min(4, polish_blocks);
                for (int bi = 0; bi < top_pol; ++bi) {
                    int pb = pol_order[bi];
                    if (g_ws.d_pol_net[pb] <= 0.0) break;
                    std::fill(full_dx.begin(), full_dx.end(), 0.0);
                    std::fill(full_dir.begin(), full_dir.end(), 0);
                    std::fill(full_mask.begin(), full_mask.end(), 0);
                    for (int c = 0; c < n_cands; ++c) {
                        int pi = g_ws.d_cand_pools[c];
                        full_mask[pi] = g_ws.d_pol_masks[(size_t)pb * (size_t)n_cands + c];
                        full_dx[pi] = g_ws.d_pol_dx[(size_t)pb * (size_t)n_cands + c];
                        full_dir[pi] = g_ws.d_pol_dir[(size_t)pb * (size_t)n_cands + c];
                    }
                    double sb = 0.0, sg = 0.0, sgc = 0.0, stg = 0.0;
                    double net = project_and_schedule_host(
                        g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, n_tokens, base_tok,
                        gas_price_base, base_tx_gas, max_gas_limit, nu_l1,
                        full_dx.data(), full_dir.data(), full_mask.data(),
                        tmp_steps, sb, sg, sgc, stg, false, true
                    );
                    if (net > best_net && route_within_caps(tmp_steps, stg)) {
                        best_net = net;
                        best_gross = sg; best_gcost = sgc; best_borrow = sb; best_tgas = stg; best_steps = tmp_steps;
                    }
                }
            }
        }

        auto t_l2_end = std::chrono::steady_clock::now();
        l2_ms = std::chrono::duration<double, std::milli>(t_l2_end - t_l2_start).count();
    }

    auto t_end = std::chrono::steady_clock::now();
    double total_ms = std::chrono::duration<double, std::milli>(t_end - t_start).count();

    int emit_n = std::min((int)best_steps.size(), max_steps_cap);
    for (int k = 0; k < emit_n; ++k) {
        SolverStep s = best_steps[k];
        s.pool_idx = g_ws.gpu_to_orig[s.pool_idx];
        out_steps[k] = s;
    }

    out_result->num_steps = emit_n;
    out_result->max_steps_cap = max_steps_cap;
    out_result->borrow_amount = best_borrow;
    out_result->gross_profit = best_gross;
    out_result->gas_cost_base = best_gcost;
    out_result->net_profit = best_net;
    out_result->total_gas_units = best_tgas;
    out_result->max_deficit = 0.0;
    out_result->layer1_ms = l1_ms;
    out_result->layer2_ms = l2_ms;
    out_result->total_ms = total_ms;
    out_result->l1_unpruned_gross = raw_l1_gross;
    out_result->l1_unpruned_gas_cost = raw_l1_gcost;
    out_result->l1_unpruned_net = raw_l1_net;
    out_result->l1_unpruned_steps = raw_l1_steps;
    out_result->_reserved = 0;
    return 0;
}

extern "C" {

int cfmm_get_device_info(char* out_name, int name_cap, int* out_sm, int* out_cap_major, int* out_cap_minor, int* out_managed) {
    int dev = 0;
    cudaDeviceProp p{};
    CUDA_CHECK(cudaGetDeviceProperties(&p, dev));
    int managed = 0;
    CUDA_CHECK(cudaDeviceGetAttribute(&managed, cudaDevAttrManagedMemory, dev));
    if (out_name && name_cap > 0) std::snprintf(out_name, (size_t)name_cap, "%s", p.name);
    if (out_sm) *out_sm = p.multiProcessorCount;
    if (out_cap_major) *out_cap_major = p.major;
    if (out_cap_minor) *out_cap_minor = p.minor;
    if (out_managed) *out_managed = managed;
    return 0;
}

double cfmm_quote_single(const PoolData* pool, const double* tick_sp, const double* tick_liq, int32_t dir, double dx) {
    if (!pool) return 0.0;
    return pool_quote(*pool, tick_sp, tick_liq, dir, dx);
}

int cfmm_init_market(
    const PoolData* pools,
    int32_t n_pools,
    const double* tick_sp,
    const double* tick_liq,
    int32_t n_tick_entries,
    int32_t n_tokens
) {
    return load_market_internal(pools, n_pools, tick_sp, tick_liq, n_tick_entries, n_tokens, 384, 512);
}

int cfmm_update_dirty_pools(
    const int32_t* dirty_orig_indices,
    const PoolData* dirty_pools,
    int32_t n_dirty
) {
    if (n_dirty < 0 || !g_ws.d_pools) return -1;
    if (n_dirty == 0) return 0;
    if (!dirty_orig_indices || !dirty_pools) return -1;
    for (int i = 0; i < n_dirty; ++i) {
        int orig = dirty_orig_indices[i];
        if (orig < 0 || orig >= g_ws.cur_n_pools) return -1;
        int gidx = g_ws.orig_to_gpu[orig];
        PoolData p = dirty_pools[i];
        if (p.pool_type == POOL_CURVE_STABLE && p.liq <= 0.0) {
            p.liq = stable_get_D(p.r0, p.r1, p.param0);
        }
        g_ws.d_pools[gidx] = p;
    }
    if (g_ws.init_nu_valid && g_ws.cached_base_tok >= 0) {
        // Lightweight incremental anchor refresh (2 consensus passes)
        for (int pass = 0; pass < 2; ++pass) {
            for (int i = 0; i < n_dirty; ++i) {
                int gidx = g_ws.orig_to_gpu[dirty_orig_indices[i]];
                const PoolData& p = g_ws.d_pools[gidx];
                if (p.token0 < 0 || p.token1 < 0 || p.token0 >= g_ws.cur_n_tokens || p.token1 >= g_ws.cur_n_tokens) continue;
                double r01 = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, 0);
                double r10 = pool_spot_rate(p, g_ws.d_tick_sp, g_ws.d_tick_liq, 1);
                if (r01 <= 0.0 || r10 <= 0.0) continue;
                double lp = log(sqrt(r01 / r10));
                if (p.token0 != g_ws.cached_base_tok) {
                    g_ws.d_init_log_nu[p.token0] = 0.7 * g_ws.d_init_log_nu[p.token0] + 0.3 * (g_ws.d_init_log_nu[p.token1] + lp);
                }
                if (p.token1 != g_ws.cached_base_tok) {
                    g_ws.d_init_log_nu[p.token1] = 0.7 * g_ws.d_init_log_nu[p.token1] + 0.3 * (g_ws.d_init_log_nu[p.token0] - lp);
                }
            }
        }
    }
    return 0;
}

int cfmm_solve_resident(
    int32_t base_tok,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    int32_t budget_ms,
    int32_t enable_layer2,
    SolverStep* out_steps,
    int32_t max_steps_cap,
    SolveResult* out_result
) {
    auto t0 = std::chrono::steady_clock::now();
    return solve_resident_internal(
        base_tok, gas_price_base, base_tx_gas, max_gas_limit, budget_ms, enable_layer2,
        out_steps, max_steps_cap, out_result, t0
    );
}

int cfmm_solve_gpu(
    const PoolData* pools,
    int32_t n_pools,
    const double* tick_sp,
    const double* tick_liq,
    int32_t n_tick_entries,
    int32_t n_tokens,
    int32_t base_tok,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    int32_t budget_ms,
    int32_t enable_layer2,
    SolverStep* out_steps,
    int32_t max_steps_cap,
    SolveResult* out_result
) {
    auto t0 = std::chrono::steady_clock::now();
    int l1_b = (budget_ms <= 50) ? 96 : ((budget_ms <= 120) ? 192 : ((budget_ms <= 500) ? 288 : 384));
    int l2_b = (budget_ms <= 50) ? 128 : ((budget_ms <= 120) ? 256 : ((budget_ms <= 500) ? 384 : 512));
    if (load_market_internal(pools, n_pools, tick_sp, tick_liq, n_tick_entries, n_tokens, l1_b, l2_b) != 0) {
        return -1;
    }
    return solve_resident_internal(
        base_tok, gas_price_base, base_tx_gas, max_gas_limit, budget_ms, enable_layer2,
        out_steps, max_steps_cap, out_result, t0
    );
}

int cfmm_solve_cpu_greedy_2_3hop(
    const PoolData* pools,
    int32_t n_pools,
    const double* tick_sp,
    const double* tick_liq,
    int32_t n_tokens,
    int32_t base_tok,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    SolverStep* out_steps,
    int32_t max_steps_cap,
    SolveResult* out_result
) {
    if (!pools || !out_steps || !out_result || n_pools <= 0 || n_tokens <= 0 || n_tokens > MAX_TOKENS) return -1;
    auto t0 = std::chrono::steady_clock::now();

    std::vector<PoolData> local_pools(pools, pools + n_pools);
    for (int i = 0; i < n_pools; ++i) {
        if (local_pools[i].pool_type == POOL_CURVE_STABLE && local_pools[i].liq <= 0.0) {
            local_pools[i].liq = stable_get_D(local_pools[i].r0, local_pools[i].r1, local_pools[i].param0);
        }
    }

    std::vector<double> init_nu(n_tokens, 0.0);
    compute_init_log_nu(local_pools.data(), tick_sp, tick_liq, n_pools, n_tokens, base_tok, init_nu.data());

    struct Edge { int pool_idx; int dir; double rate; };
    std::vector<std::vector<std::vector<Edge>>> adj(n_tokens, std::vector<std::vector<Edge>>(n_tokens));
    for (int i = 0; i < n_pools; ++i) {
        const PoolData& p = local_pools[i];
        if (p.token0 < 0 || p.token1 < 0 || p.token0 >= n_tokens || p.token1 >= n_tokens) continue;
        double r0 = pool_spot_rate(p, tick_sp, tick_liq, 0);
        double r1 = pool_spot_rate(p, tick_sp, tick_liq, 1);
        if (r0 > 0.0) adj[p.token0][p.token1].push_back({i, 0, r0});
        if (r1 > 0.0) adj[p.token1][p.token0].push_back({i, 1, r1});
    }
    for (int u = 0; u < n_tokens; ++u) {
        for (int v = 0; v < n_tokens; ++v) {
            auto& vec = adj[u][v];
            if (vec.size() > 4) {
                std::sort(vec.begin(), vec.end(), [](const Edge& a, const Edge& b) { return a.rate > b.rate; });
                vec.resize(4);
            }
        }
    }

    auto eval_cycle = [&](const int* pids, const int* dirs, int len, double& best_dx, double& best_net, double& best_gross, double* best_outs) -> bool {
        double tot_gas = base_tx_gas;
        double min_ref = 1e18;
        for (int s = 0; s < len; ++s) tot_gas += local_pools[pids[s]].gas_units;
        if (max_gas_limit > 0.0 && tot_gas > max_gas_limit + 1e-6) return false;

        for (int s = 0; s < len; ++s) {
            const PoolData& p = local_pools[pids[s]];
            double scale_to_base = exp(init_nu[(dirs[s] == 0) ? p.token0 : p.token1]);
            double cap = 1e6;
            if (p.pool_type == POOL_UNIV2 || p.pool_type == POOL_CURVE_STABLE ||
                p.pool_type == POOL_SOLIDLY_STABLE || p.pool_type == POOL_BALANCER_V2) {
                cap = ((dirs[s] == 0) ? p.r0 : p.r1) * scale_to_base * 0.5;
            } else if (p.pool_type == POOL_CLMM) {
                cap = p.liq * p.sqrt_p * 0.02 * exp(init_nu[p.token1]);
            } else if (p.pool_type == POOL_PROP_BINS && tick_liq) {
                cap = ((dirs[s] == 0) ? tick_liq[p.tick_offset] : tick_liq[p.tick_offset + p.tick_count]) * scale_to_base;
            } else if (p.pool_type == POOL_METRIC_OMM) {
                cap = ((dirs[s] == 0) ? p.r0 : p.r1) * scale_to_base * 0.8;
            }
            if (cap > 1e-9 && cap < min_ref) min_ref = cap;
        }
        double hi = std::max(1e-4, std::min(min_ref * 2.0, 1e8));
        double lo = hi * 1e-6;
        double gcost = tot_gas * gas_price_base;

        auto obj = [&](double x) -> double {
            double amt = x;
            for (int s = 0; s < len; ++s) amt = pool_quote(local_pools[pids[s]], tick_sp, tick_liq, dirs[s], amt);
            return amt - x - gcost;
        };

        best_net = -1e18;
        best_dx = 0.0;
        double best_k = lo;
        for (int g = 0; g <= 36; ++g) {
            double x = lo * pow(hi / lo, (double)g / 36.0);
            double v = obj(x);
            if (v > best_net) { best_net = v; best_k = x; }
        }
        if (best_net <= 0.0) return false;
        double a = best_k * 0.25, b = best_k * 4.0;
        for (int it = 0; it < 20; ++it) {
            double m1 = a + 0.381966011 * (b - a);
            double m2 = a + 0.618033989 * (b - a);
            double v1 = obj(m1), v2 = obj(m2);
            if (v1 > best_net) { best_net = v1; best_k = m1; }
            if (v2 > best_net) { best_net = v2; best_k = m2; }
            if (v1 >= v2) b = m2; else a = m1;
        }
        best_dx = best_k;
        double amt = best_dx;
        for (int s = 0; s < len; ++s) {
            amt = pool_quote(local_pools[pids[s]], tick_sp, tick_liq, dirs[s], amt);
            best_outs[s] = amt;
        }
        best_gross = amt - best_dx;
        best_net = best_gross - gcost;
        return best_net > 0.0;
    };

    double best_net = 0.0, best_gross = 0.0, best_borrow = 0.0, best_tgas = 0.0;
    std::vector<SolverStep> best_steps;
    double tmp_outs[3];

    for (int u = 0; u < n_tokens; ++u) {
        if (u == base_tok) continue;
        for (const auto& e0 : adj[base_tok][u]) {
            for (const auto& e1 : adj[u][base_tok]) {
                if (e0.pool_idx == e1.pool_idx) continue;
                if (e0.rate * e1.rate <= 1.0) continue;
                int pids[2] = {e0.pool_idx, e1.pool_idx};
                int dirs[2] = {e0.dir, e1.dir};
                double dx = 0.0, net = 0.0, gross = 0.0;
                if (eval_cycle(pids, dirs, 2, dx, net, gross, tmp_outs) && net > best_net) {
                    best_net = net; best_gross = gross; best_borrow = dx;
                    best_tgas = base_tx_gas + local_pools[pids[0]].gas_units + local_pools[pids[1]].gas_units;
                    best_steps = {
                        {pids[0], dirs[0], dx, tmp_outs[0], local_pools[pids[0]].gas_units},
                        {pids[1], dirs[1], tmp_outs[0], tmp_outs[1], local_pools[pids[1]].gas_units},
                    };
                }
            }
        }
    }

    for (int u = 0; u < n_tokens; ++u) {
        if (u == base_tok) continue;
        for (const auto& e0 : adj[base_tok][u]) {
            for (int v = 0; v < n_tokens; ++v) {
                if (v == base_tok || v == u) continue;
                for (const auto& e1 : adj[u][v]) {
                    if (e1.pool_idx == e0.pool_idx) continue;
                    double r01 = e0.rate * e1.rate;
                    for (const auto& e2 : adj[v][base_tok]) {
                        if (e2.pool_idx == e0.pool_idx || e2.pool_idx == e1.pool_idx) continue;
                        if (r01 * e2.rate <= 1.0) continue;
                        int pids[3] = {e0.pool_idx, e1.pool_idx, e2.pool_idx};
                        int dirs[3] = {e0.dir, e1.dir, e2.dir};
                        double dx = 0.0, net = 0.0, gross = 0.0;
                        if (eval_cycle(pids, dirs, 3, dx, net, gross, tmp_outs) && net > best_net) {
                            best_net = net; best_gross = gross; best_borrow = dx;
                            best_tgas = base_tx_gas + local_pools[pids[0]].gas_units + local_pools[pids[1]].gas_units + local_pools[pids[2]].gas_units;
                            best_steps = {
                                {pids[0], dirs[0], dx, tmp_outs[0], local_pools[pids[0]].gas_units},
                                {pids[1], dirs[1], tmp_outs[0], tmp_outs[1], local_pools[pids[1]].gas_units},
                                {pids[2], dirs[2], tmp_outs[1], tmp_outs[2], local_pools[pids[2]].gas_units},
                            };
                        }
                    }
                }
            }
        }
    }

    auto t1 = std::chrono::steady_clock::now();
    int emit_n = std::min((int)best_steps.size(), max_steps_cap);
    for (int k = 0; k < emit_n; ++k) out_steps[k] = best_steps[k];

    out_result->num_steps = emit_n;
    out_result->max_steps_cap = max_steps_cap;
    out_result->borrow_amount = best_borrow;
    out_result->gross_profit = best_gross;
    out_result->gas_cost_base = best_tgas * gas_price_base;
    out_result->net_profit = best_net;
    out_result->total_gas_units = best_tgas;
    out_result->max_deficit = 0.0;
    out_result->layer1_ms = std::chrono::duration<double, std::milli>(t1 - t0).count();
    out_result->layer2_ms = 0.0;
    out_result->total_ms = out_result->layer1_ms;
    out_result->l1_unpruned_gross = best_gross;
    out_result->l1_unpruned_gas_cost = out_result->gas_cost_base;
    out_result->l1_unpruned_net = best_net;
    out_result->l1_unpruned_steps = emit_n;
    out_result->_reserved = 0;
    return 0;
}

int cfmm_solve_cpu_dual_single(
    const PoolData* pools,
    int32_t n_pools,
    const double* tick_sp,
    const double* tick_liq,
    int32_t n_tokens,
    int32_t base_tok,
    double gas_price_base,
    double base_tx_gas,
    double max_gas_limit,
    int32_t num_restarts,
    int32_t num_prune_masks,
    SolverStep* out_steps,
    int32_t max_steps_cap,
    SolveResult* out_result
) {
    if (!pools || !out_steps || !out_result || n_pools <= 0 || n_tokens <= 0 || n_tokens > MAX_TOKENS) return -1;
    auto t0 = std::chrono::steady_clock::now();

    std::vector<PoolData> local_pools(pools, pools + n_pools);
    for (int i = 0; i < n_pools; ++i) {
        if (local_pools[i].pool_type == POOL_CURVE_STABLE && local_pools[i].liq <= 0.0) {
            local_pools[i].liq = stable_get_D(local_pools[i].r0, local_pools[i].r1, local_pools[i].param0);
        }
    }

    std::vector<double> init_u(n_tokens, 0.0);
    compute_init_log_nu(local_pools.data(), tick_sp, tick_liq, n_pools, n_tokens, base_tok, init_u.data());

    const double lr_table[8] = {0.0005, 0.0015, 0.004, 0.01, 0.025, 0.05, 0.002, 0.008};
    auto run_dual_host = [&](const std::vector<double>& start_u, int blk, int iters, const std::vector<int>* sub_idx,
                             std::vector<double>& out_u, std::vector<double>& out_dx, std::vector<int32_t>& out_dir) -> double {
        std::vector<double> u(start_u), nu(n_tokens), psi(n_tokens), turn(n_tokens), m(n_tokens, 0.0), v(n_tokens, 0.0);
        if (blk > 0) {
            for (int t = 0; t < n_tokens; ++t) if (t != base_tok) u[t] += 0.008 * det_jitter(blk, t, 17);
        }
        u[base_tok] = 0.0;
        for (int t = 0; t < n_tokens; ++t) nu[t] = exp(u[t]);
        double lr0 = lr_table[blk & 7];
        int cnt = sub_idx ? (int)sub_idx->size() : n_pools;
        double gap = 1e9;

        for (int it = 0; it < iters; ++it) {
            std::fill(psi.begin(), psi.end(), 0.0);
            std::fill(turn.begin(), turn.end(), 0.0);
            for (int j = 0; j < cnt; ++j) {
                int pi = sub_idx ? (*sub_idx)[j] : j;
                const PoolData& p = local_pools[pi];
                int dir = 0;
                double dx = 0.0, dy = 0.0;
                pool_opt_trade(p, tick_sp, tick_liq, nu[p.token0], nu[p.token1], dir, dx, dy);
                if (dx > 1e-13 && dy > 0.0) {
                    int tin = (dir == 0) ? p.token0 : p.token1;
                    int tout = (dir == 0) ? p.token1 : p.token0;
                    psi[tin] -= dx;
                    psi[tout] += dy;
                    turn[tin] += nu[tin] * dx;
                    turn[tout] += nu[tout] * dy;
                }
            }
            for (int t = 0; t < n_tokens; ++t) {
                if (t == base_tok) continue;
                double grad = std::max(-1.0, std::min(1.0, (nu[t] * psi[t]) / (turn[t] + 1e-12)));
                m[t] = 0.85 * m[t] + 0.15 * grad;
                v[t] = 0.96 * v[t] + 0.04 * grad * grad;
                double lr = lr0 * (0.05 + 0.95 * (1.0 - (double)it / (double)iters));
                double step = std::max(-0.05, std::min(0.05, -lr * m[t] / (sqrt(v[t]) + 1e-4)));
                u[t] = std::max(start_u[t] - 2.5, std::min(start_u[t] + 2.5, u[t] + step));
                nu[t] = exp(u[t]);
            }
            if (it == iters - 1) {
                double ap = 0.0, tt = 0.0;
                for (int t = 0; t < n_tokens; ++t) if (t != base_tok) { ap += fabs(nu[t] * psi[t]); tt += turn[t]; }
                gap = (tt > 1e-12) ? (ap / tt) : 1e9;
            }
        }
        out_u = u;
        out_dx.assign(n_pools, 0.0);
        out_dir.assign(n_pools, 0);
        for (int j = 0; j < cnt; ++j) {
            int pi = sub_idx ? (*sub_idx)[j] : j;
            int dir = 0;
            double dx = 0.0, dy = 0.0;
            pool_opt_trade(local_pools[pi], tick_sp, tick_liq, nu[local_pools[pi].token0], nu[local_pools[pi].token1], dir, dx, dy);
            out_dx[pi] = dx;
            out_dir[pi] = dir;
        }
        return gap;
    };

    double best_gap = 1e18;
    std::vector<double> best_u, best_dx_raw, cur_u, cur_dx;
    std::vector<int32_t> best_dir_raw, cur_dir;
    for (int r = 0; r < num_restarts; ++r) {
        double gap = run_dual_host(init_u, r, 140, nullptr, cur_u, cur_dx, cur_dir);
        if (gap < best_gap || best_u.empty()) {
            best_gap = gap; best_u = cur_u; best_dx_raw = cur_dx; best_dir_raw = cur_dir;
        }
    }
    auto t2 = std::chrono::steady_clock::now();
    double l1_ms = std::chrono::duration<double, std::milli>(t2 - t0).count();

    std::vector<SolverStep> l1_steps, tmp_steps;
    double l1_borrow = 0.0, l1_gross = 0.0, l1_gcost = 0.0, l1_tgas = 0.0;
    std::vector<double> nu_val(n_tokens);
    for (int t = 0; t < n_tokens; ++t) nu_val[t] = exp(best_u[t]);
    nu_val[base_tok] = 1.0;

    double l1_net = project_and_schedule_host(
        local_pools.data(), tick_sp, tick_liq, n_pools, n_tokens, base_tok, gas_price_base,
        base_tx_gas, max_gas_limit, nu_val.data(),
        best_dx_raw.data(), best_dir_raw.data(), nullptr, l1_steps, l1_borrow, l1_gross, l1_gcost, l1_tgas, true, false
    );

    bool l1_ok = l1_net > 0.0 && (int)l1_steps.size() <= max_steps_cap && (max_gas_limit <= 0.0 || l1_tgas <= max_gas_limit + 1e-6);
    double best_net = l1_ok ? l1_net : 0.0;
    double best_gross = l1_ok ? l1_gross : 0.0;
    double best_gcost = l1_ok ? l1_gcost : 0.0;
    double best_borrow = l1_ok ? l1_borrow : 0.0;
    double best_tgas = l1_ok ? l1_tgas : 0.0;
    std::vector<SolverStep> best_steps = l1_ok ? l1_steps : std::vector<SolverStep>{};

    if (num_prune_masks > 0 && !best_u.empty()) {
        struct CS { int idx; double vf; double sr; };
        std::vector<CS> sc;
        for (int i = 0; i < n_pools; ++i) {
            if (best_dx_raw[i] > 1e-12) {
                const PoolData& p = local_pools[i];
                double dy = pool_quote(p, tick_sp, tick_liq, best_dir_raw[i], best_dx_raw[i]);
                int tin = (best_dir_raw[i] == 0) ? p.token0 : p.token1;
                int tout = (best_dir_raw[i] == 0) ? p.token1 : p.token0;
                double vf = nu_val[tout] * dy - nu_val[tin] * best_dx_raw[i];
                sc.push_back({i, vf, vf / std::max(1.0, p.gas_units)});
            }
        }
        std::sort(sc.begin(), sc.end(), [](const CS& a, const CS& b) { return a.sr > b.sr; });
        int n_cands = std::min((int)sc.size(), MAX_L2_CANDS);
        const int ks[] = {2, 3, 4, 6, 8, 10, 12, 16, 20, 24, 32, 48, 64, 96, 128, 256};
        for (int m = 0; m < num_prune_masks && n_cands >= 2; ++m) {
            int lim = std::min(ks[m % 16], n_cands);
            std::vector<int> sub_idx(lim);
            std::vector<uint8_t> mask(n_pools, 0);
            for (int c = 0; c < lim; ++c) { sub_idx[c] = sc[c].idx; mask[sc[c].idx] = 1; }
            run_dual_host(init_u, m % 4, 90, &sub_idx, cur_u, cur_dx, cur_dir);
            double sb = 0.0, sg = 0.0, sgc = 0.0, stg = 0.0;
            double net = project_and_schedule_host(
                local_pools.data(), tick_sp, tick_liq, n_pools, n_tokens, base_tok, gas_price_base,
                base_tx_gas, max_gas_limit, nu_val.data(), cur_dx.data(), cur_dir.data(), mask.data(),
                tmp_steps, sb, sg, sgc, stg, false, true
            );
            if (net > best_net && (int)tmp_steps.size() <= max_steps_cap &&
                (max_gas_limit <= 0.0 || stg <= max_gas_limit + 1e-6)) {
                best_net = net; best_gross = sg; best_gcost = sgc; best_borrow = sb; best_tgas = stg;
                best_steps = tmp_steps;
            }
        }
    }
    auto t3 = std::chrono::steady_clock::now();
    double l2_ms = std::chrono::duration<double, std::milli>(t3 - t2).count();
    double total_ms = std::chrono::duration<double, std::milli>(t3 - t0).count();

    int emit_n = std::min((int)best_steps.size(), max_steps_cap);
    for (int k = 0; k < emit_n; ++k) out_steps[k] = best_steps[k];

    out_result->num_steps = emit_n;
    out_result->max_steps_cap = max_steps_cap;
    out_result->borrow_amount = best_borrow;
    out_result->gross_profit = best_gross;
    out_result->gas_cost_base = best_gcost;
    out_result->net_profit = best_net;
    out_result->total_gas_units = best_tgas;
    out_result->max_deficit = 0.0;
    out_result->layer1_ms = l1_ms;
    out_result->layer2_ms = l2_ms;
    out_result->total_ms = total_ms;
    out_result->l1_unpruned_gross = l1_gross;
    out_result->l1_unpruned_gas_cost = l1_gcost;
    out_result->l1_unpruned_net = l1_net;
    out_result->l1_unpruned_steps = (int32_t)l1_steps.size();
    out_result->_reserved = 0;
    return 0;
}

int cfmm_benchmark_um_sync(int32_t n_pools, int32_t dirty_pools, int32_t repeats, double* out_avg_ms) {
    if (n_pools <= 0 || repeats <= 0 || !out_avg_ms) return -1;
    if (g_ws.ensure(n_pools, 64, 128, 128) != 0) return -1;
    for (int i = 0; i < n_pools; ++i) {
        std::memset(&g_ws.d_pools[i], 0, sizeof(PoolData));
        g_ws.d_pools[i].token0 = i % 15;
        g_ws.d_pools[i].token1 = (i + 1) % 15;
        g_ws.d_pools[i].pool_type = POOL_UNIV2;
        g_ws.d_pools[i].fee_ppm = 3000.0;
        g_ws.d_pools[i].gas_units = 52000.0;
        g_ws.d_pools[i].r0 = 1000.0;
        g_ws.d_pools[i].r1 = 1000.0;
    }
    for (int t = 0; t < 16; ++t) g_ws.d_init_log_nu[t] = 0.0;
    kernel_layer1_dual_opt<<<128, 128>>>(
        g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, 16, 0, g_ws.d_init_log_nu, 20,
        g_ws.d_l1_dx, g_ws.d_l1_dir, g_ws.d_l1_log_nu, g_ws.d_l1_gap
    );
    CUDA_CHECK(cudaDeviceSynchronize());

    auto t0 = std::chrono::steady_clock::now();
    for (int r = 0; r < repeats; ++r) {
        int d = std::min(dirty_pools, n_pools);
        for (int i = 0; i < d; ++i) g_ws.d_pools[i].r0 = 1000.0 + (r & 7);
        kernel_layer1_dual_opt<<<128, 128>>>(
            g_ws.d_pools, g_ws.d_tick_sp, g_ws.d_tick_liq, n_pools, 16, 0, g_ws.d_init_log_nu, 10,
            g_ws.d_l1_dx, g_ws.d_l1_dir, g_ws.d_l1_log_nu, g_ws.d_l1_gap
        );
        CUDA_CHECK(cudaDeviceSynchronize());
    }
    auto t1 = std::chrono::steady_clock::now();
    *out_avg_ms = std::chrono::duration<double, std::milli>(t1 - t0).count() / (double)repeats;
    return 0;
}

} // extern "C"
