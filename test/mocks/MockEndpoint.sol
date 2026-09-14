// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {
    ILayerZeroReceiver,
    MessagingFee,
    MessagingParams,
    MessagingReceipt,
    Origin
} from "../../src/layerzero/ILayerZeroEndpointV2.sol";

/// @dev Local stand-in for EndpointV2. Each mock represents one chain (`eid`).
///      `send` queues messages; `deliverNext` on the destination mock executes
///      them, mimicking the DVN/executor hop so tests can inspect in-flight state.
contract MockEndpoint {
    uint32 public immutable eid;
    uint256 public constant FEE = 0.001 ether;

    mapping(uint32 => MockEndpoint) public remotes;
    mapping(address => address) public delegates;

    struct Packet {
        Origin origin;
        address receiver;
        bytes32 guid;
        bytes message;
    }

    Packet[] public inbox;
    uint256 public delivered;
    uint64 public outboundNonce;

    error InsufficientFee();

    constructor(uint32 _eid) {
        eid = _eid;
    }

    function connect(MockEndpoint remote) external {
        remotes[remote.eid()] = remote;
    }

    function setDelegate(address d) external {
        delegates[msg.sender] = d;
    }

    function quote(MessagingParams calldata, address) external pure returns (MessagingFee memory) {
        return MessagingFee(FEE, 0);
    }

    function send(MessagingParams calldata p, address refundAddress)
        external
        payable
        returns (MessagingReceipt memory r)
    {
        if (msg.value < FEE) revert InsufficientFee();
        uint64 nonce = ++outboundNonce;
        bytes32 guid = keccak256(abi.encode(nonce, eid, msg.sender, p.dstEid, p.receiver));
        remotes[p.dstEid].enqueue(
            Origin(eid, bytes32(uint256(uint160(msg.sender))), nonce),
            address(uint160(uint256(p.receiver))),
            guid,
            p.message
        );
        if (msg.value > FEE) payable(refundAddress).transfer(msg.value - FEE);
        r = MessagingReceipt(guid, nonce, MessagingFee(FEE, 0));
    }

    function enqueue(Origin calldata o, address receiver, bytes32 guid, bytes calldata m)
        external
    {
        inbox.push(Packet(o, receiver, guid, m));
    }

    function pending() external view returns (uint256) {
        return inbox.length - delivered;
    }

    function deliverNext() external {
        Packet memory p = inbox[delivered++];
        ILayerZeroReceiver(p.receiver).lzReceive(p.origin, p.guid, p.message, msg.sender, "");
    }
}
