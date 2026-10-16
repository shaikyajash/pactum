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
    bytes4 internal constant _ERC1271_MAGIC = 0x1626ba7e;

    /// @notice Share of a deposit paid to whoever calls `refund` on an expired swap,
    ///         in basis points. The rest goes to the swap's initiator.
    uint256 public constant KEEPER_TIP_BPS = 1_000;

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

    /// @notice Native-asset safety deposit attached to a swap, held until it settles.
    /// @dev On settlement the deposit is never sent inline; it is credited to
    ///      `depositCredit` and pulled with `withdrawDeposits`, so a party that
    ///      rejects ETH (or a contract without `receive`) can never block settlement.
    ///
    ///      Who is credited:
    ///      - `redeem`:        the swap's `redeemer` (the resolver that posted it),
    ///                         no matter who submits the secret.
    ///      - `instantRefund`: the swap's `redeemer`, who consented to the unwind.
    ///      - `refund`:        `KEEPER_TIP_BPS` to the caller as a keeper tip, the rest
    ///                         to the swap's `initiator` (the maker) as compensation for
    ///                         having funds locked by an abandoned fill. A resolver that
    ///                         griefs by filling and refunding itself loses 90%.
    ///
    ///      Trade-off: the expiry path favours makers over resolvers. A maker can
    ///      withhold the secret after a valid counter-lock and collect the deposit
    ///      at expiry; resolvers are expected to avoid makers who do that.
    mapping(bytes32 swapId => uint256) public deposits;

    /// @notice Settled deposits owed to each account; withdraw with `withdrawDeposits`.
    mapping(address account => uint256) public depositCredit;

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
    event DepositCredited(bytes32 indexed swapId, address indexed to, uint256 amount);
    event DepositsWithdrawn(address indexed account, address indexed to, uint256 amount);

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
    error NothingToWithdraw();
    error UnsupportedToken();

    /// @notice Lock `amount` of `token` for `redeemer` until `expiry`.
    /// @dev For native swaps `msg.value` must equal `amount`; for ERC20 it must be zero
    ///      and the caller must have approved this contract. Fee-on-transfer tokens are
    ///      rejected with `UnsupportedToken`; rebasing tokens are unsupported too (a
    ///      later rebase would leave escrows over- or under-collateralised).
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
        if (token != NATIVE) _pullExact(token, msg.sender, amount);
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
        _creditDeposit(swapId, s.redeemer, 0);
        _payout(s.token, s.redeemer, s.amount);
    }

    /// @notice Return funds to the initiator after expiry.
    /// @dev Callable by anyone. The caller earns `KEEPER_TIP_BPS` of the deposit and
    ///      the initiator is credited the rest (see `deposits`).
    function refund(bytes32 swapId) external {
        Swap storage s = _live(swapId);
        if (block.timestamp < s.expiry) revert NotExpired();

        s.settled = true;
        emit Refunded(swapId);
        _creditDeposit(swapId, s.initiator, KEEPER_TIP_BPS);
        _payout(s.token, s.initiator, s.amount);
    }

    /// @notice Refund before expiry with the redeemer's consent.
    /// @dev Lets both parties unwind quickly when the counter-leg is never funded,
    ///      instead of waiting out the full timelock. Contract redeemers sign via
    ///      EIP-1271 (see `_isValidSignature`).
    function instantRefund(bytes32 swapId, bytes calldata signature) external {
        Swap storage s = _live(swapId);
        if (!_isValidSignature(s.redeemer, refundDigest(swapId), signature)) {
            revert InvalidSignature();
        }

        s.settled = true;
        emit Refunded(swapId);
        _creditDeposit(swapId, s.redeemer, 0);
        _payout(s.token, s.initiator, s.amount);
    }

    /// @notice Withdraw all deposits credited to the caller.
    function withdrawDeposits() external {
        _withdraw(payable(msg.sender));
    }

    /// @notice Withdraw all deposits credited to the caller, sending them to `to`.
    /// @dev Lets contracts that cannot receive ETH route their credit elsewhere.
    function withdrawDepositsTo(address payable to) external {
        if (to == address(0)) revert ZeroAddress();
        _withdraw(to);
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

    /// @dev True if `sig` is `signer`'s signature over `digest`.
    ///
    ///      If `signer` has code it is asked via EIP-1271 first (Safe, 4337 accounts,
    ///      ...). If that call fails or returns anything but the magic value, ECDSA
    ///      is tried as well: an EIP-7702-delegated EOA has code but still owns its
    ///      key, and its delegate may not implement EIP-1271. This fallback is safe
    ///      for ordinary contracts because no private key maps to their address, so
    ///      `ecrecover` can never return it.
    function _isValidSignature(address signer, bytes32 digest, bytes calldata sig)
        internal
        view
        returns (bool)
    {
        if (signer == address(0)) return false;
        if (signer.code.length > 0) {
            (bool ok, bytes memory ret) =
                signer.staticcall(abi.encodeWithSelector(_ERC1271_MAGIC, digest, sig));
            // compare the whole word so malformed returns cannot revert the decoder
            if (ok && ret.length >= 32 && bytes32(ret) == bytes32(_ERC1271_MAGIC)) return true;
        }
        return _recover(digest, sig) == signer;
    }

    /// @dev ECDSA signer of `digest`, or address(0) for malformed or malleable
    ///      (EIP-2 high-s) signatures.
    function _recover(bytes32 digest, bytes calldata sig) internal pure returns (address) {
        if (sig.length != 65) return address(0);
        bytes32 r = bytes32(sig[0:32]);
        bytes32 s = bytes32(sig[32:64]);
        uint8 v = uint8(sig[64]);
        if (uint256(s) > 0x7FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF5D576E7357A4501DDFE92F46681B20A0) {
            return address(0);
        }
        return ecrecover(digest, v, r, s);
    }

    /// @dev Moves the swap's deposit, if any, into the pull ledger: `tipBps` of it to
    ///      the caller and the remainder to `to`. Never sends ETH.
    function _creditDeposit(bytes32 swapId, address to, uint256 tipBps) internal {
        uint256 amount = deposits[swapId];
        if (amount == 0) return;
        deposits[swapId] = 0;
        uint256 tip = amount * tipBps / 10_000;
        if (tip != 0) {
            depositCredit[msg.sender] += tip;
            emit DepositCredited(swapId, msg.sender, tip);
        }
        depositCredit[to] += amount - tip;
        emit DepositCredited(swapId, to, amount - tip);
    }

    function _withdraw(address payable to) internal {
        uint256 amount = depositCredit[msg.sender];
        if (amount == 0) revert NothingToWithdraw();
        depositCredit[msg.sender] = 0;
        emit DepositsWithdrawn(msg.sender, to, amount);
        address(to).sendNative(amount);
    }

    /// @dev Pulls exactly `amount` of `token` from `from`, reverting if the contract
    ///      received any other amount (fee-on-transfer tokens), which would leave the
    ///      escrow unable to pay out what it recorded.
    function _pullExact(address token, address from, uint256 amount) internal {
        uint256 before = token.balanceOf(address(this));
        token.safeTransferFrom(from, address(this), amount);
        uint256 received = token.balanceOf(address(this));
        if (received < before || received - before != amount) revert UnsupportedToken();
    }

    function _payout(address token, address to, uint256 amount) internal {
        if (token == NATIVE) to.sendNative(amount);
        else token.safeTransfer(to, amount);
    }
}
