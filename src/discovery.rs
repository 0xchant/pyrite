use alloy::primitives::{Address, B256, I256, U256};
use alloy::sol_types::SolCall;
use eyre::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::abi::{
    IBalancerV2Pool, IBalancerV2Vault, IERC20, IInfinityCLPoolManager, IMetricOmmPool, IMulticall3,
    ISlipstreamFactory, ISolidlyFactoryGetPair, ISolidlyFactoryGetPool, ISolidlyPair,
    IUniV2Factory, IUniV2Pair, IUniV4PoolManager, IUniV4PositionManager, IV3Factory, IV3Pool,
};
use crate::amm::{
    u256_to_f64, u256_to_human, BalancerPoolKind, BalancerPoolState, MetricPoolState, PoolState,
    TokenMeta, V4PoolKey,
};
use crate::config::{ChainConfig, DexKind, FactorySpec, FactoryStyle, FlashPoolCandidate};
use crate::rpc::{RpcPool, TxReceipt};

// Verified topic0 hashes
const TOPIC_UNIV2_SYNC: B256 =
    alloy::primitives::b256!("1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1");
const TOPIC_SOLIDLY_SYNC: B256 =
    alloy::primitives::b256!("cf2aa50876cdfbb541206f89af0ee78d44a2abf8d328e37fa4917f982149848a");
const TOPIC_UNIV3_SWAP: B256 =
    alloy::primitives::b256!("c42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67");
const TOPIC_PANCAKEV3_SWAP: B256 =
    alloy::primitives::b256!("19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83");
const TOPIC_V3_MINT: B256 =
    alloy::primitives::b256!("7a53080ba414158be7ec69b987b5fb7d07dee101fe85488f0853ae16239d0bde");
const TOPIC_V3_BURN: B256 =
    alloy::primitives::b256!("0c396cd989a39f4459b5fa1aed6a9a8dcdbc45908acfd67e028cd568da98982c");
const TOPIC_INFINITY_CL_SWAP: B256 =
    alloy::primitives::b256!("04206ad2b7c0f463bff3dd4f33c5735b0f2957a351e4f79763a4fa9e775dd237");
const TOPIC_UNIV4_SWAP: B256 =
    alloy::primitives::b256!("40e9cecb9f5f1f1c5b9c97dec2917b7ee92e57ba5563708daca94dd84ad7112f");
const TOPIC_UNIV4_INIT: B256 =
    alloy::primitives::b256!("dd466e674ea557f56295e2d0218a125ea4b4f0f6f3307b95f85e6110838d6438");
const TOPIC_METRIC_SWAP: B256 =
    alloy::primitives::b256!("9734819749a91fc3be03ea83205f924ee08479bd3f0da48efc91d94d050cac1e");
const TOPIC_METRIC_PRICE_READ: B256 =
    alloy::primitives::b256!("49435801cf735ac077f83ed90d053b54c89f865de1791ad128ff92af1ac09507");
const TOPIC_BALANCER_V2_SWAP: B256 =
    alloy::primitives::b256!("2170c741c41531aec20e7c107c24eecfdd15e69c9bb0a8dd37b1840b9e0b207b");
const TOPIC_BALANCER_V2_BALANCE_CHANGED: B256 =
    alloy::primitives::b256!("e5ce249087ce04f05a957192435400fd97868dba0e6a4b4c049abf8af80dae78");

/// Return true if `topic0` represents a DEX Sync/Swap log usable for MEV-Share state projection.
#[inline]
pub fn is_mev_share_dex_topic(topic0: B256) -> bool {
    matches!(
        topic0,
        TOPIC_UNIV2_SYNC
            | TOPIC_SOLIDLY_SYNC
            | TOPIC_UNIV3_SWAP
            | TOPIC_PANCAKEV3_SWAP
            | TOPIC_UNIV4_SWAP
            | TOPIC_INFINITY_CL_SWAP
            | TOPIC_BALANCER_V2_SWAP
    )
}

#[derive(Debug, Default)]
pub struct ProjectedLogsDelta {
    pub dirty_pools: HashSet<Address>,
    pub touched_pools: usize,
    pub max_swap_usd: f64,
    pub total_swap_usd: f64,
}

#[inline]
pub fn singleton_pool_address(manager: Address, pool_id: B256) -> Address {
    let mut buf = [0u8; 52];
    buf[..20].copy_from_slice(manager.as_slice());
    buf[20..52].copy_from_slice(pool_id.as_slice());
    let h = alloy::primitives::keccak256(buf);
    Address::from_slice(&h[12..32])
}

#[inline]
pub fn balancer_pair_address(vault: Address, pool_id: B256, t0: Address, t1: Address) -> Address {
    let (a, b) = if t0 < t1 { (t0, t1) } else { (t1, t0) };
    let mut buf = [0u8; 92];
    buf[..20].copy_from_slice(vault.as_slice());
    buf[20..52].copy_from_slice(pool_id.as_slice());
    buf[52..72].copy_from_slice(a.as_slice());
    buf[72..92].copy_from_slice(b.as_slice());
    let h = alloy::primitives::keccak256(buf);
    Address::from_slice(&h[12..32])
}

#[inline]
fn univ4_slot0_and_liq_keys(pool_id: B256) -> (B256, B256) {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(pool_id.as_slice());
    buf[63] = 6; // POOLS_SLOT = 6 in Uniswap v4 PoolManager
    let slot0_key = alloy::primitives::keccak256(buf);
    let liq_u = U256::from_be_bytes(slot0_key.0).saturating_add(U256::from(3u8));
    (slot0_key, B256::from(liq_u.to_be_bytes::<32>()))
}

#[inline]
fn decode_univ4_slot0(word: &[u8]) -> (U256, i32) {
    if word.len() < 32 {
        return (U256::ZERO, 0);
    }
    let u = U256::from_be_slice(&word[..32]);
    let mask_160 = (U256::from(1u8) << 160) - U256::from(1u8);
    let sqrt_p = u & mask_160;
    let masked: U256 = (u >> 160u8) & U256::from(0xff_ffffu32);
    let tick_u24 = masked.saturating_to::<u32>();
    let tick = if (tick_u24 & 0x80_0000) != 0 {
        (tick_u24 | 0xff00_0000) as i32
    } else {
        tick_u24 as i32
    };
    (sqrt_p, tick)
}

#[inline]
fn is_standard_v3_fee_ppm(fee_ppm: u32) -> bool {
    (1..=50_000).contains(&fee_ppm)
}

#[derive(Clone)]
pub struct MarketState {
    pub chain: ChainConfig,
    pub pools: HashMap<Address, PoolState>,
    pub tokens: HashMap<Address, TokenMeta>,
    pub token_pools: HashMap<Address, Vec<Address>>,
    pub queried_pairs: HashSet<(Address, Address)>,
    pub ignored_pools: HashSet<Address>,
    pub hub_set: HashSet<Address>,
    pub hub_pair_pools: Vec<Address>,
    pub sim_fail_counts: HashMap<Address, u8>,
    pub sim_verified_pools: HashSet<Address>,
    pub sim_verified_tokens: HashSet<Address>,
    pub pool_cooldown_until: HashMap<Address, u64>,
    pub token_cooldown_until: HashMap<Address, u64>,
    pub token_fail_strikes: HashMap<Address, (u8, u64)>,
    pub token_tax_ppm: HashMap<Address, u32>,
    pending_unknowns: HashMap<Address, (usize, DexHint)>,
    pending_unknown_v4: HashMap<B256, (usize, DexKind)>,
    pending_unknown_balancer: HashMap<B256, usize>,
    pub v4_known_keys: HashMap<B256, V4PoolKey>,
    pub balancer_pool_pairs: HashMap<B256, Vec<Address>>,
    pub balancer_pool_tokens: HashMap<B256, Vec<(Address, u8, f64, U256)>>,
    pending_expand_pairs: HashSet<(Address, Address)>,
    pub flash_pools: Vec<(FlashPoolCandidate, U256)>,
    pub resync_cursor: usize,
    pub last_block: u64,
    pub native_usd: f64,
    pub gas_price_wei: u128,
    pub persist_state: bool,
}

#[derive(Debug, Default)]
pub struct BlockDelta {
    #[allow(dead_code)]
    pub block_num: u64,
    pub touched_pools: usize,
    pub discovered_pools: usize,
    #[allow(dead_code)]
    pub dirty_tokens: HashSet<Address>,
    pub dirty_pools: HashSet<Address>,
    pub metric_refresh_pools: HashSet<Address>,
    pub metric_oracle_updated: bool,
}

impl MarketState {
    pub fn new(chain: ChainConfig) -> Self {
        let mut tokens = HashMap::new();
        let mut hub_set = HashSet::new();
        for t in &chain.hub_tokens {
            hub_set.insert(t.address);
            tokens.insert(
                t.address,
                TokenMeta {
                    address: t.address,
                    symbol: t.symbol.to_string(),
                    decimals: t.decimals,
                    approx_usd: t.approx_usd,
                },
            );
        }
        let native_usd = chain.default_native_usd;
        let gas_price_wei = chain.min_gas_price_wei();
        let flash_pools = chain
            .seed_flash_pools
            .iter()
            .cloned()
            .map(|c| (c, U256::ZERO))
            .collect();
        let mut state = Self {
            chain,
            pools: HashMap::new(),
            tokens,
            token_pools: HashMap::new(),
            queried_pairs: HashSet::new(),
            ignored_pools: HashSet::new(),
            hub_set,
            hub_pair_pools: Vec::new(),
            sim_fail_counts: HashMap::new(),
            sim_verified_pools: HashSet::new(),
            sim_verified_tokens: HashSet::new(),
            pool_cooldown_until: HashMap::new(),
            token_cooldown_until: HashMap::new(),
            token_fail_strikes: HashMap::new(),
            token_tax_ppm: HashMap::new(),
            pending_unknowns: HashMap::new(),
            pending_unknown_v4: HashMap::new(),
            pending_unknown_balancer: HashMap::new(),
            v4_known_keys: HashMap::new(),
            balancer_pool_pairs: HashMap::new(),
            balancer_pool_tokens: HashMap::new(),
            pending_expand_pairs: HashSet::new(),
            flash_pools,
            resync_cursor: 0,
            last_block: 0,
            native_usd,
            gas_price_wei,
            persist_state: true,
        };
        state.load_quarantine();
        state.load_token_taxes();
        state
    }

    #[inline]
    pub fn is_hub_token(&self, addr: Address) -> bool {
        self.hub_set.contains(&addr)
    }

    #[inline]
    fn is_active_expand_queue_full(&self) -> bool {
        self.pending_expand_pairs.len() >= 128
    }

    #[inline]
    fn queue_expand_pair(&mut self, a: Address, b: Address) {
        if a == b || self.is_active_expand_queue_full() {
            return;
        }
        let key = if a < b { (a, b) } else { (b, a) };
        if !self.queried_pairs.contains(&key) {
            self.pending_expand_pairs.insert(key);
        }
    }

    pub fn is_pinned_stable(&self, addr: Address) -> bool {
        addr == self.chain.usd_anchor
            || self
                .chain
                .hub_tokens
                .iter()
                .any(|s| s.address == addr && s.is_pinned_stable)
    }

    pub fn hub_seed_usd(&self, addr: Address) -> Option<f64> {
        self.chain
            .hub_tokens
            .iter()
            .find(|s| s.address == addr)
            .map(|s| s.approx_usd)
    }

    pub fn prune_dead_pools(&mut self) {
        let multi_pool_tokens: HashSet<Address> = self
            .token_pools
            .iter()
            .filter(|(tk, list)| {
                !self.is_hub_token(**tk)
                    && list
                        .iter()
                        .filter(|pa| {
                            !self.ignored_pools.contains(*pa)
                                && self
                                    .pools
                                    .get(*pa)
                                    .is_some_and(|p| p.is_active() && p.cached_depth_usd >= 15.0)
                        })
                        .take(2)
                        .count()
                        >= 2
            })
            .map(|(tk, _)| *tk)
            .collect();
        let over_cap = self.pools.len() > 3_600;
        let dead: Vec<Address> = self
            .pools
            .values()
            .filter(|p| {
                if self.is_hub_token(p.token0) && self.is_hub_token(p.token1) {
                    return false;
                }
                if self.sim_verified_pools.contains(&p.address) {
                    return p.cached_depth_usd < 8.0
                        && self.last_block > p.last_updated_block.saturating_add(300);
                }
                let both_sides_viable = (self.is_hub_token(p.token0)
                    || multi_pool_tokens.contains(&p.token0))
                    && (self.is_hub_token(p.token1) || multi_pool_tokens.contains(&p.token1));
                if both_sides_viable {
                    return p.cached_depth_usd < 8.0
                        && self.last_block > p.last_updated_block.saturating_add(300);
                }
                (p.cached_depth_usd < 12.0
                    && self.last_block > p.last_updated_block.saturating_add(120))
                    || (p.factory_name.starts_with("Generic")
                        && p.cached_depth_usd < 120.0
                        && self.last_block > p.last_updated_block.saturating_add(400))
                    || (p.cached_depth_usd < 25_000.0
                        && ((over_cap
                            && self.last_block > p.last_updated_block.saturating_add(1_200))
                            || self.last_block > p.last_updated_block.saturating_add(2_400)))
            })
            .map(|p| p.address)
            .collect();
        if !dead.is_empty() {
            for addr in dead {
                self.pools.remove(&addr);
            }
            self.token_pools.retain(|_, list| {
                list.retain(|a| self.pools.contains_key(a));
                !list.is_empty()
            });
        }
    }

    #[inline]
    pub fn is_pool_routable(&self, pool: &PoolState) -> bool {
        pool.is_active()
            && pool.cached_depth_usd >= 12.0
            && self.sim_fail_counts.get(&pool.address).copied().unwrap_or(0) < 2
            && self.pool_cooldown_until.get(&pool.address).copied().unwrap_or(0) <= self.last_block
            && (self.is_hub_token(pool.token0)
                || self.token_cooldown_until.get(&pool.token0).copied().unwrap_or(0) <= self.last_block)
            && (self.is_hub_token(pool.token1)
                || self.token_cooldown_until.get(&pool.token1).copied().unwrap_or(0) <= self.last_block)
    }

    #[inline]
    pub fn has_two_routable_pools(&self, token: Address) -> bool {
        if self.is_hub_token(token) {
            return true;
        }
        let Some(list) = self.token_pools.get(&token) else {
            return false;
        };
        let mut count = 0usize;
        for &pa in list {
            if let Some(p) = self.pools.get(&pa) {
                if self.is_pool_routable(p) {
                    count += 1;
                    if count >= 2 {
                        return true;
                    }
                }
            }
        }
        false
    }

    #[inline]
    pub fn is_pool_arbitrageable(&self, pool: &PoolState) -> bool {
        self.is_pool_routable(pool)
            && (pool.is_hub_0 || self.has_two_routable_pools(pool.token0))
            && (pool.is_hub_1 || self.has_two_routable_pools(pool.token1))
    }

    fn quarantine_path(&self) -> String {
        format!("logs/quarantine_{}.json", self.chain.as_str())
    }

    fn token_tax_path(&self) -> String {
        format!("logs/token_tax_{}.json", self.chain.as_str())
    }

    fn pools_cache_path(&self) -> String {
        format!("logs/pools_cache_{}.json", self.chain.as_str())
    }

