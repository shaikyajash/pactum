// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @dev Minimal subset of LayerZero V2's EndpointV2 interface used by SecretRelay.
///      Struct layouts match `ILayerZeroEndpointV2` from LayerZero-v2/protocol.

struct MessagingParams {
    uint32 dstEid;
    bytes32 receiver;
    bytes message;
    bytes options;
    bool payInLzToken;
}

struct MessagingFee {
    uint256 nativeFee;
    uint256 lzTokenFee;
}

struct MessagingReceipt {
    bytes32 guid;
    uint64 nonce;
    MessagingFee fee;
}

struct Origin {
    uint32 srcEid;
    bytes32 sender;
    uint64 nonce;
}

interface ILayerZeroEndpointV2 {
    function quote(MessagingParams calldata params, address sender)
        external
        view
        returns (MessagingFee memory);

    function send(MessagingParams calldata params, address refundAddress)
        external
        payable
        returns (MessagingReceipt memory);

    function setDelegate(address delegate) external;
}

/// @dev Interface the endpoint calls on the receiving OApp.
interface ILayerZeroReceiver {
    function allowInitializePath(Origin calldata origin) external view returns (bool);

    function nextNonce(uint32 eid, bytes32 sender) external view returns (uint64);

    function lzReceive(
        Origin calldata origin,
        bytes32 guid,
        bytes calldata message,
        address executor,
        bytes calldata extraData
    ) external payable;
}
