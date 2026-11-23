use alloy::primitives::{Address, B256};
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum DexKind {
    #[serde(rename = "univ2", alias = "UniV2", alias = "uni_v2")]
    UniV2 = 0,
    #[serde(alias = "Solidly")]
    Solidly = 1,
    #[serde(rename = "univ3", alias = "UniV3", alias = "uni_v3")]
    UniV3 = 2,
    #[serde(alias = "PancakeV3")]
    PancakeV3 = 3,
    #[serde(alias = "InfinityCl")]
    InfinityCl = 4,
    #[serde(rename = "univ4", alias = "UniV4", alias = "uni_v4")]
    UniV4 = 5,
    #[serde(alias = "MetricOmm")]
    MetricOmm = 6,
    #[serde(rename = "balancer_v2", alias = "BalancerV2")]
    BalancerV2 = 7,
}

impl DexKind {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            DexKind::UniV2 => "V2",
            DexKind::Solidly => "Solidly",
            DexKind::UniV3 => "UniV3",
            DexKind::PancakeV3 => "PancakeV3",
            DexKind::InfinityCl => "InfinityCL",
            DexKind::UniV4 => "UniV4",
            DexKind::MetricOmm => "Metric",
            DexKind::BalancerV2 => "BalV2",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FactoryStyle {
    #[serde(rename = "univ2", alias = "UniV2", alias = "uni_v2")]
    UniV2,
    SolidlyGetPair, // getPair(a,b,bool), getFee(bool)
    SolidlyGetPool, // getPool(a,b,bool), getFee(pool,bool)
    V3Fee,          // getPool(a,b,uint24)
    SlipstreamTs,   // getPool(a,b,int24)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TxType {
    Legacy,
    Eip1559,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmissionMode {
    #[serde(rename = "bep322_builders")]
    Bep322Builders,
    FlashbotsRelay,
    PublicRpc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BiddingMode {
    CoinbaseBribe,
    PriorityFee,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum L1FeeModel {
    #[default]
    None,
    OpStack,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterPoolFamily {
    Uniswap,
    Pancake,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderSpec {
    pub url: String,
    pub backend: String,
    #[serde(default)]
    pub bribe_recipient: Option<Address>,
    #[serde(default)]
    pub position_first: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContractsConfig {
    pub multicall3: Address,
    pub wrapped_native: Address,
    pub usd_anchor: Address,
    #[serde(default)]
    pub l1_gas_oracle: Option<Address>,
    #[serde(default)]
    pub morpho_blue: Option<Address>,
}

fn default_max_fee_multiplier_bps() -> u128 {
    11_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GasConfig {
    pub tx_type: TxType,
    pub min_gas_price_wei: u128,
    #[serde(default)]
    pub exec_gas_floor_wei: Option<u128>,
    #[serde(default = "default_max_fee_multiplier_bps")]
    pub max_fee_multiplier_bps: u128,
    #[serde(default)]
    pub bundle_priority_fee_wei: u128,
    #[serde(default)]
    pub l1_fee_model: L1FeeModel,
}

impl GasConfig {
    #[inline]
    pub fn effective_floor_wei(&self) -> u128 {
        self.exec_gas_floor_wei.unwrap_or(self.min_gas_price_wei)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmissionConfig {
    pub mode: SubmissionMode,
    #[serde(default)]
    pub builders: Vec<BuilderSpec>,
}

fn default_min_funding_native() -> f64 {
    0.0005
}

fn default_circuit_breaker_loss_usd() -> f64 {
    0.50
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BiddingConfig {
    pub mode: BiddingMode,
    #[serde(default = "default_min_funding_native")]
    pub min_funding_native: f64,
    #[serde(default)]
    pub init_gas_hint: String,
    #[serde(default = "default_circuit_breaker_loss_usd")]
    pub circuit_breaker_loss_usd: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RpcsConfig {
    #[serde(default)]
    pub ws: Vec<String>,
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub broadcast: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UniV4SingletonConfig {
    pub pool_manager: Address,
    pub position_manager: Address,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InfinityClSingletonConfig {
    pub pool_manager: Address,
    pub vault: Address,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BalancerV2SingletonConfig {
    pub vault: Address,
    #[serde(default)]
    pub seed_pool_ids: Vec<B256>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SingletonsConfig {
    #[serde(default)]
    pub univ4: Option<UniV4SingletonConfig>,
    #[serde(default)]
    pub infinity_cl: Option<InfinityClSingletonConfig>,
    #[serde(default)]
    pub balancer_v2: Option<BalancerV2SingletonConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MetricProtocolConfig {
    pub factory: Address,
    #[serde(default)]
    pub oracles: Vec<Address>,
    #[serde(default)]
    pub seed_pools: Vec<Address>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WatchedRouterSpec {
    pub name: String,
    pub address: Address,
    pub family: RouterPoolFamily,
    #[serde(default)]
    pub supports_exact_output_single: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MempoolConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub routers: Vec<WatchedRouterSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MevShareConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sse_url: String,
    #[serde(default)]
    pub relay_url: String,
    #[serde(default)]
    pub builders: Vec<String>,
    #[serde(default)]
    pub min_swap_usd: Option<f64>,
    #[serde(default)]
    pub max_blocks_ahead: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RawTimingOverrides {
    pub poll_interval_ms: Option<u64>,
    pub fast_poll_after_ms: Option<u64>,
    pub fast_poll_interval_ms: Option<u64>,
    pub discovery_budget_ms: Option<u64>,
    pub rpc_fast_ms: Option<u64>,
    pub rpc_fallback_ms: Option<u64>,
    pub rpc_receipts_timeout_ms: Option<u64>,
    pub rpc_call_timeout_ms: Option<u64>,
    pub rpc_bounded_timeout_ms: Option<u64>,
    pub rpc_get_logs_timeout_ms: Option<u64>,
    pub rpc_gas_price_timeout_ms: Option<u64>,
    pub sim_timeout_ms: Option<u64>,
    pub sim_fallback_budget_ms: Option<u64>,
    pub gas_refresh_blocks: Option<u64>,
    pub cache_save_blocks: Option<u64>,
    pub bundle_span_blocks: Option<u64>,
    pub passive_bundle_span_blocks: Option<u64>,
    pub backrun_span_blocks: Option<u64>,
    pub wait_receipt_ms: Option<u64>,
    pub bundle_grace_ms: Option<u64>,
    pub bundle_http_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingConfig {
    pub poll_interval_ms: u64,
    pub fast_poll_after_ms: u64,
    pub fast_poll_interval_ms: u64,
    pub discovery_budget_ms: u64,
    pub rpc_fast_ms: u64,
    pub rpc_fallback_ms: u64,
    pub rpc_receipts_timeout_ms: u64,
    pub rpc_call_timeout_ms: u64,
    pub rpc_bounded_timeout_ms: u64,
    pub rpc_get_logs_timeout_ms: u64,
    pub rpc_gas_price_timeout_ms: u64,
    pub sim_timeout_ms: u64,
    pub sim_fallback_budget_ms: u64,
    pub gas_refresh_blocks: u64,
    pub cache_save_blocks: u64,
    pub bundle_span_blocks: u64,
    pub passive_bundle_span_blocks: u64,
    pub backrun_span_blocks: u64,
    pub wait_receipt_ms: u64,
    pub bundle_grace_ms: u64,
    pub bundle_http_timeout_ms: u64,
}

impl TimingConfig {
    pub fn derive(block_time_ms: u64, ov: RawTimingOverrides) -> Self {
        let bt = block_time_ms.max(100);
        Self {
            poll_interval_ms: ov
                .poll_interval_ms
                .unwrap_or_else(|| (bt / 24).clamp(4, 500)),
            fast_poll_after_ms: ov
                .fast_poll_after_ms
                .unwrap_or_else(|| (bt * 65 / 100).max(50)),
            fast_poll_interval_ms: ov
                .fast_poll_interval_ms
                .unwrap_or_else(|| (bt / 150).clamp(1, 50)),
            discovery_budget_ms: ov
                .discovery_budget_ms
                .unwrap_or_else(|| (bt * 35 / 100).clamp(120, 5_000)),
            rpc_fast_ms: ov.rpc_fast_ms.unwrap_or_else(|| (bt * 3 / 100).clamp(38, 400)),
            rpc_fallback_ms: ov
                .rpc_fallback_ms
                .unwrap_or_else(|| (bt * 8 / 100).clamp(140, 1_200)),
            rpc_receipts_timeout_ms: ov
                .rpc_receipts_timeout_ms
                .unwrap_or_else(|| (bt / 8).clamp(110, 1_500)),
            rpc_call_timeout_ms: ov
                .rpc_call_timeout_ms
                .unwrap_or_else(|| (bt / 6).clamp(180, 1_800)),
            rpc_bounded_timeout_ms: ov
                .rpc_bounded_timeout_ms
                .unwrap_or_else(|| (bt / 4).clamp(550, 2_500)),
            rpc_get_logs_timeout_ms: ov
                .rpc_get_logs_timeout_ms
                .unwrap_or_else(|| (bt / 2).clamp(1_800, 4_500)),
            rpc_gas_price_timeout_ms: ov
                .rpc_gas_price_timeout_ms
                .unwrap_or_else(|| (bt / 12).clamp(250, 1_000)),
            sim_timeout_ms: ov
                .sim_timeout_ms
                .unwrap_or_else(|| (bt * 30 / 100).clamp(1_200, 3_600)),
            sim_fallback_budget_ms: ov
                .sim_fallback_budget_ms
                .unwrap_or_else(|| (bt * 5 / 100).clamp(95, 600)),
            gas_refresh_blocks: ov
                .gas_refresh_blocks
                .unwrap_or_else(|| (9_000 / bt).clamp(1, 30)),
            cache_save_blocks: ov
                .cache_save_blocks
                .unwrap_or_else(|| (120_000 / bt).clamp(5, 300)),
            bundle_span_blocks: ov
                .bundle_span_blocks
                .unwrap_or_else(|| (2_700 / bt).clamp(2, 10)),
            passive_bundle_span_blocks: ov
                .passive_bundle_span_blocks
                .unwrap_or_else(|| (4_500 / bt).clamp(2, 12)),
            backrun_span_blocks: ov
                .backrun_span_blocks
                .unwrap_or_else(|| (1_500 / bt).clamp(2, 6)),
            wait_receipt_ms: ov
                .wait_receipt_ms
                .unwrap_or_else(|| (bt * 5).clamp(2_200, 30_000)),
            bundle_grace_ms: ov
                .bundle_grace_ms
                .unwrap_or_else(|| (bt / 48).clamp(10, 250)),
            bundle_http_timeout_ms: ov
                .bundle_http_timeout_ms
                .unwrap_or_else(|| (bt / 4).clamp(650, 1_500)),
        }
    }

    #[inline]
    pub fn poll_interval(&self, elapsed: Duration) -> u64 {
        if elapsed >= Duration::from_millis(self.fast_poll_after_ms) {
            self.fast_poll_interval_ms
        } else {
            self.poll_interval_ms
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorySpec {
    pub name: String,
    pub address: Address,
    pub dex_kind: DexKind,
    pub style: FactoryStyle,
    pub default_fee_num: u16,
    #[serde(default)]
    pub v3_tiers: Vec<i32>,
    #[serde(default)]
    pub dynamic_fee_getter: bool,
    #[serde(default)]
    pub supports_flash_swap: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSeed {
    pub symbol: String,
    pub address: Address,
    pub decimals: u8,
    pub approx_usd: f64,
    #[serde(default)]
    pub is_base_anchor: bool,
    #[serde(default)]
    pub is_pinned_stable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashPoolCandidate {
    pub flash_mode: u8,
    pub pool: Address,
    pub base_token: Address,
    pub fee_bps: f64,
}

fn default_block_tag() -> String {
    "latest".to_string()
}

#[derive(Debug, Clone, Deserialize)]
struct ChainYaml {
    chain: String,
    chain_id: u64,
    native_symbol: String,
    default_native_usd: f64,
    block_time_ms: u64,
    #[serde(default = "default_block_tag")]
    block_tag: String,
    #[serde(default)]
    explorer_tx_url: String,
    #[serde(default)]
    phalcon_chain: String,
    #[serde(default)]
    executor_env_key: Option<String>,
    contracts: ContractsConfig,
    gas: GasConfig,
    submission: SubmissionConfig,
    bidding: BiddingConfig,
    rpcs: RpcsConfig,
    #[serde(default)]
    timing: RawTimingOverrides,
    #[serde(default)]
    singletons: SingletonsConfig,
    #[serde(default)]
    metric: Option<MetricProtocolConfig>,
    #[serde(default)]
    mempool: MempoolConfig,
    #[serde(default)]
    mev_share: MevShareConfig,
    #[serde(default)]
    hub_tokens: Vec<TokenSeed>,
    #[serde(default)]
    factories: Vec<FactorySpec>,
    #[serde(default)]
    seed_flash_pools: Vec<FlashPoolCandidate>,
}

#[derive(Debug, Clone)]
pub struct ChainConfig {
    pub chain: String,
    pub chain_id: u64,
    pub native_symbol: String,
    pub default_native_usd: f64,
    #[allow(dead_code)]
    pub block_time_ms: u64,
    pub block_tag: String,
    #[allow(dead_code)]
    pub explorer_tx_url: String,
    #[allow(dead_code)]
    pub phalcon_chain: String,
    executor_env_key_override: Option<String>,
    pub multicall3: Address,
    pub l1_gas_oracle: Option<Address>,
    #[allow(dead_code)]
    pub morpho_blue: Option<Address>,
    pub wrapped_native: Address,
    pub usd_anchor: Address,
    pub gas: GasConfig,
    pub submission: SubmissionConfig,
    pub bidding: BiddingConfig,
    pub ws_rpcs: Vec<String>,
    pub read_rpcs: Vec<String>,
    pub broadcast_rpcs: Vec<String>,
    pub bundle_rpcs: Vec<String>,
    pub timing: TimingConfig,
    pub singletons: SingletonsConfig,
    pub metric: Option<MetricProtocolConfig>,
    pub mempool: MempoolConfig,
    pub mev_share: MevShareConfig,
    pub hub_tokens: Vec<TokenSeed>,
    pub factories: Vec<FactorySpec>,
    pub seed_flash_pools: Vec<FlashPoolCandidate>,
}

impl ChainConfig {
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.chain
    }

    pub fn executor_env_key(&self) -> String {
        self.executor_env_key_override
            .clone()
            .unwrap_or_else(|| format!("EXECUTOR_ADDRESS_{}", self.chain.to_ascii_uppercase()))
    }

    #[inline]
    pub fn min_gas_price_wei(&self) -> u128 {
        self.gas.min_gas_price_wei
    }

    #[inline]
    pub fn is_mev_bundle_enabled(&self) -> bool {
        self.submission.mode != SubmissionMode::PublicRpc && !self.bundle_rpcs.is_empty()
    }

    pub fn factory_supports_flash(&self, factory_name: &str) -> bool {
        self.factories
            .iter()
            .find(|f| f.name == factory_name)
            .is_some_and(|f| f.supports_flash_swap)
    }

    #[inline]
    pub fn univ4_pool_manager(&self) -> Option<Address> {
        self.singletons.univ4.as_ref().map(|u| u.pool_manager)
    }

    #[inline]
    pub fn univ4_position_manager(&self) -> Option<Address> {
        self.singletons.univ4.as_ref().map(|u| u.position_manager)
    }

    #[inline]
    pub fn infinity_cl_pool_manager(&self) -> Option<Address> {
        self.singletons.infinity_cl.as_ref().map(|i| i.pool_manager)
    }

    #[inline]
    pub fn infinity_vault(&self) -> Option<Address> {
        self.singletons.infinity_cl.as_ref().map(|i| i.vault)
    }

    #[inline]
    pub fn metric_enabled(&self) -> bool {
        self.metric.is_some()
    }

    #[inline]
    pub fn metric_factory(&self) -> Option<Address> {
        self.metric.as_ref().map(|m| m.factory)
    }

    #[inline]
    pub fn metric_oracles(&self) -> &[Address] {
        self.metric.as_ref().map_or(&[], |m| &m.oracles)
    }

    #[inline]
    pub fn seed_metric_pools(&self) -> &[Address] {
        self.metric.as_ref().map_or(&[], |m| &m.seed_pools)
    }

    #[inline]
    pub fn balancer_v2_vault(&self) -> Option<Address> {
        self.singletons.balancer_v2.as_ref().map(|b| b.vault)
    }

    #[inline]
    pub fn balancer_v2_seed_pool_ids(&self) -> &[B256] {
        self.singletons
            .balancer_v2
            .as_ref()
            .map_or(&[], |b| &b.seed_pool_ids)
    }

    pub fn watched_router_addrs(&self) -> Vec<Address> {
        if !self.mempool.enabled {
            return Vec::new();
        }
        self.mempool.routers.iter().map(|r| r.address).collect()
    }

    #[inline]
    pub fn is_mev_share_enabled(&self) -> bool {
        self.mev_share.enabled
            && self.submission.mode == SubmissionMode::FlashbotsRelay
            && !self.mev_share.sse_url.is_empty()
            && !self.mev_share.relay_url.is_empty()
    }

    #[inline]
    pub fn mev_share_min_swap_usd(&self) -> f64 {
        self.mev_share.min_swap_usd.unwrap_or(50.0).max(1.0)
    }

    #[inline]
    pub fn mev_share_max_blocks_ahead(&self) -> u64 {
        self.mev_share.max_blocks_ahead.unwrap_or(3).max(1)
    }
}

fn parse_env_list(var: &str) -> Vec<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeModeConfig {
    pub dry_run: Option<bool>,
    pub min_profit_usd: Option<f64>,
    pub target_usd: Option<f64>,
    pub max_blocks: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeRpcsConfig {
    pub read: Vec<String>,
    pub ws: Vec<String>,
    pub broadcast: Vec<String>,
    pub bundle: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeMevShareConfig {
    pub enabled: Option<bool>,
    pub sse_url: Option<String>,
    pub relay_url: Option<String>,
    pub min_swap_usd: Option<f64>,
    pub max_blocks_ahead: Option<u64>,
    pub builders: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeSolverConfig {
    pub mode: Option<String>,
    pub gpu_lib: Option<String>,
    pub gpu_budget_ms: Option<u64>,
    pub gpu_max_gas_limit: Option<u64>,
    pub gpu_enable_layer2: Option<bool>,
    pub gpu_base_tx_gas: Option<u64>,
    pub gpu_gas_scale: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeLoggingConfig {
    pub rust_log: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RuntimeJsonConfig {
    pub chain: Option<String>,
    pub mode: RuntimeModeConfig,
    pub rpcs: RuntimeRpcsConfig,
    pub mev_share: RuntimeMevShareConfig,
    pub solver: RuntimeSolverConfig,
    pub executors: HashMap<String, String>,
    pub logging: RuntimeLoggingConfig,
}

fn set_env_if_missing(key: &str, value: &str) {
    let val = value.trim();
    if val.is_empty() {
        return;
    }
    let already_set = std::env::var(key)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if !already_set {
        std::env::set_var(key, val);
    }
}

impl RuntimeJsonConfig {
    /// Propagate non-secret runtime settings from `pyrite.json` into environment variables
    /// only when the corresponding env var is absent or empty.
    pub fn apply_to_env_if_unset(&self) {
        if !self.rpcs.read.is_empty() {
            set_env_if_missing("RPC_URLS", &self.rpcs.read.join(","));
        }
        if !self.rpcs.ws.is_empty() {
            set_env_if_missing("WS_RPC_URLS", &self.rpcs.ws.join(","));
        }
        if !self.rpcs.broadcast.is_empty() {
            set_env_if_missing("BROADCAST_RPC_URLS", &self.rpcs.broadcast.join(","));
        }
        if !self.rpcs.bundle.is_empty() {
            set_env_if_missing("BUNDLE_RPC_URLS", &self.rpcs.bundle.join(","));
        }
        if let Some(enabled) = self.mev_share.enabled {
            set_env_if_missing("MEV_SHARE_ENABLED", if enabled { "true" } else { "false" });
        }
        if let Some(ref url) = self.mev_share.sse_url {
            set_env_if_missing("MEV_SHARE_SSE_URL", url);
        }
        if let Some(ref url) = self.mev_share.relay_url {
            set_env_if_missing("MEV_SHARE_RELAY_URL", url);
        }
        if let Some(min_swap) = self.mev_share.min_swap_usd {
            if min_swap.is_finite() && min_swap > 0.0 {
                set_env_if_missing("MEV_SHARE_MIN_SWAP_USD", &min_swap.to_string());
            }
        }
        if let Some(max_ahead) = self.mev_share.max_blocks_ahead {
            if max_ahead > 0 {
                set_env_if_missing("MEV_SHARE_MAX_BLOCKS_AHEAD", &max_ahead.to_string());
            }
        }
        if !self.mev_share.builders.is_empty() {
            set_env_if_missing("MEV_SHARE_BUILDERS", &self.mev_share.builders.join(","));
        }
        if let Some(ref mode) = self.solver.mode {
            set_env_if_missing("SOLVER_MODE", mode);
        }
        if let Some(ref lib) = self.solver.gpu_lib {
            set_env_if_missing("CFMM_GPU_LIB", lib);
        }
        if let Some(ms) = self.solver.gpu_budget_ms {
            set_env_if_missing("GPU_BUDGET_MS", &ms.to_string());
        }
        if let Some(cap) = self.solver.gpu_max_gas_limit {
            set_env_if_missing("GPU_MAX_GAS_LIMIT", &cap.to_string());
        }
        if let Some(l2) = self.solver.gpu_enable_layer2 {
            set_env_if_missing("GPU_ENABLE_LAYER2", if l2 { "1" } else { "0" });
        }
        if let Some(base) = self.solver.gpu_base_tx_gas {
            set_env_if_missing("GPU_BASE_TX_GAS", &base.to_string());
        }
        if let Some(scale) = self.solver.gpu_gas_scale {
            if scale.is_finite() && scale > 0.0 {
                set_env_if_missing("GPU_GAS_SCALE", &scale.to_string());
            }
        }
        for (chain_name, addr) in &self.executors {
            let clean_chain = chain_name.trim().to_ascii_uppercase();
            if !clean_chain.is_empty() && !addr.trim().is_empty() {
                set_env_if_missing(&format!("EXECUTOR_ADDRESS_{clean_chain}"), addr);
            }
        }
        if let Some(ref rust_log) = self.logging.rust_log {
            set_env_if_missing("RUST_LOG", rust_log);
        }
    }
}

/// Load optional JSON runtime config (`--config`, `$PYRITE_CONFIG`, or `./pyrite.json`).
pub fn load_runtime_json_config(explicit_path: Option<&str>) -> Result<Option<RuntimeJsonConfig>> {
    let resolved = if let Some(p) = explicit_path.map(str::trim).filter(|s| !s.is_empty()) {
        let path = PathBuf::from(p);
        if !path.is_file() {
            return Err(eyre!("JSON config file not found: {}", path.display()));
        }
        Some(path)
    } else if let Ok(env_p) = std::env::var("PYRITE_CONFIG") {
        let p = env_p.trim();
        if !p.is_empty() {
            let path = PathBuf::from(p);
            if !path.is_file() {
                return Err(eyre!("PYRITE_CONFIG file not found: {}", path.display()));
            }
            Some(path)
        } else {
            None
        }
    } else {
        let default_path = PathBuf::from("pyrite.json");
        default_path.is_file().then_some(default_path)
    };

    let Some(path) = resolved else {
        return Ok(None);
    };
    let raw = fs::read_to_string(&path)
        .map_err(|e| eyre!("failed reading JSON config {}: {e}", path.display()))?;
    let parsed: RuntimeJsonConfig = serde_json::from_str(&raw)
        .map_err(|e| eyre!("failed parsing JSON config {}: {e}", path.display()))?;
    Ok(Some(parsed))
}

pub fn resolve_chain_yaml_path(chain_or_path: &str) -> Result<PathBuf> {
    let direct = Path::new(chain_or_path);
    if chain_or_path.ends_with(".yaml")
        || chain_or_path.ends_with(".yml")
        || chain_or_path.contains('/')
        || chain_or_path.contains('\\')
        || direct.is_file()
    {
        if direct.is_file() {
            return Ok(direct.to_path_buf());
        }
        return Err(eyre!("chain YAML file not found: {chain_or_path}"));
    }

    let name = chain_or_path.trim().to_ascii_lowercase();
    let filename = format!("{name}.yaml");

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("PYRITE_CHAINS_DIR") {
        if !dir.trim().is_empty() {
            candidates.push(Path::new(dir.trim()).join(&filename));
        }
    }
    candidates.push(Path::new("chains").join(&filename));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("chains").join(&filename));
        }
    }
    candidates.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("chains")
            .join(&filename),
    );

    for p in &candidates {
        if p.is_file() {
            return Ok(p.clone());
        }
    }

    Err(eyre!(
        "unknown chain '{chain_or_path}'; expected YAML config in chains/{filename}"
    ))
}

pub fn load_chain_config(chain_or_path: &str) -> Result<ChainConfig> {
    let path = resolve_chain_yaml_path(chain_or_path)?;
    let raw = fs::read_to_string(&path)
        .map_err(|e| eyre!("failed reading {}: {e}", path.display()))?;
    let parsed: ChainYaml = serde_yaml::from_str(&raw)
        .map_err(|e| eyre!("failed parsing {}: {e}", path.display()))?;

    let custom_ws = parse_env_list("WS_RPC_URLS");
    let custom_read = parse_env_list("RPC_URLS");
    let custom_broadcast = parse_env_list("BROADCAST_RPC_URLS");
    let custom_bundle = parse_env_list("BUNDLE_RPC_URLS");

    let ws_rpcs = if custom_ws.is_empty() {
        parsed.rpcs.ws
    } else {
        custom_ws
    };
    let read_rpcs = if custom_read.is_empty() {
        parsed.rpcs.read
    } else {
        custom_read
    };
    let broadcast_rpcs = if custom_broadcast.is_empty() {
        parsed.rpcs.broadcast
    } else {
        custom_broadcast
    };
    let mut submission = parsed.submission;
    let bundle_rpcs = if custom_bundle.is_empty() {
        submission.builders.iter().map(|b| b.url.clone()).collect()
    } else {
        submission.builders.retain(|b| custom_bundle.contains(&b.url));
        for u in &custom_bundle {
            if !submission.builders.iter().any(|b| &b.url == u) {
                submission.builders.push(BuilderSpec {
                    url: u.clone(),
                    backend: u.clone(),
                    bribe_recipient: None,
                    position_first: false,
                });
            }
        }
        custom_bundle
    };

    if parsed.chain_id == 0 {
        return Err(eyre!("{}: chain_id must be > 0", path.display()));
    }
    if read_rpcs.is_empty() {
        return Err(eyre!("{}: rpcs.read must be non-empty", path.display()));
    }

    let timing = TimingConfig::derive(parsed.block_time_ms, parsed.timing);

    let mut mev_share = parsed.mev_share;
    if let Ok(v) = std::env::var("MEV_SHARE_ENABLED") {
        let low = v.trim().to_ascii_lowercase();
        if matches!(low.as_str(), "0" | "false" | "no") {
            mev_share.enabled = false;
        } else if matches!(low.as_str(), "1" | "true" | "yes") {
            mev_share.enabled = true;
        }
    }
    if let Ok(url) = std::env::var("MEV_SHARE_SSE_URL") {
        let u = url.trim();
        if !u.is_empty() {
            mev_share.sse_url = u.to_string();
        }
    }
    if let Ok(url) = std::env::var("MEV_SHARE_RELAY_URL") {
        let u = url.trim();
        if !u.is_empty() {
            mev_share.relay_url = u.to_string();
        }
    }
    if let Ok(v) = std::env::var("MEV_SHARE_MIN_SWAP_USD") {
        if let Ok(num) = v.trim().parse::<f64>() {
            if num.is_finite() && num > 0.0 {
                mev_share.min_swap_usd = Some(num);
            }
        }
    }
    if let Ok(v) = std::env::var("MEV_SHARE_MAX_BLOCKS_AHEAD") {
        if let Ok(num) = v.trim().parse::<u64>() {
            if num > 0 {
                mev_share.max_blocks_ahead = Some(num);
            }
        }
    }
    let custom_ms_builders = parse_env_list("MEV_SHARE_BUILDERS");
    if !custom_ms_builders.is_empty() {
        mev_share.builders = custom_ms_builders;
    }
    if mev_share.enabled {
        if mev_share.sse_url.trim().is_empty() {
            mev_share.sse_url = "https://mev-share.flashbots.net".to_string();
        }
        if mev_share.relay_url.trim().is_empty() {
            mev_share.relay_url = "https://relay.flashbots.net".to_string();
        }
    }

    Ok(ChainConfig {
        chain: parsed.chain,
        chain_id: parsed.chain_id,
        native_symbol: parsed.native_symbol,
        default_native_usd: parsed.default_native_usd,
        block_time_ms: parsed.block_time_ms,
        block_tag: parsed.block_tag,
        explorer_tx_url: parsed.explorer_tx_url,
        phalcon_chain: parsed.phalcon_chain,
        executor_env_key_override: parsed.executor_env_key,
        multicall3: parsed.contracts.multicall3,
        l1_gas_oracle: parsed.contracts.l1_gas_oracle,
        morpho_blue: parsed.contracts.morpho_blue,
        wrapped_native: parsed.contracts.wrapped_native,
        usd_anchor: parsed.contracts.usd_anchor,
        gas: parsed.gas,
        submission,
        bidding: parsed.bidding,
        ws_rpcs,
        read_rpcs,
        broadcast_rpcs,
        bundle_rpcs,
        timing,
        singletons: parsed.singletons,
        metric: parsed.metric,
        mempool: parsed.mempool,
        mev_share,
        hub_tokens: parsed.hub_tokens,
        factories: parsed.factories,
        seed_flash_pools: parsed.seed_flash_pools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    #[test]
    fn test_load_bsc_eth_base_yaml_configs() {
        let bsc = load_chain_config("bsc").unwrap();
        assert_eq!(bsc.chain, "bsc");
        assert_eq!(bsc.chain_id, 56);
        assert_eq!(bsc.native_symbol, "BNB");
        assert_eq!(bsc.gas.tx_type, TxType::Legacy);
        assert_eq!(bsc.submission.mode, SubmissionMode::Bep322Builders);
        assert_eq!(bsc.bidding.mode, BiddingMode::CoinbaseBribe);
        assert!(bsc.metric_enabled());
        assert_eq!(bsc.seed_metric_pools().len(), 10);
        assert_eq!(bsc.factories.len(), 12);
        assert_eq!(bsc.seed_flash_pools.len(), 11);
        assert!(bsc.univ4_pool_manager().is_some());
        assert!(bsc.infinity_cl_pool_manager().is_some());
        assert_eq!(bsc.timing.discovery_budget_ms, 140);
        assert_eq!(bsc.timing.gas_refresh_blocks, 20);
        assert_eq!(bsc.timing.bundle_span_blocks, 6);
        assert!(bsc.factories.iter().find(|f| f.name == "BiSwapV2").unwrap().dynamic_fee_getter);
        assert!(!bsc.is_mev_share_enabled());

        let eth = load_chain_config("eth").unwrap();
        assert_eq!(eth.chain, "eth");
        assert!(eth.is_mev_share_enabled());
        assert!(eth.mev_share.builders.contains(&"Titan".to_string()));
        assert_eq!(eth.chain_id, 1);
        assert_eq!(eth.native_symbol, "ETH");
        assert_eq!(eth.gas.tx_type, TxType::Eip1559);
        assert_eq!(eth.submission.mode, SubmissionMode::FlashbotsRelay);
        assert_eq!(eth.bidding.mode, BiddingMode::CoinbaseBribe);
        assert_eq!(
            eth.univ4_pool_manager(),
            Some(address!("000000000004444c5dc75cB358380D2e3dE08A90"))
        );
        assert_eq!(
            eth.univ4_position_manager(),
            Some(address!("bD216513d74C8cf14cf4747E6AaA6420FF64ee9e"))
        );
        assert!(eth.infinity_cl_pool_manager().is_none());
        assert!(eth.metric_enabled());
        assert_eq!(eth.seed_metric_pools().len(), 18);
        assert!(eth.balancer_v2_vault().is_some());
        assert!(eth.morpho_blue.is_some());
        assert_eq!(eth.factories.len(), 6);
        assert_eq!(eth.seed_flash_pools.len(), 16);
        assert_eq!(eth.watched_router_addrs().len(), 4);
        assert_eq!(eth.timing.discovery_budget_ms, 4_200);
        assert_eq!(eth.timing.gas_refresh_blocks, 1);
        assert_eq!(eth.timing.sim_timeout_ms, 3_600);
        assert_eq!(eth.timing.bundle_span_blocks, 2);

        let base = load_chain_config("base").unwrap();
        assert_eq!(base.chain, "base");
        assert_eq!(base.chain_id, 8453);
        assert_eq!(base.gas.tx_type, TxType::Eip1559);
        assert_eq!(base.gas.l1_fee_model, L1FeeModel::OpStack);
        assert_eq!(base.submission.mode, SubmissionMode::PublicRpc);
        assert_eq!(base.bidding.mode, BiddingMode::PriorityFee);
        assert!(!base.is_mev_bundle_enabled());
        assert!(base.watched_router_addrs().is_empty());
        assert_eq!(base.factories.len(), 8);
    }

    #[test]
    fn test_load_chain_by_file_path_and_timing_derive() {
        let manifest_eth = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("chains")
            .join("eth.yaml");
        let cfg = load_chain_config(manifest_eth.to_str().unwrap()).unwrap();
        assert_eq!(cfg.chain_id, 1);

        let derived_450 = TimingConfig::derive(450, RawTimingOverrides::default());
        assert_eq!(derived_450.gas_refresh_blocks, 20);
        assert_eq!(derived_450.bundle_span_blocks, 6);
        assert_eq!(derived_450.passive_bundle_span_blocks, 10);
        assert_eq!(derived_450.backrun_span_blocks, 3);
        assert_eq!(derived_450.rpc_fast_ms, 38);
        assert_eq!(derived_450.rpc_fallback_ms, 140);

        let derived_12000 = TimingConfig::derive(12_000, RawTimingOverrides::default());
        assert_eq!(derived_12000.discovery_budget_ms, 4_200);
        assert_eq!(derived_12000.gas_refresh_blocks, 1);
        assert_eq!(derived_12000.sim_timeout_ms, 3_600);
        assert_eq!(derived_12000.cache_save_blocks, 10);
        assert_eq!(derived_12000.bundle_span_blocks, 2);
    }

    #[test]
    fn test_load_pyrite_example_json_config() {
        let ex_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("pyrite.example.json");
        let cfg = load_runtime_json_config(Some(ex_path.to_str().unwrap()))
            .unwrap()
            .expect("pyrite.example.json should parse");
        assert_eq!(cfg.chain.as_deref(), Some("eth"));
        assert_eq!(cfg.mode.dry_run, Some(true));
        assert_eq!(cfg.mode.min_profit_usd, Some(0.001));
        assert_eq!(cfg.solver.mode.as_deref(), Some("cpu"));
        assert_eq!(cfg.mev_share.enabled, Some(true));
        assert_eq!(
            cfg.mev_share.sse_url.as_deref(),
            Some("https://mev-share.flashbots.net")
        );
    }
}
