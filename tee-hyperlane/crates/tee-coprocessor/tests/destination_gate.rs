//! A route proves for its own destination, not for whatever the origin happened to send.
//!
//! An origin has one mailbox and one merkle tree shared by every outbound message, so all
//! three Celestia-origin routes see every Celestia dispatch. Gating on "was anything
//! dispatched" rather than "was anything dispatched to me" turned one transfer to Sepolia
//! into three ninety-minute proofs, two of which reached the submission step only to skip the
//! message as another chain's.

use hyperlane_types::{encode_hyperlane_message, HyperlaneMessage};
use tee_coprocessor::commands::any_for_destination;

const CELESTIA: u32 = 1297040200;
const SEPOLIA: u32 = 11155111;
const BASE: u32 = 84532;

fn message_to(destination: u32) -> Vec<u8> {
    encode_hyperlane_message(&HyperlaneMessage {
        version: 3,
        nonce: 7,
        origin: CELESTIA,
        sender: [1u8; 32],
        destination,
        recipient: [2u8; 32],
        body: vec![9u8; 64],
    })
}

#[test]
fn a_batch_for_another_chain_is_not_worth_proving() {
    let batch = vec![message_to(SEPOLIA)];
    assert!(
        any_for_destination(&batch, SEPOLIA),
        "the route it is addressed to must prove"
    );
    assert!(
        !any_for_destination(&batch, BASE),
        "the routes it is not addressed to must not"
    );
}

#[test]
fn one_message_for_us_among_many_is_enough() {
    // The batch still has to carry every leaf in the range, so a mixed batch is the norm
    // rather than the exception. One of ours is the whole question.
    let batch = vec![message_to(BASE), message_to(SEPOLIA), message_to(BASE)];
    assert!(any_for_destination(&batch, SEPOLIA));
    assert!(any_for_destination(&batch, BASE));
    assert!(!any_for_destination(&batch, 421614));
}

#[test]
fn an_empty_or_unparsable_batch_is_not_worth_proving() {
    assert!(!any_for_destination(&[], SEPOLIA));
    assert!(
        !any_for_destination(&[vec![0xde, 0xad]], SEPOLIA),
        "a short message is not ours"
    );
}
