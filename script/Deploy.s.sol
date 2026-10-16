// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {PactumIntents} from "../src/PactumIntents.sol";

/// @notice Deploys PactumIntents (HashLock + signed intents) via the deterministic
///         CREATE2 factory so it lands at the same address on every EVM chain.
///
///   forge script script/Deploy.s.sol --rpc-url $RPC_URL --broadcast --account deployer
contract Deploy is Script {
    bytes32 internal constant SALT = keccak256("pactum.intents.v1");

    function run() external returns (PactumIntents pactum) {
        address predicted = vm.computeCreate2Address(
            SALT, keccak256(type(PactumIntents).creationCode), CREATE2_FACTORY
        );

        if (predicted.code.length > 0) {
            console2.log("PactumIntents already deployed at", predicted);
            return PactumIntents(predicted);
        }

        vm.broadcast();
        pactum = new PactumIntents{salt: SALT}();

        require(address(pactum) == predicted, "address mismatch");
        console2.log("PactumIntents deployed at", address(pactum), "on chain", block.chainid);
    }
}
