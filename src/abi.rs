use alloy::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
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

    #[derive(Debug, PartialEq, Eq)]
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

    interface IDagArbExecutor {
        event ArbExecuted(
            address indexed baseToken,
            uint256 borrowAmount,
            uint256 netProfit,
            uint256 bribeWei,
            uint256 gasUsed
        );
        function owner() external view returns (address);
        function callers(address addr) external view returns (bool);
        function setCallers(address[] calldata addrs, bool allowed) external;
        function executeArb(ArbParams calldata params) external returns (uint256 netProfit, uint256 gasUsed);
        function rescueTokens(address token, uint256 amount) external;
        function rescueETH() external;
    }

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

    interface IMulticall3 {
        struct Call3 {
            address target;
            bool allowFailure;
            bytes callData;
        }
        struct Result {
            bool success;
            bytes returnData;
        }
        function aggregate3(Call3[] calldata calls) external payable returns (Result[] memory returnData);
        function getEthBalance(address addr) external view returns (uint256 balance);
    }

    interface IERC20 {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string memory);
        function balanceOf(address account) external view returns (uint256);
    }

    interface IUniV2Pair {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function factory() external view returns (address);
        function swapFee() external view returns (uint32);
        function getReserves() external view returns (uint112 reserve0, uint112 reserve1, uint32 blockTimestampLast);
    }

    interface ISolidlyPair {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function factory() external view returns (address);
        function stable() external view returns (bool);
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
    }

    interface IV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
        function factory() external view returns (address);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
        function liquidity() external view returns (uint128);
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick);
    }

    interface IInfinityCLPoolManager {
        function getSlot0(bytes32 id) external view returns (uint160 sqrtPriceX96, int24 tick, uint24 protocolFee, uint24 lpFee);
        function getLiquidity(bytes32 id) external view returns (uint128 liquidity);
        function poolIdToPoolKey(bytes32 id) external view returns (address currency0, address currency1, address hooks, address poolManager, uint24 fee, bytes32 parameters);
    }

    interface IUniV4PoolManager {
        function extsload(bytes32 slot) external view returns (bytes32 value);
    }

    interface IUniV4PositionManager {
        function poolKeys(bytes25 poolId) external view returns (address currency0, address currency1, uint24 fee, int24 tickSpacing, address hooks);
    }

    interface IMetricOmmPool {
        function getImmutables() external view;
        function getSellAndBuyPrices() external view returns (uint128 sellPriceX64, uint128 buyPriceX64);
        function extsload(bytes32 slot) external view returns (bytes32 value);
    }

    interface IUniV2Factory {
        function getPair(address tokenA, address tokenB) external view returns (address pair);
    }

    interface ISolidlyFactoryGetPair {
        function getPair(address tokenA, address tokenB, bool stable) external view returns (address pair);
        function getFee(bool stable) external view returns (uint256);
    }

    interface ISolidlyFactoryGetPool {
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool);
        function getFee(address pool, bool stable) external view returns (uint256);
    }

    interface IV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }

    interface ISlipstreamFactory {
        function getPool(address tokenA, address tokenB, int24 tickSpacing) external view returns (address pool);
    }

    interface IBalancerV2Vault {
        function getPoolTokens(bytes32 poolId) external view returns (address[] memory tokens, uint256[] memory balances, uint256 lastChangeBlock);
    }

    interface IBalancerV2Pool {
        function getSwapFeePercentage() external view returns (uint256);
        function getNormalizedWeights() external view returns (uint256[] memory);
        function getAmplificationParameter() external view returns (uint256 value, bool isUpdating, uint256 precision);
        function getScalingFactors() external view returns (uint256[] memory);
        function getBptIndex() external view returns (uint256);
    }

    interface IL1GasOracle {
        function getL1Fee(bytes calldata data) external view returns (uint256);
    }
}
