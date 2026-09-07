use hyperlane_types::*;

#[test]
fn message_round_trips() {
    let m = HyperlaneMessage {
        version: 3,
        nonce: 42,
        origin: 11155111,
        sender: [1u8; 32],
        destination: 1297040200,
        recipient: [2u8; 32],
        body: vec![9u8; 96],
    };
    let b = encode_hyperlane_message(&m);
    assert_eq!(b.len(), 77 + 96);
    assert_eq!(decode_hyperlane_message(&b).unwrap(), m);
}

#[test]
fn header_fields_sit_at_the_hyperlane_offsets() {
    let m = HyperlaneMessage {
        version: 3,
        nonce: 0x01020304,
        origin: 0x0a0b0c0d,
        sender: [0xaa; 32],
        destination: 0x11223344,
        recipient: [0xbb; 32],
        body: vec![],
    };
    let b = encode_hyperlane_message(&m);
    assert_eq!(b[0], 3);
    assert_eq!(&b[1..5], &[1, 2, 3, 4]);
    assert_eq!(&b[5..9], &[0x0a, 0x0b, 0x0c, 0x0d]);
    assert_eq!(&b[9..41], &[0xaa; 32]);
    assert_eq!(&b[41..45], &[0x11, 0x22, 0x33, 0x44]);
    assert_eq!(&b[45..77], &[0xbb; 32]);
}

#[test]
fn a_short_message_is_rejected() {
    assert!(decode_hyperlane_message(&[0u8; 76]).is_err());
    assert!(decode_hyperlane_message(&[0u8; 77]).is_ok());
}

#[test]
fn token_message_round_trips() {
    let t = TokenMessage {
        recipient: [7u8; 32],
        amount: {
            let mut a = [0u8; 32];
            a[31] = 100;
            a
        },
        metadata: vec![1, 2, 3],
    };
    let b = encode_token_message_body(&t);
    assert_eq!(decode_token_message_body(&b).unwrap(), t);
}

#[test]
fn a_short_token_body_is_rejected() {
    assert!(decode_token_message_body(&[0u8; 63]).is_err());
}

/// Cross-check against a live Hyperlane dispatch on Sepolia.
///
/// `message_id` in the fixture is the value the deployed Mailbox itself emitted in its
/// `DispatchId` event, so this asserts our encoding and keccak agree with production
/// Solidity rather than with our own reimplementation.
mod live_sepolia {
    use hyperlane_types::*;

    #[derive(serde::Deserialize)]
    struct Fixture {
        message: String,
        message_id: String,
        tx: String,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!("../testdata/sepolia_dispatch.json")).unwrap()
    }

    fn unhex(s: &str) -> Vec<u8> {
        hex::decode(s.trim_start_matches("0x")).unwrap()
    }

    #[test]
    fn our_message_id_matches_the_mailboxes_own_dispatch_id() {
        let f = fixture();
        let m = decode_hyperlane_message(&unhex(&f.message)).unwrap();
        assert_eq!(
            hex::encode(get_message_id(&m)),
            f.message_id.trim_start_matches("0x"),
            "message id disagrees with the DispatchId emitted by tx {}",
            f.tx
        );
    }

    #[test]
    fn a_live_message_round_trips_exactly() {
        let raw = unhex(&fixture().message);
        let m = decode_hyperlane_message(&raw).unwrap();
        assert_eq!(encode_hyperlane_message(&m), raw);
    }

    #[test]
    fn a_live_warp_body_decodes_as_a_token_message() {
        let f = fixture();
        let m = decode_hyperlane_message(&unhex(&f.message)).unwrap();
        assert_eq!(m.version, 3);
        assert_eq!(m.origin, 11155111, "sepolia");
        assert_eq!(m.destination, 84532, "base sepolia");

        let t = decode_token_message_body(&m.body).unwrap();
        // 3 * 10^18, as sent.
        let mut want = [0u8; 32];
        want[16..].copy_from_slice(&3_000_000_000_000_000_000u128.to_be_bytes());
        assert_eq!(t.amount, want);
        assert!(t.metadata.is_empty());
        assert_eq!(encode_token_message_body(&t), m.body);
    }
}
