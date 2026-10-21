// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ERC20} from "@openzeppelin-contracts/token/ERC20/ERC20.sol";

import {Vault} from "../../src/Vault.sol";

/// @notice A token that deposits into the vault again, through one of its entry points and from
/// its own funded balance, during a transfer into the vault, to check the vault refuses re-entry.
contract ReentrantToken is ERC20 {
    /// @notice The vault entry point the token re-enters.
    enum Entry {
        Deposit,
        DepositFor,
        DepositToHouse
    }

    Vault public vault;
    Entry public entry;

    constructor() ERC20("Reentrant Token", "REENTER") {}

    function arm(Vault vault_, Entry entry_) external {
        vault = vault_;
        entry = entry_;
        uint256 amount = vault_.MIN_DEPOSIT();
        _mint(address(this), amount);
        _approve(address(this), address(vault_), amount);
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (address(vault) != address(0) && to == address(vault)) {
            Vault target = vault;
            vault = Vault(address(0));
            uint256 amount = target.MIN_DEPOSIT();
            if (entry == Entry.Deposit) target.deposit(amount);
            else if (entry == Entry.DepositFor) target.depositFor(address(this), amount);
            else target.depositToHouse(amount);
        }
    }
}
