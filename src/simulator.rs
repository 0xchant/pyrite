use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolCall, SolError, SolValue};
use eyre::{eyre, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

use crate::abi::{
    DiagFlashRepayShortfall, DiagProfitBelowMin, DiagStepSwapReverted, DiagStepTransferFailed,
    DiagStepZeroReceived, IDagArbExecutor, IL1GasOracle,
};
use crate::amm::{human_to_u256, u256_to_f64, u256_to_human};
use crate::discovery::MarketState;
use crate::router::{build_arb_params, encode_compact_arb, RouteCandidate};
use crate::rpc::{AccountOverride, RpcPool};

/// Virtual executor address used during `eth_call` state-override verification prior to deployment.
pub const VIRTUAL_EXECUTOR_ADDR: Address =
    alloy::primitives::address!("0000000000000000000000000000000000DA6A7B");

#[derive(Debug, Clone)]
pub struct CompiledContract {
    pub creation_bytecode: Vec<u8>,
    #[allow(dead_code)]
    pub deployed_bytecode: Vec<u8>,
    #[allow(dead_code)]
    pub diag_deployed_bytecode: Vec<u8>,
    pub diag_code_hex: String,
}

#[derive(Debug, Clone)]
pub struct SimulationResult {
    #[allow(dead_code)]
    pub executor_addr: Address,
    pub calldata: Bytes,
    pub params: crate::abi::ArbParams,
    #[allow(dead_code)]
    pub net_profit_raw: U256,
    #[allow(dead_code)]
    pub net_profit_human: f64,
    pub gross_profit_usd: f64,
    pub gas_used: u64,
    #[allow(dead_code)]
    pub l1_fee_wei: u128,
    pub gas_cost_usd: f64,
    pub net_profit_usd: f64,
}

impl SimulationResult {
    #[allow(dead_code)]
    pub fn calldata_with_min_profit(&self, new_min_profit_raw: U256) -> Bytes {
        let capped = new_min_profit_raw.min(self.net_profit_raw.saturating_sub(U256::from(1u64)));
        if capped <= self.params.minNetProfit {
            return self.calldata.clone();
        }
        let mut p = self.params.clone();
        p.minNetProfit = capped;
        encode_compact_arb(&p, self.executor_addr).unwrap_or_else(|_| self.calldata.clone())
    }

    pub fn calldata_for_builder(
        &self,
        new_min_profit_raw: U256,
        bribe_recipient: Address,
        bribe_bps: u16,
    ) -> Bytes {
        let capped = new_min_profit_raw.min(self.net_profit_raw.saturating_sub(U256::from(1u64)));
        let mut p = self.params.clone();
        p.minNetProfit = capped;
        p.bribeRecipient = bribe_recipient;
        p.bribeBps = bribe_bps;
        encode_compact_arb(&p, self.executor_addr).unwrap_or_else(|_| self.calldata.clone())
    }
}

#[derive(Debug, Clone)]
pub enum SimDiagnostic {
    ProfitBelowMin {
        realized_usd: f64,
        min_usd: f64,
    },
    FlashShortfall {
        repay_human: f64,
        final_human: f64,
        final_raw: U256,
        shortfall_ppm: u32,
        shortfall_usd: f64,
        tax_token: Address,
        tax_ppm: u32,
    },
    StepZeroReceived {
        step_idx: u8,
        pool: Address,
        token: Address,
    },
    StepTransferFailed {
        step_idx: u8,
        pool: Address,
        token: Address,
        reason: String,
    },
    StepSwapReverted {
        step_idx: u8,
        pool: Address,
        dex_kind: u8,
        reason: String,
    },
    GasOrTimeout {
        summary: String,
    },
    OtherRevert {
        reason: String,
    },
}

impl SimDiagnostic {
    pub fn category(&self) -> &'static str {
        match self {
            SimDiagnostic::ProfitBelowMin { .. } => "MIN_PROFIT",
            SimDiagnostic::FlashShortfall { tax_ppm, .. } if *tax_ppm >= 300 => {
                "FLASH_SHORTFALL_TAX"
            }
            SimDiagnostic::FlashShortfall { .. } => "FLASH_SHORTFALL",
            SimDiagnostic::StepZeroReceived { .. } => "STEP_ZERO_RECEIVED",
            SimDiagnostic::StepTransferFailed { .. } => "STEP_TRANSFER_FAIL",
            SimDiagnostic::StepSwapReverted { .. } => "STEP_SWAP_REVERT",
            SimDiagnostic::GasOrTimeout { .. } => "GAS_OR_TIMEOUT",
            SimDiagnostic::OtherRevert { .. } => "OTHER_REVERT",
        }
    }

    pub fn summary(&self, market: &MarketState) -> String {
        match self {
            SimDiagnostic::ProfitBelowMin {
                realized_usd,
                min_usd,
            } => format!("MIN_PROFIT: realized=${realized_usd:.4} < min=${min_usd:.4}"),
            SimDiagnostic::FlashShortfall {
                repay_human,
                final_human,
                shortfall_ppm,
                shortfall_usd,
                tax_token,
                tax_ppm,
                ..
            } => {
                let bps = (*shortfall_ppm as f64) / 100.0;
                if *tax_token != Address::ZERO && *tax_ppm > 0 {
                    format!(
                        "FLASH_SHORTFALL: got {final_human:.4}/{repay_human:.4} (-{bps:.1}bps, -${shortfall_usd:.4}) tax_token={}@{}bps",
                        market.token_symbol(*tax_token),
                        (*tax_ppm as f64) / 100.0
                    )
                } else {
                    format!(
                        "FLASH_SHORTFALL: got {final_human:.4}/{repay_human:.4} (-{bps:.1}bps, -${shortfall_usd:.4})"
                    )
                }
            }
            SimDiagnostic::StepZeroReceived {
                step_idx,
                pool,
                token,
            } => {
                let dex = market
                    .pools
                    .get(pool)
                    .map(|p| p.factory_name.as_str())
                    .unwrap_or("Pool");
                format!(
                    "STEP_ZERO_RECEIVED: step#{step_idx} pool={pool:#x}({dex}) token={}({token:#x})",
                    market.token_symbol(*token)
                )
            }
            SimDiagnostic::StepTransferFailed {
                step_idx,
                pool,
                token,
                reason,
            } => {
                let dex = market
                    .pools
                    .get(pool)
                    .map(|p| p.factory_name.as_str())
                    .unwrap_or("Pool");
                format!(
                    "STEP_TRANSFER_FAIL: step#{step_idx} pool={pool:#x}({dex}) token={} reason=\"{reason}\"",
                    market.token_symbol(*token)
                )
            }
            SimDiagnostic::StepSwapReverted {
                step_idx,
                pool,
                dex_kind,
                reason,
            } => {
                let dex = market
                    .pools
                    .get(pool)
                    .map(|p| p.factory_name.as_str())
                    .unwrap_or_else(|| match dex_kind {
                        0 => "UniV2",
                        1 => "Solidly",
                        2 => "UniV3",
                        3 => "PancakeV3",
                        _ => "UnknownDex",
                    });
                format!("STEP_SWAP_REVERT: step#{step_idx} pool={pool:#x}({dex}) reason=\"{reason}\"")
            }
            SimDiagnostic::GasOrTimeout { summary } => format!("GAS_OR_TIMEOUT: {summary}"),
            SimDiagnostic::OtherRevert { reason } => format!("OTHER_REVERT: {reason}"),
        }
    }
}

