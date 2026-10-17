// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Client, IAny2EVMMessageReceiver, IERC165} from "../../src/ccip/Client.sol";

/// @dev Local stand-in for a CCIP router. Each mock is one chain (`selector`).
///      `ccipSend` queues the message on the destination mock; `deliverNext`
///      executes it, mimicking the DON hop so tests can inspect in-flight state.
contract MockCCIPRouter {
    uint64 public immutable selector;
    uint256 public constant FEE = 0.001 ether;

    mapping(uint64 => MockCCIPRouter) public remotes;

    struct Pending {
        address receiver;
        Client.Any2EVMMessage message;
    }

    Pending[] internal inbox;
    uint256 public delivered;
    uint256 public sent;

    error UnsupportedChain();
    error InsufficientFee();
    error NotAReceiver();

    constructor(uint64 _selector) {
        selector = _selector;
    }

    function connect(MockCCIPRouter remote) external {
        remotes[remote.selector()] = remote;
    }

    function isChainSupported(uint64 dst) external view returns (bool) {
        return address(remotes[dst]) != address(0);
    }

    function getFee(uint64, Client.EVM2AnyMessage memory) external pure returns (uint256) {
        return FEE;
    }

    function ccipSend(uint64 dst, Client.EVM2AnyMessage calldata m)
        external
        payable
        returns (bytes32 id)
    {
        MockCCIPRouter remote = remotes[dst];
        if (address(remote) == address(0)) revert UnsupportedChain();
        if (msg.value < FEE) revert InsufficientFee();
        id = keccak256(abi.encode(selector, dst, ++sent, msg.sender));
        remote.enqueue(
            abi.decode(m.receiver, (address)), id, selector, abi.encode(msg.sender), m.data
        );
    }

    function enqueue(
        address receiver,
        bytes32 id,
        uint64 src,
        bytes calldata sender,
        bytes calldata data
    ) external {
        Pending storage p = inbox.push();
        p.receiver = receiver;
        p.message.messageId = id;
        p.message.sourceChainSelector = src;
        p.message.sender = sender;
        p.message.data = data;
    }

    function pending() external view returns (uint256) {
        return inbox.length - delivered;
    }

    function deliverNext() external {
        Pending storage p = inbox[delivered++];
        // the real router only delivers to contracts advertising the receiver interface
        if (!IERC165(p.receiver).supportsInterface(type(IAny2EVMMessageReceiver).interfaceId)) {
            revert NotAReceiver();
        }
        IAny2EVMMessageReceiver(p.receiver).ccipReceive(p.message);
    }
}
