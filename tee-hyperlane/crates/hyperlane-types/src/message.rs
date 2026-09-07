//! Hyperlane's wire message and the id every ISM ultimately authorises.

use crate::keccak256;

pub const MESSAGE_HEADER_BYTES: usize = 77;

const OFF_VERSION: usize = 0;
const OFF_NONCE: usize = 1;
const OFF_ORIGIN: usize = 5;
const OFF_SENDER: usize = 9;
const OFF_DESTINATION: usize = 41;
const OFF_RECIPIENT: usize = 45;
const OFF_BODY: usize = 77;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyperlaneMessage {
    pub version: u8,
    pub nonce: u32,
    pub origin: u32,
    pub sender: [u8; 32],
    pub destination: u32,
    pub recipient: [u8; 32],
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("hyperlane message is shorter than its {MESSAGE_HEADER_BYTES}-byte header")]
pub struct MessageTooShort;

pub fn encode_hyperlane_message(m: &HyperlaneMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(OFF_BODY + m.body.len());
    out.push(m.version);
    out.extend_from_slice(&m.nonce.to_be_bytes());
    out.extend_from_slice(&m.origin.to_be_bytes());
    out.extend_from_slice(&m.sender);
    out.extend_from_slice(&m.destination.to_be_bytes());
    out.extend_from_slice(&m.recipient);
    out.extend_from_slice(&m.body);
    out
}

pub fn decode_hyperlane_message(b: &[u8]) -> Result<HyperlaneMessage, MessageTooShort> {
    if b.len() < MESSAGE_HEADER_BYTES {
        return Err(MessageTooShort);
    }
    let be32 = |o: usize| u32::from_be_bytes(b[o..o + 4].try_into().unwrap());
    Ok(HyperlaneMessage {
        version: b[OFF_VERSION],
        nonce: be32(OFF_NONCE),
        origin: be32(OFF_ORIGIN),
        sender: b[OFF_SENDER..OFF_SENDER + 32].try_into().unwrap(),
        destination: be32(OFF_DESTINATION),
        recipient: b[OFF_RECIPIENT..OFF_RECIPIENT + 32].try_into().unwrap(),
        body: b[OFF_BODY..].to_vec(),
    })
}

pub fn get_message_id(m: &HyperlaneMessage) -> [u8; 32] {
    keccak256(&encode_hyperlane_message(m))
}

/// The warp-route body: who receives the tokens and how many.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenMessage {
    pub recipient: [u8; 32],
    /// Big-endian uint256, kept raw so no precision is lost.
    pub amount: [u8; 32],
    pub metadata: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("token message body is shorter than 64 bytes")]
pub struct TokenMessageTooShort;

pub fn decode_token_message_body(b: &[u8]) -> Result<TokenMessage, TokenMessageTooShort> {
    if b.len() < 64 {
        return Err(TokenMessageTooShort);
    }
    Ok(TokenMessage {
        recipient: b[..32].try_into().unwrap(),
        amount: b[32..64].try_into().unwrap(),
        metadata: b[64..].to_vec(),
    })
}

pub fn encode_token_message_body(t: &TokenMessage) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + t.metadata.len());
    out.extend_from_slice(&t.recipient);
    out.extend_from_slice(&t.amount);
    out.extend_from_slice(&t.metadata);
    out
}
