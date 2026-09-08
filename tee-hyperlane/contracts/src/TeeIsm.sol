// SPDX-License-Identifier: Apache-2.0
pragma solidity ^0.8.28;

/// @notice SP1's on-chain Groth16 verifier.
interface ISP1Verifier {
    function verifyProof(bytes32 programVKey, bytes calldata publicValues, bytes calldata proofBytes)
        external
        view;
}

/// @notice The subset of Hyperlane's ISM interface a Mailbox calls.
interface IInterchainSecurityModule {
    function moduleType() external view returns (uint8);
    function verify(bytes calldata metadata, bytes calldata message) external returns (bool);
}

/// @title TeeIsm
/// @notice Authorises Hyperlane messages from a TEE-attested origin state root.
///
/// A deliberate port of Celestia's `x/zkism`, so one relayer code path drives both chains
/// and the two implementations can be compared line by line. Same two-phase protocol, same
/// public-value encodings, same stored fields:
///
///  1. `updateState` advances the trusted origin state, proving an enclave attested the
///     transition from exactly the state this contract already holds.
///  2. `submitMessages` authorises one batch of message ids under that state root.
///  3. `verify` is what the Mailbox calls, and is a consume-once lookup - all the
///     cryptography happened in step 1 and 2.
///
/// The proofs are two SP1 proofs of the *same* enclave attestation, projected into the two
/// public-value shapes `x/zkism` requires. One blob cannot satisfy both decoders, so the
/// two-transaction shape is forced rather than chosen.
contract TeeIsm is IInterchainSecurityModule {
    /// Hyperlane reserves module types 0..11; this is outside that range, matching the
    /// convention `x/zkism` follows on the Celestia side.
    uint8 public constant MODULE_TYPE = 43;

    /// x/zkism's bounds on a state blob, mirrored so both chains reject the same things.
    uint256 public constant MIN_STATE_BYTES = 32;
    uint256 public constant MAX_STATE_BYTES = 2048;
    uint256 public constant MAX_MESSAGE_ID_COUNT = 1_000_000;

    /// How stale an attested origin head may be before messages stop being authorised.
    /// Celestia's module cannot express this; here we can, so we do.
    uint256 public immutable maxStateAge;

    ISP1Verifier public immutable verifier;
    bytes32 public immutable stateTransitionVkey;
    bytes32 public immutable stateMembershipVkey;
    bytes32 public immutable merkleTreeAddress;
    /// The only address allowed to consume an authorisation.
    address public immutable mailbox;

    /// Opaque to this contract except for the first 32 bytes, which are the state root.
    bytes public state;
    /// At most one message batch per distinct state root, exactly as x/zkism enforces.
    bool public messagesSubmittedForRoot;

    mapping(bytes32 => bool) public authorizedMessages;

    event StateUpdated(bytes32 indexed stateRoot, uint64 height, uint64 timestamp);
    event MessagesAuthorized(bytes32 indexed stateRoot, uint256 count);
    event MessageConsumed(bytes32 indexed messageId);

    error InvalidStateLength();
    error TrustedStateMismatch();
    error StateRootMismatch();
    error MerkleTreeMismatch();
    error MessagesAlreadySubmitted();
    error MalformedPublicValues();
    error TooManyMessages();
    error StateTooOld();
    error NotMailbox();

    constructor(
        ISP1Verifier _verifier,
        bytes32 _stateTransitionVkey,
        bytes32 _stateMembershipVkey,
        bytes32 _merkleTreeAddress,
        address _mailbox,
        bytes memory _genesisState,
        uint256 _maxStateAge
    ) {
        if (_genesisState.length < MIN_STATE_BYTES || _genesisState.length > MAX_STATE_BYTES) {
            revert InvalidStateLength();
        }
        verifier = _verifier;
        stateTransitionVkey = _stateTransitionVkey;
        stateMembershipVkey = _stateMembershipVkey;
        merkleTreeAddress = _merkleTreeAddress;
        mailbox = _mailbox;
        state = _genesisState;
        maxStateAge = _maxStateAge;
    }

    function moduleType() external pure returns (uint8) {
        return MODULE_TYPE;
    }

    /// @notice Advance the trusted origin state.
    /// @dev Permissionless: security comes from the proof, not from who submits it.
    function updateState(bytes calldata proof, bytes calldata publicValues) external {
        (bytes memory prevState, bytes memory newState) = decodeStateTransition(publicValues);

        // Binds the proof to the state this contract already holds, which is what makes a
        // replayed proof stale rather than dangerous.
        if (keccak256(prevState) != keccak256(state)) revert TrustedStateMismatch();

        verifier.verifyProof(stateTransitionVkey, publicValues, proof);

        bytes32 oldRoot = readStateRoot(state);
        bytes32 newRoot = readStateRoot(newState);
        state = newState;

        // Re-arm message submission only when the root actually moved; otherwise a second
        // batch could be authorised under a root that already had one.
        if (oldRoot != newRoot) messagesSubmittedForRoot = false;

        emit StateUpdated(newRoot, readHeight(newState), readTimestamp(newState));
    }

    /// @notice Authorise one batch of Hyperlane message ids under the current state root.
    function submitMessages(bytes calldata proof, bytes calldata publicValues) external {
        (bytes32 stateRoot, bytes32 treeAddress, uint256 count) =
            decodeStateMembershipHeader(publicValues);

        if (stateRoot != readStateRoot(state)) revert StateRootMismatch();
        if (messagesSubmittedForRoot) revert MessagesAlreadySubmitted();
        if (treeAddress != merkleTreeAddress) revert MerkleTreeMismatch();
        if (count > MAX_MESSAGE_ID_COUNT) revert TooManyMessages();
        if (block.timestamp > uint256(readTimestamp(state)) + maxStateAge) revert StateTooOld();

        verifier.verifyProof(stateMembershipVkey, publicValues, proof);

        for (uint256 i = 0; i < count; i++) {
            bytes32 id;
            uint256 offset = 72 + i * 32;
            assembly {
                id := calldataload(add(publicValues.offset, offset))
            }
            authorizedMessages[id] = true;
        }
        messagesSubmittedForRoot = true;
        emit MessagesAuthorized(stateRoot, count);
    }

    /// @inheritdoc IInterchainSecurityModule
    /// @dev Consume-once. All verification already happened; this is a set lookup, so the
    /// Mailbox pays a storage read rather than a proof verification per message.
    ///
    /// Only the Mailbox may call it, because the call is not a query - it deletes the
    /// authorisation, and a burned authorisation cannot be reissued. See docs/security.md.
    function verify(bytes calldata, bytes calldata message) external returns (bool) {
        if (msg.sender != mailbox) revert NotMailbox();
        bytes32 id = keccak256(message);
        if (!authorizedMessages[id]) return false;
        delete authorizedMessages[id];
        emit MessageConsumed(id);
        return true;
    }

    // ---------------------------------------------------------------------
    // x/zkism public-value decoding
    //
    // Little-endian length prefixes, because the Go module decodes these with Rust bincode's
    // default configuration. Everything inside our own state blob is big-endian.
    // ---------------------------------------------------------------------

    function decodeStateTransition(bytes calldata publicValues)
        public
        pure
        returns (bytes memory prevState, bytes memory newState)
    {
        if (publicValues.length < 16) revert MalformedPublicValues();
        uint256 stateLen = readUint64LE(publicValues, 0);
        if (stateLen < MIN_STATE_BYTES || stateLen > MAX_STATE_BYTES) revert InvalidStateLength();
        if (publicValues.length < 8 + stateLen + 8) revert MalformedPublicValues();

        uint256 newLen = readUint64LE(publicValues, 8 + stateLen);
        if (newLen < MIN_STATE_BYTES || newLen > MAX_STATE_BYTES) revert InvalidStateLength();
        if (publicValues.length < 16 + stateLen + newLen) revert MalformedPublicValues();

        prevState = publicValues[8:8 + stateLen];
        newState = publicValues[16 + stateLen:16 + stateLen + newLen];
    }

    function decodeStateMembershipHeader(bytes calldata publicValues)
        public
        pure
        returns (bytes32 stateRoot, bytes32 treeAddress, uint256 count)
    {
        if (publicValues.length < 72) revert MalformedPublicValues();
        stateRoot = bytes32(publicValues[0:32]);
        treeAddress = bytes32(publicValues[32:64]);
        count = readUint64LE(publicValues, 64);
        // Strict, matching the Go decoder: no trailing bytes, no truncation.
        if (publicValues.length != 72 + count * 32) revert MalformedPublicValues();
    }

    function readUint64LE(bytes calldata data, uint256 offset) internal pure returns (uint256 v) {
        for (uint256 i = 0; i < 8; i++) {
            v |= uint256(uint8(data[offset + i])) << (8 * i);
        }
    }

    // ---------------------------------------------------------------------
    // ISM state layout: root(32) | domain(4) | height(8) | timestamp(8) | ...
    // ---------------------------------------------------------------------

    function readStateRoot(bytes memory s) public pure returns (bytes32 root) {
        assembly {
            root := mload(add(s, 32))
        }
    }

    function readHeight(bytes memory s) public pure returns (uint64) {
        return readUint64BE(s, 36);
    }

    function readTimestamp(bytes memory s) public pure returns (uint64) {
        return readUint64BE(s, 44);
    }

    function readUint64BE(bytes memory s, uint256 offset) internal pure returns (uint64 v) {
        require(s.length >= offset + 8, "state too short");
        for (uint256 i = 0; i < 8; i++) {
            v = (v << 8) | uint64(uint8(s[offset + i]));
        }
    }

    function stateRoot() external view returns (bytes32) {
        return readStateRoot(state);
    }
}
