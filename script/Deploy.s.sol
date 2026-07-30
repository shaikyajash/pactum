// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {HashLock} from "../src/HashLock.sol";

/// @notice Deploys HashLock via the deterministic CREATE2 factory so it lands at
///         the same address on every EVM chain.
///
///   forge script script/Deploy.s.sol --rpc-url $RPC_URL --broadcast --account deployer
contract Deploy is Script {
    bytes32 internal constant SALT = keccak256("hashlock.v1");

    function run() external returns (HashLock htlc) {
        address predicted = vm.computeCreate2Address(
            SALT, keccak256(type(HashLock).creationCode), CREATE2_FACTORY
        );

        if (predicted.code.length > 0) {
            console2.log("HashLock already deployed at", predicted);
            return HashLock(predicted);
        }

        vm.broadcast();
        htlc = new HashLock{salt: SALT}();

        require(address(htlc) == predicted, "address mismatch");
        console2.log("HashLock deployed at", address(htlc), "on chain", block.chainid);
    }
}
