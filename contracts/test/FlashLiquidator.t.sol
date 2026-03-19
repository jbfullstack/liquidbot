// SPDX-License-Identifier: MIT
pragma solidity ^0.8.23;

import {Test, console2} from "forge-std/Test.sol";
import {FlashLiquidator} from "../src/FlashLiquidator.sol";

// ═══════════════════════════════════════════════════════════════
// MOCKS
// ═══════════════════════════════════════════════════════════════

contract MockERC20 {
    string public name;
    uint8  public decimals;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    bool public requireZeroAllowance;

    constructor(string memory _n, uint8 _d) { name = _n; decimals = _d; }
    function setRequireZero(bool v) external { requireZeroAllowance = v; }
    function mint(address to, uint256 a) external { balanceOf[to] += a; }

    function transfer(address to, uint256 a) external returns (bool) {
        require(balanceOf[msg.sender] >= a, "bal");
        balanceOf[msg.sender] -= a; balanceOf[to] += a; return true;
    }
    function transferFrom(address f, address t, uint256 a) external returns (bool) {
        require(allowance[f][msg.sender] >= a, "allow");
        require(balanceOf[f] >= a, "bal");
        allowance[f][msg.sender] -= a; balanceOf[f] -= a; balanceOf[t] += a; return true;
    }
    function approve(address s, uint256 a) external returns (bool) {
        if (requireZeroAllowance && allowance[msg.sender][s] != 0 && a != 0) revert("USDT approve");
        allowance[msg.sender][s] = a; return true;
    }
}

contract MockAaveProvider {
    address public pool;
    constructor(address _p) { pool = _p; }
    function getPool() external view returns (address) { return pool; }
}

contract MockAavePool {
    uint128 public flashloanPremiumTotal = 5;

    // Simulated user data
    mapping(address => uint256) public healthFactors;
    uint256 public liquidationBonus = 500; // 5% in BPS

    MockERC20 public debtToken;
    MockERC20 public collToken;

    function setUserHf(address u, uint256 hf) external { healthFactors[u] = hf; }
    function setTokens(address _debt, address _coll) external {
        debtToken = MockERC20(_debt); collToken = MockERC20(_coll);
    }
    function setBonus(uint256 b) external { liquidationBonus = b; }

    // Must match IPool interface (SCREAMING_SNAKE_CASE is the actual Aave function name)
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128) {
        return flashloanPremiumTotal;
    }

    function getUserAccountData(address user) external view returns (
        uint256, uint256, uint256, uint256, uint256, uint256
    ) {
        return (0, 0, 0, 0, 0, healthFactors[user]);
    }

    function flashLoanSimple(
        address receiver, address asset, uint256 amount,
        bytes calldata params, uint16
    ) external {
        uint256 premium = amount * flashloanPremiumTotal / 10_000;
        require(MockERC20(asset).transfer(receiver, amount), "flash: transfer failed");
        (bool ok, bytes memory ret) = receiver.call(
            abi.encodeWithSignature(
                "executeOperation(address,uint256,uint256,address,bytes)",
                asset, amount, premium, receiver, params
            )
        );
        if (!ok) { assembly { revert(add(ret, 32), mload(ret)) } }
        require(MockERC20(asset).transferFrom(receiver, address(this), amount + premium), "flash: repay failed");
    }

    function liquidationCall(
        address collateralAsset, address debtAsset, address user,
        uint256 debtToCover, bool
    ) external {
        require(healthFactors[user] < 1e18, "healthy");
        // Take debt from liquidator
        require(MockERC20(debtAsset).transferFrom(msg.sender, address(this), debtToCover), "liq: debt transfer failed");
        // Give collateral + bonus
        uint256 collateralOut = debtToCover + (debtToCover * liquidationBonus / 10_000);
        require(MockERC20(collateralAsset).transfer(msg.sender, collateralOut), "liq: coll transfer failed");
        // Improve health factor
        healthFactors[user] = 2e18;
    }
}

