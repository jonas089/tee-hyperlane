// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Ownable} from "@openzeppelin/contracts/access/Ownable.sol";
import {HypERC20} from "@hyperlane-xyz/core/contracts/token/HypERC20.sol";

/// Launches the EVM side of a token whose home is the Celestia hub: a synthetic HypERC20
/// pointed at this chain's TEE ISM and enrolled with its hub token, in one call.
///
/// The factory keeps ownership of every router it makes. A launcher gets a router nobody can
/// reconfigure against them, and an enclave identity rotation can still re-point all of them,
/// which is the one change a router ever needs. The owner of this factory can do that and
/// nothing else to a launched router.
contract TeeTokenFactory is Ownable {
    struct Launch {
        address router;
        bytes32 hubToken;
        address launcher;
    }

    address public immutable mailbox;
    uint32 public immutable hubDomain;
    /// The ISM new routers point at, and existing ones are re-pointed to.
    address public ism;

    Launch[] public launches;
    mapping(address => bool) public isLaunched;

    event Launched(bytes32 indexed hubToken, address indexed router, address indexed launcher, string name, string symbol);
    event IsmChanged(address ism);

    constructor(address _mailbox, uint32 _hubDomain, address _ism) Ownable() {
        mailbox = _mailbox;
        hubDomain = _hubDomain;
        ism = _ism;
    }

    /// Deploy a router for `hubToken`. Anyone may, for any hub token: which router a token
    /// trusts is decided on the hub, where the token enrolls exactly one per chain.
    function launch(bytes32 hubToken, string calldata name, string calldata symbol) external returns (address) {
        HypERC20 router = new HypERC20(6, 1, 1, mailbox);
        router.initialize(0, name, symbol, address(0), ism, address(this));
        router.enrollRemoteRouter(hubDomain, hubToken);
        launches.push(Launch(address(router), hubToken, msg.sender));
        isLaunched[address(router)] = true;
        emit Launched(hubToken, address(router), msg.sender, name, symbol);
        return address(router);
    }

    function launchCount() external view returns (uint256) {
        return launches.length;
    }

    /// Point future launches at `newIsm`. Existing ones follow through `repoint`.
    function setIsm(address newIsm) external onlyOwner {
        ism = newIsm;
        emit IsmChanged(newIsm);
    }

    /// Re-point launches `[from, to)` at the current ISM. In ranges, so the gas of one call
    /// stays bounded however many tokens have launched.
    function repoint(uint256 from, uint256 to) external onlyOwner {
        if (to > launches.length) to = launches.length;
        for (uint256 i = from; i < to; i++) {
            HypERC20 router = HypERC20(launches[i].router);
            if (address(router.interchainSecurityModule()) != ism) router.setInterchainSecurityModule(ism);
        }
    }
}
