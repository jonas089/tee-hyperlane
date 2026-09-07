// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {TreeAndPaymasterHook} from "../src/TreeAndPaymasterHook.sol";
import {IPostDispatchHook} from "@hyperlane-xyz/core/contracts/interfaces/hooks/IPostDispatchHook.sol";

contract DeployTreeAndPaymasterHook is Script {
    function run() external {
        address merkleTree = vm.envAddress("MERKLE_TREE_HOOK");
        address paymaster = vm.envAddress("IGP");

        vm.startBroadcast();
        TreeAndPaymasterHook hook = new TreeAndPaymasterHook(
            IPostDispatchHook(merkleTree),
            IPostDispatchHook(paymaster)
        );
        vm.stopBroadcast();

        console.log("TreeAndPaymasterHook", address(hook));
        console.log("merkle tree hook    ", merkleTree);
        console.log("paymaster           ", paymaster);
    }
}
