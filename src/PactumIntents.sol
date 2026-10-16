// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {HashLock} from "./HashLock.sol";
import {MerkleSecrets} from "./lib/MerkleSecrets.sol";
import {SafeTransfer} from "./lib/SafeTransfer.sol";

/// @title PactumIntents
/// @notice Gasless, partially fillable cross-chain swap intents settled by HTLCs.
///
/// @dev The maker signs one EIP-712 `Order` and never sends a transaction:
///
///      1. A resolver calls `fill` here with the signed order. The contract pulls
///         the maker's tokens (pre-approved once) into an HTLC escrow payable to
///         the resolver, locked behind one of the maker's committed secrets.
///      2. The resolver opens the counter-escrow on the destination chain paying
///         `order.receiver` at least `owedDstAmount`, behind the same secret hash.
///      3. The maker's client checks the destination escrow and only then hands
///         the secret to the resolver, who redeems both legs (redeem is
///         permissionless and always pays the escrow's fixed redeemer).
///
///      Price discovery is a Dutch auction: the destination amount the maker asks
///      for decays linearly from `startDstAmount` to `endDstAmount`, so resolvers
///      compete on latency and the first profitable fill wins.
///
///      Partial fills: the maker commits to `parts + 1` secrets in a Merkle tree.
///      Each fill must use the secret whose index matches the cumulative fill
///      level after it, and indices only move forward, so a secret revealed for one
///      fill can never be reused to open a later one.
///
///          parts = 4, srcAmount = 100
///          filled after:  1..25 -> secret 0   26..50 -> 1   51..75 -> 2
///                         76..99 -> 3         100    -> 4
///
///      With `parts == 1` the order must be filled in one go with secret 0.
contract PactumIntents is HashLock {
    using SafeTransfer for address;

    struct Order {
        address maker;
        address receiver;
        address srcToken;
        uint256 srcAmount;
        uint256 dstChainId;
        address dstToken;
        uint256 startDstAmount;
        uint256 endDstAmount;
        uint64 auctionStart;
        uint64 auctionEnd;
        uint64 deadline;
        uint64 srcLock;
        uint16 parts;
        bytes32 secretsRoot;
        uint256 nonce;
    }

    bytes32 public constant ORDER_TYPEHASH = keccak256(
        "Order(address maker,address receiver,address srcToken,uint256 srcAmount,"
        "uint256 dstChainId,address dstToken,uint256 startDstAmount,uint256 endDstAmount,"
        "uint64 auctionStart,uint64 auctionEnd,uint64 deadline,uint64 srcLock,uint16 parts,"
        "bytes32 secretsRoot,uint256 nonce)"
    );

    uint16 public constant MAX_PARTS = 1024;
    bytes4 private constant _ERC1271_MAGIC = 0x1626ba7e;

    struct Fill {
        uint256 filled;
        /// next secret index that may be used; earlier ones are spent
        uint32 nextIndex;
        bool cancelled;
    }

    mapping(bytes32 orderHash => Fill) public fills;

    event OrderFilled(
        bytes32 indexed orderHash,
        bytes32 indexed swapId,
        address indexed resolver,
        uint256 fillAmount,
        uint256 secretIndex,
        uint256 owedDstAmount
    );
    event OrderCancelled(bytes32 indexed orderHash);

    error OrderExpired();
    error OrderCancelledErr();
    error BadOrder();
    error Overfill();
    error WrongSecretIndex(uint256 expected, uint256 given);
    error InvalidProof();
    error NotMaker();
    error PartialFillNotAllowed();

    // ------------------------------------------------------------------ fill

    /// @notice Fill `fillAmount` of a signed order.
    /// @param secretIndex index of the maker secret this fill locks behind
    /// @param secretHash  sha256 of that secret, proven against `order.secretsRoot`
    function fill(
        Order calldata order,
        bytes calldata signature,
        uint256 fillAmount,
        uint256 secretIndex,
        bytes32 secretHash,
        bytes32[] calldata proof
    ) external returns (bytes32 swapId) {
        bytes32 orderHash = hashOrder(order);
        _consume(order, orderHash, signature, fillAmount, secretIndex, secretHash, proof);
        swapId = _open(order, orderHash, fillAmount, secretIndex, secretHash);
        order.srcToken.safeTransferFrom(order.maker, address(this), fillAmount);
    }

    /// @notice Maker cancels the unfilled remainder of an order.
    function cancel(Order calldata order) external {
        if (msg.sender != order.maker) revert NotMaker();
        bytes32 orderHash = hashOrder(order);
        fills[orderHash].cancelled = true;
        emit OrderCancelled(orderHash);
    }

    // ----------------------------------------------------------------- views

    function hashOrder(Order calldata order) public view returns (bytes32) {
        // every Order field is a static type, so abi.encode(struct) is exactly
        // the concatenation EIP-712 `hashStruct` requires
        bytes32 structHash = keccak256(abi.encode(ORDER_TYPEHASH, order));
        return keccak256(abi.encodePacked("\x19\x01", domainSeparator(), structHash));
    }

    /// @notice Destination amount the maker asks for the whole order at time `t`.
    function auctionPrice(Order calldata order, uint256 t) public pure returns (uint256) {
        if (t <= order.auctionStart) return order.startDstAmount;
        if (t >= order.auctionEnd) return order.endDstAmount;
        uint256 elapsed = t - order.auctionStart;
        uint256 span = order.auctionEnd - order.auctionStart;
        return order.startDstAmount - (order.startDstAmount - order.endDstAmount) * elapsed / span;
    }

    /// @notice Destination amount a fill of `fillAmount` owes at time `t`, rounded up
    ///         in the maker's favour.
    function owedDstAmount(Order calldata order, uint256 fillAmount, uint256 t)
        public
        pure
        returns (uint256)
    {
        uint256 price = auctionPrice(order, t);
        return (price * fillAmount + order.srcAmount - 1) / order.srcAmount;
    }

    /// @notice Secret index a fill must use given the cumulative amount after it.
    function secretIndexFor(uint256 srcAmount, uint16 parts, uint256 filledAfter)
        public
        pure
        returns (uint256)
    {
        if (parts == 1) {
            if (filledAfter != srcAmount) revert PartialFillNotAllowed();
            return 0;
        }
        if (filledAfter == srcAmount) return parts;
        return (filledAfter * parts - 1) / srcAmount;
    }

    // -------------------------------------------------------------- internal

    /// @dev Checks the order, signature, fill size and secret, then records the fill.
    function _consume(
        Order calldata order,
        bytes32 orderHash,
        bytes calldata signature,
        uint256 fillAmount,
        uint256 secretIndex,
        bytes32 secretHash,
        bytes32[] calldata proof
    ) internal {
        Fill storage f = fills[orderHash];
        if (f.cancelled) revert OrderCancelledErr();
        if (block.timestamp > order.deadline) revert OrderExpired();
        _validate(order);
        // a recorded fill proves the signature was already checked for this hash
        if (f.filled == 0) _checkSignature(order.maker, orderHash, signature);

        uint256 filledAfter = f.filled + fillAmount;
        if (fillAmount == 0 || filledAfter > order.srcAmount) revert Overfill();

        uint256 expected = secretIndexFor(order.srcAmount, order.parts, filledAfter);
        if (secretIndex != expected || expected < f.nextIndex) {
            revert WrongSecretIndex(expected, secretIndex);
        }
        if (!MerkleSecrets.verify(order.secretsRoot, secretIndex, secretHash, proof)) {
            revert InvalidProof();
        }

        f.filled = filledAfter;
        f.nextIndex = uint32(expected + 1);
    }

    /// @dev Opens the source escrow: maker -> resolver, behind `secretHash`.
    function _open(
        Order calldata order,
        bytes32 orderHash,
        uint256 fillAmount,
        uint256 secretIndex,
        bytes32 secretHash
    ) internal returns (bytes32 swapId) {
        swapId = _create(
            order.maker,
            msg.sender,
            order.srcToken,
            fillAmount,
            secretHash,
            uint64(block.timestamp) + order.srcLock,
            keccak256(abi.encode(orderHash, secretIndex))
        );
        emit OrderFilled(
            orderHash,
            swapId,
            msg.sender,
            fillAmount,
            secretIndex,
            owedDstAmount(order, fillAmount, block.timestamp)
        );
    }

    function _validate(Order calldata o) internal pure {
        if (
            o.srcToken == NATIVE // gasless pull needs an allowance
                || o.srcAmount == 0 || o.parts == 0 || o.parts > MAX_PARTS
                || o.startDstAmount < o.endDstAmount || o.auctionEnd < o.auctionStart
                || o.srcLock == 0 || o.receiver == address(0)
        ) {
            revert BadOrder();
        }
    }

    function _checkSignature(address maker, bytes32 digest, bytes calldata sig) internal view {
        if (maker.code.length > 0) {
            // EIP-1271 smart-contract wallets (Safe, 4337 accounts, ...)
            (bool ok, bytes memory ret) =
                maker.staticcall(abi.encodeWithSelector(_ERC1271_MAGIC, digest, sig));
            if (!ok || ret.length < 32 || abi.decode(ret, (bytes4)) != _ERC1271_MAGIC) {
                revert InvalidSignature();
            }
            return;
        }
        if (_recover(digest, sig) != maker) revert InvalidSignature();
    }
}