fn read_bytecode_pair(path: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    let raw = std::fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&raw)?;
    let creation_hex = v["bytecode"]["object"]
        .as_str()
        .ok_or_else(|| eyre!("missing bytecode.object in {path}"))?;
    let deployed_hex = v["deployedBytecode"]["object"]
        .as_str()
        .ok_or_else(|| eyre!("missing deployedBytecode.object in {path}"))?;
    let creation = hex::decode(creation_hex.strip_prefix("0x").unwrap_or(creation_hex))?;
    let deployed = hex::decode(deployed_hex.strip_prefix("0x").unwrap_or(deployed_hex))?;
    Ok((creation, deployed))
}

pub fn load_compiled_executor() -> Result<CompiledContract> {
    let candidates = [
        "contracts/out/DagArbExecutor.sol/DagArbExecutor.json",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/contracts/out/DagArbExecutor.sol/DagArbExecutor.json"
        ),
    ];
    let path = candidates
        .iter()
        .find(|p| Path::new(p).exists())
        .ok_or_else(|| eyre!("DagArbExecutor.json not found; run `cd contracts && forge build`"))?;

    let diag_candidates = [
        "contracts/out/DagArbExecutor.sol/DagArbDiagExecutor.json",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/contracts/out/DagArbExecutor.sol/DagArbDiagExecutor.json"
        ),
    ];
    let diag_path = diag_candidates.iter().find(|p| Path::new(p).exists()).ok_or_else(|| {
        eyre!("DagArbDiagExecutor.json not found; run `cd contracts && forge build`")
    })?;

    let (creation_bytecode, deployed_bytecode) = read_bytecode_pair(path)?;
    let (_, diag_deployed_bytecode) = read_bytecode_pair(diag_path)?;
    let diag_code_hex = format!("0x{}", hex::encode(&diag_deployed_bytecode));
    Ok(CompiledContract {
        creation_bytecode,
        deployed_bytecode,
        diag_deployed_bytecode,
        diag_code_hex,
    })
}

