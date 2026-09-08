// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {TeeIsm, ISP1Verifier} from "../src/TeeIsm.sol";

/// Deploys the ISM that authorises Celestia-origin messages on Sepolia.
///
/// Verifies against SP1's *concrete* v5.0.0 Groth16 verifier rather than the gateway. The
/// gateway is owner-mutable - whoever owns it can re-point a version selector - while a
/// deployed verifier cannot change under us.
contract DeployTeeIsm is Script {
    address constant SP1_VERIFIER_GROTH16_V5 = 0x50ACFBEdecf4cbe350E1a86fC6f03a821772f1e5;

    function run() external {
        bytes32 stateTransitionVkey = vm.envBytes32("STATE_TRANSITION_VKEY");
        bytes32 stateMembershipVkey = vm.envBytes32("STATE_MEMBERSHIP_VKEY");
        bytes32 merkleTreeAddress = vm.envBytes32("ORIGIN_MERKLE_TREE");
        address mailbox = vm.envAddress("MAILBOX");
        bytes memory genesisState = vm.envBytes("GENESIS_STATE");
        uint256 maxStateAge = vm.envOr("MAX_STATE_AGE", uint256(6 hours));

        vm.startBroadcast();
        TeeIsm ism = new TeeIsm(
            ISP1Verifier(SP1_VERIFIER_GROTH16_V5),
            stateTransitionVkey,
            stateMembershipVkey,
            merkleTreeAddress,
            mailbox,
            genesisState,
            maxStateAge
        );
        vm.stopBroadcast();

        console.log("TeeIsm            ", address(ism));
        console.log("mailbox           ", mailbox);
        console.log("verifier          ", SP1_VERIFIER_GROTH16_V5);
        console.log("max state age (s) ", maxStateAge);
        console.logBytes32(ism.stateRoot());
    }
}
