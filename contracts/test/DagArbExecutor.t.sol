// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import {DagArbExecutor, DagArbDiagExecutor} from "../src/DagArbExecutor.sol";
import {IUniV4PoolManager} from "../src/interfaces/IDexInterfaces.sol";

interface Vm {
    function expectRevert(bytes calldata msgBytes) external;
    function prank(address msgSender) external;
}

contract MockERC20 {
    string public symbol;
    uint8 public decimals = 18;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    constructor(string memory _symbol) {
        symbol = _symbol;
    }

    function mint(address to, uint256 amt) external {
        balanceOf[to] += amt;
    }

    function transfer(address to, uint256 amt) external returns (bool) {
        require(balanceOf[msg.sender] >= amt, "ERC20_BAL");
        balanceOf[msg.sender] -= amt;
        balanceOf[to] += amt;
        return true;
    }

    function approve(address spender, uint256 amt) external returns (bool) {
        allowance[msg.sender][spender] = amt;
        return true;
    }

    function transferFrom(address from, address to, uint256 amt) external returns (bool) {
        require(allowance[from][msg.sender] >= amt, "ERC20_ALLOW");
        require(balanceOf[from] >= amt, "ERC20_BAL");
        allowance[from][msg.sender] -= amt;
        balanceOf[from] -= amt;
        balanceOf[to] += amt;
        return true;
    }
}

contract MockWBNB is MockERC20 {
    constructor() MockERC20("WBNB") {}

    receive() external payable {}

    function deposit() external payable {
        balanceOf[msg.sender] += msg.value;
    }

    function withdraw(uint256 wad) external {
        require(balanceOf[msg.sender] >= wad, "WBNB_BAL");
        balanceOf[msg.sender] -= wad;
        (bool ok,) = msg.sender.call{value: wad}("");
        require(ok, "WBNB_WITHDRAW_FAIL");
    }
}

contract MockUniV2Pair {
    address public token0;
    address public token1;
    uint112 public reserve0;
    uint112 public reserve1;
    uint16 public feeNumerator;

    constructor(address _a, address _b, uint112 _ra, uint112 _rb, uint16 _feeNum) {
        if (_a < _b) {
            token0 = _a;
            token1 = _b;
            reserve0 = _ra;
            reserve1 = _rb;
        } else {
            token0 = _b;
            token1 = _a;
            reserve0 = _rb;
            reserve1 = _ra;
        }
        feeNumerator = _feeNum;
    }

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, 1);
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external {
        require(amount0Out > 0 || amount1Out > 0, "ZERO_OUT");
        if (amount0Out > 0) MockERC20(token0).transfer(to, amount0Out);
        if (amount1Out > 0) MockERC20(token1).transfer(to, amount1Out);
        if (data.length > 0) {
            DagArbExecutor(payable(msg.sender)).pancakeCall(msg.sender, amount0Out, amount1Out, data);
        }

        uint256 bal0 = MockERC20(token0).balanceOf(address(this));
        uint256 bal1 = MockERC20(token1).balanceOf(address(this));
        uint256 in0 = bal0 > reserve0 ? bal0 - reserve0 : 0;
        uint256 in1 = bal1 > reserve1 ? bal1 - reserve1 : 0;
        require(in0 > 0 || in1 > 0, "NO_IN");

        uint256 bal0Adj = bal0 * 10000 - in0 * (10000 - feeNumerator);
        uint256 bal1Adj = bal1 * 10000 - in1 * (10000 - feeNumerator);
        require(bal0Adj * bal1Adj >= uint256(reserve0) * uint256(reserve1) * 10000 * 10000, "K");

        reserve0 = uint112(bal0);
        reserve1 = uint112(bal1);
    }
}

contract MockPancakeV3Pool {
    address public token0;
    address public token1;
    uint256 public fixedRateNum; // numerator per 10000 when zeroForOne or vice versa
    uint256 public fixedRateDen;

    constructor(address _a, address _b, uint256 _num, uint256 _den) {
        if (_a < _b) {
            token0 = _a;
            token1 = _b;
        } else {
            token0 = _b;
            token1 = _a;
        }
        fixedRateNum = _num;
        fixedRateDen = _den;
    }

    function swap(
        address recipient,
        bool zeroForOne,
        int256 amountSpecified,
        uint160,
        bytes calldata data
    ) external returns (int256 amount0, int256 amount1) {
        require(amountSpecified > 0, "EXACT_IN_ONLY");
        uint256 amtIn = uint256(amountSpecified);
        uint256 amtOut = (amtIn * fixedRateNum) / fixedRateDen;
        if (zeroForOne) {
            MockERC20(token1).transfer(recipient, amtOut);
            amount0 = int256(amtIn);
            amount1 = -int256(amtOut);
        } else {
            MockERC20(token0).transfer(recipient, amtOut);
            amount0 = -int256(amtOut);
            amount1 = int256(amtIn);
        }
        (bool ok, bytes memory ret) = msg.sender.call(
            abi.encodeWithSignature("pancakeV3SwapCallback(int256,int256,bytes)", amount0, amount1, data)
        );
        require(ok, string(ret));
    }

    function flash(address recipient, uint256 amount0, uint256 amount1, bytes calldata data) external {
        uint256 bal0Before = MockERC20(token0).balanceOf(address(this));
        uint256 bal1Before = MockERC20(token1).balanceOf(address(this));
        if (amount0 > 0) MockERC20(token0).transfer(recipient, amount0);
        if (amount1 > 0) MockERC20(token1).transfer(recipient, amount1);
        uint256 fee0 = amount0 / 10000;
        uint256 fee1 = amount1 / 10000;
        (bool ok, bytes memory ret) = msg.sender.call(
            abi.encodeWithSignature("pancakeV3FlashCallback(uint256,uint256,bytes)", fee0, fee1, data)
        );
        require(ok, string(ret));
        require(MockERC20(token0).balanceOf(address(this)) >= bal0Before + fee0, "FLASH0");
        require(MockERC20(token1).balanceOf(address(this)) >= bal1Before + fee1, "FLASH1");
    }
}

contract MockMorpho {
    function flashLoan(address token, uint256 assets, bytes calldata data) external {
        require(MockERC20(token).transfer(msg.sender, assets), "T_OUT");
        (bool ok, bytes memory ret) = msg.sender.call(
            abi.encodeWithSignature("onMorphoFlashLoan(uint256,bytes)", assets, data)
        );
        require(ok, string(ret));
        require(MockERC20(token).transferFrom(msg.sender, address(this), assets), "T_REPAY");
    }
}

contract MockMoolah {
    function flashLoan(address token, uint256 assets, bytes calldata data) external {
        require(MockERC20(token).transfer(msg.sender, assets), "T_OUT");
        (bool ok, bytes memory ret) = msg.sender.call(
            abi.encodeWithSignature("onMoolahFlashLoan(uint256,bytes)", assets, data)
        );
        require(ok, string(ret));
        require(MockERC20(token).transferFrom(msg.sender, address(this), assets), "T_REPAY");
    }
}

contract MockSolidlyPair {
    address public token0;
    address public token1;
    uint256 public reserve0;
    uint256 public reserve1;

    constructor(address _a, address _b, uint256 _ra, uint256 _rb) {
        if (_a < _b) {
            token0 = _a;
            token1 = _b;
            reserve0 = _ra;
            reserve1 = _rb;
        } else {
            token0 = _b;
            token1 = _a;
            reserve0 = _rb;
            reserve1 = _ra;
        }
    }

    function getReserves() external view returns (uint256, uint256, uint256) {
        return (reserve0, reserve1, 1);
    }

    function getAmountOut(uint256 amountIn, address tokenIn) public view returns (uint256) {
        bool isT0 = tokenIn == token0;
        (uint256 rIn, uint256 rOut) = isT0 ? (reserve0, reserve1) : (reserve1, reserve0);
        uint256 amtFee = amountIn * 9995;
        return (amtFee * rOut) / (rIn * 10000 + amtFee);
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata) external {
        if (amount0Out > 0) require(MockERC20(token0).transfer(to, amount0Out), "T0");
        if (amount1Out > 0) require(MockERC20(token1).transfer(to, amount1Out), "T1");
        reserve0 = MockERC20(token0).balanceOf(address(this));
        reserve1 = MockERC20(token1).balanceOf(address(this));
    }
}

