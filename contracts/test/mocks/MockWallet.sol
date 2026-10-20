// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";

import {Vault} from "../../src/Vault.sol";

/// @notice A contract wallet: it holds USDC but cannot sign the engine's requests, so it funds its
/// owner's signing key through `depositFor`.
contract MockWallet {
    address public immutable owner;

    constructor(address owner_) {
        owner = owner_;
    }

    function fund(Vault vault, uint256 amount) external returns (uint256 id) {
        IERC20 usdc = vault.usdc();
        usdc.approve(address(vault), amount);
        id = vault.depositFor(owner, amount);
    }
}
