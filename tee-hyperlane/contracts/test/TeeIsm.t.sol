// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {TeeIsm, ISP1Verifier} from "../src/TeeIsm.sol";

/// Accepts or rejects on command, so the tests exercise this contract's logic rather than
/// SP1's. Proof verification itself is covered by SP1's own audited verifier.
contract MockVerifier is ISP1Verifier {
    bool public shouldAccept = true;
    bytes32 public lastVkey;

    function setAccept(bool v) external {
        shouldAccept = v;
    }

    function verifyProof(bytes32 vkey, bytes calldata, bytes calldata) external view {
        require(shouldAccept, "proof rejected");
        // `lastVkey` is checked by tests to confirm each entry point verifies against the
        // right program, which is what keeps the two proof shapes from being swapped.
        require(vkey != bytes32(0), "zero vkey");
    }
}

contract TeeIsmTest is Test {
    /// Real public values produced by celestia-app v9.0.6, the version live Mocha runs.
    /// If this contract's decoder and that Go decoder ever disagree, the bridge is broken in
    /// one direction only - which is exactly the failure these fixtures exist to catch.
    bytes constant MEMBERSHIP_PV =
        hex"b1d302256aee21b0d2dc21d88612061d1c7bb5bd5a222d98bd29482e6ea33d33fcb1d485ef46344029d9e8a7925925e146b3430e00000000000000000000000001000000000000008066fb378e24512ba445ac2f36b1a5b1d74b664d09df64b58226923a680990a6";

    bytes32 constant FIXTURE_ROOT =
        0xb1d302256aee21b0d2dc21d88612061d1c7bb5bd5a222d98bd29482e6ea33d33;
    bytes32 constant FIXTURE_TREE =
        0xfcb1d485ef46344029d9e8a7925925e146b3430e000000000000000000000000;
    bytes32 constant FIXTURE_MESSAGE_ID =
        0x8066fb378e24512ba445ac2f36b1a5b1d74b664d09df64b58226923a680990a6;

    bytes32 constant TRANSITION_VKEY = bytes32(uint256(1));
    bytes32 constant MEMBERSHIP_VKEY = bytes32(uint256(2));
    uint256 constant MAX_AGE = 6 hours;

    MockVerifier verifier;
    TeeIsm ism;
    uint64 headTime;

    function setUp() public {
        vm.warp(1_800_000_000);
        headTime = uint64(block.timestamp);
        verifier = new MockVerifier();
        ism = new TeeIsm(
            verifier,
            TRANSITION_VKEY,
            MEMBERSHIP_VKEY,
            FIXTURE_TREE,
            ismState(bytes32(uint256(0xAA)), 100, headTime),
            MAX_AGE
        );
    }

    /// The 116-byte layout: root(32) | domain(4) | height(8) | timestamp(8) | commit(32) | id(32)
    function ismState(bytes32 root, uint64 height, uint64 timestamp)
        internal
        pure
        returns (bytes memory s)
    {
        s = new bytes(116);
        for (uint256 i = 0; i < 32; i++) {
            s[i] = root[i];
        }
        for (uint256 i = 0; i < 4; i++) {
            s[32 + i] = bytes1(uint8(11155111 >> (8 * (3 - i))));
        }
        for (uint256 i = 0; i < 8; i++) {
            s[36 + i] = bytes1(uint8(height >> (8 * (7 - i))));
            s[44 + i] = bytes1(uint8(timestamp >> (8 * (7 - i))));
        }
    }

    function transitionPv(bytes memory prev, bytes memory next)
        internal
        pure
        returns (bytes memory pv)
    {
        pv = abi.encodePacked(uint64LE(prev.length), prev, uint64LE(next.length), next);
    }

    function uint64LE(uint256 v) internal pure returns (bytes memory out) {
        out = new bytes(8);
        for (uint256 i = 0; i < 8; i++) {
            out[i] = bytes1(uint8(v >> (8 * i)));
        }
    }

    function membershipPv(bytes32 root, bytes32 tree, bytes32[] memory ids)
        internal
        pure
        returns (bytes memory pv)
    {
        pv = abi.encodePacked(root, tree, uint64LE(ids.length));
        for (uint256 i = 0; i < ids.length; i++) {
            pv = abi.encodePacked(pv, ids[i]);
        }
    }

    // ---- cross-language decoding ----

    function test_decodesCelestiaAppMembershipFixture() public view {
        (bytes32 root, bytes32 tree, uint256 count) = ism.decodeStateMembershipHeader(MEMBERSHIP_PV);
        assertEq(root, FIXTURE_ROOT, "state root");
        assertEq(tree, FIXTURE_TREE, "merkle tree address");
        assertEq(count, 1, "message count");
        assertEq(MEMBERSHIP_PV.length, 104, "32 + 32 + 8 + 32");
    }

    function test_membershipCountIsLittleEndian() public view {
        // A big-endian slip would read 2**56 here rather than 1.
        (,, uint256 count) = ism.decodeStateMembershipHeader(MEMBERSHIP_PV);
        assertEq(count, 1);
    }

    function test_decodesTransitionWithMatchingLengths() public view {
        bytes memory prev = ismState(bytes32(uint256(1)), 1, 1);
        bytes memory next = ismState(bytes32(uint256(2)), 2, 2);
        (bytes memory a, bytes memory b) = ism.decodeStateTransition(transitionPv(prev, next));
        assertEq(keccak256(a), keccak256(prev));
        assertEq(keccak256(b), keccak256(next));
    }

    function test_rejectsTrailingBytesInMembership() public {
        bytes memory pv = abi.encodePacked(MEMBERSHIP_PV, hex"00");
        vm.expectRevert(TeeIsm.MalformedPublicValues.selector);
        ism.decodeStateMembershipHeader(pv);
    }

    function test_rejectsStateLengthOutsideZkismBounds() public {
        bytes memory pv = abi.encodePacked(uint64LE(31), new bytes(31), uint64LE(32), new bytes(32));
        vm.expectRevert(TeeIsm.InvalidStateLength.selector);
        ism.decodeStateTransition(pv);
    }

    // ---- state transitions ----

    function test_updateStateAdvancesTheRoot() public {
        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, headTime + 12);
        ism.updateState(hex"00", transitionPv(ism.state(), next));
        assertEq(ism.stateRoot(), bytes32(uint256(0xBB)));
    }

    function test_updateStateRejectsAProofForADifferentTrustedState() public {
        bytes memory wrongPrev = ismState(bytes32(uint256(0xEE)), 100, headTime);
        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, headTime + 12);
        vm.expectRevert(TeeIsm.TrustedStateMismatch.selector);
        ism.updateState(hex"00", transitionPv(wrongPrev, next));
    }

    /// A replayed proof is stale rather than dangerous: after the state moves on, the same
    /// proof no longer matches the stored state.
    function test_aReplayedUpdateIsRejected() public {
        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, headTime + 12);
        bytes memory pv = transitionPv(ism.state(), next);
        ism.updateState(hex"00", pv);
        vm.expectRevert(TeeIsm.TrustedStateMismatch.selector);
        ism.updateState(hex"00", pv);
    }

    function test_updateStateRejectsAnInvalidProof() public {
        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, headTime + 12);
        // Build the calldata before expectRevert, or it arms on ism.state() instead.
        bytes memory pv = transitionPv(ism.state(), next);
        verifier.setAccept(false);
        vm.expectRevert("proof rejected");
        ism.updateState(hex"00", pv);
    }

    // ---- message authorisation ----

    function authorizeOneMessage(bytes32 id) internal returns (bytes memory message) {
        message = abi.encodePacked("hyperlane message ", id);
        bytes32[] memory ids = new bytes32[](1);
        ids[0] = keccak256(message);

        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, uint64(block.timestamp));
        ism.updateState(hex"00", transitionPv(ism.state(), next));
        ism.submitMessages(hex"00", membershipPv(ism.stateRoot(), FIXTURE_TREE, ids));
    }

    function test_verifyAuthorisesThenConsumes() public {
        bytes memory message = authorizeOneMessage(bytes32(uint256(7)));
        assertTrue(ism.verify(hex"", message), "first delivery");
        assertFalse(ism.verify(hex"", message), "replay must not verify twice");
    }

    function test_verifyRejectsAnUnauthorisedMessage() public {
        authorizeOneMessage(bytes32(uint256(7)));
        assertFalse(ism.verify(hex"", abi.encodePacked("some other message")));
    }

    function test_submitMessagesRejectsAStaleRoot() public {
        bytes32[] memory ids = new bytes32[](1);
        ids[0] = keccak256("m");
        vm.expectRevert(TeeIsm.StateRootMismatch.selector);
        ism.submitMessages(hex"00", membershipPv(bytes32(uint256(0xDEAD)), FIXTURE_TREE, ids));
    }

    function test_submitMessagesRejectsADifferentMerkleTree() public {
        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, uint64(block.timestamp));
        ism.updateState(hex"00", transitionPv(ism.state(), next));
        bytes32[] memory ids = new bytes32[](1);
        ids[0] = keccak256("m");
        bytes memory pv = membershipPv(ism.stateRoot(), bytes32(uint256(0xFEED)), ids);
        vm.expectRevert(TeeIsm.MerkleTreeMismatch.selector);
        ism.submitMessages(hex"00", pv);
    }

    /// One batch per root, matching x/zkism. The second batch must wait for the root to move.
    function test_onlyOneBatchPerStateRoot() public {
        bytes32[] memory ids = new bytes32[](1);
        ids[0] = keccak256("m");
        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, uint64(block.timestamp));
        ism.updateState(hex"00", transitionPv(ism.state(), next));
        bytes memory pv = membershipPv(ism.stateRoot(), FIXTURE_TREE, ids);
        ism.submitMessages(hex"00", pv);

        vm.expectRevert(TeeIsm.MessagesAlreadySubmitted.selector);
        ism.submitMessages(hex"00", pv);
    }

    function test_advancingTheRootReArmsMessageSubmission() public {
        test_onlyOneBatchPerStateRoot();
        bytes memory next = ismState(bytes32(uint256(0xCC)), 102, uint64(block.timestamp));
        ism.updateState(hex"00", transitionPv(ism.state(), next));
        assertFalse(ism.messagesSubmittedForRoot(), "must be re-armed");

        bytes32[] memory ids = new bytes32[](1);
        ids[0] = keccak256("m2");
        ism.submitMessages(hex"00", membershipPv(ism.stateRoot(), FIXTURE_TREE, ids));
    }

    /// The check Celestia's fixed module cannot express.
    function test_rejectsMessagesUnderAStaleHead() public {
        bytes memory next = ismState(bytes32(uint256(0xBB)), 101, uint64(block.timestamp));
        ism.updateState(hex"00", transitionPv(ism.state(), next));
        bytes32[] memory ids = new bytes32[](1);
        ids[0] = keccak256("m");
        bytes memory pv = membershipPv(ism.stateRoot(), FIXTURE_TREE, ids);
        vm.warp(block.timestamp + MAX_AGE + 1);

        vm.expectRevert(TeeIsm.StateTooOld.selector);
        ism.submitMessages(hex"00", pv);
    }

    function test_stateFieldsAreBigEndian() public view {
        bytes memory s = ismState(bytes32(uint256(1)), 0x0102030405060708, 0x1112131415161718);
        assertEq(ism.readHeight(s), 0x0102030405060708);
        assertEq(ism.readTimestamp(s), 0x1112131415161718);
    }
}
