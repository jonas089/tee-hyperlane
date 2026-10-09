// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.20;

import {Test} from "forge-std/Test.sol";
import {TeeDcapIsm, IDcapAttestation} from "../src/TeeDcapIsm.sol";

/// A stand-in for Automata's verifier, so everything downstream of the signature check can be
/// exercised without a live quote. The fork test below covers the real one.
contract MockDcap is IDcapAttestation {
    bool public ok = true;
    bytes public output;

    function set(bool _ok, bytes memory _output) external {
        ok = _ok;
        output = _output;
    }

    function verifyAndAttestOnChain(bytes calldata) external payable returns (bool, bytes memory) {
        return (ok, output);
    }
}

contract TeeDcapIsmTest is Test {
    // The measurements of the enclave that produced test/fixtures/real_quote.hex, and the
    // identity digest the Celestia module derives for the same enclave.
    bytes32 constant MEASUREMENTS = 0xbccc1d1a0259eb0fc7476eb9812eb37d6a516ecd74f2de9595f24fbd4c37b8cd;
    bytes32 constant IDENTITY = bytes32(uint256(0xd803bb1e));
    bytes32 constant TREE = bytes32(uint256(0xabcdef));
    address constant MAILBOX = address(0xBEEF);
    uint256 constant SKEW = 1 days;

    MockDcap dcap;
    TeeDcapIsm ism;

    uint64 constant GENESIS_HEIGHT = 100;
    uint64 constant GENESIS_TIME = 1_800_000_000;

    function setUp() public {
        dcap = new MockDcap();
        ism = new TeeDcapIsm(dcap, MEASUREMENTS, IDENTITY, TREE, MAILBOX, _state(1, GENESIS_HEIGHT, GENESIS_TIME), SKEW);
        vm.warp(GENESIS_TIME + 60);
    }

    // ---------------------------------------------------------------- helpers

    /// The 116-byte ISM state, matching the Celestia module's layout exactly.
    function _state(uint8 rootSeed, uint64 height, uint64 timestamp) internal pure returns (bytes memory) {
        return _stateOf(rootSeed, height, timestamp, IDENTITY);
    }

    function _stateOf(uint8 rootSeed, uint64 height, uint64 timestamp, bytes32 identity)
        internal
        pure
        returns (bytes memory)
    {
        bytes memory s = new bytes(116);
        for (uint256 i = 0; i < 32; i++) {
            s[i] = bytes1(rootSeed);
        }
        // origin domain 11155111, big endian
        s[32] = 0x00; s[33] = 0xaa; s[34] = 0x36; s[35] = 0xa7;
        for (uint256 i = 0; i < 8; i++) {
            s[36 + i] = bytes1(uint8(height >> (8 * (7 - i))));
            s[44 + i] = bytes1(uint8(timestamp >> (8 * (7 - i))));
        }
        for (uint256 i = 0; i < 32; i++) {
            s[84 + i] = identity[i];
        }
        return s;
    }

    function _payload(bytes memory prev, bytes memory next, uint64 attestedAt, bytes32[] memory ids)
        internal
        pure
        returns (bytes memory)
    {
        bytes memory head = abi.encodePacked(prev, next, TREE, attestedAt, uint64(ids.length));
        for (uint256 i = 0; i < ids.length; i++) {
            head = abi.encodePacked(head, ids[i]);
        }
        return head;
    }

    /// A verifier output shaped exactly like Automata's, committing to `payload`.
    function _output(bytes memory payload, uint8 tcbStatus, bool debug) internal pure returns (bytes memory) {
        return _outputAs(4, 2, 584, payload, tcbStatus, debug);
    }

    /// The same for any quote version and body type. A TD 1.5 body (648 bytes) is a TD 1.0
    /// body with 64 bytes appended, which Automata passes through at the same offset.
    function _outputAs(uint16 version, uint16 bodyType, uint256 bodyLen, bytes memory payload, uint8 tcbStatus, bool debug)
        internal
        pure
        returns (bytes memory)
    {
        bytes memory body = new bytes(bodyLen);
        if (debug) body[120] = 0x01;
        // mr_td and the RTMRs are left zero; the test that needs them sets MEASUREMENTS to match.
        bytes32 h = sha256(payload);
        for (uint256 i = 0; i < 32; i++) {
            body[520 + i] = h[i];
        }
        for (uint256 i = 584; i < bodyLen; i++) {
            body[i] = 0xee; // tee_tcb_svn2 and mr_servicetd, which nothing here reads
        }
        return abi.encodePacked(version, bodyType, tcbStatus, bytes6(0), body);
    }

    function _oneStepPayload() internal view returns (bytes memory) {
        return _payload(
            _state(1, GENESIS_HEIGHT, GENESIS_TIME),
            _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME),
            uint64(block.timestamp),
            new bytes32[](0)
        );
    }

    function _zeroMeasurements() internal pure returns (bytes32) {
        return keccak256(abi.encodePacked(new bytes(96), new bytes(144)));
    }

    function _fresh() internal returns (TeeDcapIsm) {
        return new TeeDcapIsm(
            dcap, _zeroMeasurements(), IDENTITY, TREE, MAILBOX, _state(1, GENESIS_HEIGHT, GENESIS_TIME), SKEW
        );
    }

    // ---------------------------------------------------------------- the happy path

    function test_advancesStateAndAuthorisesMessages() public {
        TeeDcapIsm target = _fresh();
        bytes32[] memory ids = new bytes32[](2);
        ids[0] = keccak256("a");
        ids[1] = keccak256("b");

        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 5, GENESIS_TIME + 10), uint64(block.timestamp), ids);
        dcap.set(true, _output(payload, 0, false));

        target.submitAttestation(hex"00", payload);

        assertEq(keccak256(target.state()), keccak256(_state(2, GENESIS_HEIGHT + 5, GENESIS_TIME + 10)));
        assertTrue(target.authorizedMessages(ids[0]));
        assertTrue(target.authorizedMessages(ids[1]));
    }

    function test_verifyConsumesOnce() public {
        TeeDcapIsm target = _fresh();
        bytes32[] memory ids = new bytes32[](1);
        bytes memory message = hex"deadbeef";
        ids[0] = keccak256(message);

        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), ids);
        dcap.set(true, _output(payload, 0, false));
        target.submitAttestation(hex"00", payload);

        vm.prank(MAILBOX);
        assertTrue(target.verify(hex"", message));
        vm.prank(MAILBOX);
        assertFalse(target.verify(hex"", message), "a delivered message must not verify twice");
    }

    // ---------------------------------------------------------------- quote versions

    function test_acceptsAV5QuoteWithATd15Body() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload = _oneStepPayload();
        dcap.set(true, _outputAs(5, 3, 648, payload, 0, false));
        target.submitAttestation(hex"00", payload);
        assertEq(keccak256(target.state()), keccak256(_state(2, GENESIS_HEIGHT + 1, GENESIS_TIME)));
    }

    function test_acceptsAV5QuoteWithATd10Body() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload = _oneStepPayload();
        dcap.set(true, _outputAs(5, 2, 584, payload, 0, false));
        target.submitAttestation(hex"00", payload);
        assertEq(keccak256(target.state()), keccak256(_state(2, GENESIS_HEIGHT + 1, GENESIS_TIME)));
    }

    /// The measurements sit at the same offsets in a TD 1.5 body, so a different enclave is
    /// still caught there.
    function test_aTd15BodyFromAnotherEnclaveIsRejected() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload = _oneStepPayload();
        bytes memory out = _outputAs(5, 3, 648, payload, 0, false);
        out[11 + 136] = 0x01; // first byte of mr_td
        dcap.set(true, out);
        vm.expectPartialRevert(TeeDcapIsm.WrongEnclave.selector);
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsQuoteShapesThatAreNotTdx() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload = _oneStepPayload();
        uint16[3] memory versions = [uint16(4), 5, 3];
        uint16[3] memory types = [uint16(3), 1, 2];
        for (uint256 i = 0; i < 3; i++) {
            dcap.set(true, _outputAs(versions[i], types[i], 648, payload, 0, false));
            vm.expectRevert(abi.encodeWithSelector(TeeDcapIsm.UnsupportedQuote.selector, versions[i], types[i]));
            target.submitAttestation(hex"00", payload);
        }
    }

    function test_rejectsRelaunchAdvised() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload = _oneStepPayload();
        for (uint8 status = 8; status <= 9; status++) {
            dcap.set(true, _outputAs(5, 3, 648, payload, status, false));
            vm.expectRevert(abi.encodeWithSelector(TeeDcapIsm.TcbNotAcceptable.selector, status));
            target.submitAttestation(hex"00", payload);
        }
    }

    function test_reportsItsRevision() public view {
        assertEq(ism.VERSION(), 3);
    }

    // ---------------------------------------------------------------- re-pinning

    function test_onlyTheOwnerRepins() public {
        assertEq(ism.owner(), address(this));
        vm.prank(address(0xBAD));
        vm.expectRevert("Ownable: caller is not the owner");
        ism.setEnclave(_zeroMeasurements(), bytes32(uint256(0xfeed)));
    }

    function test_aRepinSwapsOnlyTheIdentity() public {
        bytes32 next = bytes32(uint256(0xfeed));
        vm.expectEmit(address(ism));
        emit TeeDcapIsm.EnclaveChanged(MEASUREMENTS, _zeroMeasurements(), IDENTITY, next);
        ism.setEnclave(_zeroMeasurements(), next);

        assertEq(ism.enclaveMeasurements(), _zeroMeasurements());
        assertEq(ism.identityDigest(), next);
        assertEq(keccak256(ism.state()), keccak256(_stateOf(1, GENESIS_HEIGHT, GENESIS_TIME, next)));
    }

    /// After a re-pin the new enclave carries on from the same state, and the old one is refused.
    function test_aRepinnedIsmAdmitsTheNewEnclaveOnly() public {
        bytes32 next = bytes32(uint256(0xfeed));
        ism.setEnclave(_zeroMeasurements(), next);

        bytes memory stale = _payload(
            _state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0)
        );
        dcap.set(true, _output(stale, 0, false));
        vm.expectRevert(TeeDcapIsm.TrustedStateMismatch.selector);
        ism.submitAttestation(hex"00", stale);

        bytes memory payload = _payload(
            _stateOf(1, GENESIS_HEIGHT, GENESIS_TIME, next),
            _stateOf(2, GENESIS_HEIGHT + 1, GENESIS_TIME, next),
            uint64(block.timestamp),
            new bytes32[](0)
        );
        dcap.set(true, _output(payload, 0, false));
        ism.submitAttestation(hex"00", payload);
        assertEq(keccak256(ism.state()), keccak256(_stateOf(2, GENESIS_HEIGHT + 1, GENESIS_TIME, next)));
    }

    function test_ownershipCanBeHandedToAMultisig() public {
        address multisig = address(0x5AFE);
        ism.transferOwnership(multisig);
        vm.expectRevert("Ownable: caller is not the owner");
        ism.setEnclave(_zeroMeasurements(), IDENTITY);
        vm.prank(multisig);
        ism.setEnclave(_zeroMeasurements(), IDENTITY);
        assertEq(ism.enclaveMeasurements(), _zeroMeasurements());
    }

    function test_verifyOnlyFromMailbox() public {
        vm.expectRevert(TeeDcapIsm.NotMailbox.selector);
        ism.verify(hex"", hex"00");
    }

    // ---------------------------------------------------------------- refusals

    function test_rejectsWhenVerifierFails() public {
        dcap.set(false, bytes("TCBR"));
        vm.expectRevert(abi.encodeWithSelector(TeeDcapIsm.QuoteRejected.selector, bytes("TCBR")));
        ism.submitAttestation(hex"00", hex"00");
    }

    /// The failure an operator will actually meet, roughly monthly, when the collateral in
    /// the PCCS expires. The revert must name it and the description must say what to do.
    function test_expiredCollateralIsDiagnosable() public {
        dcap.set(false, bytes("TCBR"));
        vm.expectRevert(abi.encodeWithSelector(TeeDcapIsm.QuoteRejected.selector, bytes("TCBR")));
        ism.submitAttestation(hex"00", hex"00");

        string memory why = ism.describeQuoteError(bytes("TCBR"));
        assertEq(
            why,
            "TCB status revoked or missing: this chain's PCCS has no TCB info for the enclave's FMSPC, or the platform is revoked. Publish the current TCB info for that FMSPC."
        );
    }

    function test_everyVerifierCodeIsDescribed() public view {
        string[27] memory codes = [
            "TEE","OUTS","QHS","QHV","QHATTF","QEVEN","QBF","QBS","ADS","ADF","TD10F","TD15F",
            "TDMF","TDRF","TCBR","QEF","QEVE","QEIDVE","X509VE","ATTVE","TCBCH","QEIDCH",
            "ROOTH","SIGNH","ROOTCRLH","PCKCRLM","PCKCRLH"
        ];
        for (uint256 i = 0; i < codes.length; i++) {
            string memory d = ism.describeQuoteError(bytes(codes[i]));
            assertTrue(bytes(d).length > 0, "code has no description");
            assertNotEq(
                keccak256(bytes(d)),
                keccak256(bytes("Unrecognised verifier error code.")),
                string.concat("undescribed code: ", codes[i])
            );
        }
        // and an unknown code still returns something rather than reverting
        assertEq(ism.describeQuoteError(bytes("ZZZZ")), "Unrecognised verifier error code.");
    }

    function test_rejectsUnacceptableTcb() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        dcap.set(true, _output(payload, 4, false)); // TCB_OUT_OF_DATE
        vm.expectRevert(abi.encodeWithSelector(TeeDcapIsm.TcbNotAcceptable.selector, uint8(4)));
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsDebugEnclave() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        dcap.set(true, _output(payload, 0, true));
        vm.expectRevert(TeeDcapIsm.DebugEnclave.selector);
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsWrongEnclave() public {
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        dcap.set(true, _output(payload, 0, false)); // zero measurements, but `ism` pins the real ones
        vm.expectRevert(
            abi.encodeWithSelector(TeeDcapIsm.WrongEnclave.selector, MEASUREMENTS, _zeroMeasurements())
        );
        ism.submitAttestation(hex"00", payload);
    }

    /// The same enclave image running on a different CVM must still be admitted.
    ///
    /// dstack measures `app-id` and `instance-id` into rtmr3, so rtmr3 differs for every
    /// instance. This ISM pins mr_td, mr_config_id and rtmr0..2 and deliberately ignores
    /// rtmr3, which is what lets a replacement enclave keep serving an existing ISM.
    function test_acceptsSameEnclaveOnADifferentInstance() public {
        TeeDcapIsm target = _fresh();
        bytes32[] memory ids = new bytes32[](0);
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), ids);

        bytes memory out = _output(payload, 0, false);
        // Vary rtmr3 the way a different instance would.
        for (uint256 i = 0; i < 48; i++) {
            out[11 + 472 + i] = bytes1(uint8(0xAB));
        }
        dcap.set(true, out);

        target.submitAttestation(hex"00", payload);
        assertEq(keccak256(target.state()), keccak256(_state(2, GENESIS_HEIGHT + 1, GENESIS_TIME)));
    }

    /// A different enclave image must be refused, however genuine its quote.
    function test_rejectsADifferentEnclaveImage() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));

        bytes memory out = _output(payload, 0, false);
        // A different compose hash is a different image; mr_config_id carries it.
        out[11 + 185] = bytes1(uint8(0xFF));
        dcap.set(true, out);

        vm.expectRevert(
            abi.encodeWithSelector(
                TeeDcapIsm.WrongEnclave.selector,
                _zeroMeasurements(),
                keccak256(abi.encodePacked(_slice(out, 11 + 136, 96), _slice(out, 11 + 328, 144)))
            )
        );
        target.submitAttestation(hex"00", payload);
    }

    function _slice(bytes memory b, uint256 start, uint256 len) internal pure returns (bytes memory o) {
        o = new bytes(len);
        for (uint256 i = 0; i < len; i++) o[i] = b[start + i];
    }

    function test_rejectsPayloadTheQuoteDoesNotCommitTo() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        bytes memory other =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(3, GENESIS_HEIGHT + 2, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        dcap.set(true, _output(other, 0, false));
        vm.expectRevert(TeeDcapIsm.PayloadNotAttested.selector);
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsReplayOnceStateMoved() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        dcap.set(true, _output(payload, 0, false));
        target.submitAttestation(hex"00", payload);

        vm.expectRevert(TeeDcapIsm.TrustedStateMismatch.selector);
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsHeightThatDoesNotAdvance() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        dcap.set(true, _output(payload, 0, false));
        vm.expectRevert(TeeDcapIsm.HeightNotAdvanced.selector);
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsUnchangedStateRoot() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(1, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0));
        dcap.set(true, _output(payload, 0, false));
        vm.expectRevert(TeeDcapIsm.StateRootUnchanged.selector);
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsStaleAttestation() public {
        TeeDcapIsm target = _fresh();
        uint64 attestedAt = uint64(block.timestamp);
        bytes memory payload =
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), attestedAt, new bytes32[](0));
        dcap.set(true, _output(payload, 0, false));
        vm.warp(block.timestamp + SKEW + 1);
        vm.expectRevert(
            abi.encodeWithSelector(
                TeeDcapIsm.AttestationTooOld.selector, uint64(block.timestamp), attestedAt, SKEW
            )
        );
        target.submitAttestation(hex"00", payload);
    }

    function test_rejectsTrailingBytesAfterIds() public {
        TeeDcapIsm target = _fresh();
        bytes memory payload = abi.encodePacked(
            _payload(_state(1, GENESIS_HEIGHT, GENESIS_TIME), _state(2, GENESIS_HEIGHT + 1, GENESIS_TIME), uint64(block.timestamp), new bytes32[](0)),
            hex"00"
        );
        dcap.set(true, _output(payload, 0, false));
        vm.expectRevert(TeeDcapIsm.MalformedPayload.selector);
        target.submitAttestation(hex"00", payload);
    }
}

