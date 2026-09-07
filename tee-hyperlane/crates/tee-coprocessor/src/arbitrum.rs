//! Gathering the proof that Ethereum has confirmed an Arbitrum state root.
//!
//! Arbitrum Sepolia runs BoLD, so L1 stores the *hash* of the confirmed assertion and nothing
//! about the L2 block it commits to. Three things therefore have to be assembled: the storage
//! proofs that say which assertion L1 confirmed and that it is `Confirmed` rather than
//! pending, the assertion's preimage, and the L2 header whose hash that preimage names. The
//! enclave rechecks all three, so everything here is untrusted input.
//!
//! Note which rollup this reads. The addresses in circulation point at the pre-BoLD contract,
//! which still answers `latestConfirmed()` with a node number and has confirmed nothing in
//! weeks. The live one is `inbox.bridge().rollup()`.

use alloy_primitives::{keccak256, Address, B256, U256};
use anyhow::{Context, Result};
use tee_node::origins::ethereum_l2::{
    get_assertion_node_slot, AssertionState, ArbitrumRootProof, RollupLayout,
};

use crate::ethereum::ExecutionReader;

/// `AssertionCreated(bytes32 indexed assertionHash, bytes32 indexed parentAssertionHash, ...)`.
const ASSERTION_CREATED_TOPIC: &str =
    "0x901c3aee23cf4478825462caaab375c606ab83516060388344f0650340753630";

/// Where `afterState` sits in the event's data, in 32-byte words. Its six words are the two
/// global-state hashes, the two u64s, the machine status and the end history root.
const AFTER_STATE_WORD: usize = 13;
const INBOX_ACC_WORD: usize = 19;

/// How far back to search L1 for the assertion's creation. Arbitrum Sepolia creates one every
/// few minutes, so a confirmed one is never far back.
const ASSERTION_SEARCH_BLOCKS: u64 = 5_000;

/// Assemble everything the enclave needs to derive Arbitrum's state root from L1.
pub async fn get_arbitrum_root_proof(
    l1: &ExecutionReader,
    l2: &ExecutionReader,
    rollup: Address,
    layout: RollupLayout,
    l1_block: u64,
) -> Result<ArbitrumRootProof> {
    // Which assertion is confirmed is read from L1, never chosen here.
    let latest_slot = B256::from(U256::from(layout.latest_confirmed_slot));
    let confirmed: B256 = l1
        .call(
            "eth_getStorageAt",
            serde_json::json!([rollup, latest_slot, format!("0x{l1_block:x}")]),
        )
        .await?
        .as_str()
        .context("eth_getStorageAt returned no value")?
        .parse()?;

    let node_slot = get_assertion_node_slot(confirmed, &layout);
    let proof = l1
        .call(
            "eth_getProof",
            serde_json::json!([rollup, [latest_slot, node_slot], format!("0x{l1_block:x}")]),
        )
        .await
        .context("eth_getProof on the rollup; the L1 block may be outside the proof window")?;

    let slots = proof["storageProof"].as_array().context("storageProof")?;
    anyhow::ensure!(slots.len() == 2, "expected two storage proofs");

    let assertion = read_assertion_created(l1, rollup, confirmed).await?;
    let l2_header_rlp = get_l2_header_rlp(l2, assertion.after_state.l2_block_hash).await?;

    Ok(ArbitrumRootProof {
        rollup,
        layout,
        account: serde_json::from_value(serde_json::json!({
            "nonce": proof["nonce"],
            "balance": proof["balance"],
            "storage_root": proof["storageHash"],
            "code_hash": proof["codeHash"],
        }))?,
        account_proof: serde_json::from_value(proof["accountProof"].clone())?,
        latest_confirmed_proof: serde_json::from_value(slots[0]["proof"].clone())?,
        assertion_node_proof: serde_json::from_value(slots[1]["proof"].clone())?,
        assertion_node_slot_value: slots[1]["value"].as_str().context("slot value")?.parse()?,
        prev_assertion_hash: assertion.prev_assertion_hash,
        after_state: assertion.after_state,
        inbox_accumulator: assertion.inbox_accumulator,
        l2_header_rlp: l2_header_rlp.into(),
    })
}

struct Assertion {
    prev_assertion_hash: B256,
    after_state: AssertionState,
    inbox_accumulator: B256,
}

