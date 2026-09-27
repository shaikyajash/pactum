// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {HashLock} from "../src/HashLock.sol";
import {SecretRelay} from "../src/layerzero/SecretRelay.sol";

/// @notice Deploy a SecretRelay next to an existing HashLock and wire up peers.
///
///   LZ_ENDPOINT=0x1a44076050125825900e736c501f859c50fE728c \
///   HASHLOCK=0x... OWNER=0x... \
///   forge script script/DeployRelay.s.sol --rpc-url $RPC_URL --broadcast --account deployer
///
/// Then on each chain, once both relays exist:
///
///   cast send $RELAY "setPeer(uint32,bytes32)" $REMOTE_EID $(cast to-bytes32 $REMOTE_RELAY)
///
/// EndpointV2 is 0x1a44076050125825900e736c501f859c50fE728c on most mainnets and
/// 0x6EDCE65403992e310A62460808c4b910D972f10f on most testnets; check the
/// LayerZero deployments page for the chain you are targeting.
contract DeployRelay is Script {
    function run() external returns (SecretRelay relay) {
        address endpoint = vm.envAddress("LZ_ENDPOINT");
        HashLock htlc = HashLock(vm.envAddress("HASHLOCK"));
        address owner = vm.envAddress("OWNER");

        require(address(htlc).code.length > 0, "HASHLOCK not deployed on this chain");

        vm.broadcast();
        relay = new SecretRelay(endpoint, htlc, owner);
        console2.log("SecretRelay deployed at", address(relay), "on chain", block.chainid);
    }
}