fn extract_rpc_revert_data(err_str: &str) -> Option<Vec<u8>> {
    for needle in ["\"data\":\"0x", "\"data\": \"0x"] {
        if let Some(pos) = err_str.find(needle) {
            let start = pos + needle.len();
            if let Some(end) = err_str[start..].find('"') {
                let hex_str = &err_str[start..start + end];
                if let Ok(bytes) = hex::decode(hex_str) {
                    return Some(bytes);
                }
            }
        }
    }
    None
}

pub fn decode_revert_bytes(data: &[u8]) -> String {
    if data.is_empty() {
        return "empty(0x)".to_string();
    }
    // Standard Error(string) selector = 0x08c379a0
    if data.len() >= 68 && data[0..4] == [0x08, 0xc3, 0x79, 0xa0] {
        let len_word = U256::from_be_slice(&data[36..68]).saturating_to::<usize>();
        if 68 + len_word <= data.len() {
            return String::from_utf8_lossy(&data[68..68 + len_word]).to_string();
        }
    }
    // Standard Panic(uint256) selector = 0x4e487b71
    if data.len() >= 36 && data[0..4] == [0x4e, 0x48, 0x7b, 0x71] {
        let code = U256::from_be_slice(&data[4..36]);
        return format!("Panic(0x{code:x})");
    }
    if data.len() >= 4 {
        let ascii: String = data
            .iter()
            .filter(|&&b| (0x20..=0x7e).contains(&b))
            .map(|&b| b as char)
            .collect();
        if ascii.len() >= 4 && ascii.chars().all(|c| !c.is_control()) && ascii.len() * 2 >= data.len()
        {
            return ascii;
        }
        return format!("0x{}", hex::encode(&data[..data.len().min(36)]));
    }
    format!("0x{}", hex::encode(data))
}