contract MockMetricOmmPool {
    address public token0;
    address public token1;
    uint256 public sellRateNum; // token1 per token0 when zeroForOne == true (scaled by 1e4)
    uint256 public buyRateDen;  // token1 per token0 when zeroForOne == false (scaled by 1e4)

    // Preserve constructor order intentionally without sorting to verify unsorted Metric token0/token1 support!
    constructor(address _t0, address _t1, uint256 _sellNum, uint256 _buyDen) {
        token0 = _t0;
        token1 = _t1;
        sellRateNum = _sellNum;
        buyRateDen = _buyDen;
    }

    function swap(
        address recipient,
        bool zeroForOne,
        int128 amountSpecified,
        uint128 priceLimit,
        bytes calldata swapData,
        bytes calldata
    ) external returns (int256 amount0Delta, int256 amount1Delta) {
        require(amountSpecified > 0, "EXACT_IN_ONLY");
        if (zeroForOne) {
            require(priceLimit >= 1, "BAD_MIN_LIMIT");
        } else {
            require(priceLimit == type(uint128).max, "BAD_MAX_LIMIT");
        }
        uint256 amtIn = uint256(int256(amountSpecified));
        if (zeroForOne) {
            uint256 amtOut = (amtIn * sellRateNum) / 10000;
            MockERC20(token1).transfer(recipient, amtOut);
            amount0Delta = int256(amtIn);
            amount1Delta = -int256(amtOut);
        } else {
            uint256 amtOut = (amtIn * 10000) / buyRateDen;
            MockERC20(token0).transfer(recipient, amtOut);
            amount0Delta = -int256(amtOut);
            amount1Delta = int256(amtIn);
        }
        uint256 balBefore = zeroForOne
            ? MockERC20(token0).balanceOf(address(this))
            : MockERC20(token1).balanceOf(address(this));
        (bool ok, bytes memory ret) = msg.sender.call(
            abi.encodeWithSignature("metricOmmSwapCallback(int256,int256,bytes)", amount0Delta, amount1Delta, swapData)
        );
        if (!ok) {
            assembly {
                revert(add(ret, 32), mload(ret))
            }
        }
        uint256 balAfter = zeroForOne
            ? MockERC20(token0).balanceOf(address(this))
            : MockERC20(token1).balanceOf(address(this));
        require(balAfter >= balBefore + amtIn, "METRIC_REPAY_FAIL");
    }
}

contract MockBalancerV2Vault {
    uint256 public swapRateNum = 10500; // 1.05x out per in (scaled by 1e4)
    bytes32 public lastPoolId;

    struct SingleSwap {
        bytes32 poolId;
        uint8 kind;
        address assetIn;
        address assetOut;
        uint256 amount;
        bytes userData;
    }

    struct FundManagement {
        address sender;
        bool fromInternalBalance;
        address payable recipient;
        bool toInternalBalance;
    }

    function setSwapRate(uint256 num) external {
        swapRateNum = num;
    }

    function swap(
        SingleSwap calldata singleSwap,
        FundManagement calldata funds,
        uint256 limit,
        uint256 deadline
    ) external payable returns (uint256 amountCalculated) {
        require(deadline >= block.timestamp, "BAL_EXPIRED");
        lastPoolId = singleSwap.poolId;
        MockERC20(singleSwap.assetIn).transferFrom(funds.sender, address(this), singleSwap.amount);
        amountCalculated = (singleSwap.amount * swapRateNum) / 10000;
        require(amountCalculated >= limit, "BAL_SLIPPAGE");
        MockERC20(singleSwap.assetOut).transfer(funds.recipient, amountCalculated);
    }

    function flashLoan(
        address recipient,
        address[] calldata tokens,
        uint256[] calldata amounts,
        bytes calldata userData
    ) external {
        require(tokens.length == amounts.length && tokens.length == 1, "BAL_LEN");
        uint256 balBefore = MockERC20(tokens[0]).balanceOf(address(this));
        MockERC20(tokens[0]).transfer(recipient, amounts[0]);
        uint256[] memory fees = new uint256[](1);
        fees[0] = 0;
        (bool ok, bytes memory ret) = recipient.call(
            abi.encodeWithSignature(
                "receiveFlashLoan(address[],uint256[],uint256[],bytes)",
                tokens,
                amounts,
                fees,
                userData
            )
        );
        if (!ok) {
            assembly {
                revert(add(ret, 32), mload(ret))
            }
        }
        uint256 balAfter = MockERC20(tokens[0]).balanceOf(address(this));
        require(balAfter >= balBefore, "BAL_FLASH_REPAY_FAIL");
    }
}

contract MockUniV4PoolManager {
    mapping(bytes32 => uint256) public rateNum; // amtOut = (amtIn * rateNum) / 10000
    mapping(address => int256) public delta;
    address[] private touched;
    address private syncedCurrency;
    mapping(address => uint256) private syncedBal;

    uint256 public unlockCalls;
    uint256 public swapCalls;
    uint256 public syncCalls;
    uint256 public settleCalls;
    uint256 public takeCalls;

    function setPoolRate(IUniV4PoolManager.PoolKey calldata key, uint256 num) external {
        rateNum[keccak256(abi.encode(key))] = num;
    }

    function unlock(bytes calldata data) external returns (bytes memory ret) {
        ++unlockCalls;
        delete touched;
        (bool ok, bytes memory r) = msg.sender.call(abi.encodeWithSignature("unlockCallback(bytes)", data));
        if (!ok) {
            assembly {
                revert(add(r, 32), mload(r))
            }
        }
        uint256 len = touched.length;
        for (uint256 i = 0; i < len; ++i) {
            require(delta[touched[i]] == 0, "V4_DELTA_NONZERO");
        }
        return r;
    }

    function swap(
        IUniV4PoolManager.PoolKey calldata key,
        IUniV4PoolManager.SwapParams calldata params,
        bytes calldata
    ) external returns (int256 bDelta) {
        ++swapCalls;
        uint256 num = rateNum[keccak256(abi.encode(key))];
        require(num > 0, "NO_V4_POOL");
        require(params.amountSpecified < 0, "EXACT_IN");
        uint256 amtIn = uint256(-params.amountSpecified);
        uint256 amtOut = (amtIn * num) / 10000;
        int128 d0 = params.zeroForOne ? -int128(int256(amtIn)) : int128(int256(amtOut));
        int128 d1 = params.zeroForOne ? int128(int256(amtOut)) : -int128(int256(amtIn));
        delta[key.currency0] += int256(d0);
        delta[key.currency1] += int256(d1);
        touched.push(key.currency0);
        touched.push(key.currency1);
        bDelta = (int256(d0) << 128) | int256(uint256(uint128(d1)));
    }

    function sync(address currency) external {
        ++syncCalls;
        syncedCurrency = currency;
        syncedBal[currency] =
            currency == address(0) ? address(this).balance : MockERC20(currency).balanceOf(address(this));
    }

    function settle() external payable returns (uint256 paid) {
        ++settleCalls;
        if (msg.value > 0) {
            delta[address(0)] += int256(msg.value);
            return msg.value;
        }
        address c = syncedCurrency;
        uint256 curBal = MockERC20(c).balanceOf(address(this));
        paid = curBal - syncedBal[c];
        delta[c] += int256(paid);
    }

    function take(address currency, address to, uint256 amount) external {
        ++takeCalls;
        delta[currency] -= int256(amount);
        if (currency == address(0)) {
            (bool ok,) = to.call{value: amount}("");
            require(ok, "V4_ETH");
        } else {
            MockERC20(currency).transfer(to, amount);
        }
    }
}

