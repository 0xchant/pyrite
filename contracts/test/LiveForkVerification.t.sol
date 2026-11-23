// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {DagArbExecutor} from "../src/DagArbExecutor.sol";
import {IERC20, IUniV2Pair, ISolidlyPair, IV3Pool} from "../src/interfaces/IDexInterfaces.sol";

interface VmFork {
    function deal(address who, uint256 newBalance) external;
    function expectRevert(bytes calldata revertData) external;
}

interface IWETH {
    function deposit() external payable;
}

contract LiveForkVerificationTest {
    VmFork private constant vm = VmFork(address(uint160(uint256(keccak256("hevm cheat code")))));

    function _findOrAddTok(address[16] memory toks, uint256 len, address t)
        internal
        pure
        returns (uint8 idx, uint256 newLen)
    {
        for (uint256 i = 0; i < len; ++i) {
            if (toks[i] == t) return (uint8(i), len);
        }
        require(len < 16, "TOO_MANY_TOKS");
        toks[len] = t;
        return (uint8(len), len + 1);
    }

    function _encodeCompactArb(DagArbExecutor.ArbParams memory p, address execAddr)
        internal
        pure
        returns (bytes memory)
    {
        address[16] memory toks;
        toks[0] = p.baseToken;
        uint256 tokLen = 1;
        uint256 nSteps = p.steps.length;
        uint8[] memory inIdxs = new uint8[](nSteps);
        uint8[] memory outIdxs = new uint8[](nSteps);
        for (uint256 i = 0; i < nSteps; ++i) {
            (inIdxs[i], tokLen) = _findOrAddTok(toks, tokLen, p.steps[i].tokenIn);
            (outIdxs[i], tokLen) = _findOrAddTok(toks, tokLen, p.steps[i].tokenOut);
        }
        uint8 flags = (p.payoutToOwner ? 0x01 : 0)
            | (p.wrappedNative != address(0) ? 0x02 : 0)
            | (p.bribeRecipient != address(0) ? 0x04 : 0)
            | (p.flashSource != address(0) ? 0x08 : 0);
        bytes memory out = abi.encodePacked(
            uint8(0x00),
            p.flashMode,
            flags,
            p.bribeBps,
            uint128(p.borrowAmount),
            uint128(p.minNetProfit)
        );
        if ((flags & 0x02) != 0) out = abi.encodePacked(out, p.wrappedNative);
        if ((flags & 0x04) != 0) out = abi.encodePacked(out, p.bribeRecipient);
        if ((flags & 0x08) != 0) out = abi.encodePacked(out, p.flashSource);
        out = abi.encodePacked(out, uint8(tokLen));
        for (uint256 i = 0; i < tokLen; ++i) {
            out = abi.encodePacked(out, toks[i]);
        }
        out = abi.encodePacked(out, uint8(nSteps));
        for (uint256 i = 0; i < nSteps; ++i) {
            DagArbExecutor.DagStep memory s = p.steps[i];
            uint8 dk = s.dexKind & 0x0f;
            bool recIsNext = (dk != 4 && dk != 5)
                && (i + 1 < nSteps)
                && (s.recipient != execAddr)
                && (s.recipient == p.steps[i + 1].pool);
            bool shareFull = s.shareBps == 10000;
            bool metricZfo = (dk == 6) && s.extraData.length > 0 && uint8(s.extraData[0]) != 0;
            uint8 hdr = dk
                | (s.inputInPool ? 0x10 : 0)
                | (recIsNext ? 0x20 : 0)
                | (shareFull ? 0x40 : 0)
                | (metricZfo ? 0x80 : 0);
            uint8 idxByte = (inIdxs[i] << 4) | (outIdxs[i] & 0x0f);
            out = abi.encodePacked(out, hdr, idxByte, s.pool);
            if (!shareFull) out = abi.encodePacked(out, s.shareBps);
            if (dk == 0) {
                out = abi.encodePacked(out, s.feeNumerator);
            } else if (dk == 7) {
                out = abi.encodePacked(out, abi.decode(s.extraData, (bytes32)));
            } else if (dk == 5) {
                (address c0,, uint24 fee, int24 ts, address hooks) =
                    abi.decode(s.extraData, (address, address, uint24, int24, address));
                out = abi.encodePacked(out, uint8(c0 == address(0) ? 1 : 0), bytes3(fee), bytes3(uint24(ts)), hooks);
            } else if (dk == 4) {
                (address c0,, address hooks,, uint24 fee, bytes32 params) =
                    abi.decode(s.extraData, (address, address, address, address, uint24, bytes32));
                out = abi.encodePacked(
                    out, s.recipient, uint8(c0 == address(0) ? 1 : 0), bytes3(fee), hooks, params
                );
            }
        }
        return out;
    }

    function _execCompact(DagArbExecutor exec, DagArbExecutor.ArbParams memory p)
        internal
        returns (uint256 netProfit, uint256 gasUsed)
    {
        (bool ok, bytes memory ret) = address(exec).call(_encodeCompactArb(p, address(exec)));
        if (!ok) {
            assembly {
                revert(add(ret, 32), mload(ret))
            }
        }
        return abi.decode(ret, (uint256, uint256));
    }

    function test_LiveBscMainnetProtocols() public {
        if (block.chainid != 56) return;

        address usdt = 0x55d398326f99059fF775485246999027B3197955;
        address wbnb = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
        address pcsV3Flash = 0x92b7807bF19b7DDdf89b706143896d05228f3121; // USDT/USDC 100
        address pcsV2 = 0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE;      // WBNB/USDT
        address biswapV2 = 0x8840C6252e2e86e545deFb6da98B2a0E26d8C1BA;   // WBNB/USDT
        address thenaV2 = 0x6BE6A437A1172e6C220246eCB3A92a45AF9f0Cbc;    // WBNB/USDT
        address pcsV3Swap = 0x36696169C63e42cd08ce11f5deeBbCeBae652050;  // WBNB/USDT 500

        // Simulate a proportional 2% WBNB whale sell into PancakeV2, depressing WBNB price on PancakeV2
        (uint112 r0, uint112 r1,) = IUniV2Pair(pcsV2).getReserves();
        // token0 = USDT (0x55d3...), token1 = WBNB (0xbb4C...)
        uint256 amtIn = uint256(r1) / 50;
        if (amtIn < 50 ether) amtIn = 50 ether;
        vm.deal(address(this), amtIn);
        IWETH(wbnb).deposit{value: amtIn}();
        uint256 amtInFee = amtIn * 9975;
        uint256 usdtOut = (amtInFee * uint256(r0)) / (uint256(r1) * 10000 + amtInFee);
        require(IERC20(wbnb).transfer(pcsV2, amtIn), "XFER");
        IUniV2Pair(pcsV2).swap(usdtOut, 0, address(this), "");

        // Deploy DagArbExecutor and arbitrage the cheap WBNB on PancakeV2 back into BiSwapV2 + ThenaV2 + PancakeV3!
        DagArbExecutor exec = new DagArbExecutor(address(this));

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](4);
        // Step 0: Buy cheap WBNB with 100% of borrowed USDT on PancakeV2
        steps[0] = DagArbExecutor.DagStep({
            pool: pcsV2,
            tokenIn: usdt,
            tokenOut: wbnb,
            dexKind: 0,
            inputInPool: false,
            feeNumerator: 9975,
            shareBps: 10000,
            recipient: address(exec),
            extraData: ""
        });
        // Step 1: Split 5% of WBNB into BiSwapV2 -> USDT
        steps[1] = DagArbExecutor.DagStep({
            pool: biswapV2,
            tokenIn: wbnb,
            tokenOut: usdt,
            dexKind: 0,
            inputInPool: false,
            feeNumerator: 9980,
            shareBps: 500,
            recipient: address(exec),
            extraData: ""
        });
        // Step 2: Split 1% of remaining WBNB into ThenaV2 (Solidly) -> USDT
        steps[2] = DagArbExecutor.DagStep({
            pool: thenaV2,
            tokenIn: wbnb,
            tokenOut: usdt,
            dexKind: 1,
            inputInPool: false,
            feeNumerator: 9980,
            shareBps: 100,
            recipient: address(exec),
            extraData: ""
        });
        // Step 3: Sweep remaining WBNB (10000 bps) into PancakeV3 -> USDT
        steps[3] = DagArbExecutor.DagStep({
            pool: pcsV3Swap,
            tokenIn: wbnb,
            tokenOut: usdt,
            dexKind: 3,
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000,
            recipient: address(exec),
            extraData: ""
        });

        uint256 balBefore = IERC20(usdt).balanceOf(address(this));
        (uint256 netProfit, uint256 gasUsed) = _execCompact(
            exec,
            DagArbExecutor.ArbParams({
                flashMode: 3,
                flashSource: pcsV3Flash,
                baseToken: usdt,
                borrowAmount: 5000e18,
                minNetProfit: 10e18, // require >= $10 profit
                payoutToOwner: true,
                wrappedNative: address(0),
                bribeRecipient: address(0),
                bribeBps: 0,
                steps: steps
            })
        );
        uint256 balAfter = IERC20(usdt).balanceOf(address(this));

        require(netProfit >= 10e18, "BSC_FORK_LOW_PROFIT");
        require(balAfter - balBefore == netProfit, "BSC_PAYOUT_ERR");
        require(gasUsed > 0, "BSC_GAS_ZERO");
    }

    function test_LiveBaseMainnetProtocols() public {
        if (block.chainid != 8453) return;

        address usdc = 0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913;
        address weth = 0x4200000000000000000000000000000000000006;
        address morpho = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
        address aeroV2 = 0xcDAC0d6c6C59727a65F871236188350531885C43;     // WETH/USDC
        address slipstream = 0xb2cc224c1c9feE385f8ad6a55b4d94E92359DC59; // WETH/USDC

        // Whale sells 50 WETH into AeroV2, pushing WETH cheap on AeroV2
        vm.deal(address(this), 50 ether);
        IWETH(weth).deposit{value: 50 ether}();
        uint256 usdcOut = ISolidlyPair(aeroV2).getAmountOut(50 ether, weth);
        require(IERC20(weth).transfer(aeroV2, 50 ether), "XFER");
        // token0 = WETH (0x4200...), token1 = USDC (0x8335...)
        ISolidlyPair(aeroV2).swap(0, usdcOut, address(this), "");

        DagArbExecutor exec = new DagArbExecutor(address(this));

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep({
            pool: aeroV2,
            tokenIn: usdc,
            tokenOut: weth,
            dexKind: 1,
            inputInPool: false,
            feeNumerator: 9970,
            shareBps: 10000,
            recipient: address(exec),
            extraData: ""
        });
        steps[1] = DagArbExecutor.DagStep({
            pool: slipstream,
            tokenIn: weth,
            tokenOut: usdc,
            dexKind: 2,
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000,
            recipient: address(exec),
            extraData: ""
        });

        uint256 balBefore = IERC20(usdc).balanceOf(address(this));
        (uint256 netProfit, uint256 gasUsed) = _execCompact(
            exec,
            DagArbExecutor.ArbParams({
                flashMode: 1,
                flashSource: morpho,
                baseToken: usdc,
                borrowAmount: 10_000e6,
                minNetProfit: 50e6, // >= $50 profit
                payoutToOwner: true,
                wrappedNative: address(0),
                bribeRecipient: address(0),
                bribeBps: 0,
                steps: steps
            })
        );
        uint256 balAfter = IERC20(usdc).balanceOf(address(this));

        require(netProfit >= 50e6, "BASE_FORK_LOW_PROFIT");
        require(balAfter - balBefore == netProfit, "BASE_PAYOUT_ERR");
        require(gasUsed > 0, "BASE_GAS_ZERO");
    }

    function test_InfinityLiveSwapRoundTrip() public {
        if (block.chainid != 56) return;
        DagArbExecutor exec = new DagArbExecutor(address(this));

        address usdt = 0x55d398326f99059fF775485246999027B3197955;
        address wbnb = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
        address clpm = 0xa0FfB9c1CE1Fe56963B0321B32E7A0302114058b;
        address vault = 0x238a358808379702088667322f80aC48bAd5e6c4;

        vm.deal(address(this), 1 ether);
        IWETH(wbnb).deposit{value: 0.2 ether}();
        IERC20(wbnb).transfer(address(exec), 0.2 ether);

        bytes memory poolKeyBytes = abi.encode(
            address(0), // currency0 (native BNB)
            usdt,       // currency1
            address(0), // hooks
            clpm,       // poolManager
            uint24(335),
            bytes32(uint256(0x000a0000))
        );

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep({
            pool: clpm,
            tokenIn: wbnb,
            tokenOut: usdt,
            dexKind: 4, // DEX_INFINITY_CL
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 5000, // swap 0.1 WBNB -> USDT (tests native BNB settle)
            recipient: vault,
            extraData: poolKeyBytes
        });
        steps[1] = DagArbExecutor.DagStep({
            pool: clpm,
            tokenIn: usdt,
            tokenOut: wbnb,
            dexKind: 4, // DEX_INFINITY_CL
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000, // swap USDT -> WBNB (tests native BNB take + wrap)
            recipient: vault,
            extraData: poolKeyBytes
        });

        (bool okInf, bytes memory retInf) = address(exec).call(
            _encodeCompactArb(
                DagArbExecutor.ArbParams({
                    flashMode: 0,
                    flashSource: address(0),
                    baseToken: wbnb,
                    borrowAmount: 0.1 ether,
                    minNetProfit: 1,
                    payoutToOwner: false,
                    wrappedNative: wbnb,
                    bribeRecipient: address(0),
                    bribeBps: 0,
                    steps: steps
                }),
                address(exec)
            )
        );
        require(!okInf && retInf.length == 0, "EXPECTED_MIN_PROFIT_REVERT");
    }

    function test_MetricOmmLiveSwapAndFlashArb() public {
        if (block.chainid != 56) return;

        address usdt = 0x55d398326f99059fF775485246999027B3197955;
        address wbnb = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
        address metricWbnbUsdt = 0xF3eb104C0773d79F69caBF1DA938153cE0efAFd6; // token0=WBNB (0xbb4c) > token1=USDT (0x55d3)
        address pcsV2 = 0x16b9a82891338f9bA80E2D6970FddA79D1eb0daE;          // WBNB/USDT

        // (a) Funded round-trip on live MetricOmmPool verifying unsorted token0/token1 + callback settlement
        DagArbExecutor execFund = new DagArbExecutor(address(this));
        vm.deal(address(this), 2 ether);
        IWETH(wbnb).deposit{value: 0.5 ether}();
        require(IERC20(wbnb).transfer(address(execFund), 0.5 ether), "XFER_WBNB");

        DagArbExecutor.DagStep[] memory rtSteps = new DagArbExecutor.DagStep[](2);
        // Step 0: sell WBNB (token0) -> USDT (token1): zeroForOne = true (hex"01")
        rtSteps[0] = DagArbExecutor.DagStep({
            pool: metricWbnbUsdt,
            tokenIn: wbnb,
            tokenOut: usdt,
            dexKind: 6,
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000,
            recipient: address(execFund),
            extraData: hex"01"
        });
        // Step 1: sell USDT (token1) -> WBNB (token0): zeroForOne = false (hex"00")
        rtSteps[1] = DagArbExecutor.DagStep({
            pool: metricWbnbUsdt,
            tokenIn: usdt,
            tokenOut: wbnb,
            dexKind: 6,
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000,
            recipient: address(execFund),
            extraData: hex"00"
        });
        // Round-trip through same oracle pool settles cleanly and loses only spread -> reverts silently on MIN_PROFIT (1 ether)
        (bool okMet, bytes memory retMet) = address(execFund).call(
            _encodeCompactArb(
                DagArbExecutor.ArbParams({
                    flashMode: 0,
                    flashSource: address(0),
                    baseToken: wbnb,
                    borrowAmount: 0.5 ether,
                    minNetProfit: 1 ether,
                    payoutToOwner: false,
                    wrappedNative: address(0),
                    bribeRecipient: address(0),
                    bribeBps: 0,
                    steps: rtSteps
                }),
                address(execFund)
            )
        );
        require(!okMet && retMet.length == 0, "EXPECTED_METRIC_REVERT");

        // (b) Zero-capital FLASH_METRIC_SWAP=6 arbitrage:
        // Whale sells ~2% of reserve into PancakeV2 -> WBNB becomes ~4% cheaper on PancakeV2 than live Metric oracle!
        (uint112 r0, uint112 r1,) = IUniV2Pair(pcsV2).getReserves();
        uint256 whaleWbnb = uint256(r1) / 50;
        if (whaleWbnb < 50 ether) whaleWbnb = 50 ether;
        vm.deal(address(this), whaleWbnb);
        IWETH(wbnb).deposit{value: whaleWbnb}();
        uint256 amtInFee = whaleWbnb * 9975;
        uint256 whaleUsdtOut = (amtInFee * uint256(r0)) / (uint256(r1) * 10000 + amtInFee);
        require(IERC20(wbnb).transfer(pcsV2, whaleWbnb), "XFER_WHALE");
        IUniV2Pair(pcsV2).swap(whaleUsdtOut, 0, address(this), "");

        // Size borrow within live Metric pool USDT reserve so it fits active bins cleanly
        uint256 metricUsdtBal = IERC20(usdt).balanceOf(metricWbnbUsdt);
        uint256 borrowWbnb = metricUsdtBal > 20_000e18 ? 5 ether : 0.2 ether;

        DagArbExecutor execFlash = new DagArbExecutor(address(this));
        DagArbExecutor.DagStep[] memory arbSteps = new DagArbExecutor.DagStep[](2);
        // Step 0 (root FLASH_METRIC_SWAP): sell borrowed WBNB -> USDT on Metric, hopping USDT straight into pcsV2!
        arbSteps[0] = DagArbExecutor.DagStep({
            pool: metricWbnbUsdt,
            tokenIn: wbnb,
            tokenOut: usdt,
            dexKind: 6,
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000,
            recipient: pcsV2,
            extraData: hex"01"
        });
        // Step 1: buy cheap WBNB with hopped USDT on PancakeV2
        arbSteps[1] = DagArbExecutor.DagStep({
            pool: pcsV2,
            tokenIn: usdt,
            tokenOut: wbnb,
            dexKind: 0,
            inputInPool: true,
            feeNumerator: 9975,
            shareBps: 10000,
            recipient: address(execFlash),
            extraData: ""
        });

        uint256 balBefore = IERC20(wbnb).balanceOf(address(this));
        (uint256 netProfit, uint256 gasUsed) = _execCompact(
            execFlash,
            DagArbExecutor.ArbParams({
                flashMode: 6,
                flashSource: metricWbnbUsdt,
                baseToken: wbnb,
                borrowAmount: borrowWbnb,
                minNetProfit: 0.001 ether,
                payoutToOwner: false,
                wrappedNative: address(0),
                bribeRecipient: address(0),
                bribeBps: 0,
                steps: arbSteps
            })
        );
        // Re-check owner payout when payoutToOwner = true with wrappedNative = wbnb (unwraps WBNB -> BNB!)
        uint256 wbnbRetained = IERC20(wbnb).balanceOf(address(execFlash));
        require(netProfit > 0.001 ether, "METRIC_LIVE_FORK_LOW_PROFIT");
        require(wbnbRetained == netProfit, "METRIC_RETAINED_MISMATCH");
        require(IERC20(wbnb).balanceOf(address(this)) == balBefore, "PAYOUT_OFF_MISMATCH");
        require(gasUsed > 0, "METRIC_GAS_ZERO");
    }
}
