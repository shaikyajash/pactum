// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

/// @dev Minimal subset of Chainlink CCIP's `Client` library and router interfaces
///      used by SecretRelay. Struct layouts match chainlink-ccip `libraries/Client.sol`.
library Client {
    struct EVMTokenAmount {
        address token;
        uint256 amount;
    }

    struct Any2EVMMessage {
        bytes32 messageId;
        uint64 sourceChainSelector;
        bytes sender; // abi.encode(sender address)
        bytes data;
        EVMTokenAmount[] destTokenAmounts;
    }

    struct EVM2AnyMessage {
        bytes receiver; // abi.encode(receiver address)
        bytes data;
        EVMTokenAmount[] tokenAmounts;
        address feeToken; // address(0) = pay in native
        bytes extraArgs;
    }

    bytes4 internal constant GENERIC_EXTRA_ARGS_V2_TAG = 0x181dcf10;

    struct GenericExtraArgsV2 {
        uint256 gasLimit;
        bool allowOutOfOrderExecution;
    }

    function _argsToBytes(GenericExtraArgsV2 memory args) internal pure returns (bytes memory) {
        return abi.encodeWithSelector(GENERIC_EXTRA_ARGS_V2_TAG, args);
    }
}

interface IRouterClient {
    function isChainSupported(uint64 destChainSelector) external view returns (bool);

    function getFee(uint64 destinationChainSelector, Client.EVM2AnyMessage memory message)
        external
        view
        returns (uint256 fee);

    function ccipSend(uint64 destinationChainSelector, Client.EVM2AnyMessage calldata message)
        external
        payable
        returns (bytes32 messageId);
}

/// @dev Interface the router calls on receivers; advertised via ERC-165.
interface IAny2EVMMessageReceiver {
    function ccipReceive(Client.Any2EVMMessage calldata message) external;
}

interface IERC165 {
    function supportsInterface(bytes4 interfaceId) external view returns (bool);
}
