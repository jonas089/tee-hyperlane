// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {TeeTokenFactory} from "../src/TeeTokenFactory.sol";
import {HypERC20} from "@hyperlane-xyz/core/contracts/token/HypERC20.sol";

/// Run against a fork, because a router needs a real mailbox: `forge test --fork-url <sepolia rpc>`.
contract TeeTokenFactoryTest is Test {
    address constant MAILBOX = 0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766;
    uint32 constant HUB = 1297040299;
    // Two contracts on Sepolia to stand in for ISMs: ours, and the mailbox's default.
    address constant ISM = 0x83B41448ADfBdde1926575f774489892F88190A8;
    address constant OTHER = 0x4917a9746A7B6E0A57159cCb7F5a6744247f2d0d;
    TeeTokenFactory factory;

    function setUp() public {
        if (MAILBOX.code.length == 0) vm.skip(true);
        factory = new TeeTokenFactory(MAILBOX, HUB, ISM);
    }

    function test_a_launch_is_wired_and_owned_by_the_factory() public {
        bytes32 hub = bytes32(uint256(0xabc));
        vm.prank(address(0xbeef));
        HypERC20 router = HypERC20(factory.launch(hub, "Moon", "MOON"));
        assertEq(router.symbol(), "MOON");
        assertEq(router.decimals(), 6);
        assertEq(router.totalSupply(), 0);
        assertEq(address(router.interchainSecurityModule()), ISM);
        assertEq(router.routers(HUB), hub);
        assertEq(router.owner(), address(factory));
        (address r, bytes32 h, address launcher) = factory.launches(0);
        assertEq(r, address(router));
        assertEq(h, hub);
        assertEq(launcher, address(0xbeef));
        assertTrue(factory.isLaunched(address(router)));
    }

    function test_only_the_owner_repoints_and_it_reaches_every_launch() public {
        HypERC20 a = HypERC20(factory.launch(bytes32(uint256(1)), "A", "A"));
        HypERC20 b = HypERC20(factory.launch(bytes32(uint256(2)), "B", "B"));
        vm.prank(address(0xbad));
        vm.expectRevert();
        factory.setIsm(address(0x9));

        factory.setIsm(OTHER);
        factory.repoint(0, 100);
        assertEq(address(a.interchainSecurityModule()), OTHER);
        assertEq(address(b.interchainSecurityModule()), OTHER);

        vm.expectRevert();
        a.setInterchainSecurityModule(ISM);
    }
}
