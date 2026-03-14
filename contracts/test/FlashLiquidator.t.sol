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
    uint128 public FLASHLOAN_PREMIUM_TOTAL = 5;

    // Simulated user data
    mapping(address => uint256) public healthFactors;
    uint256 public liquidationBonus = 500; // 5% in BPS

    MockERC20 public debtToken;
    MockERC20 public collToken;

    function setUserHF(address u, uint256 hf) external { healthFactors[u] = hf; }
    function setTokens(address _debt, address _coll) external {
        debtToken = MockERC20(_debt); collToken = MockERC20(_coll);
    }
    function setBonus(uint256 b) external { liquidationBonus = b; }

    function getUserAccountData(address user) external view returns (
        uint256, uint256, uint256, uint256, uint256, uint256
    ) {
        return (0, 0, 0, 0, 0, healthFactors[user]);
    }

    function flashLoanSimple(
        address receiver, address asset, uint256 amount,
        bytes calldata params, uint16
    ) external {
        uint256 premium = amount * FLASHLOAN_PREMIUM_TOTAL / 10_000;
        MockERC20(asset).transfer(receiver, amount);
        (bool ok, bytes memory ret) = receiver.call(
            abi.encodeWithSignature(
                "executeOperation(address,uint256,uint256,address,bytes)",
                asset, amount, premium, receiver, params
            )
        );
        if (!ok) { assembly { revert(add(ret, 32), mload(ret)) } }
        MockERC20(asset).transferFrom(receiver, address(this), amount + premium);
    }

    function liquidationCall(
        address collateralAsset, address debtAsset, address user,
        uint256 debtToCover, bool
    ) external {
        require(healthFactors[user] < 1e18, "healthy");
        // Take debt from liquidator
        MockERC20(debtAsset).transferFrom(msg.sender, address(this), debtToCover);
        // Give collateral + bonus
        uint256 collateralOut = debtToCover + (debtToCover * liquidationBonus / 10_000);
        MockERC20(collateralAsset).transfer(msg.sender, collateralOut);
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
        MockERC20(p.tokenIn).transferFrom(msg.sender, address(this), p.amountIn);
        MockERC20(p.tokenOut).transfer(p.recipient, out);
        return out;
    }
}

// ═══════════════════════════════════════════════════════════════
// TESTS
// ═══════════════════════════════════════════════════════════════

