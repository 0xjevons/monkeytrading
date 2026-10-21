// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {ERC20} from "@openzeppelin-contracts/token/ERC20/ERC20.sol";

/// @notice A 6-decimal token that burns 1% of every transfer, to check the vault credits only what
/// it receives.
contract FeeOnTransferToken is ERC20 {
    constructor() ERC20("Fee Token", "FEE") {}

    function decimals() public pure override returns (uint8) {
        return 6;
    }

    function mint(address to, uint256 amount) external {
        _mint(to, amount);
    }

    function _update(address from, address to, uint256 value) internal override {
        if (from == address(0) || to == address(0)) return super._update(from, to, value);
        uint256 fee = value / 100;
        super._update(from, address(0), fee);
        super._update(from, to, value - fee);
    }
}