    fn load_quarantine(&mut self) {
        let path = self.quarantine_path();
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(addrs) = serde_json::from_str::<Vec<Address>>(&raw) {
                for a in addrs {
                    self.ignored_pools.insert(a);
                }
            }
        }
    }

    fn save_quarantine(&self) {
        if !self.persist_state {
            return;
        }
        let _ = std::fs::create_dir_all("logs");
        let list: Vec<Address> = self.ignored_pools.iter().copied().collect();
        if let Ok(body) = serde_json::to_string(&list) {
            let _ = std::fs::write(self.quarantine_path(), body);
        }
    }

    fn load_token_taxes(&mut self) {
        let path = self.token_tax_path();
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(entries) = serde_json::from_str::<Vec<(Address, u32)>>(&raw) {
                let mut loaded = 0usize;
                for (addr, ppm) in entries {
                    if ppm > 0 && !self.is_hub_token(addr) {
                        self.token_tax_ppm.insert(addr, ppm.min(999_999));
                        loaded += 1;
                    }
                }
                if loaded > 0 {
                    tracing::info!(count = loaded, "loaded persisted calibrated token taxes");
                }
            }
        }
    }

    pub fn save_token_taxes(&self) {
        if !self.persist_state {
            return;
        }
        let _ = std::fs::create_dir_all("logs");
        let mut entries: Vec<(Address, u32)> = self
            .token_tax_ppm
            .iter()
            .filter_map(|(&addr, &ppm)| {
                if ppm > 0 && !self.is_hub_token(addr) {
                    Some((addr, ppm.min(999_999)))
                } else {
                    None
                }
            })
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let path = self.token_tax_path();
        let tmp_path = format!("{path}.tmp");
        if let Ok(body) = serde_json::to_string(&entries) {
            if std::fs::write(&tmp_path, body).is_ok() {
                let _ = std::fs::rename(&tmp_path, &path);
            }
        }
    }

    pub fn save_pools_cache(&self) {
        if !self.persist_state {
            return;
        }
        self.save_token_taxes();
        let _ = std::fs::create_dir_all("logs");
        let mut tokens: Vec<(Address, String, u8)> = self
            .tokens
            .values()
            .map(|t| (t.address, t.symbol.clone(), t.decimals))
            .collect();
        tokens.sort_by(|a, b| a.0.cmp(&b.0));

        let mut active_meta: Vec<(&PoolState, bool, f64)> = self
            .pools
            .values()
            .filter(|p| {
                !self.ignored_pools.contains(&p.address)
                    && ((self.is_hub_token(p.token0) && self.is_hub_token(p.token1))
                        || (!p.factory_name.starts_with("Generic") && p.cached_depth_usd >= 3.0)
                        || (p.factory_name.starts_with("Generic")
                            && (self.sim_verified_pools.contains(&p.address)
                                || (p.cached_depth_usd >= 400.0
                                    && self.last_block <= p.last_updated_block.saturating_add(4_000)))))
            })
            .map(|p| {
                let is_hub_pair = self.is_hub_token(p.token0) && self.is_hub_token(p.token1);
                let base_score = if p.factory_name.starts_with("Generic") {
                    p.cached_depth_usd * 0.25
                } else if matches!(
                    p.dex_kind,
                    DexKind::UniV3
                        | DexKind::PancakeV3
                        | DexKind::InfinityCl
                        | DexKind::UniV4
                        | DexKind::Solidly
                        | DexKind::MetricOmm
                        | DexKind::BalancerV2
                ) {
                    p.cached_depth_usd * 20.0
                } else {
                    p.cached_depth_usd
                };
                let viable_2core = (self.is_hub_token(p.token0)
                    || self.token_pools.get(&p.token0).is_some_and(|l| l.len() >= 2))
                    && (self.is_hub_token(p.token1)
                        || self.token_pools.get(&p.token1).is_some_and(|l| l.len() >= 2));
                let mut score = if viable_2core { base_score * 50.0 } else { base_score };
                if self.last_block <= p.last_updated_block.saturating_add(900) {
                    score *= 4.0;
                }
                (p, is_hub_pair, score)
            })
            .collect();
        active_meta.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
        });
        active_meta.truncate(8_500);

        let mut pools: Vec<CachedPoolMeta> = active_meta
            .into_iter()
            .map(|(p, _, _)| CachedPoolMeta {
                address: p.address,
                factory_name: p.factory_name.clone(),
                dex_kind: p.dex_kind,
                token0: p.token0,
                token1: p.token1,
                dec0: p.dec0,
                dec1: p.dec1,
                fee_num: p.fee_num,
                fee_ppm: p.fee_ppm,
                stable: p.stable,
                tick_spacing: p.tick_spacing,
                v4_key: p.v4_key.clone(),
                metric: p.metric.clone(),
                balancer: p.balancer.clone(),
            })
            .collect();
        pools.sort_by(|a, b| a.address.cmp(&b.address));

        let cache = PoolsCacheFile { tokens, pools };
        let path = self.pools_cache_path();
        let tmp_path = format!("{path}.tmp");
        if let Ok(body) = serde_json::to_string(&cache) {
            if std::fs::write(&tmp_path, body).is_ok() {
                let _ = std::fs::rename(&tmp_path, &path);
            }
        }
    }

    async fn load_pools_cache(&mut self, rpc: &RpcPool) -> Result<usize> {
        let path = self.pools_cache_path();
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(0);
        };
        let Ok(cache) = serde_json::from_str::<PoolsCacheFile>(&raw) else {
            return Ok(0);
        };

        for (addr, sym, dec) in cache.tokens {
            self.tokens.entry(addr).or_insert(TokenMeta {
                address: addr,
                symbol: sym,
                decimals: dec,
                approx_usd: 0.0,
            });
        }

        let items: Vec<CachedPoolMeta> = cache
            .pools
            .into_iter()
            .filter(|p| {
                if self.ignored_pools.contains(&p.address) || self.pools.contains_key(&p.address) {
                    return false;
                }
                if p.factory_name.starts_with("Generic")
                    && matches!(p.dex_kind, DexKind::UniV3 | DexKind::PancakeV3)
                    && !is_standard_v3_fee_ppm(p.fee_ppm)
                {
                    self.ignored_pools.insert(p.address);
                    return false;
                }
                true
            })
            .collect();
        if items.is_empty() {
            return Ok(0);
        }

        let mut cached_bal_ids: Vec<B256> = Vec::new();
        for item in &items {
            if item.dex_kind == DexKind::BalancerV2 {
                if let Some(ref bal) = item.balancer {
                    if !cached_bal_ids.contains(&bal.pool_id) {
                        cached_bal_ids.push(bal.pool_id);
                    }
                }
            }
        }

        // Batch-refresh cached pools in chunks so Multicall3 bodies stay safe on public nodes.
        // Stamp cold cached pools as aged (`-1200` blocks) until they emit live on-chain Sync/Swap events.
        let cold_ts = self.last_block.saturating_sub(1_200);
        let mut loaded = 0usize;
        for chunk in items.chunks(400) {
            let mut calls = Vec::with_capacity(chunk.len() * 2);
            let mut ranges = Vec::with_capacity(chunk.len());
            for p in chunk {
                let start = calls.len();
                match p.dex_kind {
                    DexKind::UniV2 => calls.push(IMulticall3::Call3 {
                        target: p.address,
                        allowFailure: true,
                        callData: IUniV2Pair::getReservesCall {}.abi_encode().into(),
                    }),
                    DexKind::Solidly => calls.push(IMulticall3::Call3 {
                        target: p.address,
                        allowFailure: true,
                        callData: ISolidlyPair::getReservesCall {}.abi_encode().into(),
                    }),
                    DexKind::UniV3 | DexKind::PancakeV3 => {
                        calls.push(IMulticall3::Call3 {
                            target: p.address,
                            allowFailure: true,
                            callData: IV3Pool::slot0Call {}.abi_encode().into(),
                        });
                        calls.push(IMulticall3::Call3 {
                            target: p.address,
                            allowFailure: true,
                            callData: IV3Pool::liquidityCall {}.abi_encode().into(),
                        });
                    }
                    DexKind::InfinityCl => {
                        if let Some(ref k) = p.v4_key {
                            calls.push(IMulticall3::Call3 {
                                target: k.manager,
                                allowFailure: true,
                                callData: IInfinityCLPoolManager::getSlot0Call { id: k.pool_id }
                                    .abi_encode()
                                    .into(),
                            });
                            calls.push(IMulticall3::Call3 {
                                target: k.manager,
                                allowFailure: true,
                                callData: IInfinityCLPoolManager::getLiquidityCall { id: k.pool_id }
                                    .abi_encode()
                                    .into(),
                            });
                        }
                    }
                    DexKind::UniV4 => {
                        if let Some(ref k) = p.v4_key {
                            let (s0_key, liq_key) = univ4_slot0_and_liq_keys(k.pool_id);
                            calls.push(IMulticall3::Call3 {
                                target: k.manager,
                                allowFailure: true,
                                callData: IUniV4PoolManager::extsloadCall { slot: s0_key }
                                    .abi_encode()
                                    .into(),
                            });
                            calls.push(IMulticall3::Call3 {
                                target: k.manager,
                                allowFailure: true,
                                callData: IUniV4PoolManager::extsloadCall { slot: liq_key }
                                    .abi_encode()
                                    .into(),
                            });
                        }
                    }
                    DexKind::MetricOmm => {
                        push_metric_state_calls(&mut calls, p.address);
                    }
                    DexKind::BalancerV2 => {}
                }
                ranges.push((start, calls.len()));
            }
            let res = if calls.is_empty() {
                Vec::new()
            } else {
                let Ok(r) = rpc.multicall(calls).await else {
                    continue;
                };
                r
            };
            for (idx, p) in chunk.iter().enumerate() {
                let (s, e) = ranges[idx];
                let slice = &res[s..e];
                match p.dex_kind {
                    DexKind::UniV2 | DexKind::Solidly => {
                        if !slice[0].success || slice[0].returnData.len() < 64 {
                            continue;
                        }
                        let r0 = U256::from_be_slice(&slice[0].returnData[0..32]);
                        let r1 = U256::from_be_slice(&slice[0].returnData[32..64]);
                        self.insert_pool(PoolState {
                            address: p.address,
                            factory_name: p.factory_name.clone(),
                            dex_kind: p.dex_kind,
                            token0: p.token0,
                            token1: p.token1,
                            dec0: p.dec0,
                            dec1: p.dec1,
                            fee_num: p.fee_num,
                            fee_ppm: p.fee_ppm,
                            extra_tax_ppm: 0,
                            stable: p.stable,
                            r0_raw: r0,
                            r1_raw: r1,
                            sqrt_price_x96: U256::ZERO,
                            liquidity: 0,
                            tick: 0,
                            tick_spacing: p.tick_spacing,
                            allow_multi_tick: false,
                            is_hub_0: false,
                            is_hub_1: false,
                            cached_depth_usd: 0.0,
                            last_updated_block: cold_ts,
                            v4_key: None,
                            metric: None,
                            balancer: None,
                        });
                        loaded += 1;
                    }
                    DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                        if slice.len() < 2
                            || !slice[0].success
                            || !slice[1].success
                            || slice[1].returnData.len() < 32
                        {
                            continue;
                        }
                        let (sqrt_p, tick) = if p.dex_kind == DexKind::UniV4 {
                            if slice[0].returnData.len() < 32 {
                                continue;
                            }
                            decode_univ4_slot0(&slice[0].returnData[0..32])
                        } else {
                            if slice[0].returnData.len() < 64 {
                                continue;
                            }
                            (
                                U256::from_be_slice(&slice[0].returnData[0..32]),
                                decode_i24_word(&slice[0].returnData[32..64]),
                            )
                        };
                        let liq = U256::from_be_slice(&slice[1].returnData[0..32]).saturating_to::<u128>();
                        if let Some(ref k) = p.v4_key {
                            self.v4_known_keys.insert(k.pool_id, k.clone());
                        }
                        self.insert_pool(PoolState {
                            address: p.address,
                            factory_name: p.factory_name.clone(),
                            dex_kind: p.dex_kind,
                            token0: p.token0,
                            token1: p.token1,
                            dec0: p.dec0,
                            dec1: p.dec1,
                            fee_num: p.fee_num,
                            fee_ppm: p.fee_ppm,
                            extra_tax_ppm: 0,
                            stable: p.stable,
                            r0_raw: U256::ZERO,
                            r1_raw: U256::ZERO,
                            sqrt_price_x96: sqrt_p,
                            liquidity: liq,
                            tick,
                            tick_spacing: p.tick_spacing,
                            allow_multi_tick: false,
                            is_hub_0: false,
                            is_hub_1: false,
                            cached_depth_usd: 0.0,
                            last_updated_block: cold_ts,
                            v4_key: p.v4_key.clone(),
                            metric: None,
                            balancer: None,
                        });
                        loaded += 1;
                    }
                    DexKind::MetricOmm => {
                        let (sell, buy, r0_raw, r1_raw, active_bin, paused) =
                            if let Some(st) = decode_metric_state_slice(slice) {
                                st
                            } else if let Some((r0_raw, r1_raw, active_bin, _)) =
                                decode_metric_slots_only(slice)
                            {
                                (0, 0, r0_raw, r1_raw, active_bin, true)
                            } else {
                                continue;
                            };
                        let num_bins = p.metric.as_ref().map_or(40, |m| m.num_bins).max(2);
                        self.insert_pool(PoolState {
                            address: p.address,
                            factory_name: p.factory_name.clone(),
                            dex_kind: DexKind::MetricOmm,
                            token0: p.token0,
                            token1: p.token1,
                            dec0: p.dec0,
                            dec1: p.dec1,
                            fee_num: 10_000,
                            fee_ppm: 0,
                            extra_tax_ppm: 0,
                            stable: false,
                            r0_raw,
                            r1_raw,
                            sqrt_price_x96: U256::ZERO,
                            liquidity: 0,
                            tick: 0,
                            tick_spacing: 1,
                            allow_multi_tick: false,
                            is_hub_0: false,
                            is_hub_1: false,
                            cached_depth_usd: 0.0,
                            last_updated_block: cold_ts,
                            v4_key: None,
                            metric: Some(MetricPoolState {
                                sell_price_x64: sell,
                                buy_price_x64: buy,
                                num_bins,
                                active_bin,
                                paused,
                            }),
                            balancer: None,
                        });
                        loaded += 1;
                    }
                    DexKind::BalancerV2 => {}
                }
            }
        }
        if !cached_bal_ids.is_empty() {
            loaded += self
                .discover_balancer_v2_pools(rpc, &cached_bal_ids)
                .await
                .unwrap_or(0);
        }
        Ok(loaded)
    }

    pub fn token_symbol(&self, addr: Address) -> String {
        self.tokens
            .get(&addr)
            .map(|m| m.symbol.clone())
            .unwrap_or_else(|| format!("0x{}", hex::encode(&addr.as_slice()[..4])))
    }

    pub fn token_decimals(&self, addr: Address) -> u8 {
        self.tokens.get(&addr).map(|m| m.decimals).unwrap_or(18)
    }

    pub fn token_usd(&self, addr: Address) -> f64 {
        self.tokens.get(&addr).map(|m| m.approx_usd).unwrap_or(0.0)
    }

    /// Bootstrap cached topology + hub token matrix + flash balances + recent block warmup.
    pub async fn bootstrap(&mut self, rpc: &RpcPool) -> Result<()> {
        self.bootstrap_inner(rpc, None).await
    }

    /// Bootstrap full market state pinned deterministically at a specific historical block.
    pub async fn bootstrap_at_block(&mut self, rpc: &RpcPool, target_block: u64) -> Result<()> {
        self.persist_state = false;
        self.bootstrap_inner(rpc, Some(target_block)).await
    }

    async fn bootstrap_inner(&mut self, rpc: &RpcPool, pinned_block: Option<u64>) -> Result<()> {
        let rpc_owned;
        let query_rpc = if let Some(bn) = pinned_block {
            self.last_block = bn;
            rpc_owned = rpc.with_block_tag(format!("0x{bn:x}"));
            &rpc_owned
        } else {
            self.last_block = rpc.block_number().await.unwrap_or(0);
            rpc
        };
        if self.chain.gas.tx_type == crate::config::TxType::Eip1559 && self.last_block > 0 {
            if let Ok(Some(bf)) = rpc.get_block_next_base_fee(self.last_block).await {
                self.gas_price_wei = bf.max(self.chain.min_gas_price_wei());
            } else if let Ok(gp) = rpc.gas_price().await {
                self.gas_price_wei = gp.max(self.chain.min_gas_price_wei());
            }
        } else if let Ok(gp) = rpc.gas_price().await {
            self.gas_price_wei = gp.max(self.chain.min_gas_price_wei());
        }

        let cached_count = self.load_pools_cache(query_rpc).await.unwrap_or(0);
        if cached_count > 0 {
            tracing::info!(cached_pools = cached_count, "reloaded pools from topology cache");
        }

        let hub_addrs: Vec<Address> = self.chain.hub_tokens.iter().map(|t| t.address).collect();
        let mut pairs = Vec::new();
        for i in 0..hub_addrs.len() {
            for j in (i + 1)..hub_addrs.len() {
                pairs.push((hub_addrs[i], hub_addrs[j]));
            }
        }
        self.expand_pairs_across_factories(query_rpc, &pairs).await?;

        if !self.chain.seed_metric_pools().is_empty() {
            let seeds = self.chain.seed_metric_pools().to_vec();
            let _ = self.discover_metric_pools(query_rpc, &seeds).await?;
        }
        if !self.chain.balancer_v2_seed_pool_ids().is_empty() {
            let seeds = self.chain.balancer_v2_seed_pool_ids().to_vec();
            let _ = self.discover_balancer_v2_pools(query_rpc, &seeds).await;
        }

        // Backfill PancakeV1 counterparts for cached tokens when starting up or migrating cache
        if let Some(p1_spec) = self.chain.factories.iter().find(|f| f.name == "PancakeV1").cloned() {
            let p1_count = self.pools.values().filter(|p| p.factory_name == "PancakeV1").count();
            if p1_count < 20 {
                let mut non_hubs: Vec<Address> = self
                    .tokens
                    .keys()
                    .copied()
                    .filter(|t| !self.is_hub_token(*t))
                    .collect();
                non_hubs.sort();
                non_hubs.truncate(200);
                let mut v1_pairs = Vec::with_capacity(non_hubs.len() * 2);
                for tk in non_hubs {
                    v1_pairs.push((tk, self.chain.wrapped_native));
                    v1_pairs.push((tk, self.chain.usd_anchor));
                }
                if !v1_pairs.is_empty() {
                    let _ = self.expand_pairs_on_factory(query_rpc, &p1_spec, &v1_pairs).await;
                }
            }
        }

        // Cold-start warmup over the last 8 blocks if topology is still small
        if self.pools.len() < 600 && self.last_block > 8 {
            let start_b = self.last_block.saturating_sub(7);
            let end_b = self.last_block;
            for bn in start_b..=end_b {
                if let Ok(receipts) = rpc.get_block_receipts_by_number(bn).await {
                    let mut delta = self.apply_receipts_in_place(bn, &receipts);
                    let _ = self.discover_unknowns(rpc, bn, &mut delta).await;
                }
            }
            self.last_block = end_b;
        }

        self.refresh_flash_balances(query_rpc).await?;
        self.refresh_token_usd_prices_passes(3);

        // Backfill official cross-factory pools across all base anchors (WBNB, USDT, USDC, BUSD, USD1)
        let mut active_non_hubs: Vec<(Address, bool, f64)> = self
            .token_pools
            .iter()
            .filter(|(tk, addrs)| !self.is_hub_token(**tk) && !addrs.is_empty())
            .map(|(tk, addrs)| {
                let max_depth = addrs
                    .iter()
                    .filter_map(|pa| self.pools.get(pa).map(|p| p.cached_depth_usd))
                    .fold(0.0_f64, f64::max);
                (*tk, addrs.len() >= 2, max_depth)
            })
            .collect();
        active_non_hubs.sort_by(|a, b| {
            b.1.cmp(&a.1)
                .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
                .then_with(|| a.0.cmp(&b.0))
        });
        active_non_hubs.truncate(96);

        let anchors = self.base_anchors();
        let mut v3_backfill = Vec::new();
        let mut backfill_seen = HashSet::new();
        'backfill: for (tk, _, _) in active_non_hubs {
            for &anchor in &anchors {
                if tk == anchor {
                    continue;
                }
                let key = if tk < anchor { (tk, anchor) } else { (anchor, tk) };
                if !self.queried_pairs.contains(&key) && backfill_seen.insert(key) {
                    v3_backfill.push(key);
                    if v3_backfill.len() >= 160 {
                        break 'backfill;
                    }
                }
            }
        }
        if !v3_backfill.is_empty() {
            let _ = self.expand_pairs_across_factories(query_rpc, &v3_backfill).await;
            self.refresh_token_usd_prices_passes(2);
        }
        if pinned_block.is_none() && !self.pools.is_empty() {
            self.save_pools_cache();
        }
        Ok(())
    }

    async fn expand_pairs_on_factory(
        &mut self,
        rpc: &RpcPool,
        fac: &FactorySpec,
        pairs: &[(Address, Address)],
    ) -> Result<()> {
        let mut calls = Vec::with_capacity(pairs.len());
        let mut valid_pairs = Vec::with_capacity(pairs.len());
        for &(a, b) in pairs {
            if a == b {
                continue;
            }
            calls.push(IMulticall3::Call3 {
                target: fac.address,
                allowFailure: true,
                callData: IUniV2Factory::getPairCall { tokenA: a, tokenB: b }
                    .abi_encode()
                    .into(),
            });
            valid_pairs.push((a, b));
        }
        if calls.is_empty() {
            return Ok(());
        }
        let res = rpc.multicall(calls).await?;
        let mut to_load = Vec::new();
        for (idx, r) in res.into_iter().enumerate() {
            if !r.success || r.returnData.len() < 32 {
                continue;
            }
            let addr = Address::from_slice(&r.returnData[12..32]);
            if addr.is_zero() || self.pools.contains_key(&addr) || self.ignored_pools.contains(&addr) {
                continue;
            }
            let (a, b) = valid_pairs[idx];
            to_load.push((addr, a, b, fac.clone()));
        }
        if !to_load.is_empty() {
            self.load_pool_states(rpc, &to_load).await?;
        }
        Ok(())
    }

    pub async fn refresh_flash_balances(&mut self, rpc: &RpcPool) -> Result<()> {
        if self.flash_pools.is_empty() {
            return Ok(());
        }
        let calls: Vec<IMulticall3::Call3> = self
            .flash_pools
            .iter()
            .map(|(fp, _)| IMulticall3::Call3 {
                target: fp.base_token,
                allowFailure: true,
                callData: IERC20::balanceOfCall { account: fp.pool }.abi_encode().into(),
            })
            .collect();
        let res = rpc.multicall(calls).await?;
        for (i, r) in res.into_iter().enumerate() {
            if r.success && r.returnData.len() >= 32 {
                if let Ok(bal) = IERC20::balanceOfCall::abi_decode_returns(&r.returnData) {
                    self.flash_pools[i].1 = bal;
                }
            }
        }
        Ok(())
    }

    /// Hot-path step 1: Update known pools in-place from Sync/Swap log data in microseconds
    /// without waiting for any RPC calls, and buffer active unknown pools for periodic expansion.
    pub fn apply_receipts_in_place(
        &mut self,
        block_num: u64,
        receipts: &[TxReceipt],
    ) -> BlockDelta {
        self.last_block = block_num;
        let mut delta = BlockDelta {
            block_num,
            ..Default::default()
        };
        let mut touched_pairs: HashSet<(Address, Address)> = HashSet::new();

        let mut healed_pools: Vec<Address> = Vec::new();
        for rx in receipts {
            for lg in &rx.logs {
                if self.chain.metric_enabled()
                    && self.chain.metric_oracles().contains(&lg.address)
                    && lg.topics.first().copied() != Some(TOPIC_METRIC_PRICE_READ)
                {
                    delta.metric_oracle_updated = true;
                }
                let Some(topic0) = lg.topics.first().copied() else {
                    continue;
                };
                if topic0 == TOPIC_UNIV2_SYNC {
                    if let Some(pool) = self.pools.get_mut(&lg.address) {
                        if lg.data.len() >= 64 {
                            pool.r0_raw = U256::from_be_slice(&lg.data[0..32]);
                            pool.r1_raw = U256::from_be_slice(&lg.data[32..64]);
                            pool.last_updated_block = block_num;
                            delta.touched_pools += 1;
                            delta.dirty_pools.insert(lg.address);
                            touched_pairs.insert((pool.token0, pool.token1));
                            healed_pools.push(lg.address);
                        }
                    } else if !self.ignored_pools.contains(&lg.address)
                        && (self.pending_unknowns.contains_key(&lg.address)
                            || self.pending_unknowns.len() < 500)
                    {
                        let e = self
                            .pending_unknowns
                            .entry(lg.address)
                            .or_insert((0, DexHint::V2));
                        e.0 += 1;
                    }
                } else if topic0 == TOPIC_SOLIDLY_SYNC {
                    if let Some(pool) = self.pools.get_mut(&lg.address) {
                        if lg.data.len() >= 64 {
                            pool.r0_raw = U256::from_be_slice(&lg.data[0..32]);
                            pool.r1_raw = U256::from_be_slice(&lg.data[32..64]);
                            pool.last_updated_block = block_num;
                            delta.touched_pools += 1;
                            delta.dirty_pools.insert(lg.address);
                            touched_pairs.insert((pool.token0, pool.token1));
                            healed_pools.push(lg.address);
                        }
                    } else if !self.ignored_pools.contains(&lg.address)
                        && (self.pending_unknowns.contains_key(&lg.address)
                            || self.pending_unknowns.len() < 500)
                    {
                        let e = self
                            .pending_unknowns
                            .entry(lg.address)
                            .or_insert((0, DexHint::Solidly));
                        e.0 += 1;
                    }
                } else if topic0 == TOPIC_UNIV3_SWAP || topic0 == TOPIC_PANCAKEV3_SWAP {
                    if let Some(pool) = self.pools.get_mut(&lg.address) {
                        // First 5 words are (amount0, amount1, sqrtPriceX96, liquidity, tick) for both UniV3 & PancakeV3
                        if lg.data.len() >= 160 {
                            pool.sqrt_price_x96 = U256::from_be_slice(&lg.data[64..96]);
                            let liq_u256 = U256::from_be_slice(&lg.data[96..128]);
                            pool.liquidity = liq_u256.saturating_to::<u128>();
                            pool.tick = decode_i24_word(&lg.data[128..160]);
                            pool.last_updated_block = block_num;
                            delta.touched_pools += 1;
                            delta.dirty_pools.insert(lg.address);
                            touched_pairs.insert((pool.token0, pool.token1));
                            healed_pools.push(lg.address);
                        }
                    } else if !self.ignored_pools.contains(&lg.address)
                        && (self.pending_unknowns.contains_key(&lg.address)
                            || self.pending_unknowns.len() < 500)
                    {
                        let hint = if topic0 == TOPIC_PANCAKEV3_SWAP {
                            DexHint::PancakeV3
                        } else {
                            DexHint::UniV3
                        };
                        let e = self.pending_unknowns.entry(lg.address).or_insert((0, hint));
                        e.0 += 1;
                    }
                } else if {
                    let is_infinity = self.chain.infinity_cl_pool_manager() == Some(lg.address);
                    let is_univ4 = self.chain.univ4_pool_manager() == Some(lg.address);
                    ((is_infinity && topic0 == TOPIC_INFINITY_CL_SWAP)
                        || (is_univ4 && topic0 == TOPIC_UNIV4_SWAP))
                        && lg.topics.len() >= 2
                        && lg.data.len() >= 192
                } {
                    let pool_id = lg.topics[1];
                    let is_infinity = self.chain.infinity_cl_pool_manager() == Some(lg.address);
                    let synth_addr = singleton_pool_address(lg.address, pool_id);
                    if let Some(pool) = self.pools.get_mut(&synth_addr) {
                        pool.sqrt_price_x96 = U256::from_be_slice(&lg.data[64..96]);
                        pool.liquidity =
                            U256::from_be_slice(&lg.data[96..128]).saturating_to::<u128>();
                        pool.tick = decode_i24_word(&lg.data[128..160]);
                        let fee = U256::from_be_slice(&lg.data[160..192]).saturating_to::<u32>();
                        if is_standard_v3_fee_ppm(fee) {
                            pool.fee_ppm = fee;
                        }
                        pool.last_updated_block = block_num;
                        delta.touched_pools += 1;
                        delta.dirty_pools.insert(synth_addr);
                        touched_pairs.insert((pool.token0, pool.token1));
                        healed_pools.push(synth_addr);
                    } else if !self.ignored_pools.contains(&synth_addr)
                        && (self.pending_unknown_v4.contains_key(&pool_id)
                            || self.pending_unknown_v4.len() < 500)
                    {
                        let kind = if is_infinity {
                            DexKind::InfinityCl
                        } else {
                            DexKind::UniV4
                        };
                        let e = self.pending_unknown_v4.entry(pool_id).or_insert((0, kind));
                        e.0 += 1;
                    }
                } else if self.chain.univ4_pool_manager() == Some(lg.address)
                    && topic0 == TOPIC_UNIV4_INIT
                    && lg.topics.len() >= 4
                    && lg.data.len() >= 160
                {
                    let uv4_mgr = lg.address;
                    let pool_id = lg.topics[1];
                    let currency0 = Address::from_word(lg.topics[2]);
                    let currency1 = Address::from_word(lg.topics[3]);
                    let fee = U256::from_be_slice(&lg.data[0..32]).saturating_to::<u32>();
                    let tick_spacing = decode_i24_word(&lg.data[32..64]);
                    let hooks = Address::from_slice(&lg.data[76..96]);
                    let synth_addr = singleton_pool_address(uv4_mgr, pool_id);
                    if hooks == Address::ZERO && is_standard_v3_fee_ppm(fee) && tick_spacing > 0 {
                        self.v4_known_keys.insert(
                            pool_id,
                            V4PoolKey {
                                manager: uv4_mgr,
                                vault: uv4_mgr,
                                pool_id,
                                currency0,
                                currency1,
                                fee,
                                tick_spacing,
                                hooks,
                                parameters: B256::ZERO,
                            },
                        );
                        if !self.pools.contains_key(&synth_addr)
                            && !self.ignored_pools.contains(&synth_addr)
                        {
                            let e = self
                                .pending_unknown_v4
                                .entry(pool_id)
                                .or_insert((0, DexKind::UniV4));
                            e.0 += 1;
                        }
                    } else {
                        self.ignored_pools.insert(synth_addr);
                    }
                } else if topic0 == TOPIC_V3_MINT {
                    if let Some(pool) = self.pools.get_mut(&lg.address) {
                        if lg.topics.len() >= 4 && lg.data.len() >= 64 {
                            let tick_lo = decode_i24_word(lg.topics[2].as_slice());
                            let tick_hi = decode_i24_word(lg.topics[3].as_slice());
                            let amt = U256::from_be_slice(&lg.data[32..64]).saturating_to::<u128>();
                            if tick_lo <= pool.tick && pool.tick < tick_hi && amt > 0 {
                                pool.liquidity = pool.liquidity.saturating_add(amt);
                                pool.last_updated_block = block_num;
                                delta.touched_pools += 1;
                                delta.dirty_pools.insert(lg.address);
                                touched_pairs.insert((pool.token0, pool.token1));
                                healed_pools.push(lg.address);
                            }
                        }
                    }
                } else if topic0 == TOPIC_V3_BURN {
                    if let Some(pool) = self.pools.get_mut(&lg.address) {
                        if lg.topics.len() >= 4 && lg.data.len() >= 32 {
                            let tick_lo = decode_i24_word(lg.topics[2].as_slice());
                            let tick_hi = decode_i24_word(lg.topics[3].as_slice());
                            let amt = U256::from_be_slice(&lg.data[0..32]).saturating_to::<u128>();
                            if tick_lo <= pool.tick && pool.tick < tick_hi && amt > 0 {
                                pool.liquidity = pool.liquidity.saturating_sub(amt);
                                pool.last_updated_block = block_num;
                                delta.touched_pools += 1;
                                delta.dirty_pools.insert(lg.address);
                                touched_pairs.insert((pool.token0, pool.token1));
                            }
                        }
                    }
                } else if self.chain.metric_enabled()
                    && topic0 == TOPIC_METRIC_PRICE_READ
                    && lg.topics.len() >= 2
                {
                    let pool_addr = Address::from_slice(&lg.topics[1].as_slice()[12..32]);
                    if let Some(pool) = self.pools.get(&pool_addr) {
                        if pool.dex_kind == DexKind::MetricOmm {
                            delta.metric_refresh_pools.insert(pool_addr);
                            delta.dirty_pools.insert(pool_addr);
                            delta.dirty_tokens.insert(pool.token0);
                            delta.dirty_tokens.insert(pool.token1);
                            touched_pairs.insert((pool.token0, pool.token1));
                        }
                    } else if !self.ignored_pools.contains(&pool_addr)
                        && (self.pending_unknowns.contains_key(&pool_addr)
                            || self.pending_unknowns.len() < 500)
                    {
                        let e = self
                            .pending_unknowns
                            .entry(pool_addr)
                            .or_insert((0, DexHint::MetricOmm));
                        e.0 += 1;
                    }
                } else if self.chain.metric_enabled() && topic0 == TOPIC_METRIC_SWAP {
                    if let Some(pool) = self.pools.get_mut(&lg.address) {
                        if pool.dex_kind == DexKind::MetricOmm {
                            pool.last_updated_block = block_num;
                            delta.touched_pools += 1;
                            delta.metric_refresh_pools.insert(lg.address);
                            delta.dirty_pools.insert(lg.address);
                            delta.dirty_tokens.insert(pool.token0);
                            delta.dirty_tokens.insert(pool.token1);
                            touched_pairs.insert((pool.token0, pool.token1));
                            healed_pools.push(lg.address);
                        }
                    } else if !self.ignored_pools.contains(&lg.address)
                        && (self.pending_unknowns.contains_key(&lg.address)
                            || self.pending_unknowns.len() < 500)
                    {
                        let e = self
                            .pending_unknowns
                            .entry(lg.address)
                            .or_insert((0, DexHint::MetricOmm));
                        e.0 += 1;
                    }
                } else if self.chain.balancer_v2_vault() == Some(lg.address) {
                    if topic0 == TOPIC_BALANCER_V2_SWAP
                        && lg.topics.len() >= 4
                        && lg.data.len() >= 64
                    {
                        let pool_id = lg.topics[1];
                        let token_in = Address::from_word(lg.topics[2]);
                        let token_out = Address::from_word(lg.topics[3]);
                        let amt_in = U256::from_be_slice(&lg.data[0..32]);
                        let amt_out = U256::from_be_slice(&lg.data[32..64]);
                        if let Some(tok_list) = self.balancer_pool_tokens.get_mut(&pool_id) {
                            for entry in tok_list.iter_mut() {
                                if entry.0 == token_in {
                                    entry.3 = entry.3.saturating_add(amt_in);
                                } else if entry.0 == token_out {
                                    entry.3 = entry.3.saturating_sub(amt_out);
                                }
                            }
                            let snap = tok_list.clone();
                            let pair_addrs = self
                                .balancer_pool_pairs
                                .get(&pool_id)
                                .cloned()
                                .unwrap_or_default();
                            for pa in pair_addrs {
                                if let Some(pool) = self.pools.get_mut(&pa) {
                                    for &(tk, _, _, raw) in &snap {
                                        if tk == pool.token0 {
                                            pool.r0_raw = raw;
                                        } else if tk == pool.token1 {
                                            pool.r1_raw = raw;
                                        }
                                    }
                                    if let Some(BalancerPoolState {
                                        kind:
                                            BalancerPoolKind::Stable {
                                                other_scaled_balances,
                                                ..
                                            },
                                        ..
                                    }) = &mut pool.balancer
                                    {
                                        *other_scaled_balances = snap
                                            .iter()
                                            .filter(|&&(tk, _, _, _)| {
                                                tk != pool.token0 && tk != pool.token1
                                            })
                                            .map(|&(_, dec, hs, raw)| u256_to_human(raw, dec) * hs)
                                            .collect();
                                    }
                                    pool.last_updated_block = block_num;
                                    delta.touched_pools += 1;
                                    delta.dirty_pools.insert(pa);
                                    touched_pairs.insert((pool.token0, pool.token1));
                                    healed_pools.push(pa);
                                }
                            }
                        } else if self.pending_unknown_balancer.contains_key(&pool_id)
                            || self.pending_unknown_balancer.len() < 500
                        {
                            *self.pending_unknown_balancer.entry(pool_id).or_insert(0) += 1;
                        }
                    } else if topic0 == TOPIC_BALANCER_V2_BALANCE_CHANGED && lg.topics.len() >= 2 {
                        let pool_id = lg.topics[1];
                        if self.pending_unknown_balancer.contains_key(&pool_id)
                            || self.pending_unknown_balancer.len() < 500
                        {
                            *self.pending_unknown_balancer.entry(pool_id).or_insert(0) += 1;
                        }
                    }
                }
            }
        }

        for addr in healed_pools {
            self.pool_cooldown_until.remove(&addr);
        }

        if !self.is_active_expand_queue_full() {
            let extra_anchors: Vec<Address> = self
                .chain
                .hub_tokens
                .iter()
                .filter(|h| h.is_base_anchor)
                .take(4)
                .map(|h| h.address)
                .collect();
            for (t0, t1) in touched_pairs {
                if self.is_active_expand_queue_full() {
                    break;
                }
                self.queue_expand_pair(t0, t1);
                for tk in [t0, t1] {
                    if self.is_hub_token(tk) {
                        continue;
                    }
                    for &anchor in &extra_anchors {
                        self.queue_expand_pair(tk, anchor);
                    }
                }
            }
        }

        if block_num % 8 == 0 {
            self.refresh_token_usd_prices_passes(1);
        } else {
            self.refresh_dirty_pools(&delta.dirty_pools);
        }
        delta
    }

    #[inline]
    fn token_tax_allows_swap(&self, token0: Address, token1: Address, check_v3_in: Option<bool>) -> bool {
        let tax0 = self.token_tax_ppm.get(&token0).copied().unwrap_or(0);
        let tax1 = self.token_tax_ppm.get(&token1).copied().unwrap_or(0);
        if tax0 >= 100_000 || tax1 >= 100_000 {
            return false;
        }
        match check_v3_in {
            Some(true) => tax0 < 500,
            Some(false) => tax1 < 500,
            None => true,
        }
    }

    #[inline]
    fn projected_swap_usd(&self, token0: Address, dec0: u8, abs0: U256, token1: Address, dec1: u8, abs1: U256) -> f64 {
        let p0 = self.token_usd(token0);
        let p1 = self.token_usd(token1);
        let u0 = if p0 > 0.0 {
            u256_to_human(abs0, dec0) * p0
        } else {
            0.0
        };
        let u1 = if p1 > 0.0 {
            u256_to_human(abs1, dec1) * p1
        } else {
            0.0
        };
        u0.max(u1)
    }

    /// Zero-clone pre-check returning true if at least one log maps to a known, routable pool
    /// with acceptable token transfer tax configuration.
    pub fn has_known_routable_pool_for_logs(&self, logs: &[crate::rpc::ReceiptLog]) -> bool {
        let infinity_mgr = self.chain.infinity_cl_pool_manager();
        let univ4_mgr = self.chain.univ4_pool_manager();
        let bal_vault = self.chain.balancer_v2_vault();

        for lg in logs {
            let Some(topic0) = lg.topics.first().copied() else {
                continue;
            };
            if (topic0 == TOPIC_UNIV2_SYNC || topic0 == TOPIC_SOLIDLY_SYNC) && lg.data.len() >= 64 {
                if !self.ignored_pools.contains(&lg.address) {
                    if let Some(pool) = self.pools.get(&lg.address) {
                        if self.is_pool_routable(pool)
                            && self.token_tax_allows_swap(pool.token0, pool.token1, None)
                        {
                            return true;
                        }
                    }
                }
            } else if (topic0 == TOPIC_UNIV3_SWAP || topic0 == TOPIC_PANCAKEV3_SWAP)
                && lg.data.len() >= 160
            {
                if !self.ignored_pools.contains(&lg.address) {
                    if let Some(pool) = self.pools.get(&lg.address) {
                        let a0 = I256::from_raw(U256::from_be_slice(&lg.data[0..32]));
                        let a1 = I256::from_raw(U256::from_be_slice(&lg.data[32..64]));
                        let check_v3_in = if a0 > I256::ZERO {
                            Some(true)
                        } else if a1 > I256::ZERO {
                            Some(false)
                        } else {
                            None
                        };
                        if self.is_pool_routable(pool)
                            && self.token_tax_allows_swap(pool.token0, pool.token1, check_v3_in)
                        {
                            return true;
                        }
                    }
                }
            } else if ((infinity_mgr == Some(lg.address) && topic0 == TOPIC_INFINITY_CL_SWAP)
                || (univ4_mgr == Some(lg.address) && topic0 == TOPIC_UNIV4_SWAP))
                && lg.topics.len() >= 2
                && lg.data.len() >= 192
            {
                let synth_addr = singleton_pool_address(lg.address, lg.topics[1]);
                if !self.ignored_pools.contains(&synth_addr) {
                    if let Some(pool) = self.pools.get(&synth_addr) {
                        let a0 = I256::from_raw(U256::from_be_slice(&lg.data[0..32]));
                        let a1 = I256::from_raw(U256::from_be_slice(&lg.data[32..64]));
                        let check_v3_in = if a0 > I256::ZERO {
                            Some(true)
                        } else if a1 > I256::ZERO {
                            Some(false)
                        } else {
                            None
                        };
                        if self.is_pool_routable(pool)
                            && self.token_tax_allows_swap(pool.token0, pool.token1, check_v3_in)
                        {
                            return true;
                        }
                    }
                }
            } else if bal_vault == Some(lg.address)
                && topic0 == TOPIC_BALANCER_V2_SWAP
                && lg.topics.len() >= 4
                && lg.data.len() >= 64
            {
                if let Some(pairs) = self.balancer_pool_pairs.get(&lg.topics[1]) {
                    for pa in pairs {
                        if !self.ignored_pools.contains(pa) {
                            if let Some(pool) = self.pools.get(pa) {
                                if self.is_pool_routable(pool)
                                    && self.token_tax_allows_swap(pool.token0, pool.token1, None)
                                {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        false
    }

    /// Apply simulated DEX Sync/Swap logs onto a shadow `MarketState` without mutating unknown
    /// discovery queues.
    pub fn apply_projected_logs(
        &mut self,
        logs: &[crate::rpc::ReceiptLog],
        target_block: u64,
    ) -> ProjectedLogsDelta {
        let mut delta = ProjectedLogsDelta::default();
        let infinity_mgr = self.chain.infinity_cl_pool_manager();
        let univ4_mgr = self.chain.univ4_pool_manager();
        let bal_vault = self.chain.balancer_v2_vault();

        for lg in logs {
            let Some(topic0) = lg.topics.first().copied() else {
                continue;
            };
            if (topic0 == TOPIC_UNIV2_SYNC || topic0 == TOPIC_SOLIDLY_SYNC) && lg.data.len() >= 64 {
                if self.ignored_pools.contains(&lg.address) {
                    continue;
                }
                let Some((token0, token1, dec0, dec1, routable)) = self.pools.get(&lg.address).map(|p| {
                    (p.token0, p.token1, p.dec0, p.dec1, self.is_pool_routable(p))
                }) else {
                    continue;
                };
                if !routable || !self.token_tax_allows_swap(token0, token1, None) {
                    continue;
                }
                let new_r0 = U256::from_be_slice(&lg.data[0..32]);
                let new_r1 = U256::from_be_slice(&lg.data[32..64]);
                let p0 = self.token_usd(token0);
                let p1 = self.token_usd(token1);
                let (old_r0, old_r1) = if let Some(pool) = self.pools.get_mut(&lg.address) {
                    let old = (pool.r0_raw, pool.r1_raw);
                    pool.r0_raw = new_r0;
                    pool.r1_raw = new_r1;
                    pool.last_updated_block = target_block;
                    pool.refresh_depth(p0, p1);
                    old
                } else {
                    continue;
                };
                let swap_usd = self.projected_swap_usd(
                    token0,
                    dec0,
                    new_r0.abs_diff(old_r0),
                    token1,
                    dec1,
                    new_r1.abs_diff(old_r1),
                );
                delta.touched_pools += 1;
                delta.dirty_pools.insert(lg.address);
                delta.max_swap_usd = delta.max_swap_usd.max(swap_usd);
                delta.total_swap_usd += swap_usd;
            } else if (topic0 == TOPIC_UNIV3_SWAP || topic0 == TOPIC_PANCAKEV3_SWAP)
                && lg.data.len() >= 160
            {
                if self.ignored_pools.contains(&lg.address) {
                    continue;
                }
                let Some((token0, token1, dec0, dec1, routable)) = self.pools.get(&lg.address).map(|p| {
                    (p.token0, p.token1, p.dec0, p.dec1, self.is_pool_routable(p))
                }) else {
                    continue;
                };
                let amount0 = I256::from_raw(U256::from_be_slice(&lg.data[0..32]));
                let amount1 = I256::from_raw(U256::from_be_slice(&lg.data[32..64]));
                let check_v3_in = if amount0 > I256::ZERO {
                    Some(true)
                } else if amount1 > I256::ZERO {
                    Some(false)
                } else {
                    None
                };
                if !routable || !self.token_tax_allows_swap(token0, token1, check_v3_in) {
                    continue;
                }
                let sqrt_price_x96 = U256::from_be_slice(&lg.data[64..96]);
                if sqrt_price_x96.is_zero() {
                    continue;
                }
                let liq = U256::from_be_slice(&lg.data[96..128]).saturating_to::<u128>();
                let tick = decode_i24_word(&lg.data[128..160]);
                let p0 = self.token_usd(token0);
                let p1 = self.token_usd(token1);
                if let Some(pool) = self.pools.get_mut(&lg.address) {
                    pool.sqrt_price_x96 = sqrt_price_x96;
                    pool.liquidity = liq;
                    pool.tick = tick;
                    pool.last_updated_block = target_block;
                    pool.refresh_depth(p0, p1);
                } else {
                    continue;
                }
                let swap_usd = self.projected_swap_usd(
                    token0,
                    dec0,
                    amount0.unsigned_abs(),
                    token1,
                    dec1,
                    amount1.unsigned_abs(),
                );
                delta.touched_pools += 1;
                delta.dirty_pools.insert(lg.address);
                delta.max_swap_usd = delta.max_swap_usd.max(swap_usd);
                delta.total_swap_usd += swap_usd;
            } else if ((infinity_mgr == Some(lg.address) && topic0 == TOPIC_INFINITY_CL_SWAP)
                || (univ4_mgr == Some(lg.address) && topic0 == TOPIC_UNIV4_SWAP))
                && lg.topics.len() >= 2
                && lg.data.len() >= 192
            {
                let synth_addr = singleton_pool_address(lg.address, lg.topics[1]);
                if self.ignored_pools.contains(&synth_addr) {
                    continue;
                }
                let Some((token0, token1, dec0, dec1, routable)) = self.pools.get(&synth_addr).map(|p| {
                    (p.token0, p.token1, p.dec0, p.dec1, self.is_pool_routable(p))
                }) else {
                    continue;
                };
                let amount0 = I256::from_raw(U256::from_be_slice(&lg.data[0..32]));
                let amount1 = I256::from_raw(U256::from_be_slice(&lg.data[32..64]));
                let check_v3_in = if amount0 > I256::ZERO {
                    Some(true)
                } else if amount1 > I256::ZERO {
                    Some(false)
                } else {
                    None
                };
                if !routable || !self.token_tax_allows_swap(token0, token1, check_v3_in) {
                    continue;
                }
                let sqrt_price_x96 = U256::from_be_slice(&lg.data[64..96]);
                if sqrt_price_x96.is_zero() {
                    continue;
                }
                let liq = U256::from_be_slice(&lg.data[96..128]).saturating_to::<u128>();
                let tick = decode_i24_word(&lg.data[128..160]);
                let fee = U256::from_be_slice(&lg.data[160..192]).saturating_to::<u32>();
                let p0 = self.token_usd(token0);
                let p1 = self.token_usd(token1);
                if let Some(pool) = self.pools.get_mut(&synth_addr) {
                    pool.sqrt_price_x96 = sqrt_price_x96;
                    pool.liquidity = liq;
                    pool.tick = tick;
                    if is_standard_v3_fee_ppm(fee) {
                        pool.fee_ppm = fee;
                    }
                    pool.last_updated_block = target_block;
                    pool.refresh_depth(p0, p1);
                } else {
                    continue;
                }
                let swap_usd = self.projected_swap_usd(
                    token0,
                    dec0,
                    amount0.unsigned_abs(),
                    token1,
                    dec1,
                    amount1.unsigned_abs(),
                );
                delta.touched_pools += 1;
                delta.dirty_pools.insert(synth_addr);
                delta.max_swap_usd = delta.max_swap_usd.max(swap_usd);
                delta.total_swap_usd += swap_usd;
            } else if bal_vault == Some(lg.address)
                && topic0 == TOPIC_BALANCER_V2_SWAP
                && lg.topics.len() >= 4
                && lg.data.len() >= 64
            {
                let pool_id = lg.topics[1];
                let token_in = Address::from_word(lg.topics[2]);
                let token_out = Address::from_word(lg.topics[3]);
                let amt_in = U256::from_be_slice(&lg.data[0..32]);
                let amt_out = U256::from_be_slice(&lg.data[32..64]);
                let dec_in = self.token_decimals(token_in);
                let dec_out = self.token_decimals(token_out);
                let swap_usd = self.projected_swap_usd(token_in, dec_in, amt_in, token_out, dec_out, amt_out);

                if let Some(tok_list) = self.balancer_pool_tokens.get_mut(&pool_id) {
                    for entry in tok_list.iter_mut() {
                        if entry.0 == token_in {
                            entry.3 = entry.3.saturating_add(amt_in);
                        } else if entry.0 == token_out {
                            entry.3 = entry.3.saturating_sub(amt_out);
                        }
                    }
                    let snap = tok_list.clone();
                    let pair_addrs = self
                        .balancer_pool_pairs
                        .get(&pool_id)
                        .cloned()
                        .unwrap_or_default();
                    let mut touched_this_log = false;
                    for pa in pair_addrs {
                        if self.ignored_pools.contains(&pa) {
                            continue;
                        }
                        let Some((token0, token1, routable)) = self.pools.get(&pa).map(|p| {
                            (p.token0, p.token1, self.is_pool_routable(p))
                        }) else {
                            continue;
                        };
                        let p0 = self.token_usd(token0);
                        let p1 = self.token_usd(token1);
                        if let Some(pool) = self.pools.get_mut(&pa) {
                            for &(tk, _, _, raw) in &snap {
                                if tk == pool.token0 {
                                    pool.r0_raw = raw;
                                } else if tk == pool.token1 {
                                    pool.r1_raw = raw;
                                }
                            }
                            if let Some(BalancerPoolState {
                                kind:
                                    BalancerPoolKind::Stable {
                                        other_scaled_balances,
                                        ..
                                    },
                                ..
                            }) = &mut pool.balancer
                            {
                                *other_scaled_balances = snap
                                    .iter()
                                    .filter(|&&(tk, _, _, _)| tk != pool.token0 && tk != pool.token1)
                                    .map(|&(_, dec, hs, raw)| u256_to_human(raw, dec) * hs)
                                    .collect();
                            }
                            pool.last_updated_block = target_block;
                            pool.refresh_depth(p0, p1);
                        }
                        if routable
                            && self.token_tax_allows_swap(token0, token1, None)
                            && (token0 == token_in
                                || token1 == token_in
                                || token0 == token_out
                                || token1 == token_out)
                        {
                            delta.touched_pools += 1;
                            delta.dirty_pools.insert(pa);
                            touched_this_log = true;
                        }
                    }
                    if touched_this_log {
                        delta.max_swap_usd = delta.max_swap_usd.max(swap_usd);
                        delta.total_swap_usd += swap_usd;
                    }
                }
            }
        }

        self.refresh_dirty_pools(&delta.dirty_pools);
        delta
    }

    /// Background/post-solve step 2: Drain top buffered unknown pools and expand cross-factory pairs via Multicall3.
    pub async fn discover_unknowns(
        &mut self,
        rpc: &RpcPool,
        block_num: u64,
        delta: &mut BlockDelta,
    ) -> Result<()> {
        if self.pending_unknowns.is_empty()
            && self.pending_unknown_v4.is_empty()
            && self.pending_unknown_balancer.is_empty()
            && self.pending_expand_pairs.is_empty()
        {
            return Ok(());
        }
        let mut ranked: Vec<(Address, usize, DexHint)> = self
            .pending_unknowns
            .iter()
            .map(|(&a, &(c, h))| (a, c, h))
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1));
        ranked.truncate(if self.pools.len() >= 4000 { 12 } else { 24 });
        for &(addr, _, _) in &ranked {
            self.pending_unknowns.remove(&addr);
        }

        let before = self.pools.len();
        let prev_flash_len = self.flash_pools.len();
        let existing_addrs: HashSet<Address> = self.pools.keys().copied().collect();
        let new_tokens = if !ranked.is_empty() {
            self.probe_unknown_pools(rpc, &ranked).await?
        } else {
            Vec::new()
        };

        if !self.pending_unknown_v4.is_empty() {
            let mut ranked_v4: Vec<(B256, usize, DexKind)> = self
                .pending_unknown_v4
                .iter()
                .map(|(&id, &(c, k))| (id, c, k))
                .collect();
            ranked_v4.sort_by(|a, b| b.1.cmp(&a.1));
            ranked_v4.truncate(if self.pools.len() >= 4000 { 12 } else { 16 });
            for &(id, _, _) in &ranked_v4 {
                self.pending_unknown_v4.remove(&id);
            }
            let _ = self.probe_unknown_v4_pools(rpc, &ranked_v4, block_num).await;
        }
        if !self.pending_unknown_balancer.is_empty() {
            let mut ranked_bal: Vec<(B256, usize)> = self
                .pending_unknown_balancer
                .iter()
                .map(|(&id, &c)| (id, c))
                .collect();
            ranked_bal.sort_by(|a, b| b.1.cmp(&a.1));
            ranked_bal.truncate(12);
            let bal_ids: Vec<B256> = ranked_bal.iter().map(|(id, _)| *id).collect();
            for id in &bal_ids {
                self.pending_unknown_balancer.remove(id);
            }
            let _ = self.discover_balancer_v2_pools(rpc, &bal_ids).await;
        }

        let mut unseen_pairs = Vec::new();
        let mut seen_local: HashSet<(Address, Address)> = HashSet::new();
        let mut push_if_fresh = |a: Address, b: Address| {
            if a == b {
                return;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            if !self.queried_pairs.contains(&key) && seen_local.insert(key) {
                unseen_pairs.push(key);
            }
        };

        let drained: Vec<_> = self.pending_expand_pairs.iter().take(20).copied().collect();
        for k in &drained {
            self.pending_expand_pairs.remove(k);
            push_if_fresh(k.0, k.1);
        }
        for tk in &new_tokens {
            for anchor in &self.chain.hub_tokens {
                push_if_fresh(*tk, anchor.address);
            }
        }
        for p_addr in self.pools.values() {
            if p_addr.last_updated_block == block_num {
                push_if_fresh(p_addr.token0, p_addr.token1);
            }
        }
        if !unseen_pairs.is_empty() {
            unseen_pairs.truncate(if self.pools.len() >= 4000 {
                12
            } else if self.pools.len() >= 1800 {
                24
            } else {
                32
            });
            self.expand_pairs_across_factories(rpc, &unseen_pairs).await?;
        }
        delta.discovered_pools = self.pools.len().saturating_sub(before);
        if self.flash_pools.len() > prev_flash_len {
            let _ = self.refresh_flash_balances(rpc).await;
        }
        self.refresh_token_usd_prices_passes(3);
        for &addr in self.pools.keys() {
            if !existing_addrs.contains(&addr) {
                delta.dirty_pools.insert(addr);
            }
        }
        Ok(())
    }

    pub async fn discover_metric_pools(
        &mut self,
        rpc: &RpcPool,
        candidates: &[Address],
    ) -> Result<Vec<Address>> {
        let Some(metric_factory) = self.chain.metric_factory() else {
            return Ok(Vec::new());
        };
        let mut unique: Vec<Address> = Vec::new();
        let mut seen: HashSet<Address> = HashSet::new();
        for &addr in candidates {
            if !addr.is_zero()
                && !self.pools.contains_key(&addr)
                && !self.ignored_pools.contains(&addr)
                && seen.insert(addr)
            {
                unique.push(addr);
            }
        }
        if unique.is_empty() {
            return Ok(Vec::new());
        }

        let imm_calls: Vec<IMulticall3::Call3> = unique
            .iter()
            .map(|&addr| IMulticall3::Call3 {
                target: addr,
                allowFailure: true,
                callData: IMetricOmmPool::getImmutablesCall {}.abi_encode().into(),
            })
            .collect();
        let imm_res = rpc.multicall(imm_calls).await?;

        let mut parsed: Vec<(Address, Address, Address, u16)> = Vec::new();
        let mut missing_tokens: HashSet<Address> = HashSet::new();
        let mut extra_tokens: Vec<Address> = Vec::new();

        for (addr, r) in unique.into_iter().zip(imm_res.into_iter()) {
            if !r.success || r.returnData.len() < 352 {
                continue;
            }
            let data = &r.returnData;
            let factory = Address::from_slice(&data[12..32]);
            if factory != metric_factory {
                continue;
            }
            // MetricOmmPool stores token0/token1 unsorted at deployment order.
            let token0 = Address::from_slice(&data[44..64]);
            let token1 = Address::from_slice(&data[76..96]);
            if token0.is_zero() || token1.is_zero() || token0 == token1 {
                continue;
            }
            let min_bin = I256::from_raw(U256::from_be_slice(&data[288..320]));
            let max_bin = I256::from_raw(U256::from_be_slice(&data[320..352]));
            let num_bins = max_bin
                .checked_sub(min_bin)
                .and_then(|d| i64::try_from(d).ok())
                .unwrap_or(39)
                .saturating_add(1)
                .clamp(2, 512) as u16;
            if !self.tokens.contains_key(&token0) {
                missing_tokens.insert(token0);
            }
            if !self.tokens.contains_key(&token1) {
                missing_tokens.insert(token1);
            }
            extra_tokens.push(token0);
            extra_tokens.push(token1);
            parsed.push((addr, token0, token1, num_bins));
        }
        if parsed.is_empty() {
            return Ok(Vec::new());
        }

        if !missing_tokens.is_empty() {
            let tks: Vec<Address> = missing_tokens.into_iter().collect();
            self.fetch_tokens_meta(rpc, &tks).await?;
        }

        let mut state_calls = Vec::with_capacity(parsed.len() * 3);
        for &(pool, _, _, _) in &parsed {
            push_metric_state_calls(&mut state_calls, pool);
        }
        let state_res = rpc.multicall(state_calls).await?;
        let mut added_pools: Vec<Address> = Vec::with_capacity(parsed.len());

        for (idx, (pool, token0, token1, num_bins)) in parsed.into_iter().enumerate() {
            let base = idx * 3;
            if base + 3 > state_res.len() {
                break;
            }
            let (sell, buy, r0_raw, r1_raw, active_bin, paused) =
                if let Some(st) = decode_metric_state_slice(&state_res[base..base + 3]) {
                    st
                } else if let Some((r0_raw, r1_raw, active_bin, _)) =
                    decode_metric_slots_only(&state_res[base..base + 3])
                {
                    (0, 0, r0_raw, r1_raw, active_bin, true)
                } else {
                    continue;
                };
            let dec0 = self.token_decimals(token0);
            let dec1 = self.token_decimals(token1);
            let tax0 = self.token_tax_ppm.get(&token0).copied().unwrap_or(0);
            let tax1 = self.token_tax_ppm.get(&token1).copied().unwrap_or(0);
            let extra_tax_ppm = tax0.saturating_add(tax1).min(999_999);
            self.insert_pool(PoolState {
                address: pool,
                factory_name: "Metric".to_string(),
                dex_kind: DexKind::MetricOmm,
                token0,
                token1,
                dec0,
                dec1,
                fee_num: 10_000,
                fee_ppm: 0,
                extra_tax_ppm,
                stable: false,
                r0_raw,
                r1_raw,
                sqrt_price_x96: U256::ZERO,
                liquidity: 0,
                tick: 0,
                tick_spacing: 1,
                allow_multi_tick: false,
                is_hub_0: self.is_hub_token(token0),
                is_hub_1: self.is_hub_token(token1),
                cached_depth_usd: 0.0,
                last_updated_block: self.last_block,
                v4_key: None,
                metric: Some(MetricPoolState {
                    sell_price_x64: sell,
                    buy_price_x64: buy,
                    num_bins,
                    active_bin,
                    paused,
                }),
                balancer: None,
            });
            added_pools.push(pool);
        }

        if !added_pools.is_empty() {
            self.refresh_token_usd_prices_passes(2);
            for addr in added_pools {
                if let Some((t0, t1)) = self.pools.get(&addr).map(|p| (p.token0, p.token1)) {
                    let p0 = self.token_usd(t0);
                    let p1 = self.token_usd(t1);
                    if let Some(p) = self.pools.get_mut(&addr) {
                        p.refresh_depth(p0, p1);
                    }
                }
            }
        }

        let non_hub_extra: Vec<Address> = extra_tokens
            .into_iter()
            .filter(|t| !self.is_hub_token(*t))
            .collect();
        Ok(non_hub_extra)
    }

    pub async fn discover_balancer_v2_pools(
        &mut self,
        rpc: &RpcPool,
        pool_ids: &[B256],
    ) -> Result<usize> {
        let Some(vault) = self.chain.balancer_v2_vault() else {
            return Ok(0);
        };
        if pool_ids.is_empty() {
            return Ok(0);
        }
        let mut uniq_ids: Vec<B256> = Vec::with_capacity(pool_ids.len());
        for &pid in pool_ids {
            if !uniq_ids.contains(&pid) {
                uniq_ids.push(pid);
            }
        }

        let mut tok_calls = Vec::with_capacity(uniq_ids.len());
        for &pid in &uniq_ids {
            tok_calls.push(IMulticall3::Call3 {
                target: vault,
                allowFailure: true,
                callData: IBalancerV2Vault::getPoolTokensCall { poolId: pid }
                    .abi_encode()
                    .into(),
            });
        }
        let tok_res = rpc.multicall(tok_calls).await?;

        struct RawBalPool {
            pool_id: B256,
            pool_addr: Address,
            tokens: Vec<Address>,
            balances: Vec<U256>,
        }
        let mut raw_pools: Vec<RawBalPool> = Vec::new();
        for (idx, r) in tok_res.into_iter().enumerate() {
            if !r.success {
                continue;
            }
            let Ok(ret) = IBalancerV2Vault::getPoolTokensCall::abi_decode_returns(&r.returnData)
            else {
                continue;
            };
            if ret.tokens.len() != ret.balances.len()
                || ret.tokens.len() < 2
                || ret.tokens.len() > 8
            {
                continue;
            }
            let pool_id = uniq_ids[idx];
            let pool_addr = Address::from_slice(&pool_id.as_slice()[..20]);
            raw_pools.push(RawBalPool {
                pool_id,
                pool_addr,
                tokens: ret.tokens,
                balances: ret.balances,
            });
        }
        if raw_pools.is_empty() {
            return Ok(0);
        }

        let mut meta_calls = Vec::with_capacity(raw_pools.len() * 5);
        for rp in &raw_pools {
            meta_calls.push(IMulticall3::Call3 {
                target: rp.pool_addr,
                allowFailure: true,
                callData: IBalancerV2Pool::getSwapFeePercentageCall {}
                    .abi_encode()
                    .into(),
            });
            meta_calls.push(IMulticall3::Call3 {
                target: rp.pool_addr,
                allowFailure: true,
                callData: IBalancerV2Pool::getNormalizedWeightsCall {}
                    .abi_encode()
                    .into(),
            });
            meta_calls.push(IMulticall3::Call3 {
                target: rp.pool_addr,
                allowFailure: true,
                callData: IBalancerV2Pool::getAmplificationParameterCall {}
                    .abi_encode()
                    .into(),
            });
            meta_calls.push(IMulticall3::Call3 {
                target: rp.pool_addr,
                allowFailure: true,
                callData: IBalancerV2Pool::getScalingFactorsCall {}
                    .abi_encode()
                    .into(),
            });
            meta_calls.push(IMulticall3::Call3 {
                target: rp.pool_addr,
                allowFailure: true,
                callData: IBalancerV2Pool::getBptIndexCall {}.abi_encode().into(),
            });
        }
        let meta_res = rpc.multicall(meta_calls).await?;

        let mut bpt_indices: Vec<Option<usize>> = vec![None; raw_pools.len()];
        let mut missing_tokens: HashSet<Address> = HashSet::new();
        for (idx, rp) in raw_pools.iter().enumerate() {
            let base = idx * 5;
            if base + 5 > meta_res.len() {
                continue;
            }
            if meta_res[base + 4].success {
                if let Ok(bi) =
                    IBalancerV2Pool::getBptIndexCall::abi_decode_returns(&meta_res[base + 4].returnData)
                {
                    bpt_indices[idx] = Some(bi.saturating_to::<usize>());
                }
            }
            let bpt_i = bpt_indices[idx];
            for (ti, &tk) in rp.tokens.iter().enumerate() {
                if tk != rp.pool_addr && Some(ti) != bpt_i && !self.tokens.contains_key(&tk) {
                    missing_tokens.insert(tk);
                }
            }
        }
        if !missing_tokens.is_empty() {
            let tks: Vec<Address> = missing_tokens.into_iter().collect();
            self.fetch_tokens_meta(rpc, &tks).await?;
        }

        let mut updated_pools = 0usize;
        let mut updated_pairs = 0usize;
        for (idx, rp) in raw_pools.into_iter().enumerate() {
            let base = idx * 5;
            if base + 5 > meta_res.len() {
                continue;
            }
            let slice = &meta_res[base..base + 5];
            if !slice[0].success {
                continue;
            }
            let Ok(fee_1e18) =
                IBalancerV2Pool::getSwapFeePercentageCall::abi_decode_returns(&slice[0].returnData)
            else {
                continue;
            };
            let fee_ppm = (fee_1e18 / U256::from(1_000_000_000_000u64)).saturating_to::<u32>();
            if fee_ppm > 100_000 {
                continue;
            }

            let bpt_i = bpt_indices[idx];
            let valid_indices: Vec<usize> = (0..rp.tokens.len())
                .filter(|&i| rp.tokens[i] != rp.pool_addr && Some(i) != bpt_i)
                .collect();
            if valid_indices.len() < 2 || valid_indices.len() > 6 {
                continue;
            }
            if valid_indices.iter().any(|&i| rp.balances[i].is_zero()) {
                continue;
            }

            let weights_opt: Option<Vec<f64>> = if slice[1].success {
                IBalancerV2Pool::getNormalizedWeightsCall::abi_decode_returns(&slice[1].returnData)
                    .ok()
                    .filter(|ws| ws.len() == rp.tokens.len())
                    .map(|ws| ws.into_iter().map(|w| u256_to_f64(w) / 1e18).collect())
            } else {
                None
            };
            let amp_opt: Option<f64> = if weights_opt.is_none() && slice[2].success {
                IBalancerV2Pool::getAmplificationParameterCall::abi_decode_returns(
                    &slice[2].returnData,
                )
                .ok()
                .and_then(|ap| {
                    if ap.precision.is_zero() || ap.value.is_zero() {
                        None
                    } else {
                        let amp = u256_to_f64(ap.value) / u256_to_f64(ap.precision);
                        if amp.is_finite() && (1.0..=100_000.0).contains(&amp) {
                            Some(amp)
                        } else {
                            None
                        }
                    }
                })
            } else {
                None
            };
            if weights_opt.is_none() && amp_opt.is_none() {
                continue;
            }
            let sfs_opt: Option<Vec<U256>> = if amp_opt.is_some() && slice[3].success {
                IBalancerV2Pool::getScalingFactorsCall::abi_decode_returns(&slice[3].returnData)
                    .ok()
                    .filter(|sfs| sfs.len() == rp.tokens.len())
            } else {
                None
            };

            let mut tok_entries: Vec<(Address, u8, f64, U256, f64)> =
                Vec::with_capacity(valid_indices.len());
            let mut invalid_entry = false;
            for &i in &valid_indices {
                let tk = rp.tokens[i];
                let dec = self.token_decimals(tk);
                let bal = rp.balances[i];
                let h_scale = if let Some(ref sfs) = sfs_opt {
                    let denom = 10f64.powi(36i32 - (dec as i32));
                    let s = u256_to_f64(sfs[i]) / denom;
                    if s.is_finite() && s > 1e-6 && s < 1e6 {
                        s
                    } else {
                        1.0
                    }
                } else {
                    1.0
                };
                let w_val = weights_opt.as_ref().map_or(0.0, |ws| ws[i]);
                if weights_opt.is_some() && w_val <= 1e-6 {
                    invalid_entry = true;
                    break;
                }
                tok_entries.push((tk, dec, h_scale, bal, w_val));
            }
            if invalid_entry {
                continue;
            }

            let canonical_tokens: Vec<(Address, u8, f64, U256)> = tok_entries
                .iter()
                .map(|&(tk, dec, hs, bal, _)| (tk, dec, hs, bal))
                .collect();
            self.balancer_pool_tokens
                .insert(rp.pool_id, canonical_tokens);

            let is_stable = amp_opt.is_some();
            let mut created_addrs: Vec<Address> = Vec::new();
            for ai in 0..tok_entries.len() {
                for bi in (ai + 1)..tok_entries.len() {
                    let e_a = tok_entries[ai];
                    let e_b = tok_entries[bi];
                    if tok_entries.len() > 4
                        && !self.is_hub_token(e_a.0)
                        && !self.is_hub_token(e_b.0)
                    {
                        continue;
                    }
                    let ((t0, d0, s0, r0, w0), (t1, d1, s1, r1, w1)) = if e_a.0 < e_b.0 {
                        (e_a, e_b)
                    } else {
                        (e_b, e_a)
                    };
                    let pair_addr = balancer_pair_address(vault, rp.pool_id, t0, t1);
                    if self.ignored_pools.contains(&pair_addr) {
                        continue;
                    }
                    let kind = if let Some(amp) = amp_opt {
                        let others: Vec<f64> = tok_entries
                            .iter()
                            .filter(|&&(tk, _, _, _, _)| tk != t0 && tk != t1)
                            .map(|&(_, dec, hs, raw, _)| u256_to_human(raw, dec) * hs)
                            .collect();
                        BalancerPoolKind::Stable {
                            amp,
                            scale0: s0,
                            scale1: s1,
                            other_scaled_balances: others,
                        }
                    } else {
                        BalancerPoolKind::Weighted { w0, w1 }
                    };
                    self.insert_pool(PoolState {
                        address: pair_addr,
                        factory_name: "BalancerV2".to_string(),
                        dex_kind: DexKind::BalancerV2,
                        token0: t0,
                        token1: t1,
                        dec0: d0,
                        dec1: d1,
                        fee_num: 10_000u16.saturating_sub((fee_ppm / 100) as u16),
                        fee_ppm,
                        extra_tax_ppm: 0,
                        stable: is_stable,
                        r0_raw: r0,
                        r1_raw: r1,
                        sqrt_price_x96: U256::ZERO,
                        liquidity: 0,
                        tick: 0,
                        tick_spacing: 0,
                        allow_multi_tick: false,
                        is_hub_0: false,
                        is_hub_1: false,
                        cached_depth_usd: 0.0,
                        last_updated_block: self.last_block,
                        v4_key: None,
                        metric: None,
                        balancer: Some(BalancerPoolState {
                            vault,
                            pool_id: rp.pool_id,
                            kind,
                        }),
                    });
                    created_addrs.push(pair_addr);
                    updated_pairs += 1;
                }
            }
            if !created_addrs.is_empty() {
                self.balancer_pool_pairs.insert(rp.pool_id, created_addrs);
                updated_pools += 1;
            }
        }
        if updated_pools > 0 {
            self.refresh_token_usd_prices_passes(1);
            tracing::info!(
                pools = updated_pools,
                pairs = updated_pairs,
                "discovered/refreshed BalancerV2 pools"
            );
        }
        Ok(updated_pairs)
    }

    pub async fn refresh_metric_pools_for_delta(
        &mut self,
        rpc: &RpcPool,
        block_num: u64,
        delta: &mut BlockDelta,
    ) {
        let poll_stride: u64 = if self.chain.block_time_ms >= 3_000 { 2 } else { 10 };
        let poll_all = delta.metric_oracle_updated
            || (self.chain.metric_enabled() && block_num % poll_stride == 0);
        let targets: Vec<(Address, Address, Address)> = if poll_all {
            self.pools
                .values()
                .filter(|p| p.dex_kind == DexKind::MetricOmm)
                .map(|p| (p.address, p.token0, p.token1))
                .collect()
        } else {
            delta
                .metric_refresh_pools
                .iter()
                .filter_map(|addr| {
                    self.pools
                        .get(addr)
                        .filter(|p| p.dex_kind == DexKind::MetricOmm)
                        .map(|p| (p.address, p.token0, p.token1))
                })
                .collect()
        };
        if targets.is_empty() {
            return;
        }

        let mut calls = Vec::with_capacity(targets.len() * 3);
        for &(addr, _, _) in &targets {
            push_metric_state_calls(&mut calls, addr);
        }
        let res = match rpc.multicall(calls).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(err = %e, pools = targets.len(), "metric pool delta refresh failed");
                return;
            }
        };

        let mut depth_updates: Vec<Address> = Vec::new();
        for (idx, (addr, t0, t1)) in targets.into_iter().enumerate() {
            let base = idx * 3;
            if base + 3 > res.len() {
                break;
            }
            let Some((sell, buy, r0_raw, r1_raw, active_bin, paused)) =
                decode_metric_state_slice(&res[base..base + 3])
            else {
                if !res[base].success {
                    if let Some(p) = self.pools.get_mut(&addr) {
                        if let Some(ref mut m) = p.metric {
                            m.paused = true;
                        }
                    }
                }
                continue;
            };
            if !paused {
                self.pool_cooldown_until.remove(&addr);
            }
            let changed = if let Some(p) = self.pools.get_mut(&addr) {
                let prev_same = p.r0_raw == r0_raw
                    && p.r1_raw == r1_raw
                    && p.metric.as_ref().is_some_and(|m| {
                        m.sell_price_x64 == sell
                            && m.buy_price_x64 == buy
                            && m.active_bin == active_bin
                            && m.paused == paused
                    });
                if let Some(ref mut m) = p.metric {
                    m.sell_price_x64 = sell;
                    m.buy_price_x64 = buy;
                    m.active_bin = active_bin;
                    m.paused = paused;
                } else {
                    p.metric = Some(MetricPoolState {
                        sell_price_x64: sell,
                        buy_price_x64: buy,
                        num_bins: 40,
                        active_bin,
                        paused,
                    });
                }
                p.r0_raw = r0_raw;
                p.r1_raw = r1_raw;
                p.last_updated_block = block_num;
                !prev_same
            } else {
                false
            };
            depth_updates.push(addr);
            if changed && !paused {
                delta.dirty_pools.insert(addr);
                delta.dirty_tokens.insert(t0);
                delta.dirty_tokens.insert(t1);
            }
        }
        for addr in depth_updates {
            if let Some((t0, t1)) = self.pools.get(&addr).map(|p| (p.token0, p.token1)) {
                let p0 = self.token_usd(t0);
                let p1 = self.token_usd(t1);
                if let Some(p) = self.pools.get_mut(&addr) {
                    p.refresh_depth(p0, p1);
                }
            }
        }
    }

    async fn probe_unknown_pools(
        &mut self,
        rpc: &RpcPool,
        candidates: &[(Address, usize, DexHint)],
    ) -> Result<Vec<Address>> {
        let mut metric_addrs: Vec<Address> = Vec::new();
        let mut other_candidates: Vec<(Address, usize, DexHint)> = Vec::new();
        for &(addr, cnt, hint) in candidates {
            if matches!(hint, DexHint::MetricOmm) {
                metric_addrs.push(addr);
            } else {
                other_candidates.push((addr, cnt, hint));
            }
        }
        let mut extra_tokens = if !metric_addrs.is_empty() {
            self.discover_metric_pools(rpc, &metric_addrs).await?
        } else {
            Vec::new()
        };
        if other_candidates.is_empty() {
            return Ok(extra_tokens);
        }

        let mut calls = Vec::with_capacity(other_candidates.len() * 3);
        for (addr, _, _) in &other_candidates {
            calls.push(IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: IUniV2Pair::token0Call {}.abi_encode().into(),
            });
            calls.push(IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: IUniV2Pair::token1Call {}.abi_encode().into(),
            });
            calls.push(IMulticall3::Call3 {
                target: *addr,
                allowFailure: true,
                callData: IUniV2Pair::factoryCall {}.abi_encode().into(),
            });
        }
        let res = rpc.multicall(calls).await?;

        let factory_map: HashMap<Address, &FactorySpec> =
            self.chain.factories.iter().map(|f| (f.address, f)).collect();

        let mut verified = Vec::new();
        let mut missing_tokens = HashSet::new();

        for (idx, (pool_addr, _, hint)) in other_candidates.iter().enumerate() {
            let base = idx * 3;
            let r_t0 = &res[base];
            let r_t1 = &res[base + 1];
            let r_fac = &res[base + 2];
            if !(r_t0.success && r_t1.success) {
                self.ignored_pools.insert(*pool_addr);
                continue;
            }
            let Ok(t0) = IUniV2Pair::token0Call::abi_decode_returns(&r_t0.returnData) else {
                self.ignored_pools.insert(*pool_addr);
                continue;
            };
            let Ok(t1) = IUniV2Pair::token1Call::abi_decode_returns(&r_t1.returnData) else {
                self.ignored_pools.insert(*pool_addr);
                continue;
            };
            if t0.is_zero() || t1.is_zero() || t0 == t1 {
                self.ignored_pools.insert(*pool_addr);
                continue;
            }
            if !self.tokens.contains_key(&t0) {
                missing_tokens.insert(t0);
            }
            if !self.tokens.contains_key(&t1) {
                missing_tokens.insert(t1);
            }
            extra_tokens.push(t0);
            extra_tokens.push(t1);

            let fac = if r_fac.success {
                IUniV2Pair::factoryCall::abi_decode_returns(&r_fac.returnData).ok()
            } else {
                None
            };

            if let Some(spec) = fac.and_then(|f| factory_map.get(&f)) {
                verified.push((*pool_addr, t0, t1, (*spec).clone(), *hint));
            } else if matches!(hint, DexHint::V2) && t0 < t1 {
                // Standard UniV2 fork pool emitting Sync(uint112,uint112): admit conservatively at 0.30% fee
                let synth = FactorySpec {
                    name: "GenericV2".to_string(),
                    address: fac.unwrap_or(Address::ZERO),
                    dex_kind: DexKind::UniV2,
                    style: FactoryStyle::UniV2,
                    default_fee_num: 9970,
                    v3_tiers: Vec::new(),
                    dynamic_fee_getter: false,
                    supports_flash_swap: false,
                };
                verified.push((*pool_addr, t0, t1, synth, *hint));
            } else if matches!(hint, DexHint::UniV3) && t0 < t1 {
                let synth = FactorySpec {
                    name: "GenericUniV3".to_string(),
                    address: fac.unwrap_or(Address::ZERO),
                    dex_kind: DexKind::UniV3,
                    style: FactoryStyle::V3Fee,
                    default_fee_num: 0,
                    v3_tiers: Vec::new(),
                    dynamic_fee_getter: false,
                    supports_flash_swap: false,
                };
                verified.push((*pool_addr, t0, t1, synth, *hint));
            } else if matches!(hint, DexHint::PancakeV3) && t0 < t1 {
                let synth = FactorySpec {
                    name: "GenericPancakeV3".to_string(),
                    address: fac.unwrap_or(Address::ZERO),
                    dex_kind: DexKind::PancakeV3,
                    style: FactoryStyle::V3Fee,
                    default_fee_num: 0,
                    v3_tiers: Vec::new(),
                    dynamic_fee_getter: false,
                    supports_flash_swap: false,
                };
                verified.push((*pool_addr, t0, t1, synth, *hint));
            } else {
                // Unknown Solidly/Algebra fork: still keep t0/t1 for cross-factory expansion
                self.ignored_pools.insert(*pool_addr);
            }
        }

        if !missing_tokens.is_empty() {
            let tks: Vec<Address> = missing_tokens.into_iter().collect();
            self.fetch_tokens_meta(rpc, &tks).await?;
        }

        if !verified.is_empty() {
            let specs_to_load: Vec<(Address, Address, Address, FactorySpec)> = verified
                .into_iter()
                .map(|(p, t0, t1, s, _)| (p, t0, t1, s))
                .collect();
            self.load_pool_states(rpc, &specs_to_load).await?;
        }
        Ok(extra_tokens)
    }

    async fn probe_unknown_v4_pools(
        &mut self,
        rpc: &RpcPool,
        ranked: &[(B256, usize, DexKind)],
        block_num: u64,
    ) -> Result<()> {
        let inf_cl_mgr = self.chain.infinity_cl_pool_manager();
        let inf_vault = self.chain.infinity_vault();
        let uv4_mgr_opt = self.chain.univ4_pool_manager();
        let uv4_pos_opt = self.chain.univ4_position_manager();
        if ranked.is_empty() || (inf_cl_mgr.is_none() && uv4_mgr_opt.is_none()) {
            return Ok(());
        }
        let inf_ids: Vec<B256> = if inf_cl_mgr.is_some() {
            ranked
                .iter()
                .filter(|(_, _, k)| *k == DexKind::InfinityCl)
                .map(|(id, _, _)| *id)
                .collect()
        } else {
            Vec::new()
        };
        let uv4_ids: Vec<B256> = if uv4_mgr_opt.is_some() {
            ranked
                .iter()
                .filter(|(_, _, k)| *k == DexKind::UniV4)
                .map(|(id, _, _)| *id)
                .collect()
        } else {
            Vec::new()
        };

        let mut ready: Vec<(DexKind, V4PoolKey, U256, i32, u128)> = Vec::new();
        let mut missing_tokens: HashSet<Address> = HashSet::new();

        if let (Some(inf_mgr), Some(inf_vault)) = (inf_cl_mgr, inf_vault) {
            if !inf_ids.is_empty() {
                let mut calls = Vec::with_capacity(inf_ids.len() * 3);
                for &id in &inf_ids {
                    calls.push(IMulticall3::Call3 {
                        target: inf_mgr,
                        allowFailure: true,
                        callData: IInfinityCLPoolManager::poolIdToPoolKeyCall { id }
                            .abi_encode()
                            .into(),
                    });
                    calls.push(IMulticall3::Call3 {
                        target: inf_mgr,
                        allowFailure: true,
                        callData: IInfinityCLPoolManager::getSlot0Call { id }
                            .abi_encode()
                            .into(),
                    });
                    calls.push(IMulticall3::Call3 {
                        target: inf_mgr,
                        allowFailure: true,
                        callData: IInfinityCLPoolManager::getLiquidityCall { id }
                            .abi_encode()
                            .into(),
                    });
                }
                if let Ok(res) = rpc.multicall(calls).await {
                    for (i, &id) in inf_ids.iter().enumerate() {
                        let synth = singleton_pool_address(inf_mgr, id);
                        let r_key = &res[i * 3];
                        let r_s0 = &res[i * 3 + 1];
                        let r_liq = &res[i * 3 + 2];
                        if !r_key.success
                            || !r_s0.success
                            || !r_liq.success
                            || r_s0.returnData.len() < 64
                            || r_liq.returnData.len() < 32
                        {
                            continue;
                        }
                        let Ok(decoded) =
                            IInfinityCLPoolManager::poolIdToPoolKeyCall::abi_decode_returns(
                                &r_key.returnData,
                            )
                        else {
                            continue;
                        };
                        let fee_u32 = decoded.fee.to::<u32>();
                        if decoded.hooks != Address::ZERO || !is_standard_v3_fee_ppm(fee_u32) {
                            self.ignored_pools.insert(synth);
                            continue;
                        }
                        let params_u = U256::from_be_bytes(decoded.parameters.0);
                        let ts_masked: U256 = (params_u >> 16u8) & U256::from(0xff_ffffu32);
                        let ts = ts_masked.saturating_to::<i32>().max(1);
                        let sqrt_p = U256::from_be_slice(&r_s0.returnData[0..32]);
                        let tick = decode_i24_word(&r_s0.returnData[32..64]);
                        let liq =
                            U256::from_be_slice(&r_liq.returnData[0..32]).saturating_to::<u128>();
                        let key = V4PoolKey {
                            manager: inf_mgr,
                            vault: inf_vault,
                            pool_id: id,
                            currency0: decoded.currency0,
                            currency1: decoded.currency1,
                            fee: fee_u32,
                            tick_spacing: ts,
                            hooks: decoded.hooks,
                            parameters: decoded.parameters,
                        };
                        self.v4_known_keys.insert(id, key.clone());
                        ready.push((DexKind::InfinityCl, key, sqrt_p, tick, liq));
                    }
                }
            }
        }

        if let (Some(uv4_mgr), Some(uv4_pos_mgr)) = (uv4_mgr_opt, uv4_pos_opt) {
            if !uv4_ids.is_empty() {
                let missing_uv4: Vec<B256> = uv4_ids
                    .iter()
                    .copied()
                    .filter(|id| !self.v4_known_keys.contains_key(id))
                    .collect();
                if !missing_uv4.is_empty() {
                    let key_calls: Vec<IMulticall3::Call3> = missing_uv4
                        .iter()
                        .map(|id| IMulticall3::Call3 {
                            target: uv4_pos_mgr,
                            allowFailure: true,
                            callData: IUniV4PositionManager::poolKeysCall {
                                poolId: alloy::primitives::FixedBytes::<25>::from_slice(
                                    &id.as_slice()[..25],
                                ),
                            }
                            .abi_encode()
                            .into(),
                        })
                        .collect();
                    if let Ok(res) = rpc.multicall(key_calls).await {
                        for (id, r) in missing_uv4.into_iter().zip(res.into_iter()) {
                            let synth = singleton_pool_address(uv4_mgr, id);
                            if r.success {
                                if let Ok(decoded) =
                                    IUniV4PositionManager::poolKeysCall::abi_decode_returns(
                                        &r.returnData,
                                    )
                                {
                                    let fee_u32 = decoded.fee.to::<u32>();
                                    let ts = decoded.tickSpacing.as_i32();
                                    if decoded.currency0 != decoded.currency1
                                        && decoded.hooks == Address::ZERO
                                        && is_standard_v3_fee_ppm(fee_u32)
                                        && ts > 0
                                    {
                                        self.v4_known_keys.insert(
                                            id,
                                            V4PoolKey {
                                                manager: uv4_mgr,
                                                vault: uv4_mgr,
                                                pool_id: id,
                                                currency0: decoded.currency0,
                                                currency1: decoded.currency1,
                                                fee: fee_u32,
                                                tick_spacing: ts,
                                                hooks: Address::ZERO,
                                                parameters: B256::ZERO,
                                            },
                                        );
                                        continue;
                                    }
                                }
                            }
                            self.ignored_pools.insert(synth);
                        }
                    }
                }

                let resolved_uv4: Vec<V4PoolKey> = uv4_ids
                    .iter()
                    .filter_map(|id| self.v4_known_keys.get(id).cloned())
                    .collect();
                if !resolved_uv4.is_empty() {
                    let mut calls = Vec::with_capacity(resolved_uv4.len() * 2);
                    for k in &resolved_uv4 {
                        let (s0_key, liq_key) = univ4_slot0_and_liq_keys(k.pool_id);
                        calls.push(IMulticall3::Call3 {
                            target: uv4_mgr,
                            allowFailure: true,
                            callData: IUniV4PoolManager::extsloadCall { slot: s0_key }
                                .abi_encode()
                                .into(),
                        });
                        calls.push(IMulticall3::Call3 {
                            target: uv4_mgr,
                            allowFailure: true,
                            callData: IUniV4PoolManager::extsloadCall { slot: liq_key }
                                .abi_encode()
                                .into(),
                        });
                    }
                    if let Ok(res) = rpc.multicall(calls).await {
                        for (i, k) in resolved_uv4.into_iter().enumerate() {
                            let r_s0 = &res[i * 2];
                            let r_liq = &res[i * 2 + 1];
                            if !r_s0.success
                                || r_s0.returnData.len() < 32
                                || !r_liq.success
                                || r_liq.returnData.len() < 32
                            {
                                continue;
                            }
                            let (sqrt_p, tick) = decode_univ4_slot0(&r_s0.returnData[0..32]);
                            let liq =
                                U256::from_be_slice(&r_liq.returnData[0..32]).saturating_to::<u128>();
                            ready.push((DexKind::UniV4, k, sqrt_p, tick, liq));
                        }
                    }
                }
            }
        }

        let wbnb = self.chain.wrapped_native;
        for (_, k, _, _, _) in &ready {
            let t0 = if k.currency0 == Address::ZERO { wbnb } else { k.currency0 };
            let t1 = if k.currency1 == Address::ZERO { wbnb } else { k.currency1 };
            if !self.tokens.contains_key(&t0) {
                missing_tokens.insert(t0);
            }
            if !self.tokens.contains_key(&t1) {
                missing_tokens.insert(t1);
            }
        }
        if !missing_tokens.is_empty() {
            let tks: Vec<Address> = missing_tokens.into_iter().collect();
            let _ = self.fetch_tokens_meta(rpc, &tks).await;
        }

        let mut added = 0usize;
        for (kind, k, sqrt_p, tick, liq) in ready {
            let t0 = if k.currency0 == Address::ZERO { wbnb } else { k.currency0 };
            let t1 = if k.currency1 == Address::ZERO { wbnb } else { k.currency1 };
            if t0 == t1 {
                continue;
            }
            let dec0 = self.token_decimals(t0);
            let dec1 = self.token_decimals(t1);
            let synth = singleton_pool_address(k.manager, k.pool_id);
            let factory_name = if kind == DexKind::InfinityCl {
                "PancakeInfinityCL".to_string()
            } else {
                "UniV4".to_string()
            };
            self.insert_pool(PoolState {
                address: synth,
                factory_name,
                dex_kind: kind,
                token0: t0,
                token1: t1,
                dec0,
                dec1,
                fee_num: 0,
                fee_ppm: k.fee,
                extra_tax_ppm: 0,
                stable: false,
                r0_raw: U256::ZERO,
                r1_raw: U256::ZERO,
                sqrt_price_x96: sqrt_p,
                liquidity: liq,
                tick,
                tick_spacing: k.tick_spacing,
                allow_multi_tick: false,
                is_hub_0: false,
                is_hub_1: false,
                cached_depth_usd: 0.0,
                last_updated_block: block_num,
                v4_key: Some(k),
                metric: None,
                balancer: None,
            });
            added += 1;
        }
        if added > 0 {
            tracing::info!(added, "discovered InfinityCL/UniV4 singleton pools");
        }
        Ok(())
    }

    pub async fn fetch_tokens_meta(&mut self, rpc: &RpcPool, tokens: &[Address]) -> Result<()> {
        let missing: Vec<Address> = tokens
            .iter()
            .copied()
            .filter(|a| !self.tokens.contains_key(a))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        let mut calls = Vec::with_capacity(missing.len() * 2);
        for &tk in &missing {
            calls.push(IMulticall3::Call3 {
                target: tk,
                allowFailure: true,
                callData: IERC20::decimalsCall {}.abi_encode().into(),
            });
            calls.push(IMulticall3::Call3 {
                target: tk,
                allowFailure: true,
                callData: IERC20::symbolCall {}.abi_encode().into(),
            });
        }
        let res = rpc.multicall(calls).await?;
        for (i, &tk) in missing.iter().enumerate() {
            let r_dec = &res[i * 2];
            let r_sym = &res[i * 2 + 1];
            let decimals = if r_dec.success && r_dec.returnData.len() >= 32 {
                IERC20::decimalsCall::abi_decode_returns(&r_dec.returnData).unwrap_or(18)
            } else {
                18
            };
            let symbol = if r_sym.success && !r_sym.returnData.is_empty() {
                IERC20::symbolCall::abi_decode_returns(&r_sym.returnData)
                    .unwrap_or_else(|_| format!("0x{}", hex::encode(&tk.as_slice()[..4])))
            } else {
                format!("0x{}", hex::encode(&tk.as_slice()[..4]))
            };
            self.tokens.insert(
                tk,
                TokenMeta {
                    address: tk,
                    symbol,
                    decimals,
                    approx_usd: 0.0,
                },
            );
        }
        Ok(())
    }

    pub async fn expand_pairs_across_factories(
        &mut self,
        rpc: &RpcPool,
        pairs: &[(Address, Address)],
    ) -> Result<()> {
        let mut fresh_pairs = Vec::new();
        for &(a, b) in pairs {
            if a == b {
                continue;
            }
            let key = if a < b { (a, b) } else { (b, a) };
            if self.queried_pairs.insert(key) {
                fresh_pairs.push(key);
            }
        }
        if fresh_pairs.is_empty() {
            return Ok(());
        }

        let mut query_meta: Vec<(Address, Address, FactorySpec)> = Vec::new();
        let mut calls: Vec<IMulticall3::Call3> = Vec::new();

        for &(a, b) in &fresh_pairs {
            for fac in &self.chain.factories {
                match fac.style {
                    FactoryStyle::UniV2 => {
                        calls.push(IMulticall3::Call3 {
                            target: fac.address,
                            allowFailure: true,
                            callData: IUniV2Factory::getPairCall { tokenA: a, tokenB: b }
                                .abi_encode()
                                .into(),
                        });
                        query_meta.push((a, b, fac.clone()));
                    }
                    FactoryStyle::SolidlyGetPair => {
                        for stable in [false, true] {
                            calls.push(IMulticall3::Call3 {
                                target: fac.address,
                                allowFailure: true,
                                callData: ISolidlyFactoryGetPair::getPairCall {
                                    tokenA: a,
                                    tokenB: b,
                                    stable,
                                }
                                .abi_encode()
                                .into(),
                            });
                            query_meta.push((a, b, fac.clone()));
                        }
                    }
                    FactoryStyle::SolidlyGetPool => {
                        for stable in [false, true] {
                            calls.push(IMulticall3::Call3 {
                                target: fac.address,
                                allowFailure: true,
                                callData: ISolidlyFactoryGetPool::getPoolCall {
                                    tokenA: a,
                                    tokenB: b,
                                    stable,
                                }
                                .abi_encode()
                                .into(),
                            });
                            query_meta.push((a, b, fac.clone()));
                        }
                    }
                    FactoryStyle::V3Fee => {
                        for &fee in &fac.v3_tiers {
                            calls.push(IMulticall3::Call3 {
                                target: fac.address,
                                allowFailure: true,
                                callData: IV3Factory::getPoolCall {
                                    tokenA: a,
                                    tokenB: b,
                                    fee: alloy::primitives::aliases::U24::from(fee as u32),
                                }
                                .abi_encode()
                                .into(),
                            });
                            query_meta.push((a, b, fac.clone()));
                        }
                    }
                    FactoryStyle::SlipstreamTs => {
                        for &ts in &fac.v3_tiers {
                            calls.push(IMulticall3::Call3 {
                                target: fac.address,
                                allowFailure: true,
                                callData: ISlipstreamFactory::getPoolCall {
                                    tokenA: a,
                                    tokenB: b,
                                    tickSpacing: alloy::primitives::aliases::I24::try_from(ts).unwrap_or_default(),
                                }
                                .abi_encode()
                                .into(),
                            });
                            query_meta.push((a, b, fac.clone()));
                        }
                    }
                }
            }
        }

        let res = rpc.multicall(calls).await?;
        let mut to_load = Vec::new();
        for (idx, r) in res.into_iter().enumerate() {
            if !r.success || r.returnData.len() < 32 {
                continue;
            }
            let addr = Address::from_slice(&r.returnData[12..32]);
            if addr.is_zero() || self.pools.contains_key(&addr) || self.ignored_pools.contains(&addr) {
                continue;
            }
            let (t0, t1, fac) = &query_meta[idx];
            to_load.push((addr, *t0, *t1, fac.clone()));
        }

        if !to_load.is_empty() {
            self.load_pool_states(rpc, &to_load).await?;
        }
        Ok(())
    }

    async fn load_pool_states(
        &mut self,
        rpc: &RpcPool,
        items: &[(Address, Address, Address, FactorySpec)],
    ) -> Result<()> {
        let missing_tokens: Vec<Address> = items
            .iter()
            .flat_map(|(_, t0, t1, _)| [*t0, *t1])
            .filter(|t| !self.tokens.contains_key(t))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        self.fetch_tokens_meta(rpc, &missing_tokens).await?;

        // Build per-pool state queries
        let mut calls = Vec::new();
        let mut ranges = Vec::with_capacity(items.len());

        for (pool, _, _, fac) in items {
            let start = calls.len();
            match fac.dex_kind {
                DexKind::UniV2 => {
                    calls.push(IMulticall3::Call3 {
                        target: *pool,
                        allowFailure: true,
                        callData: IUniV2Pair::getReservesCall {}.abi_encode().into(),
                    });
                    if fac.dynamic_fee_getter {
                        calls.push(IMulticall3::Call3 {
                            target: *pool,
                            allowFailure: true,
                            callData: IUniV2Pair::swapFeeCall {}.abi_encode().into(),
                        });
                    }
                }
                DexKind::Solidly => {
                    calls.push(IMulticall3::Call3 {
                        target: *pool,
                        allowFailure: true,
                        callData: ISolidlyPair::getReservesCall {}.abi_encode().into(),
                    });
                    calls.push(IMulticall3::Call3 {
                        target: *pool,
                        allowFailure: true,
                        callData: ISolidlyPair::stableCall {}.abi_encode().into(),
                    });
                }
                DexKind::UniV3 | DexKind::PancakeV3 => {
                    calls.push(IMulticall3::Call3 {
                        target: *pool,
                        allowFailure: true,
                        callData: IV3Pool::slot0Call {}.abi_encode().into(),
                    });
                    calls.push(IMulticall3::Call3 {
                        target: *pool,
                        allowFailure: true,
                        callData: IV3Pool::liquidityCall {}.abi_encode().into(),
                    });
                    calls.push(IMulticall3::Call3 {
                        target: *pool,
                        allowFailure: true,
                        callData: IV3Pool::feeCall {}.abi_encode().into(),
                    });
                    calls.push(IMulticall3::Call3 {
                        target: *pool,
                        allowFailure: true,
                        callData: IV3Pool::tickSpacingCall {}.abi_encode().into(),
                    });
                }
                DexKind::InfinityCl | DexKind::UniV4 | DexKind::MetricOmm | DexKind::BalancerV2 => {}
            }
            ranges.push((start, calls.len()));
        }

        let res = rpc.multicall(calls).await?;
        let mut solidly_fee_queries = Vec::new();

        for (idx, (pool_addr, a, b, fac)) in items.iter().enumerate() {
            let (s, e) = ranges[idx];
            let slice = &res[s..e];
            let (token0, token1) = if a < b { (*a, *b) } else { (*b, *a) };
            let dec0 = self.token_decimals(token0);
            let dec1 = self.token_decimals(token1);

            match fac.dex_kind {
                DexKind::UniV2 => {
                    if !slice[0].success || slice[0].returnData.len() < 64 {
                        self.ignored_pools.insert(*pool_addr);
                        continue;
                    }
                    let r0 = U256::from_be_slice(&slice[0].returnData[0..32]);
                    let r1 = U256::from_be_slice(&slice[0].returnData[32..64]);
                    let mut fee_num = fac.default_fee_num;
                    if fac.dynamic_fee_getter
                        && slice.len() > 1
                        && slice[1].success
                        && slice[1].returnData.len() >= 32
                    {
                        if let Ok(sf) = IUniV2Pair::swapFeeCall::abi_decode_returns(&slice[1].returnData) {
                            if sf <= 100 {
                                fee_num = 10_000u16.saturating_sub((sf as u16) * 10);
                            }
                        }
                    }
                    let fee_ppm = ((10_000u32.saturating_sub(fee_num as u32)) * 100) as u32;
                    self.insert_pool(PoolState {
                        address: *pool_addr,
                        factory_name: fac.name.to_string(),
                        dex_kind: DexKind::UniV2,
                        token0,
                        token1,
                        dec0,
                        dec1,
                        fee_num,
                        fee_ppm,
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
                        cached_depth_usd: 0.0,
                        last_updated_block: self.last_block,
                        v4_key: None,
                        metric: None,
                        balancer: None,
                    });
                }
                DexKind::Solidly => {
                    if !slice[0].success || slice[0].returnData.len() < 64 {
                        self.ignored_pools.insert(*pool_addr);
                        continue;
                    }
                    let r0 = U256::from_be_slice(&slice[0].returnData[0..32]);
                    let r1 = U256::from_be_slice(&slice[0].returnData[32..64]);
                    let stable = slice.get(1).is_some_and(|x| {
                        x.success
                            && x.returnData.len() >= 32
                            && ISolidlyPair::stableCall::abi_decode_returns(&x.returnData).unwrap_or(false)
                    });
                    solidly_fee_queries.push((*pool_addr, fac.clone(), stable));
                    let default_bps = if stable { 5u32 } else { 10_000u32 - fac.default_fee_num as u32 };
                    self.insert_pool(PoolState {
                        address: *pool_addr,
                        factory_name: fac.name.to_string(),
                        dex_kind: DexKind::Solidly,
                        token0,
                        token1,
                        dec0,
                        dec1,
                        fee_num: (10_000u32 - default_bps) as u16,
                        fee_ppm: default_bps * 100,
                        extra_tax_ppm: 0,
                        stable,
                        r0_raw: r0,
                        r1_raw: r1,
                        sqrt_price_x96: U256::ZERO,
                        liquidity: 0,
                        tick: 0,
                        tick_spacing: 0,
                        allow_multi_tick: false,
                        is_hub_0: false,
                        is_hub_1: false,
                        cached_depth_usd: 0.0,
                        last_updated_block: self.last_block,
                        v4_key: None,
                        metric: None,
                        balancer: None,
                    });
                }
                DexKind::UniV3 | DexKind::PancakeV3 => {
                    if !slice[0].success || slice[0].returnData.len() < 64 || !slice[1].success || slice[1].returnData.len() < 32 {
                        self.ignored_pools.insert(*pool_addr);
                        continue;
                    }
                    let sqrt_p = U256::from_be_slice(&slice[0].returnData[0..32]);
                    let tick = decode_i24_word(&slice[0].returnData[32..64]);
                    let liq = U256::from_be_slice(&slice[1].returnData[0..32]).saturating_to::<u128>();
                    let fee_ppm = if slice[2].success && slice[2].returnData.len() >= 32 {
                        U256::from_be_slice(&slice[2].returnData[0..32]).saturating_to::<u32>()
                    } else {
                        3000
                    };
                    if fac.name.starts_with("Generic") && !is_standard_v3_fee_ppm(fee_ppm) {
                        self.ignored_pools.insert(*pool_addr);
                        continue;
                    }
                    let tick_spacing = if slice.len() > 3 && slice[3].success && slice[3].returnData.len() >= 32 {
                        decode_i24_word(&slice[3].returnData[0..32]).max(1)
                    } else {
                        match fee_ppm {
                            1..=100 => 1,
                            500 => 10,
                            2500 => 50,
                            3000 => 60,
                            10000 => 200,
                            _ => 10,
                        }
                    };
                    self.insert_pool(PoolState {
                        address: *pool_addr,
                        factory_name: fac.name.to_string(),
                        dex_kind: fac.dex_kind,
                        token0,
                        token1,
                        dec0,
                        dec1,
                        fee_num: 0,
                        fee_ppm,
                        extra_tax_ppm: 0,
                        stable: false,
                        r0_raw: U256::ZERO,
                        r1_raw: U256::ZERO,
                        sqrt_price_x96: sqrt_p,
                        liquidity: liq,
                        tick,
                        tick_spacing,
                        allow_multi_tick: false,
                        is_hub_0: false,
                        is_hub_1: false,
                        cached_depth_usd: 0.0,
                        last_updated_block: self.last_block,
                        v4_key: None,
                        metric: None,
                        balancer: None,
                    });
                }
                DexKind::InfinityCl | DexKind::UniV4 | DexKind::MetricOmm | DexKind::BalancerV2 => {}
            }
        }

        // Fetch exact dynamic Solidly factory fees (AeroV2 getFee(pool,stable) / ThenaV2 getFee(stable))
        if !solidly_fee_queries.is_empty() {
            let fee_calls: Vec<IMulticall3::Call3> = solidly_fee_queries
                .iter()
                .map(|(pool, fac, stable)| match fac.style {
                    FactoryStyle::SolidlyGetPair => IMulticall3::Call3 {
                        target: fac.address,
                        allowFailure: true,
                        callData: ISolidlyFactoryGetPair::getFeeCall { stable: *stable }
                            .abi_encode()
                            .into(),
                    },
                    _ => IMulticall3::Call3 {
                        target: fac.address,
                        allowFailure: true,
                        callData: ISolidlyFactoryGetPool::getFeeCall {
                            pool: *pool,
                            stable: *stable,
                        }
                        .abi_encode()
                        .into(),
                    },
                })
                .collect();
            if let Ok(fee_res) = rpc.multicall(fee_calls).await {
                for ((pool_addr, _, _), r) in solidly_fee_queries.into_iter().zip(fee_res.into_iter()) {
                    if r.success && r.returnData.len() >= 32 {
                        let bps = U256::from_be_slice(&r.returnData[0..32]).saturating_to::<u32>();
                        if bps <= 200 {
                            if let Some(p) = self.pools.get_mut(&pool_addr) {
                                p.fee_ppm = bps * 100;
                                p.fee_num = (10_000u32 - bps) as u16;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn insert_pool(&mut self, mut pool: PoolState) {
        let addr = pool.address;
        let t0 = pool.token0;
        let t1 = pool.token1;
        let dex_kind = pool.dex_kind;
        let fee_ppm = pool.fee_ppm;
        let p0 = self.token_usd(t0);
        let p1 = self.token_usd(t1);
        pool.is_hub_0 = self.hub_set.contains(&t0);
        pool.is_hub_1 = self.hub_set.contains(&t1);
        let tax0 = self.token_tax_ppm.get(&t0).copied().unwrap_or(0);
        let tax1 = self.token_tax_ppm.get(&t1).copied().unwrap_or(0);
        pool.extra_tax_ppm = pool.extra_tax_ppm.max(tax0.saturating_add(tax1).min(999_999));
        let is_hub_pair = pool.is_hub_0 && pool.is_hub_1;
        if matches!(pool.dex_kind, DexKind::UniV3 | DexKind::PancakeV3)
            && !pool.factory_name.starts_with("Generic")
        {
            pool.allow_multi_tick = true;
        }
        pool.refresh_depth(p0, p1);
        let is_new = !self.pools.contains_key(&addr);
        self.pools.insert(addr, pool);
        if is_new {
            self.token_pools.entry(t0).or_default().push(addr);
            self.token_pools.entry(t1).or_default().push(addr);
            if is_hub_pair {
                self.hub_pair_pools.push(addr);
            }

            if fee_ppm <= 500 {
                let flash_mode = match dex_kind {
                    DexKind::UniV3 => Some(2u8),
                    DexKind::PancakeV3 => Some(3u8),
                    _ => None,
                };
                if let Some(fm) = flash_mode {
                    let anchors = self.base_anchors();
                    for base_token in [t0, t1] {
                        if anchors.contains(&base_token)
                            && !self
                                .flash_pools
                                .iter()
                                .any(|(c, _)| c.pool == addr && c.base_token == base_token)
                        {
                            self.flash_pools.push((
                                FlashPoolCandidate {
                                    flash_mode: fm,
                                    pool: addr,
                                    base_token,
                                    fee_bps: (fee_ppm as f64) / 100.0,
                                },
                                U256::ZERO,
                            ));
                        }
                    }
                }
            }
        }
    }

    /// Infer live USD prices using strictly `known_side_depth_usd` so extreme ticks or unverified
    /// paired assets cannot spoof virtual depth, pin stablecoins to 1.0, and clamp hub token prices.
    pub fn refresh_token_usd_prices_passes(&mut self, passes: usize) {
        if passes >= 2 {
            let hubs: HashSet<Address> =
                self.chain.hub_tokens.iter().map(|s| s.address).collect();
            for (&addr, meta) in &mut self.tokens {
                if !hubs.contains(&addr) {
                    meta.approx_usd = 0.0;
                }
            }
        }
        let seed_prices: Vec<(Address, f64)> = self
            .chain
            .hub_tokens
            .iter()
            .map(|seed| {
                let px = if self.is_pinned_stable(seed.address) {
                    1.0
                } else {
                    seed.approx_usd
                };
                (seed.address, px)
            })
            .collect();
        for (addr, px) in seed_prices {
            if let Some(meta) = self.tokens.get_mut(&addr) {
                meta.approx_usd = px;
            }
        }

        // Price non-stable hub tokens (WBNB, BTCB, ETH, CAKE, etc.) ONLY from direct official pools vs stablecoins
        let mut best_hub: HashMap<Address, (f64, f64)> = HashMap::new();
        for pool in self.pools.values() {
            if !pool.is_active() || pool.factory_name.starts_with("Generic") {
                continue;
            }
            let candidates = [(pool.token0, pool.token1), (pool.token1, pool.token0)];
            for (s_tk, h_tk) in candidates {
                if !self.is_pinned_stable(s_tk) || self.is_pinned_stable(h_tk) {
                    continue;
                }
                let Some(seed_px) = self.hub_seed_usd(h_tk) else {
                    continue;
                };
                if seed_px <= 0.0 {
                    continue;
                }
                let est_px = pool.marginal_rate_human(h_tk);
                let depth = pool.known_side_depth_usd(s_tk, 1.0);
                if depth >= 5_000.0
                    && est_px.is_finite()
                    && est_px >= seed_px * 0.25
                    && est_px <= seed_px * 4.0
                {
                    best_hub
                        .entry(h_tk)
                        .and_modify(|e| {
                            if depth > e.1 {
                                *e = (est_px, depth);
                            }
                        })
                        .or_insert((est_px, depth));
                }
            }
        }
        for (h_tk, (px, _)) in best_hub {
            if let Some(meta) = self.tokens.get_mut(&h_tk) {
                meta.approx_usd = px;
            }
        }
        let wnat = self.token_usd(self.chain.wrapped_native);
        if wnat > 10.0 {
            self.native_usd = wnat;
        }

        // Price non-hub tokens strictly from known-side depth
        for pass_idx in 0..passes.max(1) {
            let mut best: HashMap<Address, (f64, f64)> = HashMap::new();
            for pool in self.pools.values() {
                if !pool.is_active() {
                    continue;
                }
                if self.is_hub_token(pool.token0) && self.is_hub_token(pool.token1) {
                    continue;
                }
                let dirs = [(pool.token0, pool.token1), (pool.token1, pool.token0)];
                for (known_tk, target_tk) in dirs {
                    if self.is_hub_token(target_tk) {
                        continue;
                    }
                    if pass_idx == 0 && !self.is_hub_token(known_tk) {
                        continue;
                    }
                    let known_px = self.token_usd(known_tk);
                    if known_px <= 0.0 {
                        continue;
                    }
                    let est_px = pool.marginal_rate_human(target_tk) * known_px;
                    let depth = pool.known_side_depth_usd(known_tk, known_px);
                    if est_px.is_finite() && est_px > 0.0 && depth >= 40.0 {
                        best.entry(target_tk)
                            .and_modify(|e| {
                                if depth > e.1 {
                                    *e = (est_px, depth);
                                }
                            })
                            .or_insert((est_px, depth));
                    }
                }
            }
            for (tk, (px, _)) in best {
                if let Some(meta) = self.tokens.get_mut(&tk) {
                    meta.approx_usd = px;
                }
            }
        }

        // Official V3 pools use bounded multi-tick depth via `insert_pool`, while
        // `known_side_depth_usd` stays strictly single-tick to guard USD pricing.

        // Refresh cached usable depth (full sweep when passes >= 2, hot-path dirty window when passes == 1)
        let recent_cutoff = self.last_block.saturating_sub(8);
        let depth_updates: Vec<(Address, f64, f64)> = self
            .pools
            .values()
            .filter(|p| {
                passes >= 2 || p.last_updated_block >= recent_cutoff || p.cached_depth_usd == 0.0
            })
            .map(|p| (p.address, self.token_usd(p.token0), self.token_usd(p.token1)))
            .collect();
        for (addr, p0, p1) in depth_updates {
            if let Some(p) = self.pools.get_mut(&addr) {
                p.refresh_depth(p0, p1);
            }
        }
    }

    pub fn select_flash_source(&self, base_token: Address, borrow_raw: U256, used_pools: &[Address]) -> Option<&FlashPoolCandidate> {
        let mut best: Option<&FlashPoolCandidate> = None;
        for (cand, bal) in &self.flash_pools {
            if cand.base_token != base_token {
                continue;
            }
            if cand.flash_mode >= 2 && used_pools.contains(&cand.pool) {
                continue; // Never reenter V3 flash pool inside steps!
            }
            if cand.flash_mode == 7
            && used_pools.iter().any(|pa| {
                self.pools.get(pa).is_some_and(|p| {
                    p.dex_kind == DexKind::BalancerV2
                        && p.balancer.as_ref().is_some_and(|b| b.vault == cand.pool)
                })
            })
        {
            continue;
        }
        if *bal < borrow_raw {
                continue;
            }
            if best.map_or(true, |b| cand.fee_bps < b.fee_bps) {
                best = Some(cand);
            }
        }
        best
    }

    pub fn base_anchors(&self) -> Vec<Address> {
        let mut out: Vec<Address> = self
            .chain
            .hub_tokens
            .iter()
            .filter(|t| t.is_base_anchor)
            .map(|t| t.address)
            .collect();
        if let Some(pos) = out.iter().position(|&a| a == self.chain.wrapped_native) {
            if pos > 0 {
                let w = out.remove(pos);
                out.insert(0, w);
            }
        }
        out
    }

    fn refresh_dirty_pools(&mut self, dirty_pools: &HashSet<Address>) {
        if dirty_pools.is_empty() {
            return;
        }
        let mut updates: Vec<(Address, f64, f64)> = Vec::new();
        for &addr in dirty_pools {
            let Some(pool) = self.pools.get(&addr) else {
                continue;
            };
            if !pool.is_active() {
                continue;
            }
            let p0 = self.token_usd(pool.token0);
            let p1 = self.token_usd(pool.token1);
            if p0 > 0.0 && p1 == 0.0 && !self.is_hub_token(pool.token1) {
                let est_p1 = pool.marginal_rate_human(pool.token1) * p0;
                let depth = pool.known_side_depth_usd(pool.token0, p0);
                if est_p1.is_finite() && est_p1 > 0.0 && depth >= 40.0 {
                    updates.push((pool.token1, est_p1, depth));
                }
            } else if p1 > 0.0 && p0 == 0.0 && !self.is_hub_token(pool.token0) {
                let est_p0 = pool.marginal_rate_human(pool.token0) * p1;
                let depth = pool.known_side_depth_usd(pool.token1, p1);
                if est_p0.is_finite() && est_p0 > 0.0 && depth >= 40.0 {
                    updates.push((pool.token0, est_p0, depth));
                }
            }
        }
        for (tk, px, _) in updates {
            if !self.is_hub_token(tk) {
                if let Some(meta) = self.tokens.get_mut(&tk) {
                    if meta.approx_usd == 0.0 {
                        meta.approx_usd = px;
                    }
                }
            }
        }
        for &addr in dirty_pools {
            let Some((t0, t1)) = self.pools.get(&addr).map(|p| (p.token0, p.token1)) else {
                continue;
            };
            let p0 = self.token_usd(t0);
            let p1 = self.token_usd(t1);
            if let Some(p) = self.pools.get_mut(&addr) {
                p.refresh_depth(p0, p1);
            }
        }
    }

    fn sync_token_extra_tax(&mut self, tk: Address) {
        let Some(p_list) = self.token_pools.get(&tk).cloned() else {
            return;
        };
        for pa in p_list {
            let Some((t0, t1)) = self.pools.get(&pa).map(|p| (p.token0, p.token1)) else {
                continue;
            };
            let tax0 = self.token_tax_ppm.get(&t0).copied().unwrap_or(0);
            let tax1 = self.token_tax_ppm.get(&t1).copied().unwrap_or(0);
            if let Some(p) = self.pools.get_mut(&pa) {
                p.extra_tax_ppm = tax0.saturating_add(tax1).min(999_999);
            }
        }
    }

    pub fn record_sim_success(&mut self, used_pools: &[Address], non_hub_tokens: &[Address]) {
        for &pa in used_pools {
            self.sim_verified_pools.insert(pa);
            self.pool_cooldown_until.remove(&pa);
            self.sim_fail_counts.remove(&pa);
        }
        for &tk in non_hub_tokens {
            self.sim_verified_tokens.insert(tk);
            self.token_cooldown_until.remove(&tk);
            self.token_fail_strikes.remove(&tk);
            if let Some(cur) = self.token_tax_ppm.get_mut(&tk) {
                *cur = (*cur * 9) / 10;
            }
            self.sync_token_extra_tax(tk);
        }
    }

    fn quarantine_pool(&mut self, addr: Address, reason: &str) -> bool {
        let Some(p) = self.pools.remove(&addr) else {
            return false;
        };
        tracing::info!(pool = %addr, dex = %p.factory_name, reason, "quarantining failing pool");
        self.ignored_pools.insert(addr);
        self.sim_fail_counts.remove(&addr);
        true
    }

    fn strike_token_quarantine(&mut self, tk: Address, limit: u8, reason: &str) -> bool {
        if self.is_hub_token(tk) {
            return false;
        }
        self.token_cooldown_until.insert(tk, self.last_block + 120);
        let should_ban = {
            let entry = self.token_fail_strikes.entry(tk).or_insert((0, 0));
            if entry.0 == 0 || self.last_block > entry.1 + 20 {
                entry.0 += 1;
                entry.1 = self.last_block;
            }
            entry.0 >= limit
        };
        if !should_ban {
            return false;
        }
        let mut any_rm = false;
        if let Some(p_list) = self.token_pools.get(&tk).cloned() {
            let sym = self.token_symbol(tk);
            for pa in p_list {
                let remove_pool = self.pools.get(&pa).is_some_and(|p| {
                    !(self.is_hub_token(p.token0) && self.is_hub_token(p.token1))
                });
                if remove_pool {
                    self.pools.remove(&pa);
                    self.ignored_pools.insert(pa);
                    self.sim_fail_counts.remove(&pa);
                    any_rm = true;
                }
            }
            if any_rm {
                tracing::info!(token = %sym, reason, "quarantining persistent failing non-hub token pools");
            }
        }
        self.token_fail_strikes.remove(&tk);
        any_rm
    }

    pub fn record_sim_diagnostic(
        &mut self,
        pool_addrs: &[Address],
        non_hub_tokens: &[Address],
        diag: &crate::simulator::SimDiagnostic,
    ) {
        use crate::simulator::SimDiagnostic;

        let mut quarantined_any = false;
        let mut taxes_dirty = false;
        let mut generic_addrs: Vec<Address> = pool_addrs
            .iter()
            .copied()
            .filter(|a| {
                self.pools
                    .get(a)
                    .is_some_and(|p| p.factory_name.starts_with("Generic"))
            })
            .collect();
        generic_addrs.sort();
        generic_addrs.dedup();

        match diag {
            SimDiagnostic::ProfitBelowMin { .. } => {}
            SimDiagnostic::FlashShortfall {
                final_raw,
                shortfall_ppm,
                tax_token,
                tax_ppm,
                ..
            } => {
                if *tax_token != Address::ZERO && *tax_ppm >= 300 && !self.is_hub_token(*tax_token)
                {
                    let prev = self.token_tax_ppm.get(tax_token).copied().unwrap_or(0);
                    let new_tax = prev.max(tax_ppm.saturating_add(500).min(999_999));
                    self.token_tax_ppm.insert(*tax_token, new_tax);
                    self.sync_token_extra_tax(*tax_token);
                    taxes_dirty = true;
                    if new_tax >= 250_000 {
                        quarantined_any |= self.strike_token_quarantine(
                            *tax_token,
                            1,
                            "extreme_transfer_tax",
                        );
                        self.token_cooldown_until
                            .insert(*tax_token, self.last_block + 100_000);
                    } else {
                        let cd = if new_tax >= 200_000 { 400 } else { 4 };
                        self.token_cooldown_until
                            .insert(*tax_token, self.last_block + cd);
                    }
                    if prev == 0 || new_tax >= prev + 1_000 {
                        tracing::info!(
                            token = %self.token_symbol(*tax_token),
                            prev_ppm = prev,
                            new_ppm = new_tax,
                            "1-shot calibrated token extra_tax_ppm"
                        );
                    }
                } else {
                    let mut demoted_v3 = false;
                    if *tax_ppm < 300 && *shortfall_ppm >= 150_000 {
                        if let Some((thin_v3, _, t0, t1)) = pool_addrs
                            .iter()
                            .filter_map(|a| {
                                self.pools.get(a).and_then(|p| {
                                    if matches!(p.dex_kind, DexKind::UniV3 | DexKind::PancakeV3)
                                        && p.allow_multi_tick
                                    {
                                        Some((*a, p.cached_depth_usd, p.token0, p.token1))
                                    } else {
                                        None
                                    }
                                })
                            })
                            .min_by(|a, b| {
                                a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                            })
                        {
                            let p0 = self.token_usd(t0);
                            let p1 = self.token_usd(t1);
                            if let Some(p) = self.pools.get_mut(&thin_v3) {
                                p.allow_multi_tick = false;
                                p.refresh_depth(p0, p1);
                            }
                            tracing::info!(
                                pool = %thin_v3,
                                shortfall_ppm,
                                "demoted V3 pool allow_multi_tick=false after non-tax FlashShortfall"
                            );
                            demoted_v3 = true;
                        }
                    }
                    if !demoted_v3 && final_raw.is_zero() {
                        if generic_addrs.len() == 1 {
                            quarantined_any |=
                                self.quarantine_pool(generic_addrs[0], "flash_zero_balance");
                        } else if non_hub_tokens.len() == 1 {
                            quarantined_any |= self.strike_token_quarantine(
                                non_hub_tokens[0],
                                2,
                                "flash_zero_balance",
                            );
                        } else if let Some(thin_pool) = pool_addrs
                            .iter()
                            .filter_map(|a| self.pools.get(a).map(|p| (*a, p.cached_depth_usd)))
                            .min_by(|a, b| {
                                a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
                            })
                            .map(|(a, _)| a)
                        {
                            let until = self.last_block + 300;
                            self.pool_cooldown_until.insert(thin_pool, until);
                            tracing::info!(
                                pool = %thin_pool,
                                until,
                                "cooling down pool after zero-balance hub FlashShortfall"
                            );
                        }
                    }
                }
            }
            SimDiagnostic::StepZeroReceived { pool, token, .. }
            | SimDiagnostic::StepTransferFailed { pool, token, .. } => {
                let is_gen = self
                    .pools
                    .get(pool)
                    .is_some_and(|p| p.factory_name.starts_with("Generic"));
                if is_gen {
                    quarantined_any |= self.quarantine_pool(*pool, "generic_step_transfer_or_zero");
                } else if !self.is_hub_token(*token) {
                    quarantined_any |=
                        self.strike_token_quarantine(*token, 2, "step_transfer_or_zero");
                }
            }
            SimDiagnostic::StepSwapReverted { pool, reason, .. } => {
                let pool_tokens = self.pools.get(pool).map(|p| (p.token0, p.token1));
                if let Some((is_gen, is_metric, is_hub_pair, was_multi, t0, t1)) =
                    self.pools.get(pool).map(|p| {
                        (
                            p.factory_name.starts_with("Generic"),
                            p.dex_kind == DexKind::MetricOmm,
                            self.is_hub_token(p.token0) && self.is_hub_token(p.token1),
                            p.allow_multi_tick,
                            p.token0,
                            p.token1,
                        )
                    })
                {
                    if was_multi && !is_gen {
                        let p0 = self.token_usd(t0);
                        let p1 = self.token_usd(t1);
                        if let Some(p) = self.pools.get_mut(pool) {
                            p.allow_multi_tick = false;
                            p.refresh_depth(p0, p1);
                        }
                        tracing::info!(
                            pool = %pool,
                            "demoted V3 pool allow_multi_tick=false after StepSwapRevert"
                        );
                    } else if is_gen {
                        quarantined_any |= self.quarantine_pool(*pool, "generic_swap_revert");
                    } else if is_metric {
                        let oracle_stalled =
                            reason.contains("46a14930") || reason.contains("9a0423af");
                        if oracle_stalled {
                            if let Some(p) = self.pools.get_mut(pool) {
                                if let Some(ref mut m) = p.metric {
                                    m.paused = true;
                                }
                            }
                        }
                        let cd = if oracle_stalled { 20 } else { 40 };
                        let until = self.last_block + cd;
                        self.pool_cooldown_until.insert(*pool, until);
                        tracing::info!(pool = %pool, until, reason = %reason, "cooling down reverting MetricOmm pool");
                    } else if is_hub_pair {
                        let until = self.last_block + 300;
                        self.pool_cooldown_until.insert(*pool, until);
                        tracing::info!(pool = %pool, until, "cooling down reverting hub-pair pool");
                    } else {
                        let cnt = self.sim_fail_counts.entry(*pool).or_insert(0);
                        *cnt += 1;
                        if *cnt >= 2 {
                            quarantined_any |= self.quarantine_pool(*pool, "non_hub_swap_revert");
                        } else {
                            self.pool_cooldown_until.insert(*pool, self.last_block + 40);
                        }
                    }
                }
                if let Some((t0, t1)) = pool_tokens {
                    let upper = reason.to_ascii_uppercase();
                    if upper.contains("TRANSFER_FAILED") || upper.contains("TRANSFERHELPER") {
                        for tk in [t0, t1] {
                            if !self.is_hub_token(tk) {
                                quarantined_any |= self.strike_token_quarantine(
                                    tk,
                                    2,
                                    "swap_transfer_failed",
                                );
                            }
                        }
                    }
                }
            }
            SimDiagnostic::GasOrTimeout { .. } => {
                if generic_addrs.len() == 1 {
                    quarantined_any |= self.quarantine_pool(generic_addrs[0], "generic_gas_or_timeout");
                } else if non_hub_tokens.len() == 1 {
                    quarantined_any |=
                        self.strike_token_quarantine(non_hub_tokens[0], 2, "gas_or_timeout");
                }
            }
            SimDiagnostic::OtherRevert { reason } => {
                if !generic_addrs.is_empty() && non_hub_tokens.len() <= 1 {
                    for ga in generic_addrs {
                        quarantined_any |= self.quarantine_pool(ga, "generic_other_revert");
                    }
                } else if generic_addrs.len() == 1 {
                    let ga = generic_addrs[0];
                    let cnt = self.sim_fail_counts.entry(ga).or_insert(0);
                    *cnt += 1;
                    if *cnt >= 2 {
                        quarantined_any |= self.quarantine_pool(ga, "generic_multi_dag_revert");
                    } else {
                        self.pool_cooldown_until.insert(ga, self.last_block + 60);
                    }
                }
                let r_up = reason.to_ascii_uppercase();
                let is_tf = r_up.contains("TRANSFER_FAIL")
                    || r_up.contains("TRANSFERHELPER")
                    || r_up == "TF"
                    || r_up == "STF";
                if is_tf {
                    let unverified_nh: Vec<Address> = non_hub_tokens
                        .iter()
                        .copied()
                        .filter(|t| !self.sim_verified_tokens.contains(t))
                        .collect();
                    if unverified_nh.len() == 1 {
                        quarantined_any |= self.strike_token_quarantine(
                            unverified_nh[0],
                            2,
                            "other_transfer_fail",
                        );
                    } else {
                        for tk in unverified_nh {
                            self.token_cooldown_until.insert(tk, self.last_block + 200);
                        }
                    }
                }
            }
        }

        if taxes_dirty {
            self.save_token_taxes();
        }
        if quarantined_any {
            self.save_quarantine();
        }
    }

    /// Re-synchronize reserves and CL state in-place for hub-pair pools, recently traded non-hub pools,
    /// and a deterministic rotating slice of quiet 2-core arbitrageable pools to keep passive pairs fresh.
    pub async fn resync_active_pools(&mut self, rpc: &RpcPool) -> usize {
        let mut seen: HashSet<Address> = HashSet::with_capacity(300);
        let mut targets: Vec<Address> = Vec::with_capacity(300);

        for &pa in &self.hub_pair_pools {
            if seen.insert(pa) {
                targets.push(pa);
            }
        }

        let mut recent_non_hub: Vec<(u64, Address)> = self
            .pools
            .values()
            .filter(|p| {
                !(p.is_hub_0 && p.is_hub_1)
                    && self.last_block <= p.last_updated_block.saturating_add(600)
                    && !self.ignored_pools.contains(&p.address)
            })
            .map(|p| (p.last_updated_block, p.address))
            .collect();
        recent_non_hub.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, pa) in recent_non_hub.into_iter().take(90) {
            if seen.insert(pa) {
                targets.push(pa);
            }
        }

        let mut arb_non_hub: Vec<Address> = self
            .pools
            .values()
            .filter(|p| {
                !(p.is_hub_0 && p.is_hub_1)
                    && !seen.contains(&p.address)
                    && self.is_pool_arbitrageable(p)
            })
            .map(|p| p.address)
            .collect();
        let mut refreshed_quiet = false;
        if !arb_non_hub.is_empty() {
            arb_non_hub.sort();
            let n = arb_non_hub.len();
            let start = self.resync_cursor % n;
            let take = n.min(140);
            for k in 0..take {
                let pa = arb_non_hub[(start + k) % n];
                if seen.insert(pa) {
                    targets.push(pa);
                    refreshed_quiet = true;
                }
            }
            self.resync_cursor = (start + take) % n;
        }
        targets.truncate(300);
        if targets.is_empty() {
            return 0;
        }

        let mut calls: Vec<IMulticall3::Call3> = Vec::with_capacity(targets.len() * 2);
        let mut meta: Vec<(Address, DexKind, usize, usize)> = Vec::with_capacity(targets.len());
        let mut bal_pool_ids: Vec<B256> = Vec::new();
        for addr in targets {
            let Some(p) = self.pools.get(&addr) else {
                continue;
            };
            let start = calls.len();
            match p.dex_kind {
                DexKind::UniV2 => calls.push(IMulticall3::Call3 {
                    target: addr,
                    allowFailure: true,
                    callData: IUniV2Pair::getReservesCall {}.abi_encode().into(),
                }),
                DexKind::Solidly => calls.push(IMulticall3::Call3 {
                    target: addr,
                    allowFailure: true,
                    callData: ISolidlyPair::getReservesCall {}.abi_encode().into(),
                }),
                DexKind::UniV3 | DexKind::PancakeV3 => {
                    calls.push(IMulticall3::Call3 {
                        target: addr,
                        allowFailure: true,
                        callData: IV3Pool::slot0Call {}.abi_encode().into(),
                    });
                    calls.push(IMulticall3::Call3 {
                        target: addr,
                        allowFailure: true,
                        callData: IV3Pool::liquidityCall {}.abi_encode().into(),
                    });
                }
                DexKind::InfinityCl => {
                    if let Some(ref k) = p.v4_key {
                        calls.push(IMulticall3::Call3 {
                            target: k.manager,
                            allowFailure: true,
                            callData: IInfinityCLPoolManager::getSlot0Call { id: k.pool_id }
                                .abi_encode()
                                .into(),
                        });
                        calls.push(IMulticall3::Call3 {
                            target: k.manager,
                            allowFailure: true,
                            callData: IInfinityCLPoolManager::getLiquidityCall { id: k.pool_id }
                                .abi_encode()
                                .into(),
                        });
                    }
                }
                DexKind::UniV4 => {
                    if let Some(ref k) = p.v4_key {
                        let (s0_key, liq_key) = univ4_slot0_and_liq_keys(k.pool_id);
                        calls.push(IMulticall3::Call3 {
                            target: k.manager,
                            allowFailure: true,
                            callData: IUniV4PoolManager::extsloadCall { slot: s0_key }
                                .abi_encode()
                                .into(),
                        });
                        calls.push(IMulticall3::Call3 {
                            target: k.manager,
                            allowFailure: true,
                            callData: IUniV4PoolManager::extsloadCall { slot: liq_key }
                                .abi_encode()
                                .into(),
                        });
                    }
                }
                DexKind::MetricOmm => push_metric_state_calls(&mut calls, addr),
                DexKind::BalancerV2 => {
                    if let Some(ref bal) = p.balancer {
                        if !bal_pool_ids.contains(&bal.pool_id) {
                            bal_pool_ids.push(bal.pool_id);
                        }
                    }
                }
            }
            if calls.len() > start {
                meta.push((addr, p.dex_kind, start, calls.len()));
            }
        }
        if calls.is_empty() && bal_pool_ids.is_empty() {
            return 0;
        }
        let res = if calls.is_empty() {
            Vec::new()
        } else {
            let Ok(r) = rpc.multicall(calls).await else {
                return 0;
            };
            r
        };

        let mut updated = 0usize;
        let mut depth_refresh: Vec<(Address, f64, f64)> = Vec::with_capacity(meta.len());
        for (addr, kind, s, e) in meta {
            if e > res.len() {
                continue;
            }
            let slice = &res[s..e];
            match kind {
                DexKind::UniV2 | DexKind::Solidly => {
                    if slice[0].success && slice[0].returnData.len() >= 64 {
                        let r0 = U256::from_be_slice(&slice[0].returnData[0..32]);
                        let r1 = U256::from_be_slice(&slice[0].returnData[32..64]);
                        let (t0, t1) = if let Some(p) = self.pools.get_mut(&addr) {
                            p.r0_raw = r0;
                            p.r1_raw = r1;
                            (p.token0, p.token1)
                        } else {
                            continue;
                        };
                        depth_refresh.push((addr, self.token_usd(t0), self.token_usd(t1)));
                        updated += 1;
                    }
                }
                DexKind::UniV3 | DexKind::PancakeV3 | DexKind::InfinityCl | DexKind::UniV4 => {
                    if slice.len() < 2
                        || !slice[0].success
                        || !slice[1].success
                        || slice[1].returnData.len() < 32
                    {
                        continue;
                    }
                    let (sqrt_p, tick) = if kind == DexKind::UniV4 {
                        if slice[0].returnData.len() < 32 {
                            continue;
                        }
                        decode_univ4_slot0(&slice[0].returnData[0..32])
                    } else {
                        if slice[0].returnData.len() < 64 {
                            continue;
                        }
                        (
                            U256::from_be_slice(&slice[0].returnData[0..32]),
                            decode_i24_word(&slice[0].returnData[32..64]),
                        )
                    };
                    let liq = U256::from_be_slice(&slice[1].returnData[0..32]).saturating_to::<u128>();
                    let (t0, t1) = if let Some(p) = self.pools.get_mut(&addr) {
                        p.sqrt_price_x96 = sqrt_p;
                        p.liquidity = liq;
                        p.tick = tick;
                        (p.token0, p.token1)
                    } else {
                        continue;
                    };
                    depth_refresh.push((addr, self.token_usd(t0), self.token_usd(t1)));
                    updated += 1;
                }
                DexKind::MetricOmm => {
                    if let Some((sell, buy, r0_raw, r1_raw, active_bin, paused)) =
                        decode_metric_state_slice(slice)
                    {
                        if !paused {
                            self.pool_cooldown_until.remove(&addr);
                        }
                        let (t0, t1) = if let Some(p) = self.pools.get_mut(&addr) {
                            if let Some(ref mut m) = p.metric {
                                m.sell_price_x64 = sell;
                                m.buy_price_x64 = buy;
                                m.active_bin = active_bin;
                                m.paused = paused;
                            } else {
                                p.metric = Some(MetricPoolState {
                                    sell_price_x64: sell,
                                    buy_price_x64: buy,
                                    num_bins: 40,
                                    active_bin,
                                    paused,
                                });
                            }
                            p.r0_raw = r0_raw;
                            p.r1_raw = r1_raw;
                            p.last_updated_block = self.last_block;
                            (p.token0, p.token1)
                        } else {
                            continue;
                        };
                        depth_refresh.push((addr, self.token_usd(t0), self.token_usd(t1)));
                        updated += 1;
                    } else if !slice.is_empty() && !slice[0].success {
                        if let Some(p) = self.pools.get_mut(&addr) {
                            if let Some(ref mut m) = p.metric {
                                m.paused = true;
                            }
                        }
                    }
                }
                DexKind::BalancerV2 => {}
            }
        }

        for (addr, p0, p1) in depth_refresh {
            if let Some(p) = self.pools.get_mut(&addr) {
                p.refresh_depth(p0, p1);
            }
        }
        if !bal_pool_ids.is_empty() {
            updated += self
                .discover_balancer_v2_pools(rpc, &bal_pool_ids)
                .await
                .unwrap_or(0);
        }
        if updated > 0 {
            self.refresh_token_usd_prices_passes(if refreshed_quiet { 2 } else { 1 });
        }
        updated
    }

    pub fn estimate_gas_cost_usd(&self, gas_used: u64, l1_fee_wei: u128) -> f64 {
        let l2_wei = (gas_used as u128) * self.gas_price_wei;
        let total_eth = ((l2_wei + l1_fee_wei) as f64) / 1e18;
        total_eth * self.native_usd
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedPoolMeta {
    address: Address,
    factory_name: String,
    dex_kind: DexKind,
    token0: Address,
    token1: Address,
    dec0: u8,
    dec1: u8,
    fee_num: u16,
    fee_ppm: u32,
    stable: bool,
    tick_spacing: i32,
    #[serde(default)]
    v4_key: Option<V4PoolKey>,
    #[serde(default)]
    metric: Option<MetricPoolState>,
    #[serde(default)]
    balancer: Option<BalancerPoolState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PoolsCacheFile {
    tokens: Vec<(Address, String, u8)>,
    pools: Vec<CachedPoolMeta>,
}

#[derive(Debug, Clone, Copy)]
enum DexHint {
    V2,
    Solidly,
    UniV3,
    PancakeV3,
    MetricOmm,
}

fn decode_i24_word(word: &[u8]) -> i32 {
    if word.len() < 32 {
        return 0;
    }
    let val = I256::from_raw(U256::from_be_slice(&word[..32]));
    val.as_i32()
}

fn push_metric_state_calls(calls: &mut Vec<IMulticall3::Call3>, pool: Address) {
    calls.push(IMulticall3::Call3 {
        target: pool,
        allowFailure: true,
        callData: IMetricOmmPool::getSellAndBuyPricesCall {}.abi_encode().into(),
    });
    calls.push(IMulticall3::Call3 {
        target: pool,
        allowFailure: true,
        callData: IMetricOmmPool::extsloadCall { slot: B256::ZERO }
            .abi_encode()
            .into(),
    });
    calls.push(IMulticall3::Call3 {
        target: pool,
        allowFailure: true,
        callData: IMetricOmmPool::extsloadCall {
            slot: B256::from(U256::from(1u8).to_be_bytes::<32>()),
        }
        .abi_encode()
        .into(),
    });
}

fn decode_metric_state_slice(
    slice: &[IMulticall3::Result],
) -> Option<(u128, u128, U256, U256, i16, bool)> {
    if slice.len() < 3
        || !slice[0].success
        || !slice[1].success
        || !slice[2].success
        || slice[1].returnData.len() < 32
        || slice[2].returnData.len() < 32
    {
        return None;
    }
    let prices =
        IMetricOmmPool::getSellAndBuyPricesCall::abi_decode_returns(&slice[0].returnData).ok()?;
    if prices.sellPriceX64 == 0 || prices.buyPriceX64 < prices.sellPriceX64 {
        return None;
    }
    let s0 = &slice[1].returnData[..32];
    let paused = s0[31] != 0;
    let active_bin = i16::from_be_bytes([s0[29], s0[30]]);
    let s1 = U256::from_be_slice(&slice[2].returnData[..32]);
    let mask128 = (U256::from(1u8) << 128) - U256::from(1u8);
    let r0_raw = s1 & mask128;
    let r1_raw = s1 >> 128u8;
    Some((
        prices.sellPriceX64,
        prices.buyPriceX64,
        r0_raw,
        r1_raw,
        active_bin,
        paused,
    ))
}

fn decode_metric_slots_only(slice: &[IMulticall3::Result]) -> Option<(U256, U256, i16, bool)> {
    if slice.len() < 3
        || !slice[1].success
        || !slice[2].success
        || slice[1].returnData.len() < 32
        || slice[2].returnData.len() < 32
    {
        return None;
    }
    let s0 = &slice[1].returnData[..32];
    let paused = s0[31] != 0;
    let active_bin = i16::from_be_bytes([s0[29], s0[30]]);
    let s1 = U256::from_be_slice(&slice[2].returnData[..32]);
    let mask128 = (U256::from(1u8) << 128) - U256::from(1u8);
    let r0_raw = s1 & mask128;
    let r1_raw = s1 >> 128u8;
    Some((r0_raw, r1_raw, active_bin, paused))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::load_chain_config;
    use crate::simulator::SimDiagnostic;

    #[test]
    fn test_v3_mint_burn_topics() {
        assert_eq!(
            TOPIC_V3_MINT,
            alloy::primitives::keccak256("Mint(address,address,int24,int24,uint128,uint256,uint256)")
        );
        assert_eq!(
            TOPIC_V3_BURN,
            alloy::primitives::keccak256("Burn(address,int24,int24,uint128,uint256,uint256)")
        );
        assert_eq!(
            TOPIC_UNIV4_SWAP,
            alloy::primitives::keccak256("Swap(bytes32,address,int128,int128,uint160,uint128,int24,uint24)")
        );
        assert_eq!(
            TOPIC_METRIC_PRICE_READ,
            alloy::primitives::keccak256("PriceRead(address,bytes32)")
        );
    }

    #[test]
    fn test_extreme_tax_quarantine_and_swap_transfer_strike() {
        // Use ChainKind::Base and preserve any existing logs files around test
        let q_path = "logs/quarantine_base.json";
        let t_path = "logs/token_tax_base.json";
        let orig_q = std::fs::read(q_path).ok();
        let orig_t = std::fs::read(t_path).ok();

        let cfg = load_chain_config("base").unwrap();
        let usdc = cfg.usd_anchor;
        let safemoon_like = Address::repeat_byte(0xAA);
        let osk_like = Address::repeat_byte(0xBB);
        let pool_sm = Address::repeat_byte(0x11);
        let pool_osk_1 = Address::repeat_byte(0x22);
        let pool_osk_2 = Address::repeat_byte(0x33);

        let mut market = MarketState::new(cfg);
        market.last_block = 1_000;
        market.ignored_pools.clear();
        market.token_tax_ppm.clear();

        let mk_pool = |addr: Address, token1: Address| PoolState {
            address: addr,
            factory_name: "PancakeV2".to_string(),
            dex_kind: DexKind::UniV2,
            token0: usdc,
            token1,
            dec0: 6,
            dec1: 18,
            fee_num: 9975,
            fee_ppm: 2500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::from(100_000_000_000u64),
            r1_raw: U256::from(100_000_000_000_000_000_000_000u128),
            sqrt_price_x96: U256::ZERO,
            liquidity: 0,
            tick: 0,
            tick_spacing: 0,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: false,
            cached_depth_usd: 1000.0,
            last_updated_block: 1_000,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        market.insert_pool(mk_pool(pool_sm, safemoon_like));
        market.insert_pool(mk_pool(pool_osk_1, osk_like));
        market.insert_pool(mk_pool(pool_osk_2, osk_like));

        // 1. Extreme tax (>= 25%) 1-strike quarantines all non-hub pools & caps at 999_999
        market.record_sim_diagnostic(
            &[pool_sm],
            &[safemoon_like],
            &SimDiagnostic::FlashShortfall {
                repay_human: 100.0,
                final_human: 0.1,
                final_raw: U256::from(1u64),
                shortfall_ppm: 999_000,
                shortfall_usd: 99.9,
                tax_token: safemoon_like,
                tax_ppm: 999_999,
            },
        );
        assert_eq!(market.token_tax_ppm.get(&safemoon_like).copied(), Some(999_999));
        assert!(market.ignored_pools.contains(&pool_sm));
        assert!(!market.pools.contains_key(&pool_sm));
        assert_eq!(
            market.token_cooldown_until.get(&safemoon_like).copied(),
            Some(101_000)
        );

        // 2. StepSwapReverted with TRANSFER_FAILED cools down on strike 1 and quarantines on strike 2
        market.record_sim_diagnostic(
            &[pool_osk_1],
            &[osk_like],
            &SimDiagnostic::StepSwapReverted {
                step_idx: 1,
                pool: pool_osk_1,
                dex_kind: 0,
                reason: "Fstswap: TRANSFER_FAILED".to_string(),
            },
        );
        assert!(market.pools.contains_key(&pool_osk_1));
        assert!(market.token_cooldown_until.get(&osk_like).copied().unwrap_or(0) > 1_000);

        market.last_block = 1_030; // > 20 blocks later
        market.record_sim_diagnostic(
            &[pool_osk_2],
            &[osk_like],
            &SimDiagnostic::StepSwapReverted {
                step_idx: 1,
                pool: pool_osk_2,
                dex_kind: 0,
                reason: "Pancake: TRANSFER_FAILED".to_string(),
            },
        );
        assert!(market.ignored_pools.contains(&pool_osk_1));
        assert!(market.ignored_pools.contains(&pool_osk_2));
        assert!(!market.pools.contains_key(&pool_osk_1));
        assert!(!market.pools.contains_key(&pool_osk_2));

        // 3. FlashShortfall with tax_ppm=0 (pure V3 multi-tick slippage) must NOT pollute token_tax_ppm or extra_tax_ppm;
        //    whereas tax_ppm=30_000 still 1-shot calibrates token_tax_ppm.
        let doge_like = Address::repeat_byte(0xCC);
        let taxed_3pct = Address::repeat_byte(0xDD);
        let pool_doge = Address::repeat_byte(0x44);
        let pool_taxed = Address::repeat_byte(0x55);
        let mut gp = mk_pool(pool_doge, doge_like);
        gp.factory_name = "GenericV2".to_string();
        market.insert_pool(gp);
        market.insert_pool(mk_pool(pool_taxed, taxed_3pct));

        market.record_sim_diagnostic(
            &[pool_doge],
            &[doge_like],
            &SimDiagnostic::FlashShortfall {
                repay_human: 100.0,
                final_human: 98.5,
                final_raw: U256::from(98_500_000u64),
                shortfall_ppm: 15_000,
                shortfall_usd: 1.5,
                tax_token: Address::ZERO,
                tax_ppm: 0,
            },
        );
        assert_eq!(market.token_tax_ppm.get(&doge_like).copied(), None);
        assert_eq!(market.pools[&pool_doge].extra_tax_ppm, 0);

        market.record_sim_diagnostic(
            &[pool_taxed],
            &[taxed_3pct],
            &SimDiagnostic::FlashShortfall {
                repay_human: 100.0,
                final_human: 97.0,
                final_raw: U256::from(97_000_000u64),
                shortfall_ppm: 30_000,
                shortfall_usd: 3.0,
                tax_token: taxed_3pct,
                tax_ppm: 30_000,
            },
        );
        assert_eq!(market.token_tax_ppm.get(&taxed_3pct).copied(), Some(30_500));
        assert_eq!(market.pools[&pool_taxed].extra_tax_ppm, 30_500);

        // 4. Severe non-tax FlashShortfall (>=15% shortfall, tax_ppm=0) demotes thinnest V3 allow_multi_tick=false
        //    without poisoning token_tax_ppm, and cools down on repeat zero-balance hub shortfall.
        let deep_hub_v3 = Address::repeat_byte(0x61);
        let thin_hub_v3 = Address::repeat_byte(0x62);
        let q96 = U256::from(79228162514264337593543950336u128);
        let usdc_b = market.chain.usd_anchor;
        let weth_b = market.chain.wrapped_native;
        let mk_hub_v3 = |addr: Address, liq: u128, depth: f64| PoolState {
            address: addr,
            factory_name: "PancakeV3".to_string(),
            dex_kind: DexKind::PancakeV3,
            token0: usdc_b,
            token1: weth_b,
            dec0: 6,
            dec1: 18,
            fee_num: 0,
            fee_ppm: 500,
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::ZERO,
            r1_raw: U256::ZERO,
            sqrt_price_x96: q96,
            liquidity: liq,
            tick: 0,
            tick_spacing: 10,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: depth,
            last_updated_block: 1_030,
            v4_key: None,
            metric: None,
            balancer: None,
        };
        market.insert_pool(mk_hub_v3(deep_hub_v3, 1_000_000_000_000_000_000_000u128, 50_000.0));
        market.insert_pool(mk_hub_v3(thin_hub_v3, 10_000_000_000_000_000_000u128, 500.0));
        assert!(market.pools[&deep_hub_v3].allow_multi_tick);
        assert!(market.pools[&thin_hub_v3].allow_multi_tick);

        market.record_sim_diagnostic(
            &[deep_hub_v3, thin_hub_v3],
            &[],
            &SimDiagnostic::FlashShortfall {
                repay_human: 10.0,
                final_human: 0.0,
                final_raw: U256::ZERO,
                shortfall_ppm: 1_000_000,
                shortfall_usd: 10.0,
                tax_token: Address::ZERO,
                tax_ppm: 0,
            },
        );
        assert!(market.pools[&deep_hub_v3].allow_multi_tick);
        assert!(!market.pools[&thin_hub_v3].allow_multi_tick);
        assert!(market.token_tax_ppm.get(&usdc_b).is_none());
        assert!(market.token_tax_ppm.get(&weth_b).is_none());

        // Restore or clean up logs files
        match orig_q {
            Some(b) => { let _ = std::fs::write(q_path, b); }
            None => { let _ = std::fs::remove_file(q_path); }
        }
        match orig_t {
            Some(b) => { let _ = std::fs::write(t_path, b); }
            None => { let _ = std::fs::remove_file(t_path); }
        }
    }

    #[test]
    fn test_v3_hub_pool_revert_demotes_multitick_then_cools_down() {
        let q_path = "logs/quarantine_base.json";
        let t_path = "logs/token_tax_base.json";
        let orig_q = std::fs::read(q_path).ok();
        let orig_t = std::fs::read(t_path).ok();

        let cfg = load_chain_config("base").unwrap();
        let usdc = cfg.usd_anchor;
        let weth = cfg.wrapped_native;
        let mut market = MarketState::new(cfg);
        market.last_block = 5_000;
        market.ignored_pools.clear();

        let p_ts1 = Address::repeat_byte(0x41);
        let p_ts10 = Address::repeat_byte(0x42);
        let q96 = U256::from(79228162514264337593543950336u128);
        let mk_v3 = |addr: Address, ts: i32| PoolState {
            address: addr,
            factory_name: "PancakeV3".to_string(),
            dex_kind: DexKind::PancakeV3,
            token0: usdc,
            token1: weth,
            dec0: 6,
            dec1: 18,
            fee_num: 0,
            fee_ppm: if ts == 1 { 100 } else { 500 },
            extra_tax_ppm: 0,
            stable: false,
            r0_raw: U256::ZERO,
            r1_raw: U256::ZERO,
            sqrt_price_x96: q96,
            liquidity: 10_000_000_000_000_000_000_000u128,
            tick: 0,
            tick_spacing: ts,
            allow_multi_tick: false,
            is_hub_0: true,
            is_hub_1: true,
            cached_depth_usd: 0.0,
            last_updated_block: 5_000,
            v4_key: None,
            metric: None,
            balancer: None,
        };

        market.insert_pool(mk_v3(p_ts1, 1));
        market.insert_pool(mk_v3(p_ts10, 10));

        assert!(market.pools[&p_ts1].allow_multi_tick);
        assert!(market.pools[&p_ts10].allow_multi_tick);
        assert!(market.is_pool_routable(&market.pools[&p_ts1]));
        assert!(market.is_pool_routable(&market.pools[&p_ts10]));

        let diag_ts1 = SimDiagnostic::StepSwapReverted {
            step_idx: 1,
            pool: p_ts1,
            dex_kind: 3,
            reason: "empty(0x)".to_string(),
        };
        // 1st revert on ts=1 hub pool: demote allow_multi_tick=false, keep routable
        market.record_sim_diagnostic(&[p_ts1], &[], &diag_ts1);
        assert!(!market.pools[&p_ts1].allow_multi_tick);
        assert!(market.is_pool_routable(&market.pools[&p_ts1]));

        // 2nd revert on ts=1 hub pool: enters cooldown
        market.record_sim_diagnostic(&[p_ts1], &[], &diag_ts1);
        assert!(!market.is_pool_routable(&market.pools[&p_ts1]));

        let diag_ts10 = SimDiagnostic::StepSwapReverted {
            step_idx: 1,
            pool: p_ts10,
            dex_kind: 3,
            reason: "empty(0x)".to_string(),
        };
        // 1st revert on ts=10 hub pool: demote allow_multi_tick=false, keep routable
        market.record_sim_diagnostic(&[p_ts10], &[], &diag_ts10);
        assert!(!market.pools[&p_ts10].allow_multi_tick);
        assert!(market.is_pool_routable(&market.pools[&p_ts10]));

        // 2nd revert: hub-pair pool enters cooldown
        market.record_sim_diagnostic(&[p_ts10], &[], &diag_ts10);
        assert!(!market.is_pool_routable(&market.pools[&p_ts10]));

        match orig_q {
            Some(b) => { let _ = std::fs::write(q_path, b); }
            None => { let _ = std::fs::remove_file(q_path); }
        }
        match orig_t {
            Some(b) => { let _ = std::fs::write(t_path, b); }
            None => { let _ = std::fs::remove_file(t_path); }
        }
    }

    #[test]
    fn test_decode_metric_state_slice_extsload() {
        let sell: u128 = 549u128 << 64;
        let buy: u128 = 550u128 << 64;
        let ret_prices = IMetricOmmPool::getSellAndBuyPricesCall::abi_encode_returns(
            &IMetricOmmPool::getSellAndBuyPricesReturn {
                sellPriceX64: sell,
                buyPriceX64: buy,
            },
        );

        // On-chain pattern: slot0 with active_bin = -2 (0xfffe) and pause = 0
        let mut s0_neg2 = [0u8; 32];
        s0_neg2[29] = 0xff;
        s0_neg2[30] = 0xfe;
        s0_neg2[31] = 0x00;

        let r0_exp = U256::from(1_975_000_000_000_000_000u128);
        let r1_exp = U256::from(948_948_900_000_000_000_000u128);
        let s1_packed = (r1_exp << 128u8) | r0_exp;

        let slice = vec![
            IMulticall3::Result {
                success: true,
                returnData: ret_prices.clone().into(),
            },
            IMulticall3::Result {
                success: true,
                returnData: s0_neg2.to_vec().into(),
            },
            IMulticall3::Result {
                success: true,
                returnData: s1_packed.to_be_bytes::<32>().to_vec().into(),
            },
        ];
        let (ds, db, dr0, dr1, ab, paused) =
            decode_metric_state_slice(&slice).expect("decode neg2");
        assert_eq!(ds, sell);
        assert_eq!(db, buy);
        assert_eq!(dr0, r0_exp);
        assert_eq!(dr1, r1_exp);
        assert_eq!(ab, -2);
        assert!(!paused);

        // Positive active_bin = +4 and paused = true
        let mut s0_pos4_paused = [0u8; 32];
        s0_pos4_paused[29] = 0x00;
        s0_pos4_paused[30] = 0x04;
        s0_pos4_paused[31] = 0x01;
        let slice_paused = vec![
            IMulticall3::Result {
                success: true,
                returnData: ret_prices.into(),
            },
            IMulticall3::Result {
                success: true,
                returnData: s0_pos4_paused.to_vec().into(),
            },
            IMulticall3::Result {
                success: true,
                returnData: s1_packed.to_be_bytes::<32>().to_vec().into(),
            },
        ];
        let (_, _, _, _, ab2, paused2) =
            decode_metric_state_slice(&slice_paused).expect("decode pos4 paused");
        assert_eq!(ab2, 4);
        assert!(paused2);
    }
}