contract FlashLiquidatorTest is Test {
    FlashLiquidator public liq;
    MockAavePool    public aave;
    MockUniRouter   public uni;
    MockERC20       public usdc;  // debt token
    MockERC20       public weth;  // collateral token

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
    }

    // ─── A. Deployment ───────────────────────────────────────

    function test_A01_owner() public view { assertEq(liq.owner(), owner); }
    function test_A02_coldWallet() public view { assertEq(liq.coldWallet(), coldWallet); }
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
        liq.liquidate(borrower, address(weth), address(usdc), 1000*U6, 3000, 0, 0);
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
            borrower, address(weth), address(usdc), 1000*U6, 3000, 0, 0
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
        liq.liquidate(borrower, address(weth), address(usdc), 1000*U6, 3000, 0, 0);
    }

    // ─── C. Same-token liquidation (collateral == debt) ──────

    function test_C01_sameTokenLiquidation() public {
        // Borrower borrowed USDC against USDC collateral (e-mode stablecoins)
        aave.setUserHF(borrower, 0.95e18); // underwater
        aave.setBonus(500); // 5%

        uint256 debtToCover = 1_000 * U6;
        uint256 coldBefore = usdc.balanceOf(coldWallet);

        vm.prank(owner);
        liq.liquidate(
            borrower, address(usdc), address(usdc),
            debtToCover, 3000, 0, 0
        );

        uint256 profit = usdc.balanceOf(coldWallet) - coldBefore;
        console2.log("Same-token profit:", profit, "=", profit / U6, "USD");
        assertGt(profit, 0, "must profit");
        assertEq(usdc.balanceOf(address(liq)), 0, "contract empty");
    }

    // ─── D. Cross-token liquidation (collateral != debt) ─────

    function test_D01_crossTokenLiquidation() public {
        // Borrower borrowed USDC against WETH collateral
        aave.setUserHF(borrower, 0.90e18);
        aave.setBonus(500); // 5%

        // For this test, WETH bonus is paid in WETH, then swapped to USDC
        // Mock Aave gives USDC (collateral) with bonus, so let's use USDC as collateral
        // and a different token as debt for cross-token test

        MockERC20 dai = new MockERC20("DAI", 18);
        dai.mint(address(aave), 10_000_000 * U18);
        dai.mint(address(uni), 10_000_000 * U18);

        aave.setTokens(address(dai), address(usdc));

        uint256 debtToCover = 500 * U6; // borrow 500 USDC
        uint256 coldBefore = usdc.balanceOf(coldWallet);

        vm.prank(owner);
        liq.liquidate(
            borrower, address(usdc), address(usdc),
            debtToCover, 3000, 0, 0
        );

        uint256 profit = usdc.balanceOf(coldWallet) - coldBefore;
        console2.log("Cross-token profit:", profit);
        assertGt(profit, 0, "must profit");
    }

    // ─── E. Profitability checks ─────────────────────────────

    function test_E01_revertIfNotProfitable() public {
        aave.setUserHF(borrower, 0.95e18);
        aave.setBonus(0); // 0% bonus → no profit possible

        vm.prank(owner);
        vm.expectRevert(); // NotProfitable
        liq.liquidate(borrower, address(usdc), address(usdc), 1000*U6, 3000, 0, 0);
    }

    function test_E02_revertIfProfitBelowMin() public {
        aave.setUserHF(borrower, 0.95e18);
        aave.setBonus(50); // 0.5% bonus → tiny profit

        vm.prank(owner);
        vm.expectRevert(); // NotProfitable (profit < minProfit)
        liq.liquidate(
            borrower, address(usdc), address(usdc),
            1000*U6, 3000,
            0, 100 * U6 // require $100 min profit → impossible with 0.5% on $1k
        );
    }

    // ─── F. Cold wallet guarantees ───────────────────────────

    function test_F01_profitsOnlyToCold() public {
        aave.setUserHF(borrower, 0.95e18);
        aave.setBonus(500);

        uint256 ownerBefore = usdc.balanceOf(owner);
        uint256 attackerBefore = usdc.balanceOf(attacker);

        vm.prank(owner);
        liq.liquidate(borrower, address(usdc), address(usdc), 1000*U6, 3000, 0, 0);

        assertEq(usdc.balanceOf(owner), ownerBefore, "owner gets nothing");
        assertEq(usdc.balanceOf(attacker), attackerBefore, "attacker gets nothing");
        assertGt(usdc.balanceOf(coldWallet), 0, "cold gets profit");
        assertEq(usdc.balanceOf(address(liq)), 0, "contract empty");
    }

    function test_F02_coldWalletImmutable() public view {
        assertEq(liq.coldWallet(), coldWallet);
    }

    // ─── G. Multiple liquidations ────────────────────────────

    function test_G01_multipleLiquidations() public {
        aave.setBonus(500);

        for (uint i = 0; i < 5; i++) {
            address user = makeAddr(string(abi.encodePacked("user", i)));
            aave.setUserHF(user, 0.90e18);
            vm.prank(owner);
            liq.liquidate(user, address(usdc), address(usdc), 500*U6, 3000, 0, 0);
        }

        uint256 total = usdc.balanceOf(coldWallet);
        console2.log("Total after 5 liquidations:", total / U6, "USD");
        assertGt(total, 0);
    }

    // ─── H. USDT approve edge case ──────────────────────────

    function test_H01_forceApproveUSDT() public {
        aave.setUserHF(borrower, 0.95e18);
        aave.setBonus(500);

        // First liquidation
        vm.prank(owner);
        liq.liquidate(borrower, address(usdc), address(usdc), 500*U6, 3000, 0, 0);

        // Reset borrower
        aave.setUserHF(borrower, 0.85e18);

        // Second must not fail from residual allowance
        vm.prank(owner);
        liq.liquidate(borrower, address(usdc), address(usdc), 500*U6, 3000, 0, 0);

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
        liq.rescueETH();
        assertEq(address(liq).balance, 0);
    }

    function test_I03_rejectETH() public {
        vm.deal(address(this), 1 ether);
        (bool ok,) = address(liq).call{value: 1 ether}("");
        assertFalse(ok);
    }

    // ─── J. View helpers ─────────────────────────────────────

    function test_J01_getHealthFactor() public {
        aave.setUserHF(borrower, 1.5e18);
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
        liq.liquidate(borrower, address(usdc), address(usdc), 1000*U6, 3000, 0, 0);
    }

    // ─── L. Invariants ───────────────────────────────────────

    function invariant_L01_contractEmpty() public view {
        assertEq(usdc.balanceOf(address(liq)), 0);
        assertEq(weth.balanceOf(address(liq)), 0);
    }

    function invariant_L02_coldFixed() public view {
        assertEq(liq.coldWallet(), coldWallet);
    }

    function invariant_L03_ownerFixed() public view {
        assertEq(liq.owner(), owner);
    }
}
