// SPDX-License-Identifier: MIT
pragma solidity 0.8.24;

import {Script, console2} from "forge-std/Script.sol";
import {HashLock} from "../src/HashLock.sol";
import {SecretRelay} from "../src/ccip/SecretRelay.sol";
import {IRouterClient} from "../src/ccip/Client.sol";

/// @notice Deploy a CCIP SecretRelay next to an existing PactumIntents/HashLock.
///
///   CCIP_ROUTER=0x... HASHLOCK=0x... OWNER=0x... \
///   forge script script/DeployRelay.s.sol --rpc-url $RPC_URL --broadcast --account deployer
///
/// Then on each chain, once both relays exist, allowlist the other side:
///
///   cast send $RELAY "setPeer(uint64,address)" $REMOTE_CHAIN_SELECTOR $REMOTE_RELAY
///
/// Router addresses and chain selectors per network are listed in the CCIP
/// directory (docs.chain.link/ccip/directory). Testnet examples:
///   Sepolia       router 0x0BF3dE8c5D3e8A2B34D2BEeB17ABfCeBaf363A59  selector 16015286601757825753
///   Base Sepolia  router 0xD3b06cEbF099CE7DA4AcCf578aaebFDBd6e88a93  selector 10344971235874465080
contract DeployRelay is Script {
    function run() external returns (SecretRelay relay) {
        IRouterClient router = IRouterClient(vm.envAddress("CCIP_ROUTER"));
        HashLock htlc = HashLock(payable(vm.envAddress("HASHLOCK")));
        address owner = vm.envAddress("OWNER");

        require(owner != address(0), "OWNER must be set");
        require(address(htlc).code.length > 0, "HASHLOCK not deployed on this chain");
        require(address(router).code.length > 0, "CCIP_ROUTER not deployed on this chain");

        vm.broadcast();
        relay = new SecretRelay(router, htlc, owner);
        console2.log("SecretRelay deployed at", address(relay), "on chain", block.chainid);
    }
}
