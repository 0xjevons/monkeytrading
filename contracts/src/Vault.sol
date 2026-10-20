// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";

/// @title Vault
/// @notice Holds the engine's USDC collateral. The chain handles custody only: the engine credits
/// each deposit event once it is final, keyed by transaction hash and log index.
/// @dev Deposits only: there is no withdrawal path yet, so this contract must not hold real funds.
/// It is not upgradeable; the withdrawal feature extends it before any deployment that does.
contract Vault is ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    /// @notice Largest single deposit in USDC base units: 1e9 USDC, the engine's input bound.
    uint256 public constant MAX_DEPOSIT = 1e15;

    /// @notice The collateral token: USDC, 6 decimals.
    IERC20 public immutable usdc;

    /// @notice Smallest single deposit in USDC base units. Every deposit becomes a permanent
    /// entry in the engine's state, so a floor makes each one cost locked capital, not only gas.
    uint256 public immutable minDeposit;

    /// @notice Deposits accepted so far, trader and house alike; the next deposit's id.
    uint256 public depositCount;

    /// @notice `amount` USDC base units deposited to trader account `account`.
    event Deposited(uint256 indexed id, address indexed account, uint256 amount);

    /// @notice `amount` USDC base units added to house capital by `from`.
    event HouseDeposited(uint256 indexed id, address indexed from, uint256 amount);

    error ZeroAddress();
    error InvalidAmount(uint256 amount);
    error InvalidMinDeposit(uint256 minDeposit);

    constructor(IERC20 usdc_, uint256 minDeposit_) {
        if (address(usdc_) == address(0)) revert ZeroAddress();
        if (minDeposit_ == 0 || minDeposit_ > MAX_DEPOSIT) revert InvalidMinDeposit(minDeposit_);
        usdc = usdc_;
        minDeposit = minDeposit_;
    }

    /// @notice Deposits `amount` USDC base units from the caller into the caller's trading account.
    /// @return id The deposit's id, one more than the previous deposit's.
    function deposit(uint256 amount) external nonReentrant returns (uint256 id) {
        uint256 received = _pull(amount);
        id = depositCount++;
        emit Deposited(id, msg.sender, received);
    }

    /// @notice Adds `amount` USDC base units from the caller to house capital. The caller gets no
    /// claim on it.
    /// @return id The deposit's id, one more than the previous deposit's.
    function depositToHouse(uint256 amount) external nonReentrant returns (uint256 id) {
        uint256 received = _pull(amount);
        id = depositCount++;
        emit HouseDeposited(id, msg.sender, received);
    }

    /// @dev Pulls `amount` from the caller and returns what the vault actually received.
    function _pull(uint256 amount) private returns (uint256 received) {
        if (amount < minDeposit || amount > MAX_DEPOSIT) revert InvalidAmount(amount);
        uint256 before = usdc.balanceOf(address(this));
        usdc.safeTransferFrom(msg.sender, address(this), amount);
        received = usdc.balanceOf(address(this)) - before;
    }
}
