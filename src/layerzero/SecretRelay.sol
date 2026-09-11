// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {HashLock} from "../HashLock.sol";
import {
    ILayerZeroEndpointV2,
    ILayerZeroReceiver,
    MessagingFee,
    MessagingParams,
    MessagingReceipt,
    Origin
} from "./ILayerZeroEndpointV2.sol";

/// @title SecretRelay
/// @notice LayerZero V2 OApp that carries revealed HTLC secrets to the chain
///         holding the other leg and redeems it there automatically.
///
/// @dev Why this is safe to bolt onto an HTLC:
///      LayerZero is used for *liveness*, not *safety*. `HashLock.redeem` still
///      checks `sha256(secret) == secretHash` and always pays the swap's fixed
///      redeemer, so a forged, replayed or reordered message cannot move funds
///      anywhere they could not already go. If the message never arrives, the
///      counter-party can still redeem by hand, and the timelocks still protect
///      both sides. The relay just removes the need to watch the remote chain.
///
///      Flow (Alice locks on chain A for resolver R, R mirrors on chain B):
///        1. Alice redeems on B, revealing the secret.
///        2. Anyone calls `relay(eidA, [swapIdA], [secret])` on B's SecretRelay,
///           paying the LayerZero fee.
///        3. A's SecretRelay receives it and calls `HashLock.redeem` on A,
///           paying R. Failures (already redeemed, refunded) are logged, not
///           reverted, so a stale entry never blocks the rest of a batch.
contract SecretRelay is ILayerZeroReceiver {
    ILayerZeroEndpointV2 public immutable endpoint;
    HashLock public immutable hashLock;

    address public owner;
    mapping(uint32 eid => bytes32 peer) public peers;

    event PeerSet(uint32 indexed eid, bytes32 peer);
    event OwnershipTransferred(address indexed previous, address indexed next);
    event SecretsSent(bytes32 indexed guid, uint32 indexed dstEid, bytes32[] swapIds);
    event SecretRelayed(bytes32 indexed swapId, uint32 indexed srcEid);
    event RelayFailed(bytes32 indexed swapId, uint32 indexed srcEid, bytes reason);

    error NotOwner();
    error OnlyEndpoint();
    error NoPeer(uint32 eid);
    error UntrustedPeer(uint32 eid, bytes32 sender);
    error LengthMismatch();
    error EmptyBatch();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(address _endpoint, HashLock _hashLock, address _owner) {
        endpoint = ILayerZeroEndpointV2(_endpoint);
        hashLock = _hashLock;
        owner = _owner;
        emit OwnershipTransferred(address(0), _owner);
        endpoint.setDelegate(_owner);
    }

    // ------------------------------------------------------------------ admin

    function setPeer(uint32 eid, bytes32 peer) external onlyOwner {
        peers[eid] = peer;
        emit PeerSet(eid, peer);
    }

    function transferOwnership(address next) external onlyOwner {
        emit OwnershipTransferred(owner, next);
        owner = next;
        endpoint.setDelegate(next);
    }

    // ------------------------------------------------------------------- send

    /// @notice Fee to relay a batch to `dstEid` with the given executor options.
    function quote(
        uint32 dstEid,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        bytes calldata options
    ) external view returns (MessagingFee memory) {
        return endpoint.quote(_params(dstEid, swapIds, secrets, options), address(this));
    }

    /// @notice Send secrets for swaps living on `dstEid`. Permissionless: the
    ///         secrets are already public on this chain and are checked on arrival.
    /// @param options LayerZero executor options; must budget enough gas for
    ///        one `HashLock.redeem` per entry.
    function relay(
        uint32 dstEid,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        bytes calldata options
    ) external payable returns (MessagingReceipt memory receipt) {
        receipt = endpoint.send{value: msg.value}(
            _params(dstEid, swapIds, secrets, options), msg.sender
        );
        emit SecretsSent(receipt.guid, dstEid, swapIds);
    }

    function _params(
        uint32 dstEid,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        bytes calldata options
    ) internal view returns (MessagingParams memory) {
        if (swapIds.length == 0) revert EmptyBatch();
        if (swapIds.length != secrets.length) revert LengthMismatch();
        bytes32 peer = peers[dstEid];
        if (peer == bytes32(0)) revert NoPeer(dstEid);
        return MessagingParams({
            dstEid: dstEid,
            receiver: peer,
            message: abi.encode(swapIds, secrets),
            options: options,
            payInLzToken: false
        });
    }

    // ---------------------------------------------------------------- receive

    function lzReceive(
        Origin calldata origin,
        bytes32, /* guid */
        bytes calldata message,
        address, /* executor */
        bytes calldata /* extraData */
    ) external payable override {
        if (msg.sender != address(endpoint)) revert OnlyEndpoint();
        if (peers[origin.srcEid] != origin.sender || origin.sender == bytes32(0)) {
            revert UntrustedPeer(origin.srcEid, origin.sender);
        }

        (bytes32[] memory swapIds, bytes[] memory secrets) =
            abi.decode(message, (bytes32[], bytes[]));

        for (uint256 i; i < swapIds.length; ++i) {
            try hashLock.redeem(swapIds[i], secrets[i]) {
                emit SecretRelayed(swapIds[i], origin.srcEid);
            } catch (bytes memory reason) {
                emit RelayFailed(swapIds[i], origin.srcEid, reason);
            }
        }
    }

    function allowInitializePath(Origin calldata origin) external view override returns (bool) {
        return peers[origin.srcEid] == origin.sender;
    }

    /// @dev 0 = unordered delivery. Redemptions are idempotent, so order is irrelevant.
    function nextNonce(uint32, bytes32) external pure override returns (uint64) {
        return 0;
    }
}
