// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

/**
 * ╔══════════════════════════════════════════════════════════════╗
 * ║                  FlashLiquidator v1                          ║
 * ║     Liquidate undercollateralized Aave V3 positions          ║
 * ║     using flash loans — ZERO capital required                ║
 * ║                    Arbitrum One                               ║
 * ╠══════════════════════════════════════════════════════════════╣
 * ║  Flow:                                                       ║
 * ║  1. Bot detects account with health factor < 1               ║
 * ║  2. Flash loan the debt token from Aave                      ║
 * ║  3. Call liquidationCall() to repay debt, receive collateral  ║
 * ║  4. Swap collateral → debt token on DEX (if different)       ║
 * ║  5. Repay flash loan + fee                                   ║
 * ║  6. Sweep profit to cold wallet                              ║
 * ║                                                              ║
 * ║  Security:                                                   ║
 * ║  - coldWallet immutable                                      ║
 * ║  - owner immutable (no transferOwnership)                    ║
 * ║  - onlyOwner on execute, onlyAavePool on callback            ║
 * ║  - Profit check before repayment                             ║
 * ║  - forceApprove for USDT-like tokens                         ║
 * ╚══════════════════════════════════════════════════════════════╝
 */

// ── Interfaces ───────────────────────────────────────────────────

interface IPoolAddressesProvider {
    function getPool() external view returns (address);
}

interface IPool {
    function flashLoanSimple(
        address receiverAddress,
        address asset,
        uint256 amount,
        bytes calldata params,
        uint16 referralCode
    ) external;

    function liquidationCall(
        address collateralAsset,
        address debtAsset,
        address user,
        uint256 debtToCover,
        bool receiveAToken
    ) external;

