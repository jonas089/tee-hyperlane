// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {HypERC20Collateral} from "@hyperlane-xyz/core/contracts/token/HypERC20Collateral.sol";

/// Deploys the Sepolia side of the USDC route: real USDC is escrowed here and a synthetic
/// is minted on Celestia. The ISM matters for the return leg - messages arriving from
/// Celestia are what release the collateral.
contract DeployWarpCollateral is Script {
    function run() external {
        address mailbox = vm.envAddress("MAILBOX");
        address ism = vm.envAddress("TEE_ISM");
        address token = vm.envAddress("COLLATERAL_TOKEN");

        vm.startBroadcast();
        HypERC20Collateral router = new HypERC20Collateral(token, 1, 1, mailbox);
        router.initialize(address(0), ism, msg.sender);
        vm.stopBroadcast();

        console.log("HypERC20Collateral (USDC)", address(router));
        console.log("wrapped token            ", token);
        console.log("ism                      ", ism);
    }
}