/// The assertion's preimage, from the event that created it.
///
/// Events prove nothing on their own - an event is not in the state trie - but this is only a
/// way to *find* the preimage. The enclave rehashes it and checks the result against the
/// assertion hash L1 actually stores, so a wrong or invented event cannot pass.
async fn read_assertion_created(
    l1: &ExecutionReader,
    rollup: Address,
    assertion_hash: B256,
) -> Result<Assertion> {
    let head: u64 = u64::from_str_radix(
        l1.call("eth_blockNumber", serde_json::json!([]))
            .await?
            .as_str()
            .context("eth_blockNumber")?
            .trim_start_matches("0x"),
        16,
    )?;
    let from = head.saturating_sub(ASSERTION_SEARCH_BLOCKS);

    let logs = l1
        .call(
            "eth_getLogs",
            serde_json::json!([{
                "address": rollup,
                "topics": [ASSERTION_CREATED_TOPIC, assertion_hash],
                "fromBlock": format!("0x{from:x}"),
                "toBlock": "latest",
            }]),
        )
        .await?;
    let log = logs
        .as_array()
        .and_then(|logs| logs.first())
        .with_context(|| format!("no AssertionCreated for {assertion_hash} in the last {ASSERTION_SEARCH_BLOCKS} L1 blocks"))?;

    let topics = log["topics"].as_array().context("topics")?;
    let prev_assertion_hash: B256 = topics
        .get(2)
        .and_then(|t| t.as_str())
        .context("no parent assertion in topics")?
        .parse()?;

    let data = log["data"].as_str().context("log data")?.trim_start_matches("0x");
    let word = |index: usize| -> Result<B256> {
        let start = index * 64;
        let text = data
            .get(start..start + 64)
            .with_context(|| format!("AssertionCreated data has no word {index}"))?;
        Ok(format!("0x{text}").parse()?)
    };
    let as_u64 = |value: B256| U256::from_be_bytes(value.0).to::<u64>();

    Ok(Assertion {
        prev_assertion_hash,
        after_state: AssertionState {
            l2_block_hash: word(AFTER_STATE_WORD)?,
            send_root: word(AFTER_STATE_WORD + 1)?,
            inbox_position: as_u64(word(AFTER_STATE_WORD + 2)?),
            position_in_message: as_u64(word(AFTER_STATE_WORD + 3)?),
            machine_status: as_u64(word(AFTER_STATE_WORD + 4)?) as u8,
            end_history_root: word(AFTER_STATE_WORD + 5)?,
        },
        inbox_accumulator: word(INBOX_ACC_WORD)?,
    })
}

/// Re-encode an L2 block header as RLP.
///
/// `debug_getRawHeader` would hand this over directly, but it is not on every provider's
/// plan. Re-encoding is safe because it is self-checking: the result has to hash to the block
/// hash the assertion committed to, and this checks that here rather than leaving the enclave
/// to reject it later.
async fn get_l2_header_rlp(l2: &ExecutionReader, block_hash: B256) -> Result<Vec<u8>> {
    let block = l2
        .call("eth_getBlockByHash", serde_json::json!([block_hash, false]))
        .await?;
    anyhow::ensure!(!block.is_null(), "L2 has no block {block_hash}");

    let raw = |name: &str| -> Result<Vec<u8>> {
        let text = block[name].as_str().with_context(|| format!("header has no {name}"))?;
        Ok(hex::decode(text.trim_start_matches("0x"))?)
    };
    // Quantities are RLP-encoded minimally, so leading zero bytes have to go.
    let quantity = |name: &str| -> Result<Vec<u8>> {
        let text = block[name].as_str().with_context(|| format!("header has no {name}"))?;
        let value = u128::from_str_radix(text.trim_start_matches("0x"), 16)?;
        Ok(if value == 0 {
            Vec::new()
        } else {
            value.to_be_bytes().iter().copied().skip_while(|b| *b == 0).collect()
        })
    };

    let fields: Vec<Vec<u8>> = vec![
        raw("parentHash")?,
        raw("sha3Uncles")?,
        raw("miner")?,
        raw("stateRoot")?,
        raw("transactionsRoot")?,
        raw("receiptsRoot")?,
        raw("logsBloom")?,
        quantity("difficulty")?,
        quantity("number")?,
        quantity("gasLimit")?,
        quantity("gasUsed")?,
        quantity("timestamp")?,
        raw("extraData")?,
        raw("mixHash")?,
        raw("nonce")?,
        quantity("baseFeePerGas")?,
    ];

    let mut payload = Vec::new();
    for field in &fields {
        encode_rlp_bytes(field, &mut payload);
    }
    let mut out = Vec::new();
    encode_rlp_length(payload.len(), 0xc0, &mut out);
    out.extend_from_slice(&payload);

    let got = keccak256(&out);
    anyhow::ensure!(
        got == block_hash,
        "re-encoded header hashes to {got}, not {block_hash}; the header layout has changed"
    );
    Ok(out)
}

fn encode_rlp_bytes(value: &[u8], out: &mut Vec<u8>) {
    if value.len() == 1 && value[0] < 0x80 {
        out.push(value[0]);
        return;
    }
    encode_rlp_length(value.len(), 0x80, out);
    out.extend_from_slice(value);
}

fn encode_rlp_length(length: usize, offset: u8, out: &mut Vec<u8>) {
    if length < 56 {
        out.push(offset + length as u8);
        return;
    }
    let be: Vec<u8> = length.to_be_bytes().iter().copied().skip_while(|b| *b == 0).collect();
    out.push(offset + 55 + be.len() as u8);
    out.extend_from_slice(&be);
}
