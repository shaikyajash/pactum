// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {SafeTransfer} from "./lib/SafeTransfer.sol";

/// @title HashLock
/// @notice Hash time-locked contract for trustless cross-chain atomic swaps.
/// @dev A swap locks funds that can be claimed by `redeemer` by revealing the
///      preimage of `secretHash`, or reclaimed by `initiator` once `expiry` passes.
///      The same secret unlocks the counter-leg on the other chain, which is what
///      makes the swap atomic: revealing it to claim one side publishes it for the other.
///      `token == address(0)` denotes the chain's native asset.
contract HashLock {
    using SafeTransfer for address;

    address public constant NATIVE = address(0);

    bytes32 private constant _DOMAIN_TYPEHASH = keccak256(
        "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
    );
    bytes32 public constant REFUND_TYPEHASH = keccak256("Refund(bytes32 swapId)");

    struct Swap {
        address initiator;
        uint64 expiry;
        bool settled;
        address redeemer;
        address token;
        uint256 amount;
        bytes32 secretHash;
    }

    mapping(bytes32 swapId => Swap) public swaps;

    /// @notice Native-asset safety deposit attached to a swap. Paid to whoever
    ///         settles it, so third parties are paid to finish abandoned swaps.
    mapping(bytes32 swapId => uint256) public deposits;

    event Initiated(
        bytes32 indexed swapId,
        address indexed initiator,
        address indexed redeemer,
        address token,
        uint256 amount,
        bytes32 secretHash,
        uint64 expiry
    );
    event Redeemed(bytes32 indexed swapId, bytes32 indexed secretHash, bytes secret);
    event Refunded(bytes32 indexed swapId);
    event DepositClaimed(bytes32 indexed swapId, address indexed to, uint256 amount);

    error ZeroAddress();
    error ZeroAmount();
    error InvalidExpiry();
    error InvalidValue();
    error SwapExists();
    error SwapNotFound();
    error AlreadySettled();
    error InvalidSecret();
    error NotExpired();
    error InvalidSignature();

    /// @notice Lock `amount` of `token` for `redeemer` until `expiry`.
    /// @dev For native swaps `msg.value` must equal `amount`; for ERC20 it must be zero
    ///      and the caller must have approved this contract.
    function initiate(
        address redeemer,
        address token,
        uint256 amount,
        bytes32 secretHash,
        uint64 expiry,
        bytes32 salt
    ) external payable returns (bytes32 swapId) {
        if (msg.value != (token == NATIVE ? amount : 0)) revert InvalidValue();
        swapId = _create(msg.sender, redeemer, token, amount, secretHash, expiry, salt);
        if (token != NATIVE) token.safeTransferFrom(msg.sender, address(this), amount);
    }

    /// @dev Records a swap without moving funds. Callers must ensure the contract
    ///      receives `amount` of `token` in the same transaction.
    function _create(
        address initiator,
        address redeemer,
        address token,
        uint256 amount,
        bytes32 secretHash,
        uint64 expiry,
        bytes32 salt
    ) internal returns (bytes32 swapId) {
        if (redeemer == address(0)) revert ZeroAddress();
        if (amount == 0) revert ZeroAmount();
        if (expiry <= block.timestamp) revert InvalidExpiry();

        swapId = computeSwapId(initiator, redeemer, token, amount, secretHash, expiry, salt);
        if (swaps[swapId].initiator != address(0)) revert SwapExists();

        swaps[swapId] = Swap({
            initiator: initiator,
            expiry: expiry,
            settled: false,
            redeemer: redeemer,
            token: token,
            amount: amount,
            secretHash: secretHash
        });

        emit Initiated(swapId, initiator, redeemer, token, amount, secretHash, expiry);
    }

    /// @notice Release funds to the redeemer by revealing the secret.
    /// @dev Callable by anyone so a relayer can settle on the redeemer's behalf.
    function redeem(bytes32 swapId, bytes calldata secret) external {
        Swap storage s = _live(swapId);
        if (sha256(secret) != s.secretHash) revert InvalidSecret();

        s.settled = true;
        emit Redeemed(swapId, s.secretHash, secret);
        _payout(s.token, s.redeemer, s.amount);
        _claimDeposit(swapId);
    }

    /// @notice Return funds to the initiator after expiry.
    function refund(bytes32 swapId) external {
        Swap storage s = _live(swapId);
        if (block.timestamp < s.expiry) revert NotExpired();

        s.settled = true;
        emit Refunded(swapId);
        _payout(s.token, s.initiator, s.amount);
        _claimDeposit(swapId);
    }

    /// @notice Refund before expiry with the redeemer's consent.
    /// @dev Lets both parties unwind quickly when the counter-leg is never funded,
    ///      instead of waiting out the full timelock.
    function instantRefund(bytes32 swapId, bytes calldata signature) external {
        Swap storage s = _live(swapId);
        bytes32 digest = refundDigest(swapId);
        if (_recover(digest, signature) != s.redeemer) revert InvalidSignature();

        s.settled = true;
        emit Refunded(swapId);
        _payout(s.token, s.initiator, s.amount);
        _claimDeposit(swapId);
    }

    function refundDigest(bytes32 swapId) public view returns (bytes32) {
        bytes32 structHash = keccak256(abi.encode(REFUND_TYPEHASH, swapId));
        return keccak256(abi.encodePacked("\x19\x01", domainSeparator(), structHash));
    }

    function domainSeparator() public view returns (bytes32) {
        return keccak256(
            abi.encode(
                _DOMAIN_TYPEHASH,
                keccak256("HashLock"),
                keccak256("1"),
                block.chainid,
                address(this)
            )
        );
    }

    function computeSwapId(
        address initiator,
        address redeemer,
        address token,
        uint256 amount,
        bytes32 secretHash,
        uint64 expiry,
        bytes32 salt
    ) public view returns (bytes32) {
        return keccak256(
            abi.encode(
                block.chainid,
                address(this),
                initiator,
                redeemer,
                token,
                amount,
                secretHash,
                expiry,
                salt
            )
        );
    }

    function _live(bytes32 swapId) internal view returns (Swap storage s) {
        s = swaps[swapId];
        if (s.initiator == address(0)) revert SwapNotFound();
        if (s.settled) revert AlreadySettled();
    }

    function _recover(bytes32 digest, bytes calldata sig) internal pure returns (address signer) {
        if (sig.length != 65) revert InvalidSignature();
        bytes32 r = bytes32(sig[0:32]);
        bytes32 s = bytes32(sig[32:64]);
        uint8 v = uint8(sig[64]);
        // reject malleable high-s signatures (EIP-2)
        if (uint256(s) > 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0) {
            revert InvalidSignature();
        }
        signer = ecrecover(digest, v, r, s);
        if (signer == address(0)) revert InvalidSignature();
    }

    /// @dev Pays the swap's safety deposit, if any, to the caller that settled it.
    function _claimDeposit(bytes32 swapId) internal {
        uint256 amount = deposits[swapId];
        if (amount == 0) return;
        deposits[swapId] = 0;
        emit DepositClaimed(swapId, msg.sender, amount);
        msg.sender.sendNative(amount);
    }

    function _payout(address token, address to, uint256 amount) internal {
        if (token == NATIVE) to.sendNative(amount);
        else token.safeTransfer(to, amount);
    }
}