contract DagArbExecutorTest {
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    DagArbExecutor executor;
    MockERC20 usdt;
    MockERC20 tokenA;
    MockERC20 tokenB;
    MockUniV2Pair v2Split1;
    MockUniV2Pair v2Split2;
    MockUniV2Pair v2Hop;
    MockPancakeV3Pool v3Flash;
    MockPancakeV3Pool v3Exit;
    MockMorpho morpho;

    receive() external payable {}

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

    function encodeCompactArb(DagArbExecutor.ArbParams memory p, address execAddr)
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
                bytes32 poolId = abi.decode(s.extraData, (bytes32));
                out = abi.encodePacked(out, poolId);
            } else if (dk == 5) {
                (address c0,, uint24 fee, int24 tickSpacing, address hooks) =
                    abi.decode(s.extraData, (address, address, uint24, int24, address));
                out = abi.encodePacked(
                    out,
                    uint8(c0 == address(0) ? 1 : 0),
                    bytes3(fee),
                    bytes3(uint24(int24(tickSpacing))),
                    hooks
                );
            } else if (dk == 4) {
                (address c0,, address hooks,, uint24 fee, bytes32 parameters) =
                    abi.decode(s.extraData, (address, address, address, address, uint24, bytes32));
                out = abi.encodePacked(
                    out,
                    s.recipient,
                    uint8(c0 == address(0) ? 1 : 0),
                    bytes3(fee),
                    hooks,
                    parameters
                );
            }
        }
        return out;
    }

    function _execCompact(DagArbExecutor target, DagArbExecutor.ArbParams memory p, uint256 valueWei)
        internal
        returns (uint256 netProfit, uint256 gasUsed)
    {
        bytes memory payload = encodeCompactArb(p, address(target));
        (bool ok, bytes memory ret) = address(target).call{value: valueWei}(payload);
        if (!ok) {
            assembly {
                revert(add(ret, 32), mload(ret))
            }
        }
        return abi.decode(ret, (uint256, uint256));
    }

    function setUp() public {
        executor = new DagArbExecutor(address(this));
        usdt = new MockERC20("USDT");
        tokenA = new MockERC20("TOKA");
        tokenB = new MockERC20("TOKB");
        // Ensure deterministic token sorting behavior regardless of deployment order:
        // Pools sort token0 < token1 internally.
        v2Split1 = new MockUniV2Pair(address(usdt), address(tokenA), 100_000e18, 200_000e18, 9975);
        v2Split2 = new MockUniV2Pair(address(usdt), address(tokenA), 100_000e18, 200_000e18, 9975);
        v2Hop = new MockUniV2Pair(address(tokenA), address(tokenB), 300_000e18, 300_000e18, 9975);

        // Make v3Exit give generous USDT back for tokenB (1.35 USDT per tokenB)
        v3Exit = new MockPancakeV3Pool(address(tokenB), address(usdt), 135, 100);
        // Flash pool (separate from execution pools!)
        v3Flash = new MockPancakeV3Pool(address(usdt), address(tokenB), 100, 100);
        morpho = new MockMorpho();

        usdt.mint(address(v2Split1), 100_000e18);
        tokenA.mint(address(v2Split1), 200_000e18);
        usdt.mint(address(v2Split2), 100_000e18);
        tokenA.mint(address(v2Split2), 200_000e18);
        tokenA.mint(address(v2Hop), 300_000e18);
        tokenB.mint(address(v2Hop), 300_000e18);
        usdt.mint(address(v3Exit), 500_000e18);
        tokenB.mint(address(v3Exit), 500_000e18);
        usdt.mint(address(v3Flash), 500_000e18);
        usdt.mint(address(morpho), 500_000e18);
    }

    function test_DagSplitMergeAndFlashArb() public {
        // Borrow 2,000 USDT via PancakeV3 flash loan (1 bps = 0.2 USDT fee)
        // Split 60% into v2Split1 (USDT->TOKA), 100% of remaining 40% into v2Split2 (USDT->TOKA)
        // Merge TOKA -> v2Hop (TOKA->TOKB) -> v3Exit (TOKB->USDT)
        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](4);
        steps[0] = DagArbExecutor.DagStep({
            pool: address(v2Split1),
            tokenIn: address(usdt),
            tokenOut: address(tokenA),
            dexKind: 0,
            inputInPool: false,
            feeNumerator: 9975,
            shareBps: 6000,
            recipient: address(executor),
            extraData: ""
        });
        steps[1] = DagArbExecutor.DagStep({
            pool: address(v2Split2),
            tokenIn: address(usdt),
            tokenOut: address(tokenA),
            dexKind: 0,
            inputInPool: false,
            feeNumerator: 9975,
            shareBps: 10000,
            recipient: address(v2Hop), // Token-Hopping directly into v2Hop!
            extraData: ""
        });
        steps[2] = DagArbExecutor.DagStep({
            pool: address(v2Hop),
            tokenIn: address(tokenA),
            tokenOut: address(tokenB),
            dexKind: 0,
            inputInPool: false,
            feeNumerator: 9975,
            shareBps: 10000,
            recipient: address(executor),
            extraData: ""
        });
        steps[3] = DagArbExecutor.DagStep({
            pool: address(v3Exit),
            tokenIn: address(tokenB),
            tokenOut: address(usdt),
            dexKind: 3,
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000,
            recipient: address(executor),
            extraData: ""
        });

        DagArbExecutor.ArbParams memory params = DagArbExecutor.ArbParams({
            flashMode: 3,
            flashSource: address(v3Flash),
            baseToken: address(usdt),
            borrowAmount: 2000e18,
            minNetProfit: 100e18,
            payoutToOwner: true,
            wrappedNative: address(0),
            bribeRecipient: address(0),
            bribeBps: 0,
            steps: steps
        });

        uint256 balBefore = usdt.balanceOf(address(this));
        (uint256 netProfit, uint256 gasUsed) = _execCompact(executor, params, 0);
        uint256 balAfter = usdt.balanceOf(address(this));

        require(netProfit > 100e18, "PROFIT_TOO_LOW");
        require(balAfter - balBefore == netProfit, "PAYOUT_MISMATCH");
        require(gasUsed > 0, "GAS_ZERO");
    }

    function test_MorphoZeroFeeFlashLoan() public {
        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](3);
        steps[0] = DagArbExecutor.DagStep({
            pool: address(v2Split1),
            tokenIn: address(usdt),
            tokenOut: address(tokenA),
            dexKind: 0,
            inputInPool: false,
            feeNumerator: 9975,
            shareBps: 10000,
            recipient: address(v2Hop), // Pure Token-Hopping!
            extraData: ""
        });
        steps[1] = DagArbExecutor.DagStep({
            pool: address(v2Hop),
            tokenIn: address(tokenA),
            tokenOut: address(tokenB),
            dexKind: 0,
            inputInPool: true,
            feeNumerator: 9975,
            shareBps: 10000,
            recipient: address(executor),
            extraData: ""
        });
        steps[2] = DagArbExecutor.DagStep({
            pool: address(v3Exit),
            tokenIn: address(tokenB),
            tokenOut: address(usdt),
            dexKind: 3,
            inputInPool: false,
            feeNumerator: 0,
            shareBps: 10000,
            recipient: address(executor),
            extraData: ""
        });

        DagArbExecutor.ArbParams memory params = DagArbExecutor.ArbParams({
            flashMode: 1,
            flashSource: address(morpho),
            baseToken: address(usdt),
            borrowAmount: 1000e18,
            minNetProfit: 1e18,
            payoutToOwner: true,
            wrappedNative: address(0),
            bribeRecipient: address(0),
            bribeBps: 0,
            steps: steps
        });

        (uint256 netProfit,) = _execCompact(executor, params, 0);
        require(netProfit > 1e18, "MORPHO_PROFIT_FAIL");
    }

    function test_TokenHoppingSavesGas() public {
        DagArbExecutor.DagStep[] memory normalSteps = new DagArbExecutor.DagStep[](3);
        normalSteps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(executor), "");
        normalSteps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, false, 9975, 10000, address(executor), "");
        normalSteps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        (, uint256 gasNormal) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                1, address(morpho), address(usdt), 1000e18, 1e18, true, address(0), address(0), 0, normalSteps
            ),
            0
        );

        // Fresh pair states for fair gas comparison
        setUp();

        DagArbExecutor.DagStep[] memory hopSteps = new DagArbExecutor.DagStep[](3);
        hopSteps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(v2Hop), "");
        hopSteps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, true, 9975, 10000, address(executor), "");
        hopSteps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        (, uint256 gasHop) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                1, address(morpho), address(usdt), 1000e18, 1e18, true, address(0), address(0), 0, hopSteps
            ),
            0
        );

        require(gasHop < gasNormal, "HOP_DID_NOT_SAVE_GAS");
    }

    function test_SolidlyPairExecution() public {
        MockSolidlyPair solidly = new MockSolidlyPair(address(tokenA), address(tokenB), 300_000e18, 300_000e18);
        tokenA.mint(address(solidly), 300_000e18);
        tokenB.mint(address(solidly), 300_000e18);

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](3);
        steps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(solidly), "");
        steps[1] = DagArbExecutor.DagStep(
            address(solidly), address(tokenA), address(tokenB), 1, true, 0, 10000, address(executor), "");
        steps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        (uint256 netProfit,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                1, address(morpho), address(usdt), 1000e18, 100e18, true, address(0), address(0), 0, steps
            ),
            0
        );
        require(netProfit > 100e18, "SOLIDLY_LOW_PROFIT");
    }

    function test_RevertWhenBelowMinProfit() public {
        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](3);
        steps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(v2Hop), "");
        steps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, true, 9975, 10000, address(executor), "");
        steps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        DagArbExecutor.ArbParams memory params = DagArbExecutor.ArbParams({
            flashMode: 1,
            flashSource: address(morpho),
            baseToken: address(usdt),
            borrowAmount: 1000e18,
            minNetProfit: 5000e18,
            payoutToOwner: false,
            wrappedNative: address(0),
            bribeRecipient: address(0),
            bribeBps: 0,
            steps: steps
        });

        bytes memory payload = encodeCompactArb(params, address(executor));
        (bool ok, bytes memory ret) = address(executor).call(payload);
        require(!ok && ret.length == 0, "EXPECTED_SILENT_REVERT");
    }

    function test_RevertWhenFlashLosesMoney() public {
        MockPancakeV3Pool badExit = new MockPancakeV3Pool(address(tokenB), address(usdt), 10, 100);
        usdt.mint(address(badExit), 100_000e18);

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](3);
        steps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(v2Hop), "");
        steps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, true, 9975, 10000, address(executor), "");
        steps[2] = DagArbExecutor.DagStep(
            address(badExit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        DagArbExecutor.ArbParams memory params = DagArbExecutor.ArbParams({
            flashMode: 1,
            flashSource: address(morpho),
            baseToken: address(usdt),
            borrowAmount: 1000e18,
            minNetProfit: 1,
            payoutToOwner: false,
            wrappedNative: address(0),
            bribeRecipient: address(0),
            bribeBps: 0,
            steps: steps
        });

        bytes memory payload = encodeCompactArb(params, address(executor));
        (bool ok, bytes memory ret) = address(executor).call(payload);
        require(!ok && keccak256(ret) == keccak256(abi.encodeWithSignature("Error(string)", "ERC20_BAL")), "EXP_BAL_REV");
    }

    function test_RevertUnauthorizedCallbacks() public {
        vm.prank(address(0xdead));
        vm.expectRevert(bytes(""));
        executor.pancakeV3SwapCallback(100, -100, "");

        vm.prank(address(0xdead));
        vm.expectRevert(bytes(""));
        executor.uniswapV3SwapCallback(100, -100, "");

        vm.prank(address(0xdead));
        vm.expectRevert(bytes(""));
        executor.onMorphoFlashLoan(100, "");

        vm.prank(address(0xdead));
        vm.expectRevert(bytes(""));
        executor.onMoolahFlashLoan(100, "");

        vm.prank(address(0xdead));
        vm.expectRevert(bytes("NOT_OWNER"));
        executor.rescueETH();
    }

    function test_MoolahZeroFeeAndFlashV3Swap() public {
        // 1. Test Lista Moolah (flashMode = 1 calling onMoolahFlashLoan + cached max allowance on 2nd call)
        MockMoolah moolah = new MockMoolah();
        usdt.mint(address(moolah), 500_000e18);

        DagArbExecutor.DagStep[] memory mSteps = new DagArbExecutor.DagStep[](3);
        mSteps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(v2Hop), "");
        mSteps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, true, 9975, 10000, address(executor), "");
        mSteps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        DagArbExecutor.ArbParams memory mParams = DagArbExecutor.ArbParams(
            1, address(moolah), address(usdt), 1000e18, 1e18, true, address(0), address(0), 0, mSteps
        );
        (uint256 p1, uint256 g1) = _execCompact(executor, mParams, 0);
        (uint256 p2, uint256 g2) = _execCompact(executor, mParams, 0);
        require(p1 > 1e18 && p2 > 1e18, "MOOLAH_PROFIT_FAIL");
        require(g2 <= g1, "ALLOWANCE_CACHE_FAIL");

        // 2. Test FLASH_V3_SWAP (flashMode = 4): root V3 flash-swap hopping directly into V2 leg
        MockPancakeV3Pool v3Root = new MockPancakeV3Pool(address(usdt), address(tokenB), 200, 100);
        MockUniV2Pair v2Out = new MockUniV2Pair(address(tokenB), address(usdt), 300_000e18, 300_000e18, 9975);
        tokenB.mint(address(v3Root), 500_000e18);
        usdt.mint(address(v3Root), 500_000e18);
        tokenB.mint(address(v2Out), 300_000e18);
        usdt.mint(address(v2Out), 300_000e18);

        DagArbExecutor.DagStep[] memory v3Steps = new DagArbExecutor.DagStep[](2);
        v3Steps[0] = DagArbExecutor.DagStep(
            address(v3Root), address(usdt), address(tokenB), 3, false, 0, 10000, address(v2Out), "");
        v3Steps[1] = DagArbExecutor.DagStep(
            address(v2Out), address(tokenB), address(usdt), 0, true, 9975, 10000, address(executor), "");

        (uint256 pV3,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                4, address(v3Root), address(usdt), 1000e18, 500e18, true, address(0), address(0), 0, v3Steps
            ),
            0
        );
        require(pV3 > 500e18, "FLASH_V3_SWAP_PROFIT_FAIL");
    }

    function test_DynamicWbnbBribeAndPartialSpreadSurvival() public {
        MockWBNB wbnb = new MockWBNB();
        (bool okEth,) = address(wbnb).call{value: 500e18}("");
        require(okEth, "FUND_WBNB_FAIL");

        MockMoolah moolah = new MockMoolah();
        wbnb.mint(address(moolah), 100e18);

        // Two unbalanced pools: pBuy gives 200 tokenA/WBNB (50% cheaper than pSell's 100 tokenA/WBNB)
        MockUniV2Pair pBuy = new MockUniV2Pair(address(wbnb), address(tokenA), 100e18, 20_000e18, 9975);
        MockUniV2Pair pSell = new MockUniV2Pair(address(tokenA), address(wbnb), 10_000e18, 100e18, 9975);
        wbnb.mint(address(pBuy), 100e18);
        tokenA.mint(address(pBuy), 20_000e18);
        tokenA.mint(address(pSell), 10_000e18);
        wbnb.mint(address(pSell), 100e18);

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep(
            address(pBuy), address(wbnb), address(tokenA), 0, false, 9975, 10000, address(pSell), "");
        steps[1] = DagArbExecutor.DagStep(
            address(pSell), address(tokenA), address(wbnb), 0, true, 9975, 10000, address(executor), "");

        address bribeTarget = address(0x1266);
        uint256 floorMin = 0.01e18;
        DagArbExecutor.ArbParams memory params = DagArbExecutor.ArbParams({
            flashMode: 1,
            flashSource: address(moolah),
            baseToken: address(wbnb),
            borrowAmount: 1e18,
            minNetProfit: floorMin,
            payoutToOwner: true,
            wrappedNative: address(wbnb),
            bribeRecipient: bribeTarget,
            bribeBps: 9500,
            steps: steps
        });

        uint256 ownerBefore1 = address(this).balance;
        uint256 bribeBefore1 = bribeTarget.balance;
        (uint256 net1,) = _execCompact(executor, params, 0);
        uint256 expectedBribe1 = ((net1 - floorMin) * 9500) / 10000;
        require(net1 > 0.5e18, "WBNB_NET1_LOW");
        require(bribeTarget.balance - bribeBefore1 == expectedBribe1, "BRIBE1_MISMATCH");
        require(address(this).balance - ownerBefore1 == net1 - expectedBribe1, "OWNER1_MISMATCH");

        // Second execution sees ~45% reduced spread after pool moved, yet SAME ArbParams survives cleanly!
        uint256 ownerBefore2 = address(this).balance;
        uint256 bribeBefore2 = bribeTarget.balance;
        (uint256 net2,) = _execCompact(executor, params, 0);
        uint256 expectedBribe2 = ((net2 - floorMin) * 9500) / 10000;
        require(net2 > floorMin && net2 < net1, "PARTIAL_SPREAD_EXPECTED_LOWER");
        require(bribeTarget.balance - bribeBefore2 == expectedBribe2, "BRIBE2_MISMATCH");
        require(address(this).balance - ownerBefore2 == net2 - expectedBribe2, "OWNER2_MISMATCH");

        // Third: non-WBNB route forwards msg.value bribe and transfers ERC20 profit to owner
        DagArbExecutor.DagStep[] memory uSteps = new DagArbExecutor.DagStep[](3);
        uSteps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(v2Hop), "");
        uSteps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, true, 9975, 10000, address(executor), "");
        uSteps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");
        uint256 usdtBefore = usdt.balanceOf(address(this));
        uint256 bribeBefore3 = bribeTarget.balance;
        (uint256 netUsdt,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                1, address(morpho), address(usdt), 1000e18, 1e18, true, address(wbnb), bribeTarget, 0, uSteps
            ),
            0.05 ether
        );
        require(bribeTarget.balance - bribeBefore3 == 0.05 ether, "USDT_MSGVAL_BRIBE_FAIL");
        require(usdt.balanceOf(address(this)) - usdtBefore == netUsdt, "USDT_OWNER_PAYOUT_FAIL");
    }

    function test_FlashV2SwapAndRescueBatch() public {
        MockWBNB wbnbLocal = new MockWBNB();
        (bool okFund,) = address(wbnbLocal).call{value: 20 ether}("");
        require(okFund, "FUND_WBNB");
        MockERC20 usdtLocal = new MockERC20("USDT2");
        MockERC20 tok = new MockERC20("TKN");

        // Pool0: V2 WBNB/TKN (100 WBNB, 200 TKN -> ~2 TKN/WBNB)
        // Pool1: V2 TKN/WBNB (100 TKN, 100 WBNB -> ~1 WBNB/TKN)
        MockUniV2Pair p0 = new MockUniV2Pair(address(wbnbLocal), address(tok), 100 ether, 200 ether, 9975);
        MockUniV2Pair p1 = new MockUniV2Pair(address(tok), address(wbnbLocal), 100 ether, 100 ether, 9975);
        wbnbLocal.mint(address(p0), 100 ether);
        tok.mint(address(p0), 200 ether);
        tok.mint(address(p1), 100 ether);
        wbnbLocal.mint(address(p1), 100 ether);

        // Execute with FLASH_V2_SWAP = 5, payoutToOwner = false (keeps WBNB profit inside contract!)
        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep(
            address(p0), address(wbnbLocal), address(tok), 0, false, 9975, 10000, address(p1), "");
        steps[1] = DagArbExecutor.DagStep(
            address(p1), address(tok), address(wbnbLocal), 0, true, 9975, 10000, address(executor), "");

        address bribeTarget = address(0xBEEF);
        uint256 bribeBefore = bribeTarget.balance;
        uint256 ownerBnbBefore = address(this).balance;
        (uint256 netProfit, uint256 gasUsed) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                5, address(p0), address(wbnbLocal), 2 ether, 0.01 ether, false, address(wbnbLocal), bribeTarget, 8000, steps
            ),
            0
        );
        require(netProfit > 0.5 ether, "FLASH_V2_PROFIT_LOW");
        require(gasUsed < 180_000, "FLASH_V2_GAS_TOO_HIGH");
        require(bribeTarget.balance > bribeBefore, "BRIBE_NOT_PAID");
        require(address(this).balance == ownerBnbBefore, "SHOULD_RETAIN_IN_CONTRACT");
        require(wbnbLocal.balanceOf(address(executor)) > 0.1 ether, "WBNB_NOT_RETAINED");

        // Also seed USDT profit inside contract and sweep both WBNB (unwrapped -> BNB) + USDT in 1 rescueBatch tx
        usdtLocal.mint(address(executor), 5e18);
        uint256 usdtOwnerBefore = usdtLocal.balanceOf(address(this));
        uint256 retainedWbnb = wbnbLocal.balanceOf(address(executor));
        address[] memory toks = new address[](2);
        toks[0] = address(wbnbLocal);
        toks[1] = address(usdtLocal);
        executor.rescueBatch(toks, true, address(wbnbLocal));

        require(wbnbLocal.balanceOf(address(executor)) == 0, "WBNB_NOT_SWEPT");
        require(usdtLocal.balanceOf(address(executor)) == 0, "USDT_NOT_SWEPT");
        require(usdtLocal.balanceOf(address(this)) - usdtOwnerBefore == 5e18, "USDT_RESCUE_MISMATCH");
        require(address(this).balance - ownerBnbBefore == retainedWbnb, "BNB_UNWRAP_MISMATCH");
    }

    function test_MetricOmmUnsortedStepAndFlashSwap() public {
        // Deploy two tokens where token0 > token1 by address so unsorted Metric ordering is tested explicitly!
        MockERC20 tok1 = new MockERC20("T1");
        MockERC20 tok2 = new MockERC20("T2");
        (MockERC20 highAddrTok, MockERC20 lowAddrTok) = address(tok1) > address(tok2) ? (tok1, tok2) : (tok2, tok1);

        // Unsorted Metric pool: token0 = highAddrTok, token1 = lowAddrTok
        // zfo=true (high->low) gives 2.0 low per high; zfo=false (low->high) charges 1.0 low per high.
        MockMetricOmmPool metric = new MockMetricOmmPool(address(highAddrTok), address(lowAddrTok), 20000, 10000);
        highAddrTok.mint(address(metric), 500_000e18);
        lowAddrTok.mint(address(metric), 500_000e18);

        // 1) Funded regular DEX_METRIC_OMM=6 cycle
        highAddrTok.mint(address(executor), 100e18);
        DagArbExecutor.DagStep[] memory fundedSteps = new DagArbExecutor.DagStep[](2);
        fundedSteps[0] = DagArbExecutor.DagStep(
            address(metric), address(highAddrTok), address(lowAddrTok), 6, false, 0, 10000, address(executor), hex"01");
        fundedSteps[1] = DagArbExecutor.DagStep(
            address(metric), address(lowAddrTok), address(highAddrTok), 6, false, 0, 10000, address(executor), hex"00");

        uint256 highBefore = highAddrTok.balanceOf(address(this));
        (uint256 fundedProfit,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                0, address(0), address(highAddrTok), 100e18, 50e18, true, address(0), address(0), 0, fundedSteps
            ),
            0
        );
        require(fundedProfit == 100e18, "METRIC_FUNDED_PROFIT_ERR");
        require(highAddrTok.balanceOf(address(this)) - highBefore == 100e18, "METRIC_PAYOUT_ERR");

        // 2) Zero-capital FLASH_METRIC_SWAP=6 with Token-Hopping straight into V2 pool
        MockUniV2Pair exitV2 = new MockUniV2Pair(address(lowAddrTok), address(highAddrTok), 100_000e18, 100_000e18, 9975);
        lowAddrTok.mint(address(exitV2), 100_000e18);
        highAddrTok.mint(address(exitV2), 100_000e18);

        DagArbExecutor.DagStep[] memory flashSteps = new DagArbExecutor.DagStep[](2);
        flashSteps[0] = DagArbExecutor.DagStep(
            address(metric), address(highAddrTok), address(lowAddrTok), 6, false, 0, 10000, address(exitV2), hex"01");

        DagArbExecutor freshExec = new DagArbExecutor(address(this));
        flashSteps[1] = DagArbExecutor.DagStep(
            address(exitV2), address(lowAddrTok), address(highAddrTok), 0, true, 9975, 10000, address(freshExec), "");
        (uint256 flashProfit,) = _execCompact(
            freshExec,
            DagArbExecutor.ArbParams(
                6, address(metric), address(highAddrTok), 100e18, 50e18, true, address(0), address(0), 0, flashSteps
            ),
            0
        );
        require(flashProfit > 90e18, "METRIC_FLASH_PROFIT_ERR");

        // 3) Unauthorized metricOmmSwapCallback must revert silently
        vm.prank(address(0xdead));
        vm.expectRevert(bytes(""));
        executor.metricOmmSwapCallback(100, -100, "");
    }

    function test_BalancerV2FlashLoanAndSwap() public {
        MockBalancerV2Vault balVault = new MockBalancerV2Vault();
        usdt.mint(address(balVault), 10_000e18);
        tokenA.mint(address(balVault), 10_000e18);

        MockUniV2Pair exitV2 = new MockUniV2Pair(address(tokenA), address(usdt), 100_000e18, 120_000e18, 9975);
        tokenA.mint(address(exitV2), 100_000e18);
        usdt.mint(address(exitV2), 120_000e18);

        bytes32 poolId = bytes32(uint256(0xBEEF0001));
        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep(
            address(balVault), address(usdt), address(tokenA), 7, false, 0, 10000, address(executor), abi.encode(poolId)
        );
        steps[1] = DagArbExecutor.DagStep(
            address(exitV2),
            address(tokenA),
            address(usdt),
            0,
            false,
            9975,
            10000,
            address(executor),
            ""
        );

        uint256 amtOut0 = 105e18;
        uint256 expectedOut = (amtOut0 * 9975 * 120_000e18) / (100_000e18 * 10000 + amtOut0 * 9975);
        uint256 expectedProfit = expectedOut - 100e18;

        uint256 ownerBefore = usdt.balanceOf(address(this));
        (uint256 netProfit,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                7, address(balVault), address(usdt), 100e18, 5e18, true, address(0), address(0), 0, steps
            ),
            0
        );
        require(netProfit == expectedProfit, "BAL_FLASH_PROFIT_ERR");
        require(balVault.lastPoolId() == poolId, "BAL_POOLID_ERR");
        require(usdt.balanceOf(address(this)) == ownerBefore + expectedProfit, "BAL_PAYOUT_ERR");

        // Unauthorized receiveFlashLoan must revert silently
        address[] memory toks = new address[](1);
        toks[0] = address(usdt);
        uint256[] memory amts = new uint256[](1);
        amts[0] = 1e18;
        uint256[] memory fees = new uint256[](1);
        vm.prank(address(0xdead));
        vm.expectRevert(bytes(""));
        executor.receiveFlashLoan(toks, amts, fees, "");
    }

    function test_V2ProfitRetentionNoCrossRouteLeakage() public {
        // Seed retained multi-token profits inside executor prior to tx
        usdt.mint(address(executor), 100e18);
        tokenA.mint(address(executor), 500e18);

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](3);
        steps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(executor), "");
        steps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, false, 9975, 10000, address(executor), "");
        steps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        uint256 ownerBefore = usdt.balanceOf(address(this));
        (uint256 netProfit,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                1, address(morpho), address(usdt), 1000e18, 1e18, false, address(0), address(0), 0, steps
            ),
            0
        );

        // Pre-existing 500 tokenA untouched by per-tx delta[], and netProfit reflects ONLY this tx!
        require(tokenA.balanceOf(address(executor)) == 500e18, "CROSS_ROUTE_LEAKED_TOKA");
        require(usdt.balanceOf(address(this)) == ownerBefore, "SHOULD_RETAIN_USDT");
        require(usdt.balanceOf(address(executor)) == 100e18 + netProfit, "RETAINED_USDT_MISMATCH");

        // Zero-unwrap ETH bribe fast path: fund executor with 1 ETH, retain WBNB profit 100%
        MockWBNB wbnb = new MockWBNB();
        (bool okW,) = address(wbnb).call{value: 500e18}("");
        require(okW, "FUND_WBNB_ERR");
        MockMoolah moolah = new MockMoolah();
        wbnb.mint(address(moolah), 100e18);

        MockUniV2Pair pBuy = new MockUniV2Pair(address(wbnb), address(tokenB), 100e18, 20_000e18, 9975);
        MockUniV2Pair pSell = new MockUniV2Pair(address(tokenB), address(wbnb), 10_000e18, 100e18, 9975);
        wbnb.mint(address(pBuy), 100e18);
        tokenB.mint(address(pBuy), 20_000e18);
        tokenB.mint(address(pSell), 10_000e18);
        wbnb.mint(address(pSell), 100e18);

        (bool okE,) = address(executor).call{value: 1 ether}("");
        require(okE, "FUND_EXEC_ETH");
        uint256 execEthBefore = address(executor).balance;
        uint256 execWbnbBefore = wbnb.balanceOf(address(executor));

        DagArbExecutor.DagStep[] memory wSteps = new DagArbExecutor.DagStep[](2);
        wSteps[0] = DagArbExecutor.DagStep(
            address(pBuy), address(wbnb), address(tokenB), 0, false, 9975, 10000, address(pSell), "");
        wSteps[1] = DagArbExecutor.DagStep(
            address(pSell), address(tokenB), address(wbnb), 0, true, 9975, 10000, address(executor), "");

        address bribeTarget = address(0xB41B);
        uint256 bribeBefore = bribeTarget.balance;
        uint256 floorMin = 0.01e18;
        (uint256 wNet,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                1, address(moolah), address(wbnb), 1e18, floorMin, false, address(wbnb), bribeTarget, 5000, wSteps
            ),
            0
        );
        uint256 expectedBribe = ((wNet - floorMin) * 5000) / 10000;
        require(expectedBribe > 0 && expectedBribe < execEthBefore, "EXPECTED_SMALL_BRIBE");
        require(bribeTarget.balance - bribeBefore == expectedBribe, "ETH_BRIBE_MISMATCH");
        require(execEthBefore - address(executor).balance == expectedBribe, "DID_NOT_USE_ETH_BUFFER");
        require(wbnb.balanceOf(address(executor)) - execWbnbBefore == wNet, "WBNB_SHOULD_NOT_UNWRAP");
    }

    function test_V2BatchedUniV4MultiHopSingleUnlock() public {
        MockUniV4PoolManager v4Mgr = new MockUniV4PoolManager();
        usdt.mint(address(v4Mgr), 100_000e18);
        tokenA.mint(address(v4Mgr), 100_000e18);

        // Pool1: usdt -> tokenA gives 2.0x; Pool2: tokenA -> usdt gives 0.6x => 1.20x net in 1 unlock!
        (address c0, address c1) =
            address(usdt) < address(tokenA) ? (address(usdt), address(tokenA)) : (address(tokenA), address(usdt));
        IUniV4PoolManager.PoolKey memory k1 = IUniV4PoolManager.PoolKey(c0, c1, 500, 10, address(0x1111));
        IUniV4PoolManager.PoolKey memory k2 = IUniV4PoolManager.PoolKey(c0, c1, 3000, -60, address(0x2222));
        v4Mgr.setPoolRate(k1, 20000);
        v4Mgr.setPoolRate(k2, 6000);

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep(
            address(v4Mgr),
            address(usdt),
            address(tokenA),
            5,
            false,
            0,
            10000,
            address(v4Mgr),
            abi.encode(c0, c1, uint24(500), int24(10), address(0x1111))
        );
        steps[1] = DagArbExecutor.DagStep(
            address(v4Mgr),
            address(tokenA),
            address(usdt),
            5,
            false,
            0,
            10000,
            address(v4Mgr),
            abi.encode(c0, c1, uint24(3000), int24(-60), address(0x2222))
        );

        usdt.mint(address(executor), 100e18);
        (uint256 netProfit,) = _execCompact(
            executor,
            DagArbExecutor.ArbParams(
                0, address(0), address(usdt), 100e18, 10e18, false, address(0), address(0), 0, steps
            ),
            0
        );
        require(netProfit == 20e18, "V4_BATCH_PROFIT_ERR");
        require(v4Mgr.unlockCalls() == 1, "V4_MUST_BATCH_SINGLE_UNLOCK");
        require(v4Mgr.swapCalls() == 2, "V4_SWAP_COUNT_ERR");
        require(v4Mgr.settleCalls() == 0, "V4_NET_ZERO_SETTLE");
        require(v4Mgr.takeCalls() == 1, "V4_ONLY_TAKE_NET_OUT");
    }

    function test_V2CallerWhitelistAndZeroCollision() public {
        address caller1 = address(0xCA11);
        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](3);
        steps[0] = DagArbExecutor.DagStep(
            address(v2Split1), address(usdt), address(tokenA), 0, false, 9975, 10000, address(v2Hop), "");
        steps[1] = DagArbExecutor.DagStep(
            address(v2Hop), address(tokenA), address(tokenB), 0, true, 9975, 10000, address(executor), "");
        steps[2] = DagArbExecutor.DagStep(
            address(v3Exit), address(tokenB), address(usdt), 3, false, 0, 10000, address(executor), "");

        DagArbExecutor.ArbParams memory params = DagArbExecutor.ArbParams(
            1, address(morpho), address(usdt), 1000e18, 1e18, false, address(0), address(0), 0, steps
        );
        bytes memory payload = encodeCompactArb(params, address(executor));
        require(payload[0] == 0x00, "MAGIC_BYTE_ZERO");

        // Unauthorized caller reverts silently
        vm.prank(caller1);
        (bool okBefore, bytes memory retBefore) = address(executor).call(payload);
        require(!okBefore && retBefore.length == 0, "UNAUTH_CALLER_MUST_REVERT");

        // Non-owner cannot setCallers
        address[] memory addrs = new address[](1);
        addrs[0] = caller1;
        vm.prank(caller1);
        vm.expectRevert(bytes("NOT_OWNER"));
        executor.setCallers(addrs, true);

        // Owner whitelists caller1, then caller1 succeeds and profits remain on executor
        executor.setCallers(addrs, true);
        require(executor.callers(caller1), "CALLER_NOT_SET");
        uint256 execBalBefore = usdt.balanceOf(address(executor));
        vm.prank(caller1);
        (bool okAfter, bytes memory retAfter) = address(executor).call(payload);
        require(okAfter, "WHITELISTED_CALLER_FAILED");
        (uint256 netProfit,) = abi.decode(retAfter, (uint256, uint256));
        require(netProfit > 1e18, "CALLER_PROFIT_LOW");
        require(usdt.balanceOf(address(executor)) == execBalBefore + netProfit, "CALLER_RETAIN_ERR");
    }
}

