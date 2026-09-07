// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {HypERC20} from "@hyperlane-xyz/core/contracts/token/HypERC20.sol";

/// Deploys the EVM side of a warp route: a synthetic token minted when a message from the
/// origin is delivered, pointed at our TEE ISM.
///
/// The ISM choice is the whole point. Without `setInterchainSecurityModule` this router
/// would fall back to the mailbox default, which is Hyperlane's validator multisig - exactly
/// the thing this bridge exists to remove.
contract DeployWarpSynthetic is Script {
    function run() external {
        address mailbox = vm.envAddress("MAILBOX");
        address ism = vm.envAddress("TEE_ISM");
        uint32 originDomain = uint32(vm.envUint("ORIGIN_DOMAIN"));
        bytes32 originRouter = vm.envBytes32("ORIGIN_ROUTER");
        string memory name = vm.envOr("TOKEN_NAME", string("Celestia TIA"));
        string memory symbol = vm.envOr("TOKEN_SYMBOL", string("TIA"));
        // Both tokens this bridge carries have 6 decimals, so no scaling is needed.
        uint8 decimals = uint8(vm.envOr("TOKEN_DECIMALS", uint256(6)));

        vm.startBroadcast();
        HypERC20 token = new HypERC20(decimals, 1, 1, mailbox);
        token.initialize(0, name, symbol, address(0), ism, msg.sender);
        token.enrollRemoteRouter(originDomain, originRouter);
        vm.stopBroadcast();

        console.log("HypERC20      ", address(token));
        console.log("symbol        ", symbol);
        console.log("ism           ", ism);
        console.log("origin domain ", originDomain);
    }
}
