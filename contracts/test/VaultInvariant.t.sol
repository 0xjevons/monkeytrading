// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {Test, Vm} from "forge-std/Test.sol";

import {Vault} from "../src/Vault.sol";
import {MockUSDC} from "./mocks/MockUSDC.sol";

/// @notice Random trader and house deposits from a few actors, tracking what the events report.
contract VaultHandler is Test {
    MockUSDC internal usdc;
    Vault internal vault;
    address[3] internal actors = [makeAddr("alice"), makeAddr("bob"), makeAddr("house funder")];

    uint256 public depositsMade;
    uint256 public amountReported;

    constructor(MockUSDC usdc_, Vault vault_) {
        usdc = usdc_;
        vault = vault_;
    }

    function deposit(uint256 actorSeed, uint256 amount, bool toHouse) external {
        address actor = actors[actorSeed % actors.length];
        amount = bound(amount, vault.minDeposit(), vault.MAX_DEPOSIT());
        usdc.mint(actor, amount);
        vm.startPrank(actor);
        usdc.approve(address(vault), amount);
        vm.recordLogs();
        uint256 id = toHouse ? vault.depositToHouse(amount) : vault.deposit(amount);
        vm.stopPrank();

        assertEq(id, depositsMade);
        depositsMade++;
        // The vault emits one event per deposit; its only data word is the amount credited.
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i = 0; i < logs.length; i++) {
            if (logs[i].emitter == address(vault)) amountReported += abi.decode(logs[i].data, (uint256));
        }
    }
}

contract VaultInvariantTest is Test {
    MockUSDC internal usdc;
    Vault internal vault;
    VaultHandler internal handler;

    function setUp() public {
        usdc = new MockUSDC();
        vault = new Vault(usdc, 1e6);
        handler = new VaultHandler(usdc, vault);
        targetContract(address(handler));
    }

    /// @notice Every deposit event is backed by USDC the vault holds, and none is missing.
    function invariant_VaultHoldsExactlyTheReportedDeposits() public view {
        assertEq(usdc.balanceOf(address(vault)), handler.amountReported());
    }

    /// @notice The deposit counter matches the deposits made, so ids have no gaps.
    function invariant_DepositCountMatchesDepositsMade() public view {
        assertEq(vault.depositCount(), handler.depositsMade());
    }
}