contract MockTaxERC20 {
    mapping(address => uint256) public balanceOf;
    uint32 public taxBps; // e.g. 500 = 5%

    constructor(uint32 _taxBps) {
        taxBps = _taxBps;
    }

    function mint(address to, uint256 amt) external {
        balanceOf[to] += amt;
    }

    function transfer(address to, uint256 amt) external returns (bool) {
        require(balanceOf[msg.sender] >= amt, "ERC20_BAL");
        balanceOf[msg.sender] -= amt;
        uint256 received = amt - (amt * taxBps) / 10000;
        balanceOf[to] += received;
        return true;
    }
}

contract MockTaxUniV2Pair {
    address public token0;
    address public token1;
    uint112 public reserve0;
    uint112 public reserve1;

    constructor(address _a, address _b, uint112 _ra, uint112 _rb) {
        if (_a < _b) {
            token0 = _a;
            token1 = _b;
            reserve0 = _ra;
            reserve1 = _rb;
        } else {
            token0 = _b;
            token1 = _a;
            reserve0 = _rb;
            reserve1 = _ra;
        }
    }

    function getReserves() external view returns (uint112, uint112, uint32) {
        return (reserve0, reserve1, 1);
    }

    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata) external {
        if (amount0Out > 0) MockTaxERC20(token0).transfer(to, amount0Out);
        if (amount1Out > 0) MockTaxERC20(token1).transfer(to, amount1Out);
    }
}