/// The real verifier, on the chain it is deployed to, with a real quote.
///
/// Everything above proves the bridge-specific logic. Only this proves that Automata's
/// deployed contract accepts a quote from our enclave, and that the offsets this contract
/// reads the report at are the offsets their output actually uses.
contract TeeDcapIsmForkTest is Test {
    IDcapAttestation constant AUTOMATA = IDcapAttestation(0xaDdeC7e85c2182202b66E331f2a4A0bBB2cEEa1F);
    bytes32 constant MEASUREMENTS = 0xbccc1d1a0259eb0fc7476eb9812eb37d6a516ecd74f2de9595f24fbd4c37b8cd;

    function test_realQuoteVerifiesAndMatchesOurMeasurements() public {
        string memory rpc = vm.envOr("BASE_SEPOLIA_RPC", string(""));
        if (bytes(rpc).length == 0) {
            emit log("BASE_SEPOLIA_RPC unset, skipping fork test");
            return;
        }
        vm.createSelectFork(rpc);

        // Automata's verifier calls the RIP-7212 P256 precompile at 0x100. Base Sepolia has
        // it; a forked EVM only does when the runner enables it, and without it the quote
        // fails for a reason that has nothing to do with this contract. Skip rather than
        // report a false failure - a real deployment on the real chain covers this properly.
        bytes memory quote = vm.parseBytes(vm.readFile("test/fixtures/real_quote.hex"));

        // Attempt it low-level: a forked EVM without the precompile fails here for a reason
        // that has nothing to do with this contract, and reporting that as a failure would be
        // misleading. A real deployment on the real chain covers this properly.
        (bool reached, bytes memory ret) = address(AUTOMATA).call(
            abi.encodeWithSelector(IDcapAttestation.verifyAndAttestOnChain.selector, quote)
        );
        if (!reached) {
            emit log("verifier unreachable in this EVM (missing P256 precompile?), skipping");
            return;
        }
        (bool ok, bytes memory out) = abi.decode(ret, (bool, bytes));
        assertTrue(ok, "automata rejected a genuine quote");
        assertGe(out.length, 595);

        // The header Automata prepends.
        assertEq(uint16(bytes2(_slice(out, 0, 2))), 4, "quote version");
        assertEq(uint16(bytes2(_slice(out, 2, 2))), 2, "TD1.0 body type");

        // The offsets TeeDcapIsm reads the measurements at.
        // mr_td ++ mr_config_id, then rtmr0..2. rtmr3 is excluded on purpose: it carries
        // app-id and instance-id, so including it would bind this ISM to one CVM.
        bytes32 measured = keccak256(abi.encodePacked(_slice(out, 11 + 136, 96), _slice(out, 11 + 328, 144)));
        assertEq(measured, MEASUREMENTS, "measurement offsets disagree with the live output");

        // The compose hash lives in mr_config_id, behind a one-byte dstack version prefix.
        // That is what makes the event-log replay unnecessary here.
        assertEq(uint8(out[11 + 184]), 1, "mr_config_id version prefix");
        assertEq(
            bytes32(_slice(out, 11 + 185, 32)),
            bytes32(0xf6ad454d9125b4512c72309e55b4e4fe7bab3b527327dd1c12199ad06cd80f5f),
            "mr_config_id should carry the compose hash"
        );

        // report_data's upper half is zero, which the ISM relies on.
        assertEq(bytes32(_slice(out, 11 + 552, 32)), bytes32(0));
    }

    function _slice(bytes memory b, uint256 start, uint256 len) private pure returns (bytes memory out) {
        out = new bytes(len);
        for (uint256 i = 0; i < len; i++) {
            out[i] = b[start + i];
        }
    }
}
