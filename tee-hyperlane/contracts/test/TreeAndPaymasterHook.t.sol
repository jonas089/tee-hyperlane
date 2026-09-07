// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {TreeAndPaymasterHook} from "../src/TreeAndPaymasterHook.sol";
import {IPostDispatchHook} from "@hyperlane-xyz/core/contracts/interfaces/hooks/IPostDispatchHook.sol";

contract RecordingHook is IPostDispatchHook {
    uint256 public calls;
    uint256 public received;
    uint256 public quote;

    constructor(uint256 _quote) {
        quote = _quote;
    }

    function hookType() external pure returns (uint8) {
        return uint8(IPostDispatchHook.HookTypes.MERKLE_TREE);
    }

    function supportsMetadata(bytes calldata) external pure returns (bool) {
        return true;
    }

    function postDispatch(bytes calldata, bytes calldata) external payable {
        calls += 1;
        received += msg.value;
    }

    function quoteDispatch(bytes calldata, bytes calldata) external view returns (uint256) {
        return quote;
    }
}

contract TreeAndPaymasterHookTest is Test {
    RecordingHook tree;
    RecordingHook paymaster;
    TreeAndPaymasterHook hook;

    function setUp() public {
        tree = new RecordingHook(0);
        paymaster = new RecordingHook(1 ether);
        hook = new TreeAndPaymasterHook(tree, paymaster);
    }

    /// The point of this contract: a router picks one hook, and both must run. Missing the
    /// tree would leave the message unprovable while the transfer still succeeded.
    function test_both_hooks_run() public {
        hook.postDispatch{value: 1 ether}("", "");
        assertEq(tree.calls(), 1, "merkle tree hook must run");
        assertEq(paymaster.calls(), 1, "paymaster must run");
    }

    function test_payment_goes_to_the_paymaster() public {
        hook.postDispatch{value: 1 ether}("", "");
        assertEq(tree.received(), 0);
        assertEq(paymaster.received(), 1 ether);
    }

    function test_quote_is_the_sum() public view {
        assertEq(hook.quoteDispatch("", ""), 1 ether);
    }

    function test_hook_type_is_aggregation() public view {
        assertEq(hook.hookType(), uint8(IPostDispatchHook.HookTypes.AGGREGATION));
    }
}