pub fn decode_sim_error(err_str: &str, base_dec: u8, base_usd: f64) -> SimDiagnostic {
    let err_lc = err_str.to_ascii_lowercase();
    if err_lc.contains("sim_timeout")
        || err_lc.contains("out of gas")
        || err_lc.contains("gas required exceeds")
        || err_lc.contains("invalid opcode")
    {
        return SimDiagnostic::GasOrTimeout {
            summary: err_str.chars().take(120).collect(),
        };
    }

    if let Some(bytes) = extract_rpc_revert_data(err_str) {
        if let Ok(err) = DiagProfitBelowMin::abi_decode(&bytes) {
            return SimDiagnostic::ProfitBelowMin {
                realized_usd: u256_to_human(err.netProfit, base_dec) * base_usd,
                min_usd: u256_to_human(err.minNetProfit, base_dec) * base_usd,
            };
        }
        if let Ok(err) = DiagFlashRepayShortfall::abi_decode(&bytes) {
            let shortfall_raw = err.repayRequired.saturating_sub(err.finalBalance);
            let shortfall_ppm = if !err.repayRequired.is_zero() {
                ((u256_to_f64(shortfall_raw) / u256_to_f64(err.repayRequired)) * 1_000_000.0)
                    .clamp(0.0, 1_000_000.0) as u32
            } else {
                0
            };
            return SimDiagnostic::FlashShortfall {
                repay_human: u256_to_human(err.repayRequired, base_dec),
                final_human: u256_to_human(err.finalBalance, base_dec),
                final_raw: err.finalBalance,
                shortfall_ppm,
                shortfall_usd: u256_to_human(shortfall_raw, base_dec) * base_usd,
                tax_token: err.maxTaxToken,
                tax_ppm: err.maxTaxPpm,
            };
        }
        if let Ok(err) = DiagStepZeroReceived::abi_decode(&bytes) {
            return SimDiagnostic::StepZeroReceived {
                step_idx: err.stepIdx,
                pool: err.pool,
                token: err.token,
            };
        }
        if let Ok(err) = DiagStepTransferFailed::abi_decode(&bytes) {
            return SimDiagnostic::StepTransferFailed {
                step_idx: err.stepIdx,
                pool: err.pool,
                token: err.token,
                reason: decode_revert_bytes(&err.reason),
            };
        }
        if let Ok(err) = DiagStepSwapReverted::abi_decode(&bytes) {
            return SimDiagnostic::StepSwapReverted {
                step_idx: err.stepIdx,
                pool: err.pool,
                dex_kind: err.dexKind,
                reason: decode_revert_bytes(&err.reason),
            };
        }

        let decoded = decode_revert_bytes(&bytes);
        if decoded.contains("MIN_PROFIT") {
            return SimDiagnostic::ProfitBelowMin {
                realized_usd: 0.0,
                min_usd: 0.0,
            };
        }
        return SimDiagnostic::OtherRevert { reason: decoded };
    }

    if err_str.contains("MIN_PROFIT") {
        return SimDiagnostic::ProfitBelowMin {
            realized_usd: 0.0,
            min_usd: 0.0,
        };
    }
    SimDiagnostic::OtherRevert {
        reason: err_str.chars().take(120).collect(),
    }
}

#[derive(Debug, Clone)]
pub struct PreparedSimulation {
    pub exec_addr: Address,
    pub caller: Address,
    pub calldata: Bytes,
    pub diag_calldata: Bytes,
    pub params: crate::abi::ArbParams,
    pub base_dec: u8,
    pub base_usd: f64,
    pub gas_price_wei: u128,
    pub native_usd: f64,
    pub l1_gas_oracle: Option<Address>,
    pub block_tag: String,
    pub overrides: Option<HashMap<Address, AccountOverride>>,
    pub diag_overrides: Option<HashMap<Address, AccountOverride>>,
    pub used_pools: Vec<Address>,
}

