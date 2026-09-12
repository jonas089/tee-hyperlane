//! What each subcommand actually does.
//!
//! Split from `main.rs` so that file stays a description of the command line and nothing
//! else. Each function here is one stage of the pipeline, and each is runnable on its own -
//! which is what makes a stuck route debuggable.

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info};

use crate::config::Config;
use crate::tasks::{cpu_prover_permit, run_route, ProofStore};

mod celestia;
pub(crate) mod ethereum;
mod ethereum_l2;

pub use celestia::{attest_celestia, bootstrap_celestia};
pub use ethereum::{attest_ethereum, bootstrap_ethereum};
pub use ethereum_l2::{attest_l2, bootstrap_l2, L2Kind};

use ethereum::{bootstrap_store, rebuild_ethereum_store};

/// Record the highest origin block the next proof could attest, beside that route's proofs.
///
/// The dashboard cannot work this out for itself. For Ethereum it is the finalized block and
/// for Base a single view call, but Arbitrum's confirmed block is only recoverable by finding
/// the `AssertionCreated` log for `latestConfirmed()` and then resolving the L2 block hash it
/// carries - three calls including a log search, which is too much for something polled every
/// few seconds and would hit the same range limits the attest path already works around. The
/// prover derives this number every tick anyway, so it writes it down instead.
///
/// Written before the "nothing to attest" check, because a route with nothing to do is
/// exactly when you want to see how far it *could* go.
pub(crate) fn record_attestable_head(out: Option<&str>, target: u64) {
    let Some(out) = out else { return };
    let Some(dir) = std::path::Path::new(out).parent().and_then(|p| p.parent()) else {
        return;
    };
    let body = serde_json::json!({ "target": target, "updated_at": now_secs() });
    if let Err(e) = std::fs::write(dir.join("head.json"), body.to_string()) {
        debug!(error = %e, "could not record the attestable head");
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Remember which checkpoint reproduces the store this route just committed to.
///
/// Searching for it afterwards cannot be made to work. The search walks finalized checkpoints
/// back from the head, so its window has to exceed the store's age - but a route that is
/// failing does not update, so its store ages without bound and outruns any window. Widening
/// the constant chases a target that moves away faster than the chase. The coprocessor
/// applied the finality update itself, so it already knows the answer: the store now commits
/// to that update's finalized header, and the checkpoint is that header's root.
pub(crate) fn record_checkpoint(out: Option<&str>, checkpoint: &str) {
    let Some(dir) = out
        .and_then(|o| std::path::Path::new(o).parent())
        .and_then(|p| p.parent())
    else {
        return;
    };
    if let Err(e) = std::fs::write(dir.join("checkpoint"), checkpoint) {
        debug!(error = %e, "could not record the checkpoint");
    }
}

/// What `record_checkpoint` last wrote for this route, if anything.
pub(crate) fn recorded_checkpoint(out: Option<&str>) -> Option<String> {
    let dir = out
        .and_then(|o| std::path::Path::new(o).parent())
        .and_then(|p| p.parent())?;
    let text = std::fs::read_to_string(dir.join("checkpoint")).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// How long a route may go without advancing before it proves anyway.
///
/// Routes only prove for their own destination, which is right - but it means a quiet route
/// never moves, and a route that has not moved has to scan further every time it looks. Four
/// days of that put every Celestia route beyond what its RPC would answer, and they stopped
/// dead. Proving twice a day regardless keeps the distance small enough to stay recoverable,
/// and costs one proof per route per twelve hours when nothing is happening.
const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(12 * 60 * 60);

/// Has this route gone long enough without advancing that it should prove regardless?
///
/// A heartbeat still needs a non-empty batch: the enclave refuses to attest nothing, and a
/// chain that has dispatched nothing at all is not falling behind in any sense that matters.
pub fn heartbeat_due(out: Option<&str>) -> bool {
    let Some(dir) = out
        .and_then(|o| std::path::Path::new(o).parent())
        .and_then(|p| p.parent())
    else {
        return false;
    };
    match std::fs::metadata(dir.join("advanced")).and_then(|m| m.modified()) {
        Ok(at) => at.elapsed().map(|d| d > HEARTBEAT).unwrap_or(false),
        // Never advanced under this binary: take the heartbeat rather than wait a further
        // twelve hours to discover the route is stuck.
        Err(_) => true,
    }
}

/// Note that this route just advanced, which is what the heartbeat measures from.
pub fn record_advanced(out: Option<&str>) {
    let Some(dir) = out
        .and_then(|o| std::path::Path::new(o).parent())
        .and_then(|p| p.parent())
    else {
        return;
    };
    let _ = std::fs::write(dir.join("advanced"), b"");
}

/// Remember how far a quiet route has checked, for the dashboard.
pub(crate) fn record_scanned(out: Option<&str>, height: u64) {
    record_attestable_head(out, height);
}

/// Is any of these messages addressed to `destination`?
///
/// The gate on whether to prove, and it has to look at the destination rather than just at
/// whether anything was dispatched. An origin has one mailbox and one merkle tree shared by
/// every outbound message, so all three Celestia-origin routes see every Celestia dispatch.
/// Counting messages rather than *our* messages meant one transfer to Sepolia started three
/// proofs of ninety minutes each, two of which existed only to reach the submission step and
/// skip the message as somebody else's.
///
/// This does not narrow the batch. A batch must still carry every leaf in its range or the
/// replay cannot reproduce the on-chain root, which is what stops a caller dropping messages
/// selectively. Only the decision to start is narrowed.
pub fn any_for_destination(messages: &[Vec<u8>], destination: u32) -> bool {
    messages.iter().any(|raw| {
        hyperlane_types::decode_hyperlane_message(raw)
            .map(|m| m.destination == destination)
            .unwrap_or(false)
    })
}

/// Wrap an EVM tree proof as the enclave's internally-tagged `TreeInput`, whose variant
/// fields sit alongside `kind`.
fn evm_tree_input(proof: &tee_node::hyperlane_state::EvmTreeProof) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(proof)?;
    value
        .as_object_mut()
        .context("tree proof must be an object")?
        .insert("kind".into(), serde_json::json!("evm"));
    Ok(value)
}

/// Produce the two Groth16 proofs the destination needs.
pub async fn prove(attestation_path: &str, elf_dir: &str, out: &str) -> Result<()> {
    prove_for_route("", attestation_path, elf_dir, out).await
}

/// Prove one attestation, naming the route in every line.
///
/// A proof is tens of minutes of silence otherwise, and with several routes sharing one
/// prover there is no way to tell from the log which one is working or how far it has got.
pub async fn prove_for_route(
    route: &str,
    attestation_path: &str,
    elf_dir: &str,
    out: &str,
) -> Result<()> {
    use crate::enclave::fetch_collateral;
    use sp1_sdk::{Prover, ProverClient, SP1Stdin};
    use std::time::Instant;

    let record: serde_json::Value = serde_json::from_slice(&std::fs::read(attestation_path)?)?;
    let att = &record["attestation"];
    let quote_hex = att["quote"].as_str().context("quote")?;

    let height = update_height(att);
    info!(route, height, "fetching Intel collateral");
    let collateral = fetch_collateral(quote_hex).await?;

    let payload = hex::decode(att["payload"].as_str().context("payload")?)?;
    let update = tee_attestation::decode_attested_update(&payload)?;

    // The prover's clock, which the circuit bounds to the attested head's timestamp. It may
    // not be freely chosen: too far back and a revoked TCB could be revived, too far forward
    // and the collateral has not been issued yet.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    info!(
        route,
        height = update.new_state.height,
        attested_at = update.attested_at,
        prover_clock = now,
        messages = update.message_ids.len(),
        "proving batch"
    );

    let inputs = tee_attestation::AttestationInputs {
        quote: hex::decode(quote_hex.trim_start_matches("0x"))?,
        event_log: att["event_log"]
            .as_str()
            .context("event log")?
            .as_bytes()
            .to_vec(),
        collateral: tee_attestation::AttestationInputs::encode_collateral(&collateral),
        now,
        payload: payload.clone(),
    };

    let mut stdin = SP1Stdin::new();
    stdin.write(&inputs);

    let client = ProverClient::builder().cpu().build();
    let mut proofs = serde_json::Map::new();
    let mut step = 0;
    for (name, elf_name) in [
        ("state_transition", "tee-state-transition"),
        ("state_membership", "tee-state-membership"),
    ] {
        let elf = std::fs::read(std::path::Path::new(elf_dir).join(elf_name))
            .with_context(|| format!("{elf_name}: run `circuit-tool build` in tee-circuit"))?;
        let (pk, vk) = client.setup(&elf);
        step += 1;
        info!(
            route,
            height = update.new_state.height,
            proof = name,
            step,
            of = 2,
            "generating groth16 proof, this takes tens of minutes on CPU"
        );
        let started = Instant::now();
        let proof = client.prove(&pk, &stdin).groth16().run()?;
        client.verify(&proof, &vk)?;
        info!(
            route,
            height = update.new_state.height,
            proof = name,
            seconds = started.elapsed().as_secs(),
            bytes = proof.bytes().len(),
            "proof done"
        );

        proofs.insert(
            name.to_string(),
            serde_json::json!({
                "proof": hex::encode(proof.bytes()),
                "public_values": hex::encode(proof.public_values.as_slice()),
            }),
        );
    }

    let mut record = record.clone();
    record["proofs"] = serde_json::Value::Object(proofs);

    // The same file is what the UI reads, so record the batch in the shape it expects:
    // which messages were attested together, and the measurements of the enclave that did
    // it. Everything here is public.
    let measurements = enclave_measurements(att)?;
    record["height"] = update.new_state.height.into();
    record["state_root"] = format!("0x{}", hex::encode(update.new_state.state_root)).into();
    record["quote"] = quote_hex.into();
    record["measurements"] = serde_json::to_value(measurements)?;
    record["batch"] = update
        .message_ids
        .iter()
        .map(|id| format!("0x{}", hex::encode(id)))
        .collect::<Vec<_>>()
        .into();

    std::fs::write(out, serde_json::to_vec_pretty(&record)?)?;
    info!(
        route,
        height = update.new_state.height,
        "batch proved and written"
    );
    Ok(())
}

/// The origin height an attestation record advances to, for logging before it is decoded.
fn update_height(attestation: &serde_json::Value) -> u64 {
    attestation["new_state"]
        .as_str()
        .and_then(|s| hex::decode(s).ok())
        .and_then(|raw| tee_attestation::decode_ism_state(&raw).ok())
        .map(|state| state.height)
        .unwrap_or_default()
}

/// Pull the measurements out of the quote and event log, for display.
pub fn enclave_measurements(attestation: &serde_json::Value) -> Result<crate::api::Measurements> {
    let quote_bytes = hex::decode(
        attestation["quote"]
            .as_str()
            .context("quote")?
            .trim_start_matches("0x"),
    )?;
    let quote = dcap_qvl::quote::Quote::parse(&quote_bytes)
        .map_err(|e| anyhow::anyhow!("quote does not parse: {e:?}"))?;
    let td = quote.report.as_td10().context("not a TDX quote")?;

    let events: Vec<tee_attestation::EventLog> =
        serde_json::from_str(attestation["event_log"].as_str().context("event log")?)?;
    let read = |name: &str| {
        tee_attestation::get_event_value(&events, name)
            .map(hex::encode)
            .unwrap_or_default()
    };

    Ok(crate::api::Measurements {
        mr_td: hex::encode(td.mr_td),
        os_image_hash: read("os-image-hash"),
        compose_hash: read("compose-hash"),
    })
}

pub async fn run(config: Config) -> Result<()> {
    let tick = Duration::from_secs(config.tick_secs);
    let cpu = cpu_prover_permit();
    let store = std::sync::Arc::new(ProofStore::new(expand_home(&config.proof_dir)));

    let mut routes = Vec::new();
    for route in config.routes {
        info!(
            route = %route.name,
            origin = route.origin.domain(),
            destination = route.destination.domain(),
            "configured"
        );
        routes.push(tokio::spawn(run_route(
            route,
            store.clone(),
            cpu.clone(),
            tick,
        )));
    }
    info!(
        tick_secs = config.tick_secs,
        routes = routes.len(),
        "coprocessor running"
    );

    // A route runs until the process stops. If one panics, take the service down so systemd
    // restarts it, rather than leaving a direction silently dead.
    for handle in routes {
        handle.await?;
    }
    Ok(())
}

pub fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}
