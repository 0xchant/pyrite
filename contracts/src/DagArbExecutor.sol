// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {
    IERC20,
    IUniV2Pair,
    ISolidlyPair,
    IV3Pool,
    IMorpho,
    IWBNB,
    IInfinityVault,
    ICLPoolManager,
    IUniV4PoolManager,
    IMetricOmmPool,
    IBalancerV2Vault
} from "./interfaces/IDexInterfaces.sol";
import {SafeTransferLib} from "./solady/SafeTransferLib.sol";

/// @title DagArbExecutor
/// @notice Ultra-low-gas V2 compact binary DAG arbitrage executor with Solady SafeTransferLib,
///         per-tx isolated `delta[]` accounting (safe in-contract profit retention), zero-copy flash callbacks,
///         native multi-hop UniV4 `unlock()` batching, and zero-unwrap ETH bribe settlement.
contract DagArbExecutor {
    // Slot 0: authorized off-chain callers (virtual eth_call overrides keccak256(caller, 0) => 1).
    mapping(address => bool) public callers;
    address private immutable OWNER;

    // Cancun EIP-1153 transient storage slots
    uint256 private constant TSLOT_LOCK = 1;
    uint256 private constant TSLOT_FLASH_CB = 2;
    uint256 private constant TSLOT_SWAP_CB = 3;
    uint256 private constant TSLOT_PAY_TOKEN = 4;
    uint256 private constant TSLOT_ROOT_SWAP_POOL = 5;
    uint256 private constant TSLOT_WRAPPED_NATIVE = 6;
    uint256 private constant TSLOT_BORROW_AMT = 7;
    uint256 private constant TSLOT_STEP_OUT = 8;
    uint256 private constant TSLOT_ROOT_META = 9;
    uint256 private constant TSLOT_DELTA_BASE = 32; // 32..47 hold delta[0..15] across batched UniV4 swaps

    uint160 private constant MIN_SQRT_RATIO_PLUS_ONE = 4295128740;
    uint160 private constant MAX_SQRT_RATIO_MINUS_ONE =
        1461446703485210103287273052203988822378723970341;
    uint128 private constant METRIC_MIN_PRICE_LIMIT = 1;
    uint128 private constant METRIC_MAX_PRICE_LIMIT = type(uint128).max;

    uint8 private constant FLASH_FUNDED = 0;
    uint8 private constant FLASH_MORPHO = 1;
    uint8 private constant FLASH_UNIV3 = 2;
    uint8 private constant FLASH_PANCAKEV3 = 3;
    uint8 private constant FLASH_V3_SWAP = 4;
    uint8 private constant FLASH_V2_SWAP = 5;
    uint8 private constant FLASH_METRIC_SWAP = 6;
    uint8 private constant FLASH_BALANCER_V2 = 7;

    uint8 private constant DEX_UNIV2 = 0;
    uint8 private constant DEX_SOLIDLY = 1;
    uint8 private constant DEX_UNIV3 = 2;
    uint8 private constant DEX_PANCAKEV3 = 3;
    uint8 private constant DEX_INFINITY_CL = 4;
    uint8 private constant DEX_UNIV4 = 5;
    uint8 private constant DEX_METRIC_OMM = 6;
    uint8 private constant DEX_BALANCER_V2 = 7;

    uint8 private constant MAGIC_COMPACT_ENTRY = 0x00;
    uint8 private constant MAGIC_V4_BATCH = 0xfe;
    uint8 private constant MAGIC_V4_SINGLE = 0x01;

    // Type definitions shared with DagArbDiagExecutor & off-chain tooling (zero runtime cost).
    struct DagStep {
        address pool;
        address tokenIn;
        address tokenOut;
        uint8 dexKind;
        bool inputInPool;
        uint16 feeNumerator;
        uint16 shareBps;
        address recipient;
        bytes extraData;
    }

    struct ArbParams {
        uint8 flashMode;
        address flashSource;
        address baseToken;
        uint256 borrowAmount;
        uint256 minNetProfit;
        bool payoutToOwner;
        address wrappedNative;
        address bribeRecipient;
        uint16 bribeBps;
        DagStep[] steps;
    }

    modifier onlyOwner() {
        require(msg.sender == OWNER, "NOT_OWNER");
        _;
    }

    constructor(address _owner) {
        require(_owner != address(0), "ZERO_OWNER");
        OWNER = _owner;
    }

    receive() external payable {}

    function owner() external view returns (address) {
        return OWNER;
    }

    function setCallers(address[] calldata addrs, bool allowed) external onlyOwner {
        uint256 len = addrs.length;
        for (uint256 i = 0; i < len;) {
            callers[addrs[i]] = allowed;
            unchecked {
                ++i;
            }
        }
    }

    /// @notice Compact binary hot-path arbitrage entry (`msg.data[0] == 0x00`, len >= 38).
    fallback() external payable {
        if (msg.data.length < 38 || uint8(msg.data[0]) != MAGIC_COMPACT_ENTRY) _revert();
        if (msg.sender != OWNER && !callers[msg.sender]) _revert();
        if (_tloadUint(TSLOT_LOCK) != 0) _revert();
        uint256 gasStart = gasleft();
        _tstoreUint(TSLOT_LOCK, 1);

        uint8 flashMode = uint8(msg.data[1]);
        uint8 flags = uint8(msg.data[2]);
        uint16 bribeBps = _u16(msg.data[3:5]);
        uint256 borrowAmount = uint256(_u128(msg.data[5:21]));
        uint256 minNetProfit = uint256(_u128(msg.data[21:37]));

        uint256 off = 37;
        address wrappedNative;
        if ((flags & 0x02) != 0) {
            wrappedNative = _addr(msg.data[off:off + 20]);
            off += 20;
        }
        _tstoreAddr(TSLOT_WRAPPED_NATIVE, wrappedNative);

        address bribeRecipient;
        if ((flags & 0x04) != 0) {
            bribeRecipient = _addr(msg.data[off:off + 20]);
            off += 20;
        }
        address flashSource;
        if ((flags & 0x08) != 0) {
            flashSource = _addr(msg.data[off:off + 20]);
            off += 20;
        }

        // Plan layout at `off`: [1B numTokens][20B*numTokens][1B numSteps][steps...]
        bytes calldata planData = msg.data[off:];
        uint256 numTokens = uint8(planData[0]);
        if (numTokens == 0 || numTokens > 16) _revert();
        uint256 numSteps = uint8(planData[1 + numTokens * 20]);
        if (numSteps == 0 || numSteps > 32) _revert();

        address baseToken = _addr(planData[1:21]);
        uint256 balBefore = SafeTransferLib.balanceOf(baseToken, address(this));

        if (flashMode == FLASH_FUNDED) {
            _execPlan(planData, borrowAmount);
        } else if (flashMode <= FLASH_PANCAKEV3 || flashMode == FLASH_BALANCER_V2) {
            _executeExternalFlash(flashMode, flashSource, baseToken, borrowAmount, planData);
        } else {
            _executeRootSwapFlash(flashMode, baseToken, borrowAmount, numTokens, numSteps, planData);
        }

        uint256 netProfit = SafeTransferLib.balanceOf(baseToken, address(this)) - balBefore;
        if (netProfit < minNetProfit) _revert();

        _settleProfitAndBribe(
            baseToken,
            wrappedNative,
            bribeRecipient,
            bribeBps,
            netProfit,
            minNetProfit,
            (flags & 0x01) != 0
        );

        _tstoreUint(TSLOT_LOCK, 0);

        uint256 gasUsed = gasStart - gasleft();
        assembly ("memory-safe") {
            mstore(0x00, netProfit)
            mstore(0x20, gasUsed)
            return(0x00, 0x40)
        }
    }

    function _executeExternalFlash(
        uint8 flashMode,
        address flashSource,
        address baseToken,
        uint256 borrowAmount,
        bytes calldata planData
    ) private {
        _tstoreAddr(TSLOT_FLASH_CB, flashSource);
        if (flashMode == FLASH_MORPHO) {
            IMorpho(flashSource).flashLoan(baseToken, borrowAmount, planData);
        } else if (flashMode == FLASH_UNIV3 || flashMode == FLASH_PANCAKEV3) {
            _tstoreUint(TSLOT_BORROW_AMT, borrowAmount);
            address t0 = IV3Pool(flashSource).token0();
            (uint256 amt0, uint256 amt1) =
                baseToken == t0 ? (borrowAmount, uint256(0)) : (uint256(0), borrowAmount);
            IV3Pool(flashSource).flash(address(this), amt0, amt1, planData);
        } else if (flashMode == FLASH_BALANCER_V2) {
            address[] memory tokens = new address[](1);
            tokens[0] = baseToken;
            uint256[] memory amounts = new uint256[](1);
            amounts[0] = borrowAmount;
            IBalancerV2Vault(flashSource).flashLoan(address(this), tokens, amounts, planData);
        } else {
            _revert();
        }
        _tstoreAddr(TSLOT_FLASH_CB, address(0));
    }

    function _executeRootSwapFlash(
        uint8 flashMode,
        address baseToken,
        uint256 borrowAmount,
        uint256 numTokens,
        uint256 numSteps,
        bytes calldata planData
    ) private {
        if (numSteps < 2) _revert();
        uint256 stepsOff = 2 + numTokens * 20;
        bytes calldata stepsData = planData[stepsOff:];
        (uint8 h0, uint8 inIdx0, uint8 outIdx0, address s0Pool,, uint16 s0FeeNum, uint256 step1Off) =
            _decodeStepMeta(stepsData, 0);
        if (inIdx0 >= numTokens || outIdx0 >= numTokens) _revert();
        bool zfo = baseToken < _tok(planData[1:stepsOff - 1], outIdx0);
        bool recIsNext = (h0 & 0x20) != 0;
        address s0Recip = recIsNext ? _addr(stepsData[step1Off + 2:step1Off + 22]) : address(this);

        _tstoreAddr(TSLOT_ROOT_SWAP_POOL, s0Pool);
        _tstoreUint(TSLOT_ROOT_META, (step1Off << 16) | (uint256(outIdx0) << 8) | (recIsNext ? 2 : 3));
        if (flashMode == FLASH_V3_SWAP) {
            IV3Pool(s0Pool).swap(
                s0Recip,
                zfo,
                int256(borrowAmount),
                zfo ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE,
                planData
            );
        } else if (flashMode == FLASH_V2_SWAP) {
            (uint256 r0, uint256 r1) = _v2Reserves(s0Pool);
            (uint256 rIn, uint256 rOut) = zfo ? (r0, r1) : (r1, r0);
            uint256 amtInWithFee = borrowAmount * uint256(s0FeeNum);
            uint256 amtOut;
            unchecked {
                amtOut = (amtInWithFee * rOut) / (rIn * 10000 + amtInWithFee);
            }
            if (amtOut == 0) _revert();
            _tstoreUint(TSLOT_BORROW_AMT, borrowAmount);
            IUniV2Pair(s0Pool).swap(zfo ? 0 : amtOut, zfo ? amtOut : 0, s0Recip, planData);
        } else if (flashMode == FLASH_METRIC_SWAP) {
            if (borrowAmount > uint256(uint128(type(int128).max))) _revert();
            bool metricZfo = (h0 & 0x80) != 0;
            IMetricOmmPool(s0Pool).swap(
                s0Recip,
                metricZfo,
                int128(int256(borrowAmount)),
                metricZfo ? METRIC_MIN_PRICE_LIMIT : METRIC_MAX_PRICE_LIMIT,
                planData,
                ""
            );
        } else {
            _revert();
        }
        _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
        _tstoreUint(TSLOT_ROOT_META, 0);
    }

    function _settleProfitAndBribe(
        address baseToken,
        address wrappedNative,
        address bribeRecipient,
        uint16 bribeBps,
        uint256 netProfit,
        uint256 minNetProfit,
        bool payoutToOwner
    ) private {
        address bTarget = bribeRecipient == address(0) ? block.coinbase : bribeRecipient;
        if (wrappedNative != address(0) && baseToken == wrappedNative) {
            uint256 bribeWei;
            if (bribeBps > 0 && netProfit > minNetProfit) {
                unchecked {
                    bribeWei = ((netProfit - minNetProfit) * uint256(bribeBps)) / 10000;
                }
            }
            if (payoutToOwner) {
                if (netProfit > 0) IWBNB(wrappedNative).withdraw(netProfit);
                if (bribeWei > 0) SafeTransferLib.safeTransferETH(bTarget, bribeWei);
                uint256 ownerEth;
                unchecked {
                    ownerEth = netProfit - bribeWei + msg.value;
                }
                if (ownerEth > 0) SafeTransferLib.safeTransferETH(OWNER, ownerEth);
            } else if (bribeWei > 0) {
                // Zero-unwrap fast path when contract holds native ETH buffer; auto-fallback otherwise
                if (address(this).balance < bribeWei) {
                    IWBNB(wrappedNative).withdraw(bribeWei);
                }
                SafeTransferLib.safeTransferETH(bTarget, bribeWei);
            }
        } else {
            if (msg.value > 0) SafeTransferLib.safeTransferETH(bTarget, msg.value);
            if (payoutToOwner && netProfit > 0) {
                SafeTransferLib.safeTransfer(baseToken, OWNER, netProfit);
            }
        }
    }

    // ---------------- Isolated Per-Tx Delta Execution Engine ----------------

    function _execPlan(bytes calldata planData, uint256 initAmt) private {
        uint256 numTokens = uint8(planData[0]);
        uint256 stepsCountOff = 1 + numTokens * 20;
        bytes calldata tokTable = planData[1:stepsCountOff];
        uint256 numSteps = uint8(planData[stepsCountOff]);
        bytes calldata stepsData = planData[stepsCountOff + 1:];

        uint256[16] memory delta;
        uint256 i = 0;
        uint256 off = 0;
        uint256 meta = _tloadUint(TSLOT_ROOT_META);
        if ((meta & 2) != 0) {
            _tstoreUint(TSLOT_ROOT_META, 0);
            i = 1;
            off = meta >> 16;
            if ((meta & 1) != 0) {
                delta[uint8((meta >> 8) & 0xff)] = initAmt;
            }
        } else {
            delta[0] = initAmt;
        }

        for (; i < numSteps;) {
            (uint8 hdr, uint8 inIdx, uint8 outIdx, address pool, uint16 shareBps, uint16 feeNum, uint256 tailStart) =
                _decodeStepMeta(stepsData, off);
            if (inIdx >= numTokens || outIdx >= numTokens) _revert();
            uint8 dexKind = hdr & 0x0f;
            bool inputInPool = (hdr & 0x10) != 0;
            bool recIsNext = (hdr & 0x20) != 0;

            // Contiguous DEX_UNIV4 batching on same PoolManager with !recIsNext
            if (dexKind == DEX_UNIV4 && !recIsNext) {
                uint256 batchEnd = tailStart + 27;
                uint256 batchCount = 1;
                while (i + batchCount < numSteps) {
                    (uint8 nh,,, address npool,,, uint256 nTail) = _decodeStepMeta(stepsData, batchEnd);
                    if ((nh & 0x0f) != DEX_UNIV4 || (nh & 0x20) != 0 || npool != pool) break;
                    batchEnd = nTail + 27;
                    unchecked {
                        ++batchCount;
                    }
                }
                if (batchCount >= 2) {
                    _runUniV4Batch(tokTable, numTokens, delta, pool, batchCount, stepsData[off:batchEnd]);
                    off = batchEnd;
                    unchecked {
                        i += batchCount;
                    }
                    continue;
                }
            }

            address tokIn = _tok(tokTable, inIdx);
            address tokOut = _tok(tokTable, outIdx);
            uint256 nextOff;
            uint256 amtOut = 0;

            if (dexKind == DEX_UNIV2 || dexKind == DEX_SOLIDLY) {
                nextOff = tailStart;
                address recip = recIsNext ? _addr(stepsData[nextOff + 2:nextOff + 22]) : address(this);
                uint256 amtIn = 0;
                if (!inputInPool) {
                    amtIn = _takeDelta(delta, inIdx, hdr, shareBps);
                    if (amtIn > 0) SafeTransferLib.safeTransfer(tokIn, pool, amtIn);
                }
                if (inputInPool || amtIn > 0) {
                    bool zfo = tokIn < tokOut;
                    uint256 rIn;
                    uint256 rOut;
                    if (dexKind == DEX_UNIV2) {
                        (uint256 r0, uint256 r1) = _v2Reserves(pool);
                        (rIn, rOut) = zfo ? (r0, r1) : (r1, r0);
                    } else {
                        (uint256 r0, uint256 r1,) = ISolidlyPair(pool).getReserves();
                        rIn = zfo ? r0 : r1;
                    }
                    uint256 poolBal = SafeTransferLib.balanceOf(tokIn, pool);
                    if (poolBal > rIn) {
                        uint256 actualIn;
                        unchecked {
                            actualIn = poolBal - rIn;
                        }
                        if (dexKind == DEX_UNIV2) {
                            uint256 aFee = actualIn * uint256(feeNum);
                            unchecked {
                                amtOut = (aFee * rOut) / (rIn * 10000 + aFee);
                            }
                            if (amtOut > 0) {
                                _v2Swap(pool, zfo ? 0 : amtOut, zfo ? amtOut : 0, recip);
                            }
                        } else {
                            amtOut = ISolidlyPair(pool).getAmountOut(actualIn, tokIn);
                            if (amtOut > 0) {
                                ISolidlyPair(pool).swap(zfo ? 0 : amtOut, zfo ? amtOut : 0, recip, "");
                            }
                        }
                    }
                }
            } else if (dexKind == DEX_UNIV3 || dexKind == DEX_PANCAKEV3) {
                nextOff = tailStart;
                uint256 amtIn = _takeDelta(delta, inIdx, hdr, shareBps);
                if (amtIn > 0) {
                    address recip = recIsNext ? _addr(stepsData[nextOff + 2:nextOff + 22]) : address(this);
                    bool zfo = tokIn < tokOut;
                    _tstoreAddr(TSLOT_SWAP_CB, pool);
                    _tstoreAddr(TSLOT_PAY_TOKEN, tokIn);
                    (int256 d0, int256 d1) = _v3Swap(
                        pool,
                        recip,
                        zfo,
                        int256(amtIn),
                        zfo ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE
                    );
                    _tstoreAddr(TSLOT_SWAP_CB, address(0));
                    _tstoreAddr(TSLOT_PAY_TOKEN, address(0));
                    int256 od = d0 < 0 ? d0 : d1;
                    if (od < 0) amtOut = uint256(-od);
                }
            } else if (dexKind == DEX_METRIC_OMM) {
                nextOff = tailStart;
                uint256 amtIn = _takeDelta(delta, inIdx, hdr, shareBps);
                if (amtIn > 0) {
                    if (amtIn > uint256(uint128(type(int128).max))) _revert();
                    address recip = recIsNext ? _addr(stepsData[nextOff + 2:nextOff + 22]) : address(this);
                    bool metricZfo = (hdr & 0x80) != 0;
                    _tstoreAddr(TSLOT_SWAP_CB, pool);
                    _tstoreAddr(TSLOT_PAY_TOKEN, tokIn);
                    (int256 d0, int256 d1) = IMetricOmmPool(pool).swap(
                        recip,
                        metricZfo,
                        int128(int256(amtIn)),
                        metricZfo ? METRIC_MIN_PRICE_LIMIT : METRIC_MAX_PRICE_LIMIT,
                        "",
                        ""
                    );
                    _tstoreAddr(TSLOT_SWAP_CB, address(0));
                    _tstoreAddr(TSLOT_PAY_TOKEN, address(0));
                    int256 od = d0 < 0 ? d0 : d1;
                    if (od < 0) amtOut = uint256(-od);
                }
            } else if (dexKind == DEX_BALANCER_V2) {
                nextOff = tailStart + 32;
                uint256 amtIn = _takeDelta(delta, inIdx, hdr, shareBps);
                if (amtIn > 0) {
                    address recip = recIsNext ? _addr(stepsData[nextOff + 2:nextOff + 22]) : address(this);
                    _ensureAllowance(tokIn, pool, amtIn);
                    bytes32 poolId = _b32(stepsData[tailStart:nextOff]);
                    amtOut = IBalancerV2Vault(pool).swap(
                        IBalancerV2Vault.SingleSwap({
                            poolId: poolId,
                            kind: IBalancerV2Vault.SwapKind.GIVEN_IN,
                            assetIn: tokIn,
                            assetOut: tokOut,
                            amount: amtIn,
                            userData: ""
                        }),
                        IBalancerV2Vault.FundManagement({
                            sender: address(this),
                            fromInternalBalance: false,
                            recipient: payable(recip),
                            toInternalBalance: false
                        }),
                        0,
                        type(uint256).max
                    );
                }
            } else if (dexKind == DEX_UNIV4) {
                nextOff = tailStart + 27;
                uint256 amtIn = _takeDelta(delta, inIdx, hdr, shareBps);
                if (amtIn > 0) {
                    _tstoreAddr(TSLOT_SWAP_CB, pool);
                    _tstoreUint(TSLOT_STEP_OUT, 0);
                    IUniV4PoolManager(pool).unlock(
                        abi.encodePacked(MAGIC_V4_SINGLE, tokIn, tokOut, amtIn, stepsData[tailStart:nextOff])
                    );
                    _tstoreAddr(TSLOT_SWAP_CB, address(0));
                    amtOut = _tloadUint(TSLOT_STEP_OUT);
                }
            } else if (dexKind == DEX_INFINITY_CL) {
                nextOff = tailStart + 76;
                uint256 amtIn = _takeDelta(delta, inIdx, hdr, shareBps);
                if (amtIn > 0) {
                    address clVault = _addr(stepsData[tailStart:tailStart + 20]);
                    _tstoreAddr(TSLOT_SWAP_CB, clVault);
                    _tstoreUint(TSLOT_STEP_OUT, 0);
                    IInfinityVault(clVault).lock(
                        abi.encodePacked(pool, tokIn, tokOut, amtIn, stepsData[tailStart + 20:nextOff])
                    );
                    _tstoreAddr(TSLOT_SWAP_CB, address(0));
                    amtOut = _tloadUint(TSLOT_STEP_OUT);
                }
            } else {
                _revert();
            }

            if (!recIsNext && amtOut > 0) {
                delta[outIdx] += amtOut;
            }
            off = nextOff;
            unchecked {
                ++i;
            }
        }
    }

    function _runUniV4Batch(
        bytes calldata tokTable,
        uint256 numTokens,
        uint256[16] memory delta,
        address poolManager,
        uint256 batchCount,
        bytes calldata batchSlice
    ) private {
        for (uint256 t = 0; t < numTokens;) {
            _tstoreUint(TSLOT_DELTA_BASE + t, delta[t]);
            unchecked {
                ++t;
            }
        }
        _tstoreAddr(TSLOT_SWAP_CB, poolManager);
        IUniV4PoolManager(poolManager).unlock(
            abi.encodePacked(MAGIC_V4_BATCH, uint8(numTokens), tokTable, uint8(batchCount), batchSlice)
        );
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        for (uint256 t = 0; t < numTokens;) {
            delta[t] = _tloadUint(TSLOT_DELTA_BASE + t);
            _tstoreUint(TSLOT_DELTA_BASE + t, 0);
            unchecked {
                ++t;
            }
        }
    }

    // ---------------- Flash Loan Callbacks ----------------

    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        _handleMorphoFlashLoan(assets, data);
    }

    function onMoolahFlashLoan(uint256 assets, bytes calldata data) external {
        _handleMorphoFlashLoan(assets, data);
    }

    function _handleMorphoFlashLoan(uint256 assets, bytes calldata data) private {
        if (msg.sender != _tloadAddr(TSLOT_FLASH_CB)) _revert();
        _tstoreAddr(TSLOT_FLASH_CB, address(0));
        address baseToken = _addr(data[1:21]);
        _execPlan(data, assets);
        _ensureAllowance(baseToken, msg.sender, assets);
    }

    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        _handleV3FlashCallback(fee0, fee1, data);
    }

    function pancakeV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        _handleV3FlashCallback(fee0, fee1, data);
    }

    function _handleV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) private {
        if (msg.sender != _tloadAddr(TSLOT_FLASH_CB)) _revert();
        _tstoreAddr(TSLOT_FLASH_CB, address(0));
        uint256 borrowAmount = _tloadUint(TSLOT_BORROW_AMT);
        address baseToken = _addr(data[1:21]);
        _execPlan(data, borrowAmount);
        uint256 fee = fee0 > 0 ? fee0 : fee1;
        SafeTransferLib.safeTransfer(baseToken, msg.sender, borrowAmount + fee);
    }

    function receiveFlashLoan(
        address[] calldata tokens,
        uint256[] calldata amounts,
        uint256[] calldata feeAmounts,
        bytes calldata userData
    ) external {
        if (msg.sender != _tloadAddr(TSLOT_FLASH_CB)) _revert();
        _tstoreAddr(TSLOT_FLASH_CB, address(0));
        address baseToken = _addr(userData[1:21]);
        if (tokens[0] != baseToken) _revert();
        _execPlan(userData, amounts[0]);
        SafeTransferLib.safeTransfer(baseToken, msg.sender, amounts[0] + feeAmounts[0]);
    }

    // ---------------- V3 & Metric Swap Callbacks ----------------

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _handleV3SwapCallback(amount0Delta, amount1Delta, data);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _handleV3SwapCallback(amount0Delta, amount1Delta, data);
    }

    function metricOmmSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _handleV3SwapCallback(amount0Delta, amount1Delta, data);
    }

    function _handleV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) private {
        uint256 payAmount = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        if (data.length > 0 && msg.sender == _tloadAddr(TSLOT_ROOT_SWAP_POOL)) {
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
            int256 outDelta = amount0Delta < 0 ? amount0Delta : amount1Delta;
            uint256 recvAmt = outDelta < 0 ? uint256(-outDelta) : 0;
            address baseToken = _addr(data[1:21]);
            _execPlan(data, recvAmt);
            SafeTransferLib.safeTransfer(baseToken, msg.sender, payAmount);
            return;
        }
        if (msg.sender != _tloadAddr(TSLOT_SWAP_CB)) _revert();
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        address payToken = _tloadAddr(TSLOT_PAY_TOKEN);
        _tstoreAddr(TSLOT_PAY_TOKEN, address(0));
        SafeTransferLib.safeTransfer(payToken, msg.sender, payAmount);
    }

    // ---------------- V2 Flash Swap Callbacks ----------------

    function pancakeCall(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external {
        _handleV2FlashCallback(sender, amount0, amount1, data);
    }

    function uniswapV2Call(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external {
        _handleV2FlashCallback(sender, amount0, amount1, data);
    }

    function biswapCall(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external {
        _handleV2FlashCallback(sender, amount0, amount1, data);
    }

    function _handleV2FlashCallback(address sender, uint256 amount0, uint256 amount1, bytes calldata data) private {
        if (sender != address(this) || msg.sender != _tloadAddr(TSLOT_ROOT_SWAP_POOL)) _revert();
        _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
        address baseToken = _addr(data[1:21]);
        uint256 recvAmt = amount0 > 0 ? amount0 : amount1;
        _execPlan(data, recvAmt);
        SafeTransferLib.safeTransfer(baseToken, msg.sender, _tloadUint(TSLOT_BORROW_AMT));
    }

    // ---------------- Singleton Callbacks (Infinity CL & UniV4) ----------------

    /// @notice PancakeSwap Infinity Vault lock callback.
    function lockAcquired(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != _tloadAddr(TSLOT_SWAP_CB)) _revert();
        _tstoreAddr(TSLOT_SWAP_CB, address(0));

        // data = [20B clPoolManager][20B tokIn][20B tokOut][32B amtIn][1B c0IsNat][3B fee][20B hooks][32B parameters]
        address clPoolManager = _addr(data[0:20]);
        address tokIn = _addr(data[20:40]);
        address tokOut = _addr(data[40:60]);
        uint256 amtIn = _u256(data[60:92]);
        bool c0IsNat = uint8(data[92]) != 0;
        uint24 fee = _u24(data[93:96]);
        address hooks = _addr(data[96:116]);
        bytes32 params = _b32(data[116:148]);
        address wrapped = _tloadAddr(TSLOT_WRAPPED_NATIVE);

        address c0;
        address c1;
        bool zeroForOne;
        if (c0IsNat) {
            c0 = address(0);
            zeroForOne = (tokIn == wrapped);
            c1 = zeroForOne ? tokOut : tokIn;
        } else {
            zeroForOne = tokIn < tokOut;
            (c0, c1) = zeroForOne ? (tokIn, tokOut) : (tokOut, tokIn);
        }

        int256 delta = ICLPoolManager(clPoolManager).swap(
            ICLPoolManager.PoolKey({
                currency0: c0,
                currency1: c1,
                hooks: hooks,
                poolManager: clPoolManager,
                fee: fee,
                parameters: params
            }),
            ICLPoolManager.SwapParams({
                zeroForOne: zeroForOne,
                amountSpecified: -int256(amtIn),
                sqrtPriceLimitX96: zeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE
            }),
            ""
        );
        int128 d0 = int128(delta >> 128);
        int128 d1 = int128(delta);
        int128 inDelta = zeroForOne ? d0 : d1;
        int128 outDelta = zeroForOne ? d1 : d0;
        if (inDelta < 0) {
            _settleCurrency(msg.sender, zeroForOne ? c0 : c1, uint256(uint128(-inDelta)), wrapped);
        }
        if (outDelta > 0) {
            uint256 outAmt = uint256(uint128(outDelta));
            _takeCurrency(msg.sender, zeroForOne ? c1 : c0, outAmt, wrapped);
            _tstoreUint(TSLOT_STEP_OUT, outAmt);
        }
        return "";
    }

    /// @notice Uniswap v4 PoolManager unlock callback (single hop OR batched multi-hop flash accounting).
    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        if (msg.sender != _tloadAddr(TSLOT_SWAP_CB)) _revert();
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        address wrapped = _tloadAddr(TSLOT_WRAPPED_NATIVE);

        if (uint8(data[0]) == MAGIC_V4_SINGLE) {
            // [1B 0x01][20B tokIn][20B tokOut][32B amtIn][27B v4Tail]
            address tokIn = _addr(data[1:21]);
            address tokOut = _addr(data[21:41]);
            uint256 amtIn = _u256(data[41:73]);
            (address c0, address c1, bool zfo, int256 bDelta) = _execUniV4SingleSwap(tokIn, tokOut, amtIn, wrapped, data[73:100]);
            int128 d0 = int128(bDelta >> 128);
            int128 d1 = int128(bDelta);
            int128 inDelta = zfo ? d0 : d1;
            int128 outDelta = zfo ? d1 : d0;
            if (inDelta < 0) {
                _settleCurrency(msg.sender, zfo ? c0 : c1, uint256(uint128(-inDelta)), wrapped);
            }
            if (outDelta > 0) {
                uint256 outAmt = uint256(uint128(outDelta));
                _takeCurrency(msg.sender, zfo ? c1 : c0, outAmt, wrapped);
                _tstoreUint(TSLOT_STEP_OUT, outAmt);
            }
            return "";
        }

        if (uint8(data[0]) != MAGIC_V4_BATCH) _revert();
        // [1B 0xfe][1B nTok][tokTable][1B bCount][stepsSlice]
        uint256 nTok = uint8(data[1]);
        uint256 tokEnd = 2 + nTok * 20;
        bytes calldata tokTable = data[2:tokEnd];
        uint256 bCount = uint8(data[tokEnd]);
        bytes calldata stepsSlice = data[tokEnd + 1:];

        int256[16] memory netDelta;
        int256 nativeDelta;
        uint256 off = 0;

        for (uint256 i = 0; i < bCount;) {
            (uint8 hdr, uint8 inIdx, uint8 outIdx, , uint16 shareBps, , uint256 tailStart) =
                _decodeStepMeta(stepsSlice, off);
            off = tailStart + 27;

            uint256 avail = _tloadUint(TSLOT_DELTA_BASE + inIdx);
            uint256 amtIn = (hdr & 0x40) != 0 ? avail : (avail * uint256(shareBps)) / 10000;
            if (amtIn == 0) {
                unchecked {
                    ++i;
                }
                continue;
            }
            _tstoreUint(TSLOT_DELTA_BASE + inIdx, avail - amtIn);

            bool c0IsNat = uint8(stepsSlice[tailStart]) != 0;
            (address c0, address c1, bool zfo, int256 bDelta) = _execUniV4SingleSwap(
                _tok(tokTable, inIdx),
                _tok(tokTable, outIdx),
                amtIn,
                wrapped,
                stepsSlice[tailStart:off]
            );
            int128 d0 = int128(bDelta >> 128);
            int128 d1 = int128(bDelta);
            int128 inD = zfo ? d0 : d1;
            int128 outD = zfo ? d1 : d0;
            address curIn = zfo ? c0 : c1;
            address curOut = zfo ? c1 : c0;

            if (c0IsNat && curIn == address(0)) {
                nativeDelta += int256(inD);
            } else {
                netDelta[inIdx] += int256(inD);
            }
            if (c0IsNat && curOut == address(0)) {
                nativeDelta += int256(outD);
            } else {
                netDelta[outIdx] += int256(outD);
            }
            if (outD > 0) {
                _tstoreUint(
                    TSLOT_DELTA_BASE + outIdx,
                    _tloadUint(TSLOT_DELTA_BASE + outIdx) + uint256(uint128(outD))
                );
            }
            unchecked {
                ++i;
            }
        }

        // Settle all owed currencies first, then take positive outputs
        if (nativeDelta < 0) {
            _settleCurrency(msg.sender, address(0), uint256(-nativeDelta), wrapped);
        }
        for (uint256 t = 0; t < nTok;) {
            if (netDelta[t] < 0) {
                _settleCurrency(msg.sender, _tok(tokTable, t), uint256(-netDelta[t]), wrapped);
            }
            unchecked {
                ++t;
            }
        }
        if (nativeDelta > 0) {
            _takeCurrency(msg.sender, address(0), uint256(nativeDelta), wrapped);
        }
        for (uint256 t = 0; t < nTok;) {
            if (netDelta[t] > 0) {
                _takeCurrency(msg.sender, _tok(tokTable, t), uint256(netDelta[t]), wrapped);
            }
            unchecked {
                ++t;
            }
        }
        return "";
    }

    function _execUniV4SingleSwap(
        address tokIn,
        address tokOut,
        uint256 amtIn,
        address wrapped,
        bytes calldata tail // 27B: [1B c0IsNative][3B fee][3B int24 tickSpacing][20B hooks]
    ) private returns (address c0, address c1, bool zfo, int256 bDelta) {
        bool c0IsNat = uint8(tail[0]) != 0;
        if (c0IsNat) {
            c0 = address(0);
            zfo = (tokIn == wrapped);
            c1 = zfo ? tokOut : tokIn;
        } else {
            zfo = tokIn < tokOut;
            (c0, c1) = zfo ? (tokIn, tokOut) : (tokOut, tokIn);
        }
        bDelta = IUniV4PoolManager(msg.sender).swap(
            IUniV4PoolManager.PoolKey({
                currency0: c0,
                currency1: c1,
                fee: _u24(tail[1:4]),
                tickSpacing: _i24(tail[4:7]),
                hooks: _addr(tail[7:27])
            }),
            IUniV4PoolManager.SwapParams({
                zeroForOne: zfo,
                amountSpecified: -int256(amtIn),
                sqrtPriceLimitX96: zfo ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE
            }),
            ""
        );
    }

    function _settleCurrency(address singleton, address currency, uint256 oweAmt, address wrapped) private {
        if (currency == address(0)) {
            IWBNB(wrapped).withdraw(oweAmt);
            IUniV4PoolManager(singleton).settle{value: oweAmt}();
        } else {
            IUniV4PoolManager(singleton).sync(currency);
            SafeTransferLib.safeTransfer(currency, singleton, oweAmt);
            IUniV4PoolManager(singleton).settle();
        }
    }

    function _takeCurrency(address singleton, address currency, uint256 takeAmt, address wrapped) private {
        if (currency == address(0)) {
            IUniV4PoolManager(singleton).take(address(0), address(this), takeAmt);
            IWBNB(wrapped).deposit{value: takeAmt}();
        } else {
            IUniV4PoolManager(singleton).take(currency, address(this), takeAmt);
        }
    }

    // ---------------- Compact Bytecode Decoders & Helpers ----------------

    function _decodeStepMeta(bytes calldata steps, uint256 off)
        private
        pure
        returns (
            uint8 hdr,
            uint8 inIdx,
            uint8 outIdx,
            address pool,
            uint16 shareBps,
            uint16 feeNum,
            uint256 tailStart
        )
    {
        hdr = uint8(steps[off]);
        uint8 idxByte = uint8(steps[off + 1]);
        inIdx = idxByte >> 4;
        outIdx = idxByte & 0x0f;
        pool = _addr(steps[off + 2:off + 22]);
        uint256 p = off + 22;
        if ((hdr & 0x40) != 0) {
            shareBps = 10000;
        } else {
            shareBps = _u16(steps[p:p + 2]);
            p += 2;
        }
        if ((hdr & 0x0f) == DEX_UNIV2) {
            feeNum = _u16(steps[p:p + 2]);
            p += 2;
        }
        tailStart = p;
    }

    function _takeDelta(uint256[16] memory delta, uint8 idx, uint8 hdr, uint16 shareBps)
        private
        pure
        returns (uint256 amtIn)
    {
        uint256 avail = delta[idx];
        amtIn = (hdr & 0x40) != 0 ? avail : (avail * uint256(shareBps)) / 10000;
        if (amtIn > 0) {
            unchecked {
                delta[idx] = avail - amtIn;
            }
        }
    }

    function _v2Reserves(address pool) private view returns (uint256 r0, uint256 r1) {
        assembly ("memory-safe") {
            mstore(0x00, shl(224, 0x0902f1ac)) // getReserves()
            let ok := staticcall(gas(), pool, 0x00, 0x04, 0x00, 0x40)
            if or(iszero(ok), lt(returndatasize(), 0x40)) {
                revert(0x00, 0x00)
            }
            r0 := and(mload(0x00), 0xffffffffffffffffffffffffffff)
            r1 := and(mload(0x20), 0xffffffffffffffffffffffffffff)
        }
    }

    function _v2Swap(address pool, uint256 a0, uint256 a1, address to) private {
        assembly ("memory-safe") {
            let m := mload(0x40)
            mstore(m, shl(224, 0x022c0d9f)) // swap(uint256,uint256,address,bytes)
            mstore(add(m, 0x04), a0)
            mstore(add(m, 0x24), a1)
            mstore(add(m, 0x44), and(to, 0xffffffffffffffffffffffffffffffffffffffff))
            mstore(add(m, 0x64), 0x80)
            mstore(add(m, 0x84), 0x00)
            if iszero(call(gas(), pool, 0, m, 0xa4, 0x00, 0x00)) {
                returndatacopy(m, 0x00, returndatasize())
                revert(m, returndatasize())
            }
        }
    }

    function _v3Swap(address pool, address recip, bool zfo, int256 amtSpecified, uint160 sqrtLimit)
        private
        returns (int256 d0, int256 d1)
    {
        assembly ("memory-safe") {
            let m := mload(0x40)
            mstore(m, shl(224, 0x128acb08)) // swap(address,bool,int256,uint160,bytes)
            mstore(add(m, 0x04), and(recip, 0xffffffffffffffffffffffffffffffffffffffff))
            mstore(add(m, 0x24), iszero(iszero(zfo)))
            mstore(add(m, 0x44), amtSpecified)
            mstore(add(m, 0x64), sqrtLimit)
            mstore(add(m, 0x84), 0xa0)
            mstore(add(m, 0xa4), 0x00)
            if iszero(call(gas(), pool, 0, m, 0xc4, 0x00, 0x40)) {
                returndatacopy(m, 0x00, returndatasize())
                revert(m, returndatasize())
            }
            if lt(returndatasize(), 0x40) {
                revert(0x00, 0x00)
            }
            d0 := mload(0x00)
            d1 := mload(0x20)
        }
    }

    function _ensureAllowance(address token, address spender, uint256 needed) private {
        uint256 allowAmt;
        assembly ("memory-safe") {
            let m := mload(0x40)
            mstore(m, shl(224, 0xdd62ed3e)) // allowance(address,address)
            mstore(add(m, 0x04), address())
            mstore(add(m, 0x24), spender)
            let ok := staticcall(gas(), token, m, 0x44, 0x00, 0x20)
            if and(ok, gt(returndatasize(), 0x1f)) {
                allowAmt := mload(0x00)
            }
        }
        if (allowAmt < needed) {
            SafeTransferLib.safeApproveWithRetry(token, spender, type(uint256).max);
        }
    }

    function _tok(bytes calldata tokTable, uint256 idx) private pure returns (address) {
        return _addr(tokTable[idx * 20:idx * 20 + 20]);
    }

    function _addr(bytes calldata s) private pure returns (address a) {
        assembly ("memory-safe") {
            a := shr(96, calldataload(s.offset))
        }
    }

    function _b32(bytes calldata s) private pure returns (bytes32 b) {
        assembly ("memory-safe") {
            b := calldataload(s.offset)
        }
    }

    function _u256(bytes calldata s) private pure returns (uint256 v) {
        assembly ("memory-safe") {
            v := calldataload(s.offset)
        }
    }

    function _u128(bytes calldata s) private pure returns (uint128 v) {
        assembly ("memory-safe") {
            v := shr(128, calldataload(s.offset))
        }
    }

    function _u24(bytes calldata s) private pure returns (uint24 v) {
        assembly ("memory-safe") {
            v := shr(232, calldataload(s.offset))
        }
    }

    function _i24(bytes calldata s) private pure returns (int24 v) {
        assembly ("memory-safe") {
            v := signextend(2, shr(232, calldataload(s.offset)))
        }
    }

    function _u16(bytes calldata s) private pure returns (uint16 v) {
        assembly ("memory-safe") {
            v := shr(240, calldataload(s.offset))
        }
    }

    function _revert() private pure {
        assembly ("memory-safe") {
            revert(0x00, 0x00)
        }
    }

    function _tloadUint(uint256 slot) internal view returns (uint256 val) {
        assembly ("memory-safe") {
            val := tload(slot)
        }
    }

    function _tstoreUint(uint256 slot, uint256 val) internal {
        assembly ("memory-safe") {
            tstore(slot, val)
        }
    }

    function _tloadAddr(uint256 slot) internal view returns (address addr) {
        assembly ("memory-safe") {
            addr := and(tload(slot), 0xffffffffffffffffffffffffffffffffffffffff)
        }
    }

    function _tstoreAddr(uint256 slot, address addr) internal {
        assembly ("memory-safe") {
            tstore(slot, and(addr, 0xffffffffffffffffffffffffffffffffffffffff))
        }
    }

    // ---------------- Rescue & Helpers ----------------

    function rescueTokens(address token, uint256 amount) external onlyOwner {
        SafeTransferLib.safeTransfer(token, OWNER, amount);
    }

    function rescueETH() external onlyOwner {
        SafeTransferLib.safeTransferETH(OWNER, address(this).balance);
    }

    /// @notice Batch-withdraw accumulated multi-token profits (and optionally unwrap WETH/WBNB + sweep ETH) in 1 tx.
    function rescueBatch(address[] calldata tokens, bool unwrapWbnb, address wbnb) external onlyOwner {
        uint256 len = tokens.length;
        for (uint256 i = 0; i < len;) {
            address tok = tokens[i];
            uint256 bal = SafeTransferLib.balanceOf(tok, address(this));
            if (bal > 0) {
                if (unwrapWbnb && tok == wbnb) {
                    IWBNB(wbnb).withdraw(bal);
                } else {
                    SafeTransferLib.safeTransfer(tok, OWNER, bal);
                }
            }
            unchecked {
                ++i;
            }
        }
        uint256 ethBal = address(this).balance;
        if (ethBal > 0) {
            SafeTransferLib.safeTransferETH(OWNER, ethBal);
        }
    }
}



