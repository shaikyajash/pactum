// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {HashLock} from "../HashLock.sol";
import {Client, IAny2EVMMessageReceiver, IERC165, IRouterClient} from "./Client.sol";

/// @title SecretRelay
/// @notice Chainlink CCIP application that carries revealed HTLC secrets to the
///         chain holding the other leg and redeems it there automatically.
///
/// @dev Why this is safe to bolt onto an HTLC:
///      CCIP is used for *liveness*, not *safety*. `HashLock.redeem` still checks
///      `sha256(secret) == secretHash` and always pays the swap's fixed redeemer,
///      so a forged, replayed or reordered message cannot move funds anywhere they
///      could not already go. If the message never arrives, the counter-party can
///      still redeem by hand, and the timelocks still protect both sides. The relay
///      only removes the need to watch the remote chain.
///
///      Flow (maker's escrow on chain A pays resolver R; R's lock on B pays the maker):
///        1. The secret is revealed on B when the maker's leg is redeemed.
///        2. Anyone calls `relay(selectorA, [swapIdA], [secret], gasLimit)` on B's
///           SecretRelay, paying the CCIP fee in native gas.
///        3. A's SecretRelay receives it via the CCIP router and calls
///           `HashLock.redeem` on A, paying R. Failures (already redeemed,
///           refunded) are logged, not reverted, so one stale entry never blocks
///           the rest of a batch. Messages may execute out of order, since each
///           redemption is independent and idempotent.
contract SecretRelay is IAny2EVMMessageReceiver, IERC165 {
    IRouterClient public immutable router;
    HashLock public immutable hashLock;

    address public owner;
    /// trusted SecretRelay deployment on each remote chain, by CCIP chain selector
    mapping(uint64 chainSelector => address peer) public peers;
    /// deposits that could not be pushed to their redeemer; withdraw with `claim`
    mapping(address => uint256) public unclaimed;

    event PeerSet(uint64 indexed chainSelector, address peer);
    event OwnershipTransferred(address indexed previous, address indexed next);
    event SecretsSent(
        bytes32 indexed messageId, uint64 indexed dstChainSelector, bytes32[] swapIds
    );
    event SecretRelayed(bytes32 indexed swapId, uint64 indexed srcChainSelector);
    event RelayFailed(bytes32 indexed swapId, uint64 indexed srcChainSelector, bytes reason);

    error NotOwner();
    error OnlyRouter();
    error NoPeer(uint64 chainSelector);
    error UntrustedPeer(uint64 chainSelector, address sender);
    error LengthMismatch();
    error EmptyBatch();
    error InsufficientFee(uint256 required, uint256 provided);
    error RefundFailed();
    error OnlyHashLock();
    error NothingToClaim();
    error ClaimFailed();

    modifier onlyOwner() {
        if (msg.sender != owner) revert NotOwner();
        _;
    }

    constructor(IRouterClient _router, HashLock _hashLock, address _owner) {
        router = _router;
        hashLock = _hashLock;
        owner = _owner;
        emit OwnershipTransferred(address(0), _owner);
    }

    // ------------------------------------------------------------------ admin

    function setPeer(uint64 chainSelector, address peer) external onlyOwner {
        peers[chainSelector] = peer;
        emit PeerSet(chainSelector, peer);
    }

    function transferOwnership(address next) external onlyOwner {
        emit OwnershipTransferred(owner, next);
        owner = next;
    }

    // ------------------------------------------------------------------- send

    /// @notice Native fee to relay a batch to `dstChainSelector`.
    function quote(
        uint64 dstChainSelector,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        uint256 gasLimit
    ) external view returns (uint256) {
        return router.getFee(
            dstChainSelector, _message(dstChainSelector, swapIds, secrets, gasLimit)
        );
    }

    /// @notice Send secrets for swaps living on `dstChainSelector`. Permissionless:
    ///         the secrets are already public on this chain and are checked on arrival.
    /// @param gasLimit destination gas for `ccipReceive`; budget one `HashLock.redeem`
    ///        (plus a deposit forward) per entry. Excess `msg.value` is refunded.
    function relay(
        uint64 dstChainSelector,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        uint256 gasLimit
    ) external payable returns (bytes32 messageId) {
        Client.EVM2AnyMessage memory message =
            _message(dstChainSelector, swapIds, secrets, gasLimit);
        uint256 fee = router.getFee(dstChainSelector, message);
        if (msg.value < fee) revert InsufficientFee(fee, msg.value);

        messageId = router.ccipSend{value: fee}(dstChainSelector, message);
        emit SecretsSent(messageId, dstChainSelector, swapIds);

        if (msg.value > fee) {
            (bool ok,) = msg.sender.call{value: msg.value - fee}("");
            if (!ok) revert RefundFailed();
        }
    }

    function _message(
        uint64 dstChainSelector,
        bytes32[] calldata swapIds,
        bytes[] calldata secrets,
        uint256 gasLimit
    ) internal view returns (Client.EVM2AnyMessage memory) {
        if (swapIds.length == 0) revert EmptyBatch();
        if (swapIds.length != secrets.length) revert LengthMismatch();
        address peer = peers[dstChainSelector];
        if (peer == address(0)) revert NoPeer(dstChainSelector);
        return Client.EVM2AnyMessage({
            receiver: abi.encode(peer),
            data: abi.encode(swapIds, secrets),
            tokenAmounts: new Client.EVMTokenAmount[](0),
            feeToken: address(0),
            extraArgs: Client._argsToBytes(
                Client.GenericExtraArgsV2({gasLimit: gasLimit, allowOutOfOrderExecution: true})
            )
        });
    }

    // ---------------------------------------------------------------- receive

    function ccipReceive(Client.Any2EVMMessage calldata message) external override {
        if (msg.sender != address(router)) revert OnlyRouter();
        address sender = abi.decode(message.sender, (address));
        uint64 src = message.sourceChainSelector;
        if (sender == address(0) || peers[src] != sender) revert UntrustedPeer(src, sender);

        (bytes32[] memory swapIds, bytes[] memory secrets) =
            abi.decode(message.data, (bytes32[], bytes[]));

        for (uint256 i; i < swapIds.length; ++i) {
            (,,, address redeemer,,,) = hashLock.swaps(swapIds[i]);
            uint256 before = address(this).balance;
            try hashLock.redeem(swapIds[i], secrets[i]) {
                emit SecretRelayed(swapIds[i], src);
                // HashLock pays the settler's deposit to us; it belongs to the redeemer
                _forward(redeemer, address(this).balance - before);
            } catch (bytes memory reason) {
                emit RelayFailed(swapIds[i], src, reason);
            }
        }
    }

    function supportsInterface(bytes4 interfaceId) external pure override returns (bool) {
        return interfaceId == type(IAny2EVMMessageReceiver).interfaceId
            || interfaceId == type(IERC165).interfaceId;
    }

    // --------------------------------------------------------------- deposits

    /// @dev Only HashLock deposit payouts are accepted.
    receive() external payable {
        if (msg.sender != address(hashLock)) revert OnlyHashLock();
    }

    /// @notice Withdraw deposits that could not be pushed automatically.
    function claim() external {
        uint256 amount = unclaimed[msg.sender];
        if (amount == 0) revert NothingToClaim();
        unclaimed[msg.sender] = 0;
        (bool ok,) = msg.sender.call{value: amount}("");
        if (!ok) revert ClaimFailed();
    }

    function _forward(address to, uint256 amount) internal {
        if (amount == 0) return;
        // bounded gas so a hostile redeemer can't grief the rest of the batch
        (bool ok,) = to.call{value: amount, gas: 10_000}("");
        if (!ok) unclaimed[to] += amount;
    }
}