    function getUserAccountData(address user)
        external view returns (
            uint256 totalCollateralBase,
            uint256 totalDebtBase,
            uint256 availableBorrowsBase,
            uint256 currentLiquidationThreshold,
            uint256 ltv,
            uint256 healthFactor
        );

    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

interface IFlashLoanSimpleReceiver {
    function executeOperation(
        address asset, uint256 amount, uint256 premium,
        address initiator, bytes calldata params
    ) external returns (bool);
    function ADDRESSES_PROVIDER() external view returns (IPoolAddressesProvider);
    function POOL() external view returns (IPool);
}

interface ISwapRouter {
    struct ExactInputSingleParams {
        address tokenIn;
        address tokenOut;
        uint24  fee;
        address recipient;
        uint256 deadline;
        uint256 amountIn;
        uint256 amountOutMinimum;
        uint160 sqrtPriceLimitX96;
    }
    function exactInputSingle(ExactInputSingleParams calldata)
        external returns (uint256);
}

interface IERC20 {
    function balanceOf(address) external view returns (uint256);
    function transfer(address, uint256) external returns (bool);
    function approve(address, uint256) external returns (bool);
    function allowance(address, address) external view returns (uint256);
}

// ── Contract ─────────────────────────────────────────────────────

contract FlashLiquidator is IFlashLoanSimpleReceiver {

    // ── Events ──
    event LiquidationExecuted(
        address indexed user,
        address indexed debtAsset,
        address indexed collateralAsset,
        uint256 debtRepaid,
        uint256 collateralReceived,
        uint256 profitNet
    );
    event ProfitSwept(address indexed token, uint256 amount);
    event PauseToggled(bool paused);

    // ── Errors ──
    error NotOwner();
    error NotAavePool();
    error NotSelf();
    error Paused();
    error NothingToRescue();
    error Reentrancy();
    error NotProfitable(uint256 received, uint256 owed);
    error NotInFlashLoan();
    error InvalidAsset();

    // ── Immutables ──
    address public immutable owner;
    address public immutable coldWallet;
    IPool public immutable aavePool;
    IPoolAddressesProvider public immutable aaveProvider;
    ISwapRouter public immutable uniRouter;

    // ── Storage ──
    bool public paused;
    uint8 private _locked = 1; // 1 = unlocked, 2 = locked (1/2 pattern avoids cold SSTORE cost)

    // ── Modifiers ──
    modifier onlyOwner()    { if (msg.sender != owner) revert NotOwner(); _; }
    modifier notPaused()    { if (paused) revert Paused(); _; }
    modifier onlyAavePool() { if (msg.sender != address(aavePool)) revert NotAavePool(); _; }
    modifier nonReentrant() { if (_locked == 2) revert Reentrancy(); _locked = 2; _; _locked = 1; }

    // ── Struct for callback params ──
    struct LiqParams {
        address user;              // account to liquidate
        address collateralAsset;   // what we seize
        address debtAsset;         // what we repay (= flash loaned asset)
        uint256 debtToCover;       // how much debt to repay
        uint24  swapFeeTier;       // Uniswap fee tier for collateral→debt swap
        uint256 minSwapOut;        // min output from DEX swap (0 if same-token)
        uint256 minProfit;         // revert if profit < this (in debt token units)
    }

    constructor(
        address _provider,
        address _uniRouter,
        address _coldWallet
    ) {
        require(_provider   != address(0), "zero provider");
        require(_uniRouter  != address(0), "zero router");
        require(_coldWallet != address(0), "zero cold");

        owner      = msg.sender;
        coldWallet = _coldWallet;
        aaveProvider = IPoolAddressesProvider(_provider);
        aavePool     = IPool(aaveProvider.getPool());
        uniRouter    = ISwapRouter(_uniRouter);
    }

    // ═════════════════════════════════════════════════════════════
    // MAIN ENTRY — called by the Rust bot
    // ═════════════════════════════════════════════════════════════

    /**
     * @notice Liquidate an unhealthy Aave position via flash loan
     * @dev The bot must verify healthFactor < 1 before calling this.
     *      If collateral == debt token, no DEX swap is needed.
     *      If collateral != debt token, we swap via Uniswap.
     */
    function liquidate(
        address user,
        address collateralAsset,
        address debtAsset,
        uint256 debtToCover,
        uint24  swapFeeTier,
        uint256 minSwapOut,
        uint256 minProfit
    )
        external
        onlyOwner
        notPaused
        nonReentrant
    {
        bytes memory params = abi.encode(LiqParams({
            user: user,
            collateralAsset: collateralAsset,
            debtAsset: debtAsset,
            debtToCover: debtToCover,
            swapFeeTier: swapFeeTier,
            minSwapOut: minSwapOut,
            minProfit: minProfit
        }));

        // Flash loan the debt token
        aavePool.flashLoanSimple(
            address(this),
            debtAsset,
            debtToCover,
            params,
            0
        );

        // After flash loan callback completes, sweep all profits.
        // Sweep collateral first (dust in cross-token case), then debt asset (the profit).
        // Skip collateral sweep when same-token to avoid a redundant zero-balance call.
        if (collateralAsset != debtAsset) _sweepAll(collateralAsset);
        _sweepAll(debtAsset);
    }

    // ═════════════════════════════════════════════════════════════
    // AAVE CALLBACK
    // ═════════════════════════════════════════════════════════════

    function executeOperation(
        address asset,
        uint256 amount,
        uint256 premium,
        address initiator,
        bytes calldata params
    ) external override onlyAavePool returns (bool) {
        if (_locked != 2) revert NotInFlashLoan(); // must be inside our own flash loan call stack
        if (initiator != address(this)) revert NotSelf();
        return _doLiquidation(asset, amount, premium, params);
    }

    function _doLiquidation(
        address asset,
        uint256 amount,
        uint256 premium,
        bytes calldata params
    ) internal returns (bool) {
        LiqParams memory p = abi.decode(params, (LiqParams));
        if (asset != p.debtAsset) revert InvalidAsset();

        uint256 totalDebt = amount + premium;

        // Step 1: Approve Aave Pool to take debt token for liquidation
        _forceApprove(p.debtAsset, address(aavePool), p.debtToCover);

        // Step 2: Liquidate — repay debt, receive collateral at discount
        // receiveAToken = false → we get the underlying token, not aToken
        aavePool.liquidationCall(
            p.collateralAsset,
            p.debtAsset,
            p.user,
            p.debtToCover,
            false
        );

        // Step 3: handle collateral — swap to debt token if different assets
        uint256 collateralReceived;
        uint256 debtBalance;
        if (p.collateralAsset != p.debtAsset) {
            // Cross-token: read collateral balance once, swap all of it to debt token
            collateralReceived = IERC20(p.collateralAsset).balanceOf(address(this));
            if (collateralReceived > 0) {
                _swapToDebtToken(
                    p.collateralAsset, p.debtAsset,
                    collateralReceived, p.swapFeeTier, p.minSwapOut
                );
            }
            // Read debt balance after the swap (single call on this branch)
            debtBalance = IERC20(p.debtAsset).balanceOf(address(this));
        } else {
            // Same-token: flash loan amount == debtToCover (by construction), so
            // the balance after liquidationCall equals exactly the collateral received.
            // Read once and reuse — avoids a second balanceOf call.
            debtBalance = IERC20(p.debtAsset).balanceOf(address(this));
            collateralReceived = debtBalance;
        }

        // Step 4: Verify profitability
        if (debtBalance < totalDebt + p.minProfit) {
            revert NotProfitable(debtBalance, totalDebt + p.minProfit);
        }

        uint256 profit = debtBalance - totalDebt;

        // Step 5: Approve Aave to pull repayment
        _forceApprove(asset, address(aavePool), totalDebt);

        emit LiquidationExecuted(
            p.user, p.debtAsset, p.collateralAsset,
            p.debtToCover,
            collateralReceived,
            profit
        );

        return true;
    }

    // ═════════════════════════════════════════════════════════════
    // INTERNAL
    // ═════════════════════════════════════════════════════════════

    function _swapToDebtToken(
        address tokenIn,
        address tokenOut,
        uint256 amountIn,
        uint24 feeTier,
        uint256 minOut
    ) internal {
        _forceApprove(tokenIn, address(uniRouter), amountIn);

        uniRouter.exactInputSingle(ISwapRouter.ExactInputSingleParams({
            tokenIn:           tokenIn,
            tokenOut:          tokenOut,
            fee:               feeTier,
            recipient:         address(this),
            deadline:          block.timestamp + 120,
            amountIn:          amountIn,
            amountOutMinimum:  minOut,
            sqrtPriceLimitX96: 0
        }));
    }

    function _forceApprove(address token, address spender, uint256 amount) internal {
        IERC20 t = IERC20(token);
        if (t.allowance(address(this), spender) != 0) {
            require(t.approve(spender, 0), "approve0");
        }
        require(t.approve(spender, amount), "approve");
    }

    function _sweepAll(address token) internal {
        uint256 bal = IERC20(token).balanceOf(address(this));
        if (bal == 0) return;
        require(IERC20(token).transfer(coldWallet, bal), "sweep");
        emit ProfitSwept(token, bal);
    }

    // ═════════════════════════════════════════════════════════════
    // ADMIN
    // ═════════════════════════════════════════════════════════════

    function setPaused(bool _p) external onlyOwner { paused = _p; emit PauseToggled(_p); }

    function rescueTokens(address token) external onlyOwner {
        uint256 bal = IERC20(token).balanceOf(address(this));
        if (bal == 0) revert NothingToRescue();
        require(IERC20(token).transfer(coldWallet, bal), "rescue");
    }

    function rescueETH() external onlyOwner {
        uint256 bal = address(this).balance;
        if (bal == 0) revert NothingToRescue();
        (bool ok,) = coldWallet.call{value: bal}("");
        require(ok, "eth rescue");
    }

    // ── View helpers for the bot ──

    function getHealthFactor(address user) external view returns (uint256) {
        (,,,,, uint256 hf) = aavePool.getUserAccountData(user);
        return hf;
    }

    function getFlashPremiumBps() external view returns (uint128) {
        return aavePool.FLASHLOAN_PREMIUM_TOTAL();
    }

    // ── Interface ──
    function ADDRESSES_PROVIDER() external view override returns (IPoolAddressesProvider) { return aaveProvider; }
    function POOL() external view override returns (IPool) { return aavePool; }

    receive() external payable { revert("no ETH"); }
    fallback() external payable { revert("no fallback"); }
}