/// @title DagArbDiagExecutor
/// @notice Diagnostic twin of `DagArbExecutor` injected ONLY off-chain via `eth_call` `stateOverrides`
///         (identical storage slot 0/1 layout) to expose exact failing steps, inner DEX reverts,
///         per-token transfer taxes, and flashloan repayment shortfalls.
contract DagArbDiagExecutor {
    // Storage slot 0 MUST match `DagArbExecutor` for safe `stateOverrides` injection over deployed contracts.
    address public owner;
    address private maxTaxToken;
    uint32 private maxTaxPpm;
    uint8 private curStepIdx;

    uint256 private constant TSLOT_LOCK = 1;
    uint256 private constant TSLOT_FLASH_CB = 2;
    uint256 private constant TSLOT_SWAP_CB = 3;
    uint256 private constant TSLOT_PAY_TOKEN = 4;
    uint256 private constant TSLOT_ROOT_SWAP_POOL = 5;
    uint256 private constant TSLOT_WRAPPED_NATIVE = 6;

    uint160 private constant MIN_SQRT_RATIO_PLUS_ONE = 4295128740;
    uint160 private constant MAX_SQRT_RATIO_MINUS_ONE =
        1461446703485210103287273052203988822378723970341;
    uint128 private constant METRIC_MIN_PRICE_LIMIT = 1;
    uint128 private constant METRIC_MAX_PRICE_LIMIT = type(uint128).max;

    uint8 public constant FLASH_FUNDED = 0;
    uint8 public constant FLASH_MORPHO = 1;
    uint8 public constant FLASH_UNIV3 = 2;
    uint8 public constant FLASH_PANCAKEV3 = 3;
    uint8 public constant FLASH_V3_SWAP = 4;
    uint8 public constant FLASH_V2_SWAP = 5;
    uint8 public constant FLASH_METRIC_SWAP = 6;
    uint8 public constant FLASH_BALANCER_V2 = 7;

    uint8 public constant DEX_UNIV2 = 0;
    uint8 public constant DEX_SOLIDLY = 1;
    uint8 public constant DEX_UNIV3 = 2;
    uint8 public constant DEX_PANCAKEV3 = 3;
    uint8 public constant DEX_INFINITY_CL = 4;
    uint8 public constant DEX_UNIV4 = 5;
    uint8 public constant DEX_METRIC_OMM = 6;
    uint8 public constant DEX_BALANCER_V2 = 7;

    error DiagStepTransferFailed(uint8 stepIdx, address pool, address token, uint256 amount, bytes reason);
    error DiagStepZeroReceived(uint8 stepIdx, address pool, address token);
    error DiagStepSwapReverted(uint8 stepIdx, address pool, uint8 dexKind, bytes reason);
    error DiagFlashRepayShortfall(
        uint256 borrowAmount,
        uint256 repayRequired,
        uint256 finalBalance,
        address maxTaxToken,
        uint32 maxTaxPpm
    );
    error DiagProfitBelowMin(uint256 netProfit, uint256 minNetProfit);

    event ArbExecuted(
        address indexed baseToken,
        uint256 borrowAmount,
        uint256 netProfit,
        uint256 bribeWei,
        uint256 gasUsed
    );

    modifier onlyOwner() {
        require(msg.sender == owner, "NOT_OWNER");
        _;
    }

    constructor(address _owner) {
        require(_owner != address(0), "ZERO_OWNER");
        owner = _owner;
    }

    receive() external payable {}

    function executeArb(DagArbExecutor.ArbParams calldata params)
        external
        payable
        onlyOwner
        returns (uint256 netProfit, uint256 gasUsed)
    {
        require(_tloadUint(TSLOT_LOCK) == 0, "LOCKED");
        require(params.steps.length > 0, "EMPTY_DAG");
        uint256 gasStart = gasleft();
        _tstoreUint(TSLOT_LOCK, 1);
        _tstoreAddr(TSLOT_WRAPPED_NATIVE, params.wrappedNative);
        maxTaxToken = address(0);
        maxTaxPpm = 0;
        curStepIdx = 0;

        uint256 balBefore = IERC20(params.baseToken).balanceOf(address(this));

        if (params.flashMode == FLASH_FUNDED) {
            _executeDag(params.steps);
        } else if (params.flashMode == FLASH_MORPHO) {
            _tstoreAddr(TSLOT_FLASH_CB, params.flashSource);
            bytes memory cbData = abi.encode(params.baseToken, params.steps);
            IMorpho(params.flashSource).flashLoan(params.baseToken, params.borrowAmount, cbData);
        } else if (params.flashMode == FLASH_UNIV3 || params.flashMode == FLASH_PANCAKEV3) {
            address t0 = IV3Pool(params.flashSource).token0();
            (uint256 amt0, uint256 amt1) =
                params.baseToken == t0 ? (params.borrowAmount, uint256(0)) : (uint256(0), params.borrowAmount);
            _tstoreAddr(TSLOT_FLASH_CB, params.flashSource);
            bytes memory cbData = abi.encode(params.baseToken, params.borrowAmount, params.steps);
            IV3Pool(params.flashSource).flash(address(this), amt0, amt1, cbData);
        } else if (params.flashMode == FLASH_V3_SWAP) {
            require(params.steps.length >= 2, "V3_FLASH_STEPS");
            DagArbExecutor.DagStep memory s0 = params.steps[0];
            bool zfo = s0.tokenIn < s0.tokenOut;
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, s0.pool);
            uint160 limit = zfo ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;
            (bool okSwap, bytes memory swapRet) = s0.pool.call(
                abi.encodeWithSelector(
                    IV3Pool.swap.selector,
                    s0.recipient,
                    zfo,
                    int256(params.borrowAmount),
                    limit,
                    abi.encode(params.baseToken, params.steps)
                )
            );
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
            if (!okSwap) {
                _bubbleStepRevert(0, s0.pool, s0.dexKind, swapRet);
            }
        } else if (params.flashMode == FLASH_V2_SWAP) {
            require(params.steps.length >= 2, "V2_FLASH_STEPS");
            DagArbExecutor.DagStep memory s0 = params.steps[0];
            bool zfo = s0.tokenIn < s0.tokenOut;
            (uint112 r0, uint112 r1,) = IUniV2Pair(s0.pool).getReserves();
            (uint256 rIn, uint256 rOut) = zfo ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
            uint256 amtInWithFee = params.borrowAmount * uint256(s0.feeNumerator);
            uint256 amtOut = (amtInWithFee * rOut) / (rIn * 10000 + amtInWithFee);
            require(amtOut > 0, "V2_FLASH_ZERO");
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, s0.pool);
            (bool okSwap, bytes memory swapRet) = s0.pool.call(
                abi.encodeWithSelector(
                    IUniV2Pair.swap.selector,
                    zfo ? 0 : amtOut,
                    zfo ? amtOut : 0,
                    s0.recipient,
                    abi.encode(params.baseToken, params.borrowAmount, params.steps)
                )
            );
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
            if (!okSwap) {
                _bubbleStepRevert(0, s0.pool, s0.dexKind, swapRet);
            }
        } else if (params.flashMode == FLASH_METRIC_SWAP) {
            require(params.steps.length >= 2, "METRIC_FLASH_STEPS");
            require(params.borrowAmount <= uint256(uint128(type(int128).max)), "METRIC_AMT_OVF");
            DagArbExecutor.DagStep memory s0 = params.steps[0];
            bool metricZfo = s0.extraData.length > 0 ? (s0.extraData[0] != 0) : (s0.tokenIn < s0.tokenOut);
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, s0.pool);
            (bool okSwap, bytes memory swapRet) = s0.pool.call(
                abi.encodeWithSelector(
                    IMetricOmmPool.swap.selector,
                    s0.recipient,
                    metricZfo,
                    int128(int256(params.borrowAmount)),
                    metricZfo ? METRIC_MIN_PRICE_LIMIT : METRIC_MAX_PRICE_LIMIT,
                    abi.encode(params.baseToken, params.steps),
                    bytes("")
                )
            );
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
            if (!okSwap) {
                _bubbleStepRevert(0, s0.pool, s0.dexKind, swapRet);
            }
        } else if (params.flashMode == FLASH_BALANCER_V2) {
            _tstoreAddr(TSLOT_FLASH_CB, params.flashSource);
            address[] memory tokens = new address[](1);
            tokens[0] = params.baseToken;
            uint256[] memory amounts = new uint256[](1);
            amounts[0] = params.borrowAmount;
            IBalancerV2Vault(params.flashSource).flashLoan(
                address(this),
                tokens,
                amounts,
                abi.encode(params.baseToken, params.steps)
            );
        } else {
            revert("BAD_FLASH_MODE");
        }

        uint256 balAfter = IERC20(params.baseToken).balanceOf(address(this));
        uint256 gained = balAfter > balBefore ? balAfter - balBefore : 0;
        if (gained < params.minNetProfit) {
            revert DiagProfitBelowMin(gained, params.minNetProfit);
        }
        netProfit = gained;

        uint256 bribeWei = 0;
        if (params.wrappedNative != address(0) && params.baseToken == params.wrappedNative) {
            if (params.bribeBps > 0 && netProfit > params.minNetProfit) {
                bribeWei = ((netProfit - params.minNetProfit) * uint256(params.bribeBps)) / 10000;
            }
            if (params.payoutToOwner) {
                if (netProfit > 0) {
                    IWBNB(params.wrappedNative).withdraw(netProfit);
                }
                if (bribeWei > 0) {
                    address bTarget = params.bribeRecipient == address(0) ? block.coinbase : params.bribeRecipient;
                    (bool okB,) = bTarget.call{value: bribeWei}("");
                    require(okB, "BRIBE_FAIL");
                }
                uint256 ownerBnb = netProfit - bribeWei + msg.value;
                if (ownerBnb > 0) {
                    (bool okO,) = owner.call{value: ownerBnb}("");
                    require(okO, "OWNER_PAY_FAIL");
                }
            } else if (bribeWei > 0) {
                IWBNB(params.wrappedNative).withdraw(bribeWei);
                address bTarget = params.bribeRecipient == address(0) ? block.coinbase : params.bribeRecipient;
                (bool okB,) = bTarget.call{value: bribeWei}("");
                require(okB, "BRIBE_FAIL");
            }
        } else {
            if (msg.value > 0) {
                bribeWei = msg.value;
                address bTarget = params.bribeRecipient == address(0) ? block.coinbase : params.bribeRecipient;
                (bool okB,) = bTarget.call{value: bribeWei}("");
                require(okB, "BRIBE_FAIL");
            }
            if (params.payoutToOwner && netProfit > 0) {
                _tryTransfer(254, address(this), params.baseToken, owner, netProfit);
            }
        }

        _tstoreAddr(TSLOT_FLASH_CB, address(0));
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        _tstoreAddr(TSLOT_PAY_TOKEN, address(0));
        _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
        _tstoreAddr(TSLOT_WRAPPED_NATIVE, address(0));
        _tstoreUint(TSLOT_LOCK, 0);

        gasUsed = gasStart - gasleft();
        emit ArbExecuted(params.baseToken, params.borrowAmount, netProfit, bribeWei, gasUsed);
    }

    function _executeDag(DagArbExecutor.DagStep[] memory steps) internal {
        _executeDagFrom(steps, 0);
    }

    function _executeDagFrom(DagArbExecutor.DagStep[] memory steps, uint256 startIdx) internal {
        uint256 len = steps.length;
        for (uint256 i = startIdx; i < len;) {
            DagArbExecutor.DagStep memory step = steps[i];
            bool zeroForOne = step.tokenIn < step.tokenOut;
            if (step.dexKind == DEX_UNIV2 || step.dexKind == DEX_SOLIDLY) {
                _executeV2LikeStep(uint8(i), step, zeroForOne);
            } else if (step.dexKind == DEX_UNIV3 || step.dexKind == DEX_PANCAKEV3) {
                _executeV3Step(uint8(i), step, zeroForOne);
            } else if (step.dexKind == DEX_INFINITY_CL || step.dexKind == DEX_UNIV4) {
                _executeSingletonStep(uint8(i), step);
            } else if (step.dexKind == DEX_METRIC_OMM) {
                _executeMetricStep(uint8(i), step);
            } else if (step.dexKind == DEX_BALANCER_V2) {
                _executeBalancerV2Step(uint8(i), step);
            } else {
                revert("BAD_DEX_KIND");
            }
            unchecked {
                ++i;
            }
        }
    }

    function _executeV2LikeStep(uint8 stepIdx, DagArbExecutor.DagStep memory step, bool zeroForOne) internal {
        uint256 rIn;
        uint256 rOut;
        if (step.dexKind == DEX_UNIV2) {
            (uint112 r0, uint112 r1,) = IUniV2Pair(step.pool).getReserves();
            (rIn, rOut) = zeroForOne ? (uint256(r0), uint256(r1)) : (uint256(r1), uint256(r0));
        } else {
            (uint256 r0, uint256 r1,) = ISolidlyPair(step.pool).getReserves();
            rIn = zeroForOne ? r0 : r1;
        }

        if (!step.inputInPool) {
            uint256 bal = IERC20(step.tokenIn).balanceOf(address(this));
            uint256 amtIn = (bal * uint256(step.shareBps)) / 10000;
            if (amtIn == 0) {
                revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenIn);
            }
            uint256 prePoolBal = IERC20(step.tokenIn).balanceOf(step.pool);
            _tryTransfer(stepIdx, step.pool, step.tokenIn, step.pool, amtIn);
            uint256 postPoolBal = IERC20(step.tokenIn).balanceOf(step.pool);
            uint256 deliveredIn = postPoolBal > prePoolBal ? postPoolBal - prePoolBal : 0;
            if (deliveredIn == 0) {
                revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenIn);
            }
            _recordTax(step.tokenIn, amtIn, deliveredIn);
        }

        uint256 poolBal = IERC20(step.tokenIn).balanceOf(step.pool);
        if (poolBal <= rIn) {
            revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenIn);
        }
        uint256 actualIn = poolBal - rIn;
        uint256 amtOut;
        if (step.dexKind == DEX_UNIV2) {
            uint256 amtInWithFee = actualIn * uint256(step.feeNumerator);
            amtOut = (amtInWithFee * rOut) / (rIn * 10000 + amtInWithFee);
        } else {
            (bool okQ, bytes memory retQ) = step.pool.staticcall(
                abi.encodeWithSelector(ISolidlyPair.getAmountOut.selector, actualIn, step.tokenIn)
            );
            if (!okQ || retQ.length < 32) {
                revert DiagStepSwapReverted(stepIdx, step.pool, step.dexKind, retQ);
            }
            amtOut = abi.decode(retQ, (uint256));
        }

        if (amtOut == 0) {
            revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenOut);
        }

        uint256 preOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
        (bool okSwap, bytes memory swapRet) = step.pool.call(
            abi.encodeWithSelector(
                IUniV2Pair.swap.selector,
                zeroForOne ? uint256(0) : amtOut,
                zeroForOne ? amtOut : uint256(0),
                step.recipient,
                bytes("")
            )
        );
        if (!okSwap) {
            revert DiagStepSwapReverted(stepIdx, step.pool, step.dexKind, swapRet);
        }
        uint256 postOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
        uint256 gotOut = postOutBal > preOutBal ? postOutBal - preOutBal : 0;
        if (gotOut == 0) {
            revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenOut);
        }
        _recordTax(step.tokenOut, amtOut, gotOut);
    }

    function _bubbleStepRevert(uint8 stepIdx, address pool, uint8 dexKind, bytes memory swapRet) internal pure {
        if (swapRet.length >= 4) {
            bytes4 sel = bytes4(swapRet);
            if (
                sel == DiagStepTransferFailed.selector ||
                sel == DiagStepZeroReceived.selector ||
                sel == DiagStepSwapReverted.selector ||
                sel == DiagFlashRepayShortfall.selector
            ) {
                assembly {
                    revert(add(swapRet, 32), mload(swapRet))
                }
            }
        }
        revert DiagStepSwapReverted(stepIdx, pool, dexKind, swapRet);
    }

    function _executeV3Step(uint8 stepIdx, DagArbExecutor.DagStep memory step, bool zeroForOne) internal {
        uint256 bal = IERC20(step.tokenIn).balanceOf(address(this));
        uint256 amtIn = (bal * uint256(step.shareBps)) / 10000;
        if (amtIn == 0) {
            revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenIn);
        }

        curStepIdx = stepIdx;
        _tstoreAddr(TSLOT_SWAP_CB, step.pool);
        _tstoreAddr(TSLOT_PAY_TOKEN, step.tokenIn);
        uint256 preOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
        uint160 limit = zeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;
        (bool okSwap, bytes memory swapRet) = step.pool.call(
            abi.encodeWithSelector(IV3Pool.swap.selector, step.recipient, zeroForOne, int256(amtIn), limit, bytes(""))
        );
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        _tstoreAddr(TSLOT_PAY_TOKEN, address(0));

        if (!okSwap) {
            _bubbleStepRevert(stepIdx, step.pool, step.dexKind, swapRet);
        }

        if (swapRet.length >= 64) {
            (int256 a0, int256 a1) = abi.decode(swapRet, (int256, int256));
            int256 outSigned = zeroForOne ? -a1 : -a0;
            uint256 expectedOut = outSigned > 0 ? uint256(outSigned) : 0;
            uint256 postOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
            uint256 gotOut = postOutBal > preOutBal ? postOutBal - preOutBal : 0;
            if (expectedOut > 0 && gotOut == 0) {
                revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenOut);
            }
            _recordTax(step.tokenOut, expectedOut, gotOut);
        }
    }

    function _executeMetricStep(uint8 stepIdx, DagArbExecutor.DagStep memory step) internal {
        uint256 bal = IERC20(step.tokenIn).balanceOf(address(this));
        uint256 amtIn = (bal * uint256(step.shareBps)) / 10000;
        if (amtIn == 0) {
            revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenIn);
        }
        require(amtIn <= uint256(uint128(type(int128).max)), "METRIC_AMT_OVF");

        bool metricZfo = step.extraData.length > 0 ? (step.extraData[0] != 0) : (step.tokenIn < step.tokenOut);
        curStepIdx = stepIdx;
        _tstoreAddr(TSLOT_SWAP_CB, step.pool);
        _tstoreAddr(TSLOT_PAY_TOKEN, step.tokenIn);
        uint256 preOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
        (bool okSwap, bytes memory swapRet) = step.pool.call(
            abi.encodeWithSelector(
                IMetricOmmPool.swap.selector,
                step.recipient,
                metricZfo,
                int128(int256(amtIn)),
                metricZfo ? METRIC_MIN_PRICE_LIMIT : METRIC_MAX_PRICE_LIMIT,
                bytes(""),
                bytes("")
            )
        );
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        _tstoreAddr(TSLOT_PAY_TOKEN, address(0));

        if (!okSwap) {
            _bubbleStepRevert(stepIdx, step.pool, step.dexKind, swapRet);
        }

        if (swapRet.length >= 64) {
            (int256 a0, int256 a1) = abi.decode(swapRet, (int256, int256));
            int256 outSigned = metricZfo ? -a1 : -a0;
            uint256 expectedOut = outSigned > 0 ? uint256(outSigned) : 0;
            uint256 postOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
            uint256 gotOut = postOutBal > preOutBal ? postOutBal - preOutBal : 0;
            if (expectedOut > 0 && gotOut == 0) {
                revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenOut);
            }
            _recordTax(step.tokenOut, expectedOut, gotOut);
        }
    }

    function _executeBalancerV2Step(uint8 stepIdx, DagArbExecutor.DagStep memory step) internal {
        uint256 bal = IERC20(step.tokenIn).balanceOf(address(this));
        uint256 amtIn = (bal * uint256(step.shareBps)) / 10000;
        if (amtIn == 0) {
            revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenIn);
        }
        if (IERC20(step.tokenIn).allowance(address(this), step.pool) < amtIn) {
            _forceApprove(step.tokenIn, step.pool, type(uint256).max);
        }
        require(step.extraData.length >= 32, "BAD_BAL_POOLID");
        bytes32 poolId = abi.decode(step.extraData, (bytes32));

        curStepIdx = stepIdx;
        uint256 preOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
        (bool okSwap, bytes memory swapRet) = step.pool.call(
            abi.encodeWithSelector(
                IBalancerV2Vault.swap.selector,
                IBalancerV2Vault.SingleSwap({
                    poolId: poolId,
                    kind: IBalancerV2Vault.SwapKind.GIVEN_IN,
                    assetIn: step.tokenIn,
                    assetOut: step.tokenOut,
                    amount: amtIn,
                    userData: ""
                }),
                IBalancerV2Vault.FundManagement({
                    sender: address(this),
                    fromInternalBalance: false,
                    recipient: payable(step.recipient),
                    toInternalBalance: false
                }),
                0,
                type(uint256).max
            )
        );
        if (!okSwap) {
            _bubbleStepRevert(stepIdx, step.pool, step.dexKind, swapRet);
        }

        if (swapRet.length >= 32) {
            uint256 expectedOut = abi.decode(swapRet, (uint256));
            uint256 postOutBal = IERC20(step.tokenOut).balanceOf(step.recipient);
            uint256 gotOut = postOutBal > preOutBal ? postOutBal - preOutBal : 0;
            if (expectedOut > 0 && gotOut == 0) {
                revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenOut);
            }
            _recordTax(step.tokenOut, expectedOut, gotOut);
        }
    }

    function _executeSingletonStep(uint8 stepIdx, DagArbExecutor.DagStep memory step) internal {
        uint256 bal = IERC20(step.tokenIn).balanceOf(address(this));
        uint256 amtIn = (bal * uint256(step.shareBps)) / 10000;
        if (amtIn == 0) {
            revert DiagStepZeroReceived(stepIdx, step.pool, step.tokenIn);
        }
        curStepIdx = stepIdx;
        address target = step.dexKind == DEX_INFINITY_CL ? step.recipient : step.pool;
        bytes memory callPayload = step.dexKind == DEX_INFINITY_CL
            ? abi.encodeWithSelector(
                IInfinityVault.lock.selector,
                abi.encode(step.pool, step.tokenIn, amtIn, step.extraData)
            )
            : abi.encodeWithSelector(
                IUniV4PoolManager.unlock.selector,
                abi.encode(step.tokenIn, amtIn, step.extraData)
            );
        _tstoreAddr(TSLOT_SWAP_CB, target);
        (bool ok, bytes memory ret) = target.call(callPayload);
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        if (!ok) {
            revert DiagStepSwapReverted(stepIdx, step.pool, step.dexKind, ret);
        }
    }

    function lockAcquired(bytes calldata data) external returns (bytes memory) {
        require(_tloadUint(TSLOT_LOCK) == 1 && msg.sender == _tloadAddr(TSLOT_SWAP_CB), "UNAUTH_CB");
        _tstoreAddr(TSLOT_SWAP_CB, address(0));

        (address clPoolManager, address tokenIn, uint256 amtIn, bytes memory keyBytes) =
            abi.decode(data, (address, address, uint256, bytes));
        ICLPoolManager.PoolKey memory key = abi.decode(keyBytes, (ICLPoolManager.PoolKey));
        address wrapped = _tloadAddr(TSLOT_WRAPPED_NATIVE);
        bool zeroForOne = (tokenIn == key.currency0) || (key.currency0 == address(0) && tokenIn == wrapped);
        uint160 limit = zeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;
        int256 delta = ICLPoolManager(clPoolManager).swap(
            key,
            ICLPoolManager.SwapParams({
                zeroForOne: zeroForOne,
                amountSpecified: -int256(amtIn),
                sqrtPriceLimitX96: limit
            }),
            ""
        );
        _settleAndTakeSingleton(
            msg.sender,
            zeroForOne ? key.currency0 : key.currency1,
            zeroForOne ? key.currency1 : key.currency0,
            delta,
            zeroForOne,
            wrapped
        );
        return "";
    }

    function unlockCallback(bytes calldata data) external returns (bytes memory) {
        require(_tloadUint(TSLOT_LOCK) == 1 && msg.sender == _tloadAddr(TSLOT_SWAP_CB), "UNAUTH_CB");
        _tstoreAddr(TSLOT_SWAP_CB, address(0));

        (address tokenIn, uint256 amtIn, bytes memory keyBytes) = abi.decode(data, (address, uint256, bytes));
        IUniV4PoolManager.PoolKey memory key = abi.decode(keyBytes, (IUniV4PoolManager.PoolKey));
        address wrapped = _tloadAddr(TSLOT_WRAPPED_NATIVE);
        bool zeroForOne = (tokenIn == key.currency0) || (key.currency0 == address(0) && tokenIn == wrapped);
        uint160 limit = zeroForOne ? MIN_SQRT_RATIO_PLUS_ONE : MAX_SQRT_RATIO_MINUS_ONE;
        int256 delta = IUniV4PoolManager(msg.sender).swap(
            key,
            IUniV4PoolManager.SwapParams({
                zeroForOne: zeroForOne,
                amountSpecified: -int256(amtIn),
                sqrtPriceLimitX96: limit
            }),
            ""
        );
        _settleAndTakeSingleton(
            msg.sender,
            zeroForOne ? key.currency0 : key.currency1,
            zeroForOne ? key.currency1 : key.currency0,
            delta,
            zeroForOne,
            wrapped
        );
        return "";
    }

    function _settleAndTakeSingleton(
        address singleton,
        address currencyIn,
        address currencyOut,
        int256 delta,
        bool zeroForOne,
        address wrapped
    ) internal {
        int128 d0 = int128(delta >> 128);
        int128 d1 = int128(delta);
        int128 inDelta = zeroForOne ? d0 : d1;
        int128 outDelta = zeroForOne ? d1 : d0;
        if (inDelta < 0) {
            uint256 oweAmt = uint256(uint128(-inDelta));
            if (currencyIn == address(0)) {
                IWBNB(wrapped).withdraw(oweAmt);
                IUniV4PoolManager(singleton).settle{value: oweAmt}();
            } else {
                IUniV4PoolManager(singleton).sync(currencyIn);
                _tryTransfer(curStepIdx, singleton, currencyIn, singleton, oweAmt);
                IUniV4PoolManager(singleton).settle();
            }
        }
        if (outDelta > 0) {
            uint256 takeAmt = uint256(uint128(outDelta));
            if (currencyOut == address(0)) {
                IUniV4PoolManager(singleton).take(address(0), address(this), takeAmt);
                IWBNB(wrapped).deposit{value: takeAmt}();
            } else {
                IUniV4PoolManager(singleton).take(currencyOut, address(this), takeAmt);
            }
        }
    }

    function _recordTax(address token, uint256 expectedAmt, uint256 actualAmt) internal {
        if (expectedAmt > 0 && actualAmt < expectedAmt) {
            uint256 lost = ((expectedAmt - actualAmt) * 1_000_000) / expectedAmt;
            if (lost > 999_999) lost = 999_999;
            if (uint32(lost) > maxTaxPpm) {
                maxTaxPpm = uint32(lost);
                maxTaxToken = token;
            }
        }
    }

    function _tryTransfer(uint8 stepIdx, address pool, address token, address to, uint256 value) internal {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, value));
        if (!ok || (ret.length > 0 && (ret.length < 32 || !abi.decode(ret, (bool))))) {
            revert DiagStepTransferFailed(stepIdx, pool, token, value, ret);
        }
    }

    function _forceApprove(address token, address spender, uint256 value) internal {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, value));
        if (!ok || (ret.length > 0 && (ret.length < 32 || !abi.decode(ret, (bool))))) {
            (bool ok0, bytes memory ret0) =
                token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, 0));
            require(ok0 && (ret0.length == 0 || abi.decode(ret0, (bool))), "APPROVE_ZERO_FAIL");
            (bool ok1, bytes memory ret1) =
                token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, value));
            require(ok1 && (ret1.length == 0 || abi.decode(ret1, (bool))), "APPROVE_FAIL");
        }
    }

    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external {
        _handleMorphoFlashLoan(assets, data);
    }

    function onMoolahFlashLoan(uint256 assets, bytes calldata data) external {
        _handleMorphoFlashLoan(assets, data);
    }

    function _handleMorphoFlashLoan(uint256 assets, bytes calldata data) internal {
        require(_tloadUint(TSLOT_LOCK) == 1 && msg.sender == _tloadAddr(TSLOT_FLASH_CB), "UNAUTH_CB");
        _tstoreAddr(TSLOT_FLASH_CB, address(0));
        (address baseToken, DagArbExecutor.DagStep[] memory steps) =
            abi.decode(data, (address, DagArbExecutor.DagStep[]));
        _executeDag(steps);
        uint256 finalBal = IERC20(baseToken).balanceOf(address(this));
        if (finalBal < assets) {
            revert DiagFlashRepayShortfall(assets, assets, finalBal, maxTaxToken, maxTaxPpm);
        }
        if (IERC20(baseToken).allowance(address(this), msg.sender) < assets) {
            _forceApprove(baseToken, msg.sender, type(uint256).max);
        }
    }

    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        _handleV3FlashCallback(fee0, fee1, data);
    }

    function pancakeV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external {
        _handleV3FlashCallback(fee0, fee1, data);
    }

    function _handleV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) internal {
        require(_tloadUint(TSLOT_LOCK) == 1 && msg.sender == _tloadAddr(TSLOT_FLASH_CB), "UNAUTH_CB");
        _tstoreAddr(TSLOT_FLASH_CB, address(0));
        (address baseToken, uint256 borrowAmount, DagArbExecutor.DagStep[] memory steps) =
            abi.decode(data, (address, uint256, DagArbExecutor.DagStep[]));
        _executeDag(steps);
        uint256 fee = fee0 > 0 ? fee0 : fee1;
        uint256 repayReq = borrowAmount + fee;
        uint256 finalBal = IERC20(baseToken).balanceOf(address(this));
        if (finalBal < repayReq) {
            revert DiagFlashRepayShortfall(borrowAmount, repayReq, finalBal, maxTaxToken, maxTaxPpm);
        }
        _tryTransfer(255, msg.sender, baseToken, msg.sender, repayReq);
    }

    function receiveFlashLoan(
        address[] calldata tokens,
        uint256[] calldata amounts,
        uint256[] calldata feeAmounts,
        bytes calldata userData
    ) external {
        require(_tloadUint(TSLOT_LOCK) == 1 && msg.sender == _tloadAddr(TSLOT_FLASH_CB), "UNAUTH_CB");
        _tstoreAddr(TSLOT_FLASH_CB, address(0));
        (address baseToken, DagArbExecutor.DagStep[] memory steps) =
            abi.decode(userData, (address, DagArbExecutor.DagStep[]));
        require(tokens[0] == baseToken, "BAD_BAL_TOKEN");
        _executeDag(steps);
        uint256 repayReq = amounts[0] + feeAmounts[0];
        uint256 finalBal = IERC20(baseToken).balanceOf(address(this));
        if (finalBal < repayReq) {
            revert DiagFlashRepayShortfall(amounts[0], repayReq, finalBal, maxTaxToken, maxTaxPpm);
        }
        _tryTransfer(255, msg.sender, baseToken, msg.sender, repayReq);
    }

    function uniswapV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _handleV3SwapCallback(amount0Delta, amount1Delta, data);
    }

    function pancakeV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _handleV3SwapCallback(amount0Delta, amount1Delta, data);
    }

    function metricOmmSwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) external {
        _handleV3SwapCallback(amount0Delta, amount1Delta, data);
    }

    function _handleV3SwapCallback(int256 amount0Delta, int256 amount1Delta, bytes calldata data) internal {
        uint256 payAmount = amount0Delta > 0 ? uint256(amount0Delta) : uint256(amount1Delta);
        if (data.length > 0 && msg.sender == _tloadAddr(TSLOT_ROOT_SWAP_POOL)) {
            require(_tloadUint(TSLOT_LOCK) == 1, "UNAUTH_CB");
            _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
            (address baseToken, DagArbExecutor.DagStep[] memory steps) =
                abi.decode(data, (address, DagArbExecutor.DagStep[]));
            _executeDagFrom(steps, 1);
            uint256 finalBal = IERC20(baseToken).balanceOf(address(this));
            if (finalBal < payAmount) {
                revert DiagFlashRepayShortfall(payAmount, payAmount, finalBal, maxTaxToken, maxTaxPpm);
            }
            _tryTransfer(255, msg.sender, baseToken, msg.sender, payAmount);
            return;
        }

        require(_tloadUint(TSLOT_LOCK) == 1 && msg.sender == _tloadAddr(TSLOT_SWAP_CB), "UNAUTH_CB");
        _tstoreAddr(TSLOT_SWAP_CB, address(0));
        address payToken = _tloadAddr(TSLOT_PAY_TOKEN);
        _tstoreAddr(TSLOT_PAY_TOKEN, address(0));
        _tryTransfer(curStepIdx, msg.sender, payToken, msg.sender, payAmount);
    }

    function pancakeCall(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external {
        _handleV2FlashCallback(sender, amount0, amount1, data);
    }

    function uniswapV2Call(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external {
        _handleV2FlashCallback(sender, amount0, amount1, data);
    }

    function biswapCall(address sender, uint256 amount0, uint256 amount1, bytes calldata data) external {
        _handleV2FlashCallback(sender, amount0, amount1, data);
    }

    function _handleV2FlashCallback(address sender, uint256, uint256, bytes calldata data) internal {
        require(
            _tloadUint(TSLOT_LOCK) == 1 && sender == address(this) && msg.sender == _tloadAddr(TSLOT_ROOT_SWAP_POOL),
            "UNAUTH_CB"
        );
        _tstoreAddr(TSLOT_ROOT_SWAP_POOL, address(0));
        (address baseToken, uint256 borrowAmount, DagArbExecutor.DagStep[] memory steps) =
            abi.decode(data, (address, uint256, DagArbExecutor.DagStep[]));
        _executeDagFrom(steps, 1);
        uint256 finalBal = IERC20(baseToken).balanceOf(address(this));
        if (finalBal < borrowAmount) {
            revert DiagFlashRepayShortfall(borrowAmount, borrowAmount, finalBal, maxTaxToken, maxTaxPpm);
        }
        _tryTransfer(255, msg.sender, baseToken, msg.sender, borrowAmount);
    }

    function _tloadUint(uint256 slot) internal view returns (uint256 val) {
        assembly {
            val := tload(slot)
        }
    }

    function _tstoreUint(uint256 slot, uint256 val) internal {
        assembly {
            tstore(slot, val)
        }
    }

    function _tloadAddr(uint256 slot) internal view returns (address addr) {
        assembly {
            addr := and(tload(slot), 0xffffffffffffffffffffffffffffffffffffffff)
        }
    }

    function _tstoreAddr(uint256 slot, address addr) internal {
        assembly {
            tstore(slot, and(addr, 0xffffffffffffffffffffffffffffffffffffffff))
        }
    }
}