pub fn prepare_simulation(
    market: &MarketState,
    contract: &CompiledContract,
    deployed_executor: Option<Address>,
    caller: Address,
    cand: &RouteCandidate,
    min_net_profit_raw: U256,
    payout_to_owner: bool,
) -> Result<PreparedSimulation> {
    let exec_addr = deployed_executor.unwrap_or(VIRTUAL_EXECUTOR_ADDR);
    let mut params = build_arb_params(cand, market, exec_addr, min_net_profit_raw, payout_to_owner)
        .ok_or_else(|| eyre!("failed to compile valid acyclic DagSteps"))?;
    if params.baseToken == params.wrappedNative {
        // Trigger the exact WBNB.withdraw + coinbase.call bribe path during eth_call so
        // decoded.gasUsed includes real bribe-transfer EVM gas without affecting netProfit.
        params.bribeRecipient = caller;
        params.bribeBps = 1;
    }

    let diag_calldata: Bytes = IDagArbExecutor::executeArbCall {
        params: params.clone(),
    }
    .abi_encode()
    .into();
    let calldata = encode_compact_arb(&params, exec_addr)?;

    let (overrides, diag_map) = if deployed_executor.is_none() {
        // V2 `DagArbExecutor` slot 0 is `mapping(address => bool) public callers`:
        // compute keccak256(abi.encode(caller, uint256(0))) => 1.
        let mut slot_preimage = [0u8; 64];
        slot_preimage[12..32].copy_from_slice(caller.as_slice());
        let caller_slot = keccak256(slot_preimage);
        let mut prod_diff = HashMap::new();
        prod_diff.insert(caller_slot, B256::from(U256::from(1u8).to_be_bytes::<32>()));

        let mut prod_map = HashMap::new();
        prod_map.insert(
            exec_addr,
            AccountOverride {
                code: Some(format!("0x{}", hex::encode(&contract.deployed_bytecode))),
                balance: Some("0xde0b6b3a7640000".to_string()),
                state_diff: Some(prod_diff),
            },
        );

        // `DagArbDiagExecutor` slot 0 is `address public owner`.
        let mut diag_diff = HashMap::new();
        diag_diff.insert(B256::ZERO, B256::left_padding_from(caller.as_slice()));

        let mut d_map = HashMap::new();
        d_map.insert(
            exec_addr,
            AccountOverride {
                code: Some(contract.diag_code_hex.clone()),
                balance: Some("0xde0b6b3a7640000".to_string()),
                state_diff: Some(diag_diff),
            },
        );
        (Some(prod_map), d_map)
    } else {
        // Happy-path runs directly against deployed bytecode (zero stateOverrides payload);
        // diagnostics inject `DagArbDiagExecutor` only on revert with slot 0 (`owner`) set to `caller`.
        let mut diag_diff = HashMap::new();
        diag_diff.insert(B256::ZERO, B256::left_padding_from(caller.as_slice()));
        let mut d_map = HashMap::new();
        d_map.insert(
            exec_addr,
            AccountOverride {
                code: Some(contract.diag_code_hex.clone()),
                balance: None,
                state_diff: Some(diag_diff),
            },
        );
        (None, d_map)
    };

    Ok(PreparedSimulation {
        exec_addr,
        caller,
        calldata,
        diag_calldata,
        params,
        base_dec: market.token_decimals(cand.base_token),
        base_usd: market.token_usd(cand.base_token),
        gas_price_wei: market.gas_price_wei,
        native_usd: market.native_usd,
        l1_gas_oracle: market.chain.l1_gas_oracle,
        block_tag: if market.last_block > 0 {
            format!("0x{:x}", market.last_block)
        } else {
            market.chain.block_tag.to_string()
        },
        overrides,
        diag_overrides: Some(diag_map),
        used_pools: cand.flows.iter().map(|f| f.pool).collect(),
    })
}

#[inline]
pub fn evm_tx_overhead_gas(calldata: &[u8]) -> u64 {
    let mut gas = 21_000u64;
    for &b in calldata {
        gas += if b == 0 { 4 } else { 16 };
    }
    gas
}

pub async fn execute_simulation(
    rpc: &RpcPool,
    prep: &PreparedSimulation,
) -> Result<SimulationResult> {
    let ret = rpc
        .eth_call(
            Some(prep.caller),
            prep.exec_addr,
            &prep.calldata,
            &prep.block_tag,
            prep.overrides.as_ref(),
        )
        .await?;

    let (net_profit_raw, inner_gas_used) = <(U256, U256)>::abi_decode(&ret)
        .map_err(|e| eyre!("decode compact execute return failed: {e}"))?;

    let net_profit_human = u256_to_human(net_profit_raw, prep.base_dec);
    let gross_profit_usd = net_profit_human * prep.base_usd;

    let non_wbnb_bribe_gas = if prep.params.baseToken != prep.params.wrappedNative {
        7_500u64
    } else {
        1_500u64
    };
    let gas_used = inner_gas_used.saturating_to::<u64>()
        + evm_tx_overhead_gas(&prep.calldata)
        + non_wbnb_bribe_gas;

    let mut l1_fee_wei = 0u128;
    if let Some(oracle) = prep.l1_gas_oracle {
        let l1_call = IL1GasOracle::getL1FeeCall {
            data: prep.calldata.clone(),
        }
        .abi_encode();
        if let Ok(raw_l1) = rpc.eth_call(None, oracle, &l1_call, &prep.block_tag, None).await {
            if let Ok(fee) = IL1GasOracle::getL1FeeCall::abi_decode_returns(&raw_l1) {
                l1_fee_wei = fee.saturating_to::<u128>();
            }
        }
    }

    let l2_wei = (gas_used as u128) * prep.gas_price_wei;
    let gas_cost_usd = (((l2_wei + l1_fee_wei) as f64) / 1e18) * prep.native_usd;
    let net_profit_usd = gross_profit_usd - gas_cost_usd;

    Ok(SimulationResult {
        executor_addr: prep.exec_addr,
        calldata: prep.calldata.clone(),
        params: prep.params.clone(),
        net_profit_raw,
        net_profit_human,
        gross_profit_usd,
        gas_used,
        l1_fee_wei,
        gas_cost_usd,
        net_profit_usd,
    })
}