contract MockUniRouter {
    uint256 public rateBps = 9_990; // 0.1% loss

    struct ExactInputSingleParams {
        address tokenIn; address tokenOut; uint24 fee;
        address recipient; uint256 deadline; uint256 amountIn;
        uint256 amountOutMinimum; uint160 sqrtPriceLimitX96;
    }

    function setRate(uint256 r) external { rateBps = r; }

    function exactInputSingle(ExactInputSingleParams calldata p) external returns (uint256) {
        uint256 out = p.amountIn * rateBps / 10_000;
        require(out >= p.amountOutMinimum, "slippage");
        require(MockERC20(p.tokenIn).transferFrom(msg.sender, address(this), p.amountIn), "uni: in failed");
        require(MockERC20(p.tokenOut).transfer(p.recipient, out), "uni: out failed");
        return out;
    }
}

// ═══════════════════════════════════════════════════════════════
// INVARIANT HANDLER
// ═══════════════════════════════════════════════════════════════

/// @dev Wraps valid bounded liquidation calls for the invariant fuzzer.
///      Without this, the fuzzer sends random inputs that all revert (NotOwner,
///      wrong tokens, etc.) and the invariants pass trivially without exercising
///      any real state transitions.
contract InvariantHandler is Test {
    FlashLiquidator public liq;
    MockAavePool    public aave;
    MockERC20       public usdc;
    address         public owner;

    uint256 constant U6  = 1e6;
    uint256 constant U18 = 1e18;

    // Ghost variable: tracks total profit swept to cold wallet across all calls.
    // Used by the invariant to confirm funds actually moved.
    uint256 public ghostTotalSwept;

    constructor(
        FlashLiquidator _liq,
        MockAavePool    _aave,
        MockERC20       _usdc,
        address         _owner
    ) {
        liq   = _liq;
        aave  = _aave;
        usdc  = _usdc;
        owner = _owner;
    }

    /// @dev Bounded same-token liquidation.  The fuzzer calls this with arbitrary
    ///      seeds; bound() clamps them to a valid range.
    function liquidateSameToken(uint256 debtSeed, uint256 bonusSeed) external {
        uint256 debt  = bound(debtSeed, 100 * U6, 5_000 * U6);
        uint256 bonus = bound(bonusSeed, 100, 1000); // 1%–10%

        address victim = makeAddr(string(abi.encodePacked("victim", debtSeed)));
        aave.setUserHf(victim, 0.90e18);
        aave.setBonus(bonus);
        // Ensure the Aave mock has enough liquidity for this liquidation
        usdc.mint(address(aave), debt * 2);

        uint256 coldBefore = usdc.balanceOf(liq.COLD_WALLET());
        vm.prank(owner);
        try liq.liquidate(victim, address(usdc), address(usdc), debt, 3000, 0, 0, address(aave)) {
            ghostTotalSwept += usdc.balanceOf(liq.COLD_WALLET()) - coldBefore;
        } catch {}
    }
}

// ═══════════════════════════════════════════════════════════════
// TESTS
// ═══════════════════════════════════════════════════════════════