contract MockBubblingV3Flash {
    address public token0;
    address public token1;

    constructor(address _t0, address _t1) {
        token0 = _t0;
        token1 = _t1;
    }

    function flash(address recipient, uint256 amount0, uint256, bytes calldata data) external {
        MockTaxERC20(token0).transfer(recipient, amount0);
        uint256 fee0 = amount0 / 10000;
        (bool ok, bytes memory ret) = msg.sender.call(
            abi.encodeWithSignature("pancakeV3FlashCallback(uint256,uint256,bytes)", fee0, uint256(0), data)
        );
        if (!ok) {
            assembly {
                revert(add(ret, 32), mload(ret))
            }
        }
    }
}

contract DagArbDiagExecutorTest {
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));

    function test_DiagExecutorCustomErrors() public {
        DagArbDiagExecutor diag = new DagArbDiagExecutor(address(this));
        MockERC20 base = new MockERC20("BASE");
        MockTaxERC20 taxTok = new MockTaxERC20(500); // 5% transfer tax -> 50,000 ppm
        MockTaxUniV2Pair pair = new MockTaxUniV2Pair(address(base), address(taxTok), 1000e18, 1000e18);
        base.mint(address(pair), 1000e18);
        taxTok.mint(address(pair), 1000e18);

        MockBubblingV3Flash flash = new MockBubblingV3Flash(address(base), address(taxTok));
        base.mint(address(flash), 5000e18);

        // 1. Verify DiagProfitBelowMin in FLASH_FUNDED mode
        base.mint(address(diag), 100e18);
        DagArbExecutor.DagStep[] memory s1 = new DagArbExecutor.DagStep[](1);
        s1[0] = DagArbExecutor.DagStep(
            address(pair), address(base), address(taxTok), 0, false, 9975, 10000, address(diag), "");
        vm.expectRevert(abi.encodeWithSelector(DagArbDiagExecutor.DiagProfitBelowMin.selector, 0, 10e18));
        diag.executeArb(
            DagArbExecutor.ArbParams(0, address(0), address(base), 100e18, 10e18, false, address(0), address(0), 0, s1)
        );

        // 2. Verify DiagStepZeroReceived on 100% tax / zero balance
        DagArbDiagExecutor diagEmpty = new DagArbDiagExecutor(address(this));
        vm.expectRevert(
            abi.encodeWithSelector(
                DagArbDiagExecutor.DiagStepZeroReceived.selector, uint8(0), address(pair), address(base)
            )
        );
        diagEmpty.executeArb(
            DagArbExecutor.ArbParams(0, address(0), address(base), 100e18, 1, false, address(0), address(0), 0, s1)
        );
    }

    function test_DiagExecutorFlashShortfallTaxPpm() public {
        DagArbDiagExecutor diag = new DagArbDiagExecutor(address(this));
        MockTaxERC20 base = new MockTaxERC20(0);
        MockTaxERC20 taxTok = new MockTaxERC20(500); // 5% transfer tax = 50_000 ppm
        MockTaxUniV2Pair pBuy = new MockTaxUniV2Pair(address(base), address(taxTok), 10000e18, 10000e18);
        MockTaxUniV2Pair pSell = new MockTaxUniV2Pair(address(base), address(taxTok), 10000e18, 10000e18);
        base.mint(address(pBuy), 10000e18);
        taxTok.mint(address(pBuy), 10000e18);
        base.mint(address(pSell), 10000e18);
        taxTok.mint(address(pSell), 10000e18);

        MockBubblingV3Flash flash = new MockBubblingV3Flash(address(base), address(taxTok));
        base.mint(address(flash), 5000e18);

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep(
            address(pBuy), address(base), address(taxTok), 0, false, 9975, 10000, address(diag), "");
        steps[1] = DagArbExecutor.DagStep(
            address(pSell), address(taxTok), address(base), 0, false, 9975, 10000, address(diag), "");

        // Expect DiagFlashRepayShortfall reporting taxTok and maxTaxPpm == 50000
        try diag.executeArb(
            DagArbExecutor.ArbParams(
                3, address(flash), address(base), 100e18, 1, false, address(0), address(0), 0, steps
            )
        ) {
            revert("EXPECTED_REVERT");
        } catch (bytes memory rawErr) {
            bytes4 sel;
            assembly {
                sel := mload(add(rawErr, 32))
            }
            require(sel == DagArbDiagExecutor.DiagFlashRepayShortfall.selector, "WRONG_ERR_SELECTOR");
            address taxTokenOut;
            uint32 taxPpmOut;
            assembly {
                // Skip selector (4 bytes) + 3 words (borrow, repay, final) = 100 bytes; word4=taxToken, word5=taxPpm
                taxTokenOut := mload(add(rawErr, 132))
                taxPpmOut := mload(add(rawErr, 164))
            }
            require(taxTokenOut == address(taxTok), "WRONG_TAX_TOKEN");
            require(taxPpmOut >= 49990 && taxPpmOut <= 50000, "WRONG_TAX_PPM");
        }
    }

    function test_DiagExecutorMetricOmmStepAndFlash() public {
        DagArbDiagExecutor diag = new DagArbDiagExecutor(address(this));
        MockERC20 tA = new MockERC20("A");
        MockERC20 tB = new MockERC20("B");
        (MockERC20 highTok, MockERC20 lowTok) = address(tA) > address(tB) ? (tA, tB) : (tB, tA);
        // Unsorted Metric pool: token0 = highTok, token1 = lowTok; sell high->low 1.5x, exit via V2 1:1
        MockMetricOmmPool metric = new MockMetricOmmPool(address(highTok), address(lowTok), 15000, 10000);
        highTok.mint(address(metric), 200_000e18);
        lowTok.mint(address(metric), 200_000e18);

        MockUniV2Pair exitV2 = new MockUniV2Pair(address(lowTok), address(highTok), 100_000e18, 100_000e18, 9975);
        lowTok.mint(address(exitV2), 100_000e18);
        highTok.mint(address(exitV2), 100_000e18);

        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep(
            address(metric), address(highTok), address(lowTok), 6, false, 0, 10000, address(exitV2), hex"01");
        steps[1] = DagArbExecutor.DagStep(
            address(exitV2), address(lowTok), address(highTok), 0, true, 9975, 10000, address(diag), "");

        (uint256 prof,) = diag.executeArb(
            DagArbExecutor.ArbParams(
                6, address(metric), address(highTok), 100e18, 10e18, true, address(0), address(0), 0, steps
            )
        );
        require(prof > 40e18, "DIAG_METRIC_FLASH_PROFIT_ERR");

        // Funded regular step DEX_METRIC_OMM=6 on diag executor -> verifies DiagProfitBelowMin when gain < min
        highTok.mint(address(diag), 100e18);
        DagArbExecutor.DagStep[] memory roundTrip = new DagArbExecutor.DagStep[](2);
        roundTrip[0] = DagArbExecutor.DagStep(
            address(metric), address(highTok), address(lowTok), 6, false, 0, 10000, address(diag), hex"01");
        roundTrip[1] = DagArbExecutor.DagStep(
            address(metric), address(lowTok), address(highTok), 6, false, 0, 10000, address(diag), hex"00");
        // gain = 150 - 100 = 50e18 < 90e18
        vm.expectRevert(abi.encodeWithSelector(DagArbDiagExecutor.DiagProfitBelowMin.selector, 50e18, 90e18));
        diag.executeArb(
            DagArbExecutor.ArbParams(
                0, address(0), address(highTok), 100e18, 90e18, false, address(0), address(0), 0, roundTrip
            )
        );
    }

    function test_DiagExecutorBalancerV2StepAndFlash() public {
        DagArbDiagExecutor diag = new DagArbDiagExecutor(address(this));
        MockERC20 base = new MockERC20("WETH");
        MockERC20 quote = new MockERC20("USDC");
        MockBalancerV2Vault balVault = new MockBalancerV2Vault();
        base.mint(address(balVault), 10_000e18);
        quote.mint(address(balVault), 10_000e18);

        MockUniV2Pair exitV2 = new MockUniV2Pair(address(quote), address(base), 100_000e18, 120_000e18, 9975);
        quote.mint(address(exitV2), 100_000e18);
        base.mint(address(exitV2), 120_000e18);

        bytes32 poolId = bytes32(uint256(0xCAFE0002));
        DagArbExecutor.DagStep[] memory steps = new DagArbExecutor.DagStep[](2);
        steps[0] = DagArbExecutor.DagStep(
            address(balVault), address(base), address(quote), 7, false, 0, 10000, address(diag), abi.encode(poolId)
        );
        steps[1] = DagArbExecutor.DagStep(
            address(exitV2), address(quote), address(base), 0, address(quote) < address(base), 9975, 10000, address(diag), ""
        );

        uint256 amtOut0 = 105e18;
        uint256 expectedOut = (amtOut0 * 9975 * 120_000e18) / (100_000e18 * 10000 + amtOut0 * 9975);
        uint256 expectedProfit = expectedOut - 100e18;

        (uint256 prof,) = diag.executeArb(
            DagArbExecutor.ArbParams(
                7, address(balVault), address(base), 100e18, 5e18, true, address(0), address(0), 0, steps
            )
        );
        require(prof == expectedProfit, "DIAG_BAL_PROFIT_ERR");

        // Second run on fresh exit reserves gives slightly lower output, verify DiagProfitBelowMin
        MockUniV2Pair exitV2Tight = new MockUniV2Pair(address(quote), address(base), 100_000e18, 120_000e18, 9975);
        quote.mint(address(exitV2Tight), 100_000e18);
        base.mint(address(exitV2Tight), 120_000e18);
        steps[1].pool = address(exitV2Tight);
        vm.expectRevert(
            abi.encodeWithSelector(
                DagArbDiagExecutor.DiagProfitBelowMin.selector, expectedProfit, expectedProfit + 10e18
            )
        );
        diag.executeArb(
            DagArbExecutor.ArbParams(
                7, address(balVault), address(base), 100e18, expectedProfit + 10e18, true, address(0), address(0), 0, steps
            )
        );
    }
}
