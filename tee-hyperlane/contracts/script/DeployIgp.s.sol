// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Script, console} from "forge-std/Script.sol";
import {InterchainGasPaymaster} from "@hyperlane-xyz/core/contracts/hooks/igp/InterchainGasPaymaster.sol";
import {StorageGasOracle} from "@hyperlane-xyz/core/contracts/hooks/igp/StorageGasOracle.sol";
import {IGasOracle} from "@hyperlane-xyz/core/contracts/interfaces/IGasOracle.sol";

/// Deploys our own gas paymaster for one EVM chain, plus the oracle it reads.
///
/// Without this the warp routers fall back to the mailbox's default hook, which is
/// Hyperlane's own IGP - so the fee a sender pays leaves our system entirely. Owning the
/// paymaster is what makes the fee claimable by BENEFICIARY instead.
contract DeployIgp is Script {
    function run() external {
        address beneficiary = vm.envAddress("BENEFICIARY");
        uint32 remoteDomain = uint32(vm.envUint("REMOTE_DOMAIN"));
        uint96 gasOverhead = uint96(vm.envOr("GAS_OVERHEAD", uint256(150000)));

        vm.startBroadcast();
        StorageGasOracle oracle = new StorageGasOracle();

        InterchainGasPaymaster igp = new InterchainGasPaymaster();
        igp.initialize(msg.sender, beneficiary);

        InterchainGasPaymaster.GasParam[] memory params = new InterchainGasPaymaster.GasParam[](1);
        params[0] = InterchainGasPaymaster.GasParam({
            remoteDomain: remoteDomain,
            config: InterchainGasPaymaster.DomainGasConfig({
                gasOracle: IGasOracle(address(oracle)),
                gasOverhead: gasOverhead
            })
        });
        igp.setDestinationGasConfigs(params);
        vm.stopBroadcast();

        console.log("StorageGasOracle", address(oracle));
        console.log("IGP             ", address(igp));
        console.log("beneficiary     ", beneficiary);
        console.log("remote domain   ", remoteDomain);
    }
}
