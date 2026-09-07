// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {IPostDispatchHook} from "@hyperlane-xyz/core/contracts/interfaces/hooks/IPostDispatchHook.sol";

/// Runs the two hooks a message from this bridge needs, in order.
///
/// A warp router picks exactly one post-dispatch hook, and this bridge needs two things from
/// it: the merkle tree hook, without which the message is never inserted and the enclave can
/// never attest it, and a paymaster, without which delivery is unfunded. Pointing a router
/// straight at a paymaster silently costs it the first - the transfer succeeds and the
/// message is simply unprovable forever.
contract TreeAndPaymasterHook is IPostDispatchHook {
    IPostDispatchHook public immutable merkleTree;
    IPostDispatchHook public immutable paymaster;

    constructor(IPostDispatchHook _merkleTree, IPostDispatchHook _paymaster) {
        merkleTree = _merkleTree;
        paymaster = _paymaster;
    }

    function hookType() external pure returns (uint8) {
        return uint8(IPostDispatchHook.HookTypes.AGGREGATION);
    }

    function supportsMetadata(bytes calldata) external pure returns (bool) {
        return true;
    }

    /// The tree hook charges nothing, so the whole payment goes to the paymaster. Any excess
    /// is the paymaster's to handle, exactly as it would be if the router pointed at it.
    function postDispatch(
        bytes calldata metadata,
        bytes calldata message
    ) external payable {
        merkleTree.postDispatch{value: 0}(metadata, message);
        paymaster.postDispatch{value: msg.value}(metadata, message);
    }

    function quoteDispatch(
        bytes calldata metadata,
        bytes calldata message
    ) external view returns (uint256) {
        return
            merkleTree.quoteDispatch(metadata, message) +
            paymaster.quoteDispatch(metadata, message);
    }
}
