// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @title HashLock
/// @notice Hash time-locked contract for trustless cross-chain atomic swaps.
/// @dev A swap locks funds that can be claimed by `redeemer` by revealing the
///      preimage of `secretHash`, or reclaimed by `initiator` once `expiry` passes.
///      The same secret unlocks the counter-leg on the other chain, which is what
///      makes the swap atomic: revealing it to claim one side publishes it for the other.
contract HashLock {
    struct Swap {
        address initiator;
        uint64 expiry;
        bool settled;
        address redeemer;
        uint256 amount;
        bytes32 secretHash;
    }

    mapping(bytes32 swapId => Swap) public swaps;

    event Initiated(
        bytes32 indexed swapId,
        address indexed initiator,
        address indexed redeemer,
        bytes32 secretHash,
        uint256 amount,
        uint64 expiry
    );
    event Redeemed(bytes32 indexed swapId, bytes32 indexed secretHash, bytes secret);
    event Refunded(bytes32 indexed swapId);

    error ZeroAddress();
    error ZeroAmount();
    error InvalidExpiry();
    error SwapExists();
    error SwapNotFound();
    error AlreadySettled();
    error InvalidSecret();
    error NotExpired();
    error TransferFailed();

    /// @notice Lock native ETH for `redeemer` until `expiry`.
    function initiate(address redeemer, bytes32 secretHash, uint64 expiry, bytes32 salt)
        external
        payable
        returns (bytes32 swapId)
    {
        if (redeemer == address(0)) revert ZeroAddress();
        if (msg.value == 0) revert ZeroAmount();
        if (expiry <= block.timestamp) revert InvalidExpiry();

        swapId = computeSwapId(msg.sender, redeemer, secretHash, msg.value, expiry, salt);
        if (swaps[swapId].initiator != address(0)) revert SwapExists();

        swaps[swapId] = Swap({
            initiator: msg.sender,
            expiry: expiry,
            settled: false,
            redeemer: redeemer,
            amount: msg.value,
            secretHash: secretHash
        });

        emit Initiated(swapId, msg.sender, redeemer, secretHash, msg.value, expiry);
    }

    /// @notice Release funds to the redeemer by revealing the secret.
    /// @dev Callable by anyone so a relayer can settle on the redeemer's behalf.
    function redeem(bytes32 swapId, bytes calldata secret) external {
        Swap storage s = _live(swapId);
        if (sha256(secret) != s.secretHash) revert InvalidSecret();

        s.settled = true;
        emit Redeemed(swapId, s.secretHash, secret);
        _send(s.redeemer, s.amount);
    }

    /// @notice Return funds to the initiator after expiry.
    function refund(bytes32 swapId) external {
        Swap storage s = _live(swapId);
        if (block.timestamp < s.expiry) revert NotExpired();

        s.settled = true;
        emit Refunded(swapId);
        _send(s.initiator, s.amount);
    }

    function computeSwapId(
        address initiator,
        address redeemer,
        bytes32 secretHash,
        uint256 amount,
        uint64 expiry,
        bytes32 salt
    ) public view returns (bytes32) {
        return keccak256(
            abi.encode(
                block.chainid, address(this), initiator, redeemer, secretHash, amount, expiry, salt
            )
        );
    }

    function _live(bytes32 swapId) internal view returns (Swap storage s) {
        s = swaps[swapId];
        if (s.initiator == address(0)) revert SwapNotFound();
        if (s.settled) revert AlreadySettled();
    }

    function _send(address to, uint256 amount) internal {
        (bool ok,) = to.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }
}