pub async fn diagnose_simulation(rpc: &RpcPool, prep: &PreparedSimulation) -> Option<String> {
    rpc.eth_call(
        Some(prep.caller),
        prep.exec_addr,
        &prep.diag_calldata,
        &prep.block_tag,
        prep.diag_overrides.as_ref(),
    )
    .await
    .err()
    .map(|e| e.to_string())
}

/// Simulate a routed candidate on live mainnet EVM via `eth_call`.
/// If `deployed_executor` is `None`, injects compiled `DagArbExecutor` runtime bytecode
/// and sets slot 0 (`owner`) to `caller` via `eth_call` `stateOverrides`.
#[allow(dead_code)]
pub async fn simulate_candidate(
    rpc: &RpcPool,
    market: &MarketState,
    contract: &CompiledContract,
    deployed_executor: Option<Address>,
    caller: Address,
    cand: &RouteCandidate,
    min_net_profit_raw: U256,
    payout_to_owner: bool,
) -> Result<SimulationResult> {
    let prep = prepare_simulation(
        market,
        contract,
        deployed_executor,
        caller,
        cand,
        min_net_profit_raw,
        payout_to_owner,
    )?;
    execute_simulation(rpc, &prep).await
}

#[allow(dead_code)]
pub fn gas_cost_in_base_raw(market: &MarketState, base_token: Address, est_gas: u64) -> U256 {
    let gas_usd = market.estimate_gas_cost_usd(est_gas, 0) * 1.10; // 10% safety buffer above gas cost
    let base_usd = market.token_usd(base_token);
    if base_usd <= 0.0 {
        return U256::ZERO;
    }
    let human = gas_usd / base_usd;
    human_to_u256(human, market.token_decimals(base_token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_sim_diagnostics() {
        let tok = Address::repeat_byte(0x42);
        let err = DiagFlashRepayShortfall {
            borrowAmount: U256::from(100u128) * U256::from(10u128.pow(18)),
            repayRequired: U256::from(100_010_000_000_000_000_000u128),
            finalBalance: U256::from(98_000_000_000_000_000_000u128),
            maxTaxToken: tok,
            maxTaxPpm: 20_000,
        };
        let rpc_err = format!(
            "[EVM_REVERT] RPC https://singapore.bsc.blockrazor.xyz error: {{\"code\":3,\"data\":\"0x{}\"}}",
            hex::encode(err.abi_encode())
        );
        let diag = decode_sim_error(&rpc_err, 18, 1.0);
        match diag {
            SimDiagnostic::FlashShortfall {
                tax_token,
                tax_ppm,
                shortfall_ppm,
                ..
            } => {
                assert_eq!(tax_token, tok);
                assert_eq!(tax_ppm, 20_000);
                assert!(shortfall_ppm > 19_000 && shortfall_ppm < 21_000);
            }
            other => panic!("unexpected diag: {other:?}"),
        }

        let inner_msg = "Pancake: TRANSFER_FAILED".as_bytes();
        let mut inner = vec![0x08, 0xc3, 0x79, 0xa0];
        inner.extend_from_slice(&U256::from(32).to_be_bytes::<32>());
        inner.extend_from_slice(&U256::from(inner_msg.len()).to_be_bytes::<32>());
        inner.extend_from_slice(inner_msg);
        inner.resize(68 + ((inner_msg.len() + 31) / 32) * 32, 0);

        let pool = Address::repeat_byte(0x77);
        let swap_err = DiagStepSwapReverted {
            stepIdx: 2,
            pool,
            dexKind: 0,
            reason: inner.into(),
        };
        let rpc_err2 = format!(
            "[EVM_REVERT] RPC error: {{\"code\":3,\"data\":\"0x{}\"}}",
            hex::encode(swap_err.abi_encode())
        );
        match decode_sim_error(&rpc_err2, 18, 1.0) {
            SimDiagnostic::StepSwapReverted {
                step_idx,
                pool: p,
                reason,
                ..
            } => {
                assert_eq!(step_idx, 2);
                assert_eq!(p, pool);
                assert_eq!(reason, "Pancake: TRANSFER_FAILED");
            }
            other => panic!("unexpected diag2: {other:?}"),
        }
    }
}
