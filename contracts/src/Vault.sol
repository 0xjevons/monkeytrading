// SPDX-License-Identifier: Apache-2.0
pragma solidity 0.8.36;

import {IERC20} from "@openzeppelin-contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin-contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuardTransient} from "@openzeppelin-contracts/utils/ReentrancyGuardTransient.sol";
import {ECDSA} from "@openzeppelin-contracts/utils/cryptography/ECDSA.sol";

/// @title Vault
/// @notice Holds the engine's USDC collateral. The chain handles custody only: the engine credits
/// each deposit event once it is final, keyed by transaction hash and log index.
/// @notice A trading account must be able to sign the engine's EIP-712 requests with its own key:
/// the engine accepts no contract signatures. A contract wallet funds a key that signs for it
/// through `depositFor`; a balance credited to a contract account can be recovered only through
/// the exit (#48).
/// @dev Deposits only: there is no withdrawal path yet, so this contract must not hold real funds.
/// It is not upgradeable; the withdrawal feature extends it before any deployment that does.
contract Vault is ReentrancyGuardTransient {
    using SafeERC20 for IERC20;

    /// @notice Smallest single deposit in USDC base units: 10 USDC. Every deposit becomes a
    /// permanent entry in the engine's state, so a floor makes each one cost locked capital, not
    /// only gas.
    uint256 public constant MIN_DEPOSIT = 1e7;

    /// @notice Largest single deposit in USDC base units: 1e9 USDC, the engine's input bound.
    uint256 public constant MAX_DEPOSIT = 1e15;

    /// @notice The collateral token: USDC, 6 decimals.
    IERC20 public immutable usdc;

    /// @notice Deposits accepted so far, trader and house alike; the next deposit's id.
    uint256 public depositCount;

    /// @notice Authorized enclave address, provisioned after verifying its attestation.
    /// Rotation and recovery require a separate signer-lifecycle implementation.
    address public immutable checkpointSigner;
    bool public hasCheckpoint;
    uint64 public latestSeq;
    bytes32 public latestRoot;

    event CheckpointPosted(uint64 indexed seq, bytes32 root, bytes signature);

    /// @notice `amount` USDC base units deposited to trader account `account`.
    event Deposited(uint256 indexed id, address indexed account, uint256 amount);

    /// @notice `amount` USDC base units added to house capital by `from`.
    event HouseDeposited(uint256 indexed id, address indexed from, uint256 amount);

    error ZeroAddress();
    error InvalidAmount(uint256 amount);
    error CheckpointNotExtended();
    error InvalidCheckpointSigner();
    error CheckpointParentMismatch();
    error UnsupportedChain();

    constructor(IERC20 usdc_, address checkpointSigner_) {
        if (address(usdc_) == address(0) || checkpointSigner_ == address(0)) revert ZeroAddress();
        if (block.chainid > type(uint64).max) revert UnsupportedChain();
        usdc = usdc_;
        checkpointSigner = checkpointSigner_;
    }

    /// @notice Anyone may relay an enclave checkpoint. Only increasing seqs with the exact signed parent can
    /// commit, fencing concurrent instances and log rollback. A successful receipt must precede withdrawal signing.
    function postCheckpoint(
        uint64 seq,
        bytes32 root,
        bool hasParent,
        uint64 parentSeq,
        bytes32 parentRoot,
        bytes calldata signature
    ) external {
        if (hasCheckpoint && seq <= latestSeq) revert CheckpointNotExtended();
        if (hasParent != hasCheckpoint || parentSeq != latestSeq || parentRoot != latestRoot) {
            revert CheckpointParentMismatch();
        }
        bytes32 digest = keccak256(
            abi.encodePacked(
                "engine.checkpoint.v2",
                uint64(block.chainid),
                address(this),
                seq,
                root,
                hasParent,
                parentSeq,
                parentRoot
            )
        );
        if (ECDSA.recover(digest, signature) != checkpointSigner) revert InvalidCheckpointSigner();
        hasCheckpoint = true;
        latestSeq = seq;
        latestRoot = root;
        emit CheckpointPosted(seq, root, signature);
    }

    /// @notice Deposits `amount` USDC base units from the caller into the caller's trading account,
    /// which only a caller that signs with its own key can trade from.
    /// @return id The deposit's id, one more than the previous deposit's.
    function deposit(uint256 amount) external nonReentrant returns (uint256 id) {
        id = _deposit(msg.sender, amount);
    }

    /// @notice Deposits `amount` USDC base units from the caller into trading account `account`,
    /// which must be able to sign; the caller gets no claim on it.
    /// @return id The deposit's id, one more than the previous deposit's.
    function depositFor(address account, uint256 amount) external nonReentrant returns (uint256 id) {
        if (account == address(0)) revert ZeroAddress();
        id = _deposit(account, amount);
    }

    /// @notice Adds `amount` USDC base units from the caller to house capital. The caller gets no
    /// claim on it.
    /// @return id The deposit's id, one more than the previous deposit's.
    function depositToHouse(uint256 amount) external nonReentrant returns (uint256 id) {
        uint256 received = _pull(amount);
        id = depositCount++;
        emit HouseDeposited(id, msg.sender, received);
    }

    /// @dev Pulls `amount` from the caller and credits what arrived to trading account `account`.
    function _deposit(address account, uint256 amount) private returns (uint256 id) {
        uint256 received = _pull(amount);
        id = depositCount++;
        emit Deposited(id, account, received);
    }

    /// @dev Pulls `amount` from the caller and returns what the vault actually received.
    function _pull(uint256 amount) private returns (uint256 received) {
        if (amount < MIN_DEPOSIT || amount > MAX_DEPOSIT) revert InvalidAmount(amount);
        uint256 before = usdc.balanceOf(address(this));
        usdc.safeTransferFrom(msg.sender, address(this), amount);
        received = usdc.balanceOf(address(this)) - before;
    }
}
