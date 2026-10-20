// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {IERC20Errors} from "@openzeppelin-contracts/interfaces/draft-IERC6093.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";

import {Vault} from "../src/Vault.sol";
import {FeeOnTransferToken} from "./mocks/FeeOnTransferToken.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";
import {ReentrantToken} from "./mocks/ReentrantToken.sol";

contract VaultTest is Test {
    uint256 internal constant ONE_USDC = 1e6;

    MockUSDC internal usdc;
    Vault internal vault;
    address internal trader = makeAddr("trader");
    address internal funder = makeAddr("funder");

    function setUp() public {
        usdc = new MockUSDC();
        vault = new Vault(usdc, ONE_USDC);
    }

    function test_DepositMovesUsdcAndEmitsTheCallersAccount() public {
        _fund(trader, 250 * ONE_USDC);

        vm.expectEmit(address(vault));
        emit Vault.Deposited(0, trader, 250 * ONE_USDC);
        vm.prank(trader);
        uint256 id = vault.deposit(250 * ONE_USDC);

        assertEq(id, 0);
        assertEq(usdc.balanceOf(address(vault)), 250 * ONE_USDC);
        assertEq(usdc.balanceOf(trader), 0);
        assertEq(vault.depositCount(), 1);
    }

    function test_DepositToHouseMovesUsdcAndEmitsTheFunder() public {
        _fund(funder, 5_000_000 * ONE_USDC);

        vm.expectEmit(address(vault));
        emit Vault.HouseDeposited(0, funder, 5_000_000 * ONE_USDC);
        vm.prank(funder);
        uint256 id = vault.depositToHouse(5_000_000 * ONE_USDC);

        assertEq(id, 0);
        assertEq(usdc.balanceOf(address(vault)), 5_000_000 * ONE_USDC);
        assertEq(vault.depositCount(), 1);
    }

    function test_IdsRunInOneSequenceAcrossTraderAndHouseDeposits() public {
        _fund(trader, 2 * ONE_USDC);
        _fund(funder, ONE_USDC);

        vm.prank(trader);
        assertEq(vault.deposit(ONE_USDC), 0);
        vm.expectEmit(address(vault));
        emit Vault.HouseDeposited(1, funder, ONE_USDC);
        vm.prank(funder);
        assertEq(vault.depositToHouse(ONE_USDC), 1);
        vm.prank(trader);
        assertEq(vault.deposit(ONE_USDC), 2);

        assertEq(vault.depositCount(), 3);
    }

    function test_SmallestAndLargestDepositsAreAccepted() public {
        uint256 max = vault.MAX_DEPOSIT();
        _fund(trader, max + ONE_USDC);

        vm.startPrank(trader);
        vault.deposit(ONE_USDC);
        vault.deposit(max);
        vm.stopPrank();

        assertEq(usdc.balanceOf(address(vault)), max + ONE_USDC);
    }

    function test_RevertWhen_AmountIsBelowMinDeposit() public {
        _fund(trader, ONE_USDC);
        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, ONE_USDC - 1));
        vm.prank(trader);
        vault.deposit(ONE_USDC - 1);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, 0));
        vm.prank(funder);
        vault.depositToHouse(0);
    }

    function test_RevertWhen_MinDepositIsZeroOrAboveMax() public {
        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidMinDeposit.selector, 0));
        new Vault(usdc, 0);

        uint256 over = vault.MAX_DEPOSIT() + 1;
        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidMinDeposit.selector, over));
        new Vault(usdc, over);
    }

    function test_RevertWhen_AmountExceedsMaxDeposit() public {
        uint256 over = vault.MAX_DEPOSIT() + 1;
        _fund(trader, over);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, over));
        vm.prank(trader);
        vault.deposit(over);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, over));
        vm.prank(trader);
        vault.depositToHouse(over);
    }

    function test_RevertWhen_AllowanceIsShort() public {
        usdc.mint(trader, 10 * ONE_USDC);
        vm.prank(trader);
        usdc.approve(address(vault), 5 * ONE_USDC);

        vm.expectRevert(
            abi.encodeWithSelector(
                IERC20Errors.ERC20InsufficientAllowance.selector, address(vault), 5 * ONE_USDC, 10 * ONE_USDC
            )
        );
        vm.prank(trader);
        vault.deposit(10 * ONE_USDC);
        assertEq(vault.depositCount(), 0);
    }

    function test_RevertWhen_TokenIsZeroAddress() public {
        vm.expectRevert(Vault.ZeroAddress.selector);
        new Vault(IERC20(address(0)), ONE_USDC);
    }

    function test_FeeOnTransferTokenCreditsOnlyWhatArrived() public {
        FeeOnTransferToken token = new FeeOnTransferToken();
        Vault feeVault = new Vault(token, ONE_USDC);
        token.mint(trader, 100 * ONE_USDC);
        vm.prank(trader);
        token.approve(address(feeVault), 100 * ONE_USDC);

        vm.expectEmit(address(feeVault));
        emit Vault.Deposited(0, trader, 99 * ONE_USDC);
        vm.prank(trader);
        feeVault.deposit(100 * ONE_USDC);

        assertEq(token.balanceOf(address(feeVault)), 99 * ONE_USDC);
    }

    function test_RevertWhen_TokenReentersDeposit() public {
        ReentrantToken token = new ReentrantToken();
        // A floor of one base unit lets the token's own one-unit deposit through, but for the guard.
        Vault target = new Vault(token, 1);
        token.mint(trader, ONE_USDC);
        vm.prank(trader);
        token.approve(address(target), ONE_USDC);
        token.arm(target);

        vm.expectRevert(ReentrancyGuardTransient.ReentrancyGuardReentrantCall.selector);
        vm.prank(trader);
        target.deposit(ONE_USDC);
    }

    function testFuzz_DepositCreditsExactlyWhatWasPulled(uint256 amount, bool toHouse) public {
        amount = bound(amount, vault.minDeposit(), vault.MAX_DEPOSIT());
        _fund(trader, amount);

        vm.expectEmit(address(vault));
        if (toHouse) emit Vault.HouseDeposited(0, trader, amount);
        else emit Vault.Deposited(0, trader, amount);
        vm.prank(trader);
        if (toHouse) vault.depositToHouse(amount);
        else vault.deposit(amount);

        assertEq(usdc.balanceOf(address(vault)), amount);
        assertEq(usdc.balanceOf(trader), 0);
        assertEq(vault.depositCount(), 1);
    }

    function testFuzz_RevertWhen_AmountIsBelowMinDeposit(uint256 amount) public {
        amount = bound(amount, 0, vault.minDeposit() - 1);
        _fund(trader, amount);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, amount));
        vm.prank(trader);
        vault.deposit(amount);
    }

    function testFuzz_RevertWhen_AmountIsAboveMaxDeposit(uint256 amount) public {
        amount = bound(amount, vault.MAX_DEPOSIT() + 1, type(uint256).max);

        vm.expectRevert(abi.encodeWithSelector(Vault.InvalidAmount.selector, amount));
        vm.prank(trader);
        vault.deposit(amount);
    }

    // ── Helpers ──

    /// @dev Mints `amount` to `who` and approves the vault for it.
    function _fund(address who, uint256 amount) internal {
        usdc.mint(who, amount);
        vm.prank(who);
        usdc.approve(address(vault), amount);
    }
}