contract FlashLiquidatorTest is Test {
    FlashLiquidator  public liq;
    MockAavePool     public aave;
    MockUniRouter    public uni;
    MockERC20        public usdc;  // debt token
    MockERC20        public weth;  // collateral token
    InvariantHandler public handler;

    address owner      = makeAddr("owner");
    address coldWallet = makeAddr("cold");
    address attacker   = makeAddr("attacker");
    address borrower   = makeAddr("borrower");

    uint256 constant U6  = 1e6;
    uint256 constant U18 = 1e18;

    function setUp() public {
        usdc = new MockERC20("USDC", 6);
        weth = new MockERC20("WETH", 18);
        usdc.setRequireZero(true); // USDT-like behavior

        aave = new MockAavePool();
        uni  = new MockUniRouter();
        MockAaveProvider provider = new MockAaveProvider(address(aave));

        aave.setTokens(address(usdc), address(weth));

        vm.prank(owner);
        liq = new FlashLiquidator(address(provider), address(uni), coldWallet);

        // Seed liquidity
        usdc.mint(address(aave), 10_000_000 * U6);
        weth.mint(address(aave), 5_000 * U18);
        usdc.mint(address(uni), 10_000_000 * U6);
        weth.mint(address(uni), 5_000 * U18);

        // Wire invariant handler — only it is targeted; mocks and liq are excluded
        // so the fuzzer cannot break preconditions by calling them directly.
        handler = new InvariantHandler(liq, aave, usdc, owner);
        targetContract(address(handler));
        excludeContract(address(liq));
        excludeContract(address(aave));
        excludeContract(address(uni));
        excludeContract(address(usdc));
        excludeContract(address(weth));
    }

    // ─── A. Deployment ───────────────────────────────────────

    function test_A01_owner() public view { assertEq(liq.OWNER(), owner); }
    function test_A02_coldWallet() public view { assertEq(liq.COLD_WALLET(), coldWallet); }
    function test_A03_notPaused() public view { assertFalse(liq.paused()); }

    function test_A04_revertZeroCold() public {
        MockAaveProvider p = new MockAaveProvider(address(aave));
        vm.expectRevert("zero cold");
        new FlashLiquidator(address(p), address(uni), address(0));
    }

    // ─── B. Access Control ───────────────────────────────────

    function test_B01_onlyOwnerCanLiquidate() public {
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.NotOwner.selector);
        liq.liquidate(borrower, address(weth), address(usdc), 1000*U6, 3000, 0, 0, address(aave));
    }

    function test_B02_onlyOwnerCanPause() public {
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.NotOwner.selector);
        liq.setPaused(true);
    }

    function test_B03_onlyOwnerCanRescue() public {
        usdc.mint(address(liq), 100*U6);
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.NotOwner.selector);
        liq.rescueTokens(address(usdc));
    }

    function test_B04_executeOpOnlyAave() public {
        bytes memory params = abi.encode(FlashLiquidator.LiqParams(
            borrower, address(weth), address(usdc), 1000*U6, 3000, 0, 0, address(aave)
        ));
        vm.prank(attacker);
        vm.expectRevert(FlashLiquidator.NotAavePool.selector);
        liq.executeOperation(address(usdc), 1000*U6, 500, attacker, params);
    }

    function test_B05_cantLiquidateWhenPaused() public {
        vm.prank(owner);
        liq.setPaused(true);
        vm.prank(owner);
        vm.expectRevert(FlashLiquidator.Paused.selector);
        liq.liquidate(borrower, address(weth), address(usdc), 1000*U6, 3000, 0, 0, address(aave));
    }

    function test_B06_executeOpNotInFlashLoan() public {
        // Aave pool IS the caller (passes onlyAavePool), but _locked != 2 because
        // we are not inside a liquidate() call — must revert NotInFlashLoan.
        bytes memory params = abi.encode(FlashLiquidator.LiqParams(
            borrower, address(weth), address(usdc), 1000*U6, 3000, 0, 0, address(aave)
        ));
        vm.prank(address(aave));
        vm.expectRevert(FlashLiquidator.NotInFlashLoan.selector);
        liq.executeOperation(address(usdc), 1000*U6, 5, address(liq), params);
    }

    // ─── C. Same-token liquidation (collateral == debt) ──────

    function test_C01_sameTokenLiquidation() public {
        // Borrower borrowed USDC against USDC collateral (e-mode stablecoins)
        aave.setUserHf(borrower, 0.95e18); // underwater
        aave.setBonus(500); // 5%

        uint256 debtToCover = 1_000 * U6;
        uint256 coldBefore = usdc.balanceOf(coldWallet);

        vm.prank(owner);
        liq.liquidate(
            borrower, address(usdc), address(usdc),
            debtToCover, 3000, 0, 0, address(aave)
        );

        uint256 profit = usdc.balanceOf(coldWallet) - coldBefore;
        console2.log("Same-token profit (raw):", profit);
        console2.log("Same-token profit (USD):", profit / U6);
        assertGt(profit, 0, "must profit");
        assertEq(usdc.balanceOf(address(liq)), 0, "contract empty");
    }

    // ─── D. Cross-token liquidation (collateral != debt) ─────
    // Scenario: borrower owes USDC (debt), locked DAI as collateral.
    // Flow: flash loan USDC → liquidate → receive DAI → swap DAI→USDC → repay → profit in USDC.

    function test_D01_crossTokenLiquidation() public {
        MockERC20 dai = new MockERC20("DAI", 18);
        dai.mint(address(aave), 10_000_000 * U18);
        dai.mint(address(uni), 10_000_000 * U18);
        aave.setUserHf(borrower, 0.90e18);
        aave.setBonus(500); // 5%

        uint256 debtToCover = 500 * U6; // 500 USDC
        uint256 coldBefore  = usdc.balanceOf(coldWallet);

        vm.prank(owner);
        // collateralAsset=DAI, debtAsset=USDC → exercises the Uniswap swap path
        liq.liquidate(
            borrower, address(dai), address(usdc),
            debtToCover, 3000, 1, 0, address(aave)  // minSwapOut=1 (non-zero to exercise slippage guard)
        );

        uint256 profit = usdc.balanceOf(coldWallet) - coldBefore;
        console2.log("Cross-token profit (USDC raw):", profit);
        assertGt(profit, 0, "must profit");
    }

    function test_D02_crossTokenContractEmpty() public {
        // After cross-token: contract must hold zero of BOTH tokens (no DAI dust, no USDC residual)
        MockERC20 dai = new MockERC20("DAI", 18);
        dai.mint(address(aave), 10_000_000 * U18);
        dai.mint(address(uni), 10_000_000 * U18);
        aave.setUserHf(borrower, 0.90e18);
        aave.setBonus(500);

        vm.prank(owner);
        liq.liquidate(borrower, address(dai), address(usdc), 500*U6, 3000, 1, 0, address(aave));

        assertEq(dai.balanceOf(address(liq)),  0, "no residual DAI");
        assertEq(usdc.balanceOf(address(liq)), 0, "no residual USDC");
    }

    // ─── E. Profitability checks ─────────────────────────────

    function test_E01_revertIfNotProfitable() public {
        aave.setUserHf(borrower, 0.95e18);
        aave.setBonus(0); // 0% bonus → no profit possible

        vm.prank(owner);
        vm.expectRevert(); // NotProfitable
        liq.liquidate(borrower, address(usdc), address(usdc), 1000*U6, 3000, 0, 0, address(aave));
    }

    function test_E02_revertIfProfitBelowMin() public {
        aave.setUserHf(borrower, 0.95e18);
        aave.setBonus(50); // 0.5% bonus → tiny profit

        vm.prank(owner);
        vm.expectRevert(); // NotProfitable (profit < minProfit)
        liq.liquidate(
            borrower, address(usdc), address(usdc),
            1000*U6, 3000,
            0, 100 * U6, address(aave) // require $100 min profit → impossible with 0.5% on $1k
        );
    }

    function test_E03_revertMinSwapOut() public {
        // Cross-token: verify Uniswap slippage guard fires when minSwapOut is too high.
        // Router at 99.9% rate → 500 USDC flash → receives 525 DAI → swap yields ~524.4 USDC.
        // Setting minSwapOut to 10M USDC must revert with "slippage".
        MockERC20 dai = new MockERC20("DAI", 18);
        dai.mint(address(aave), 10_000_000 * U18);
        dai.mint(address(uni), 10_000_000 * U18);
        aave.setUserHf(borrower, 0.90e18);
        aave.setBonus(500);

        vm.prank(owner);
        vm.expectRevert(bytes("slippage"));
        liq.liquidate(
            borrower, address(dai), address(usdc),
            500*U6, 3000,
            10_000_000 * U6, // impossibly high minSwapOut
            0, address(aave)
        );
    }

    // ─── F. Cold wallet guarantees ───────────────────────────

    function test_F01_profitsOnlyToCold() public {
        aave.setUserHf(borrower, 0.95e18);
        aave.setBonus(500);

        uint256 ownerBefore = usdc.balanceOf(owner);
        uint256 attackerBefore = usdc.balanceOf(attacker);

        vm.prank(owner);
        liq.liquidate(borrower, address(usdc), address(usdc), 1000*U6, 3000, 0, 0, address(aave));

        assertEq(usdc.balanceOf(owner), ownerBefore, "owner gets nothing");
        assertEq(usdc.balanceOf(attacker), attackerBefore, "attacker gets nothing");
        assertGt(usdc.balanceOf(coldWallet), 0, "cold gets profit");
        assertEq(usdc.balanceOf(address(liq)), 0, "contract empty");
    }

    function test_F02_coldWalletImmutable() public view {
        assertEq(liq.COLD_WALLET(), coldWallet);
    }

    // ─── G. Multiple liquidations ────────────────────────────

    function test_G01_multipleLiquidations() public {
        aave.setBonus(500);

        for (uint i = 0; i < 5; i++) {
            address user = makeAddr(string(abi.encodePacked("user", i)));
            aave.setUserHf(user, 0.90e18);
            vm.prank(owner);
            liq.liquidate(user, address(usdc), address(usdc), 500*U6, 3000, 0, 0, address(aave));
        }

        uint256 total = usdc.balanceOf(coldWallet);
        console2.log("Total after 5 liquidations:", total / U6, "USD");
        assertGt(total, 0);
    }

    // ─── H. USDT approve edge case ──────────────────────────

    function test_H01_forceApproveUSDT() public {
        aave.setUserHf(borrower, 0.95e18);
        aave.setBonus(500);

        // First liquidation
        vm.prank(owner);
        liq.liquidate(borrower, address(usdc), address(usdc), 500*U6, 3000, 0, 0, address(aave));

        // Reset borrower
        aave.setUserHf(borrower, 0.85e18);

        // Second must not fail from residual allowance
        vm.prank(owner);
        liq.liquidate(borrower, address(usdc), address(usdc), 500*U6, 3000, 0, 0, address(aave));

        assertGt(usdc.balanceOf(coldWallet), 0);
    }

    // ─── I. Admin ────────────────────────────────────────────

    function test_I01_rescue() public {
        usdc.mint(address(liq), 500*U6);
        vm.prank(owner);
        liq.rescueTokens(address(usdc));
        assertEq(usdc.balanceOf(coldWallet), 500*U6);
    }

    function test_I02_rescueETH() public {
        vm.deal(address(liq), 0.1 ether);
        vm.prank(owner);
        liq.rescueEth();
        assertEq(address(liq).balance, 0);
    }

    function test_I03_rejectETH() public {
        vm.deal(address(this), 1 ether);
        (bool ok,) = address(liq).call{value: 1 ether}("");
        assertFalse(ok);
    }

    // ─── J. View helpers ─────────────────────────────────────

    function test_J01_getHealthFactor() public {
        aave.setUserHf(borrower, 1.5e18);
        assertEq(liq.getHealthFactor(borrower), 1.5e18);
    }

    function test_J02_getFlashPremium() public view {
        assertEq(liq.getFlashPremiumBps(), 5);
    }

    // ─── K. Fuzz ─────────────────────────────────────────────

    function testFuzz_K01_onlyOwner(address caller) public {
        vm.assume(caller != owner);
        vm.prank(caller);
        vm.expectRevert(FlashLiquidator.NotOwner.selector);
        liq.liquidate(borrower, address(usdc), address(usdc), 1000*U6, 3000, 0, 0, address(aave));
    }

    // ─── L. Invariants ───────────────────────────────────────

    function invariant_L01_contractEmpty() public view {
        assertEq(usdc.balanceOf(address(liq)), 0, "USDC residual in contract");
        assertEq(weth.balanceOf(address(liq)), 0, "WETH residual in contract");
        // If the handler ran any successful liquidation, profits must be in cold wallet
        if (handler.ghostTotalSwept() > 0) {
            assertGt(usdc.balanceOf(liq.COLD_WALLET()), 0, "profit not swept to cold");
        }
    }

    function invariant_L02_coldFixed() public view {
        assertEq(liq.COLD_WALLET(), coldWallet);
    }

    function invariant_L03_ownerFixed() public view {
        assertEq(liq.OWNER(), owner);
    }
}
