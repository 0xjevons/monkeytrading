// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ERC20} from "@openzeppelin-contracts/token/ERC20/ERC20.sol";

import {Vault} from "../../src/Vault.sol";

/// @notice A token that deposits into the vault again, from its own funded balance, during a
/// transfer into the vault, to check the vault refuses re-entry.
contract ReentrantToken is ERC20 {
    Vault public vault;

    constructor() ERC20("Reentrant Token", "REENTER") {}

    function arm(Vault vault_) external {
        vault = vault_;
        _mint(address(this), 1);
        _approve(address(this), address(vault_), 1);
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function _update(address from, address to, uint256 value) internal override {
        super._update(from, to, value);
        if (address(vault) != address(0) && to == address(vault)) {
            Vault target = vault;
            vault = Vault(address(0));
            target.deposit(1);
        }
    }
}
