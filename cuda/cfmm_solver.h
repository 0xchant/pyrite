#pragma once
#include <stdint.h>

#define MAX_TOKENS 256
#define MAX_L2_CANDS 256
#define MAX_CLMM_SEGS_PER_POOL 64

#define POOL_UNIV2 0
#define POOL_CLMM 1
#define POOL_CURVE_STABLE 2
#define POOL_PROP_BINS 3
#define POOL_SOLIDLY_STABLE 4
#define POOL_BALANCER_V2 5
#define POOL_METRIC_OMM 6

#define PROTO_V2 0
#define PROTO_V3 1
#define PROTO_CURVE 2
#define PROTO_PROP_BINS 3
#define PROTO_SOLIDLY 4
#define PROTO_BALANCER_V2 5
#define PROTO_METRIC_OMM 6
#define PROTO_UNIV4 7
#define PROTO_INFINITY_CL 8

#ifdef __cplusplus
extern "C" {
#endif

// 104-byte 8-byte-aligned pool record with CSR offsets into flat tick_sp/tick_liq arrays
struct PoolData {
    int32_t token0;
    int32_t token1;
    int32_t pool_type;   // 0..6
    int32_t protocol_id; // 0..8
    int32_t tick_offset; // CSR offset into tick_sp / tick_liq
    int32_t tick_count;  // CLMM: segments [1..64]; PROP_BINS: n_bids
    int32_t cur_seg;     // CLMM: active segment [0..tick_count-1]; PROP_BINS: n_asks
    int32_t _pad;
    double gas_units;    // calibrated execution gas per swap
    double fee_ppm;      // total swap fee + extra_tax_ppm
    double r0;           // human reserve 0
    double r1;           // human reserve 1
    double sqrt_p;       // CLMM: human-scaled sqrt(P); METRIC_OMM: sell_rate_human (t1/t0)
    double liq;          // CLMM: cur liq; CURVE: D; BAL_V2: w0; METRIC_OMM: buy_rate_human (t1/t0)
    double param0;       // CURVE: amp A; BAL_V2: w1; METRIC_OMM: max_walk
    double param1;       // METRIC_OMM: haircut0 (dir=0)
    double param2;       // METRIC_OMM: haircut1 (dir=1)
};

struct SolverStep {
    int32_t pool_idx;
    int32_t dir;
    double amount_in;
    double amount_out;
    double gas_units;
};

struct SolveResult {
    int32_t num_steps;
    int32_t max_steps_cap;
    double borrow_amount;
    double gross_profit;
    double gas_cost_base;
    double net_profit;
    double total_gas_units;
    double max_deficit;
    double layer1_ms;
    double layer2_ms;
    double total_ms;
    double l1_unpruned_gross;
    double l1_unpruned_gas_cost;
    double l1_unpruned_net;
    int32_t l1_unpruned_steps;
    int32_t _reserved;
};

int cfmm_get_device_info(
    char* out_name,
    int name_cap,
    int* out_sm,
    int* out_cap_major,
    int* out_cap_minor,
    int* out_managed
);

double cfmm_quote_single(
    const PoolData* pool,
    const double* tick_sp,
    const double* tick_liq,
    int32_t dir,
    double dx
);

int cfmm_init_market(
    const PoolData* pools,
    int32_t n_pools,
    const double* tick_sp,
    const double* tick_liq,
    int32_t n_tick_entries,
    int32_t n_tokens
);

int cfmm_update_dirty_pools(
    const int32_t* dirty_orig_indices,
    const PoolData* dirty_pools,
    int32_t n_dirty
);

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
);

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
);

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
);

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
);

int cfmm_benchmark_um_sync(
    int32_t n_pools,
    int32_t dirty_pools,
    int32_t repeats,
    double* out_avg_ms
);

#ifdef __cplusplus
}
#endif
