//! `tee-hyperlane`: run the bridge, send a transfer, or check where a message got to.
//!
//! One binary rather than several, because these are three views of the same state and
//! sharing the config file is the point.


use anyhow::Result;
use clap::{Parser, Subcommand};
use tee_coprocessor::commands;
use tee_coprocessor::config::Config;

#[derive(Parser)]
#[command(name = "tee-hyperlane", about = "TEE-attested Hyperlane bridge")]
struct Cli {
    /// Route configuration.
    #[arg(long, default_value = "coprocessor.toml", global = true)]
    config: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Drive every configured route: attest, prove, relay.
    Run,
    /// Send a warp transfer and print its Hyperlane message id.
    Send {
        /// Route name from the config.
        #[arg(long)]
        route: String,
        /// Token to move, e.g. "TIA" or "USDC".
        #[arg(long)]
        token: String,
        /// Amount in the token's smallest unit.
        #[arg(long)]
        amount: String,
        /// Destination address, in that chain's own format.
        #[arg(long)]
        to: String,
    },
    /// Produce the genesis ISM state for a Celestia-origin ISM.
    ///
    /// This is the trust anchor: whoever reads it can see exactly which Celestia header the
    /// bridge was started from, which is why it belongs on chain in the clear rather than
    /// buried in an enclave.
    BootstrapCelestia {
        #[arg(long, default_value = "https://rpc-mocha.pops.one")]
        rpc: String,
        /// How far behind the head to anchor. The app hash for height H lives in H+1, so
        /// this must be at least 1. Ignored when --height is given.
        #[arg(long, default_value_t = 8)]
        lag: u64,
        /// Anchor at an exact height instead. Useful when messages already sit in the
        /// origin tree and the ISM must start from before them.
        #[arg(long)]
        height: Option<u64>,
        /// Enclave identity digest from `circuit-tool vkeys`.
        #[arg(long)]
        identity_digest: String,
    },
    /// Attest one Ethereum -> Celestia step.
    ///
    /// Re-derives the light-client store from the same checkpoint the ISM was created with,
    /// walks it to the current finalized head, then proves the origin tree under that head's
    /// execution state root.
    AttestEthereum {
        #[arg(long, default_value = "https://ethereum-sepolia-beacon-api.publicnode.com")]
        beacon: String,
        #[arg(long, default_value = "https://ethereum-sepolia-rpc.publicnode.com")]
        execution: String,
        /// Serves reads at the ISM's trusted height, which public RPCs prune after ~128 blocks.
        #[arg(long)]
        archive: Option<String>,
        #[arg(long)]
        enclave: String,
        /// Override the checkpoint. Normally derived from the ISM's own state, so a route
        /// resumes from nothing but what is on chain.
        #[arg(long)]
        checkpoint: Option<String>,
        #[arg(long)]
        trusted_state: String,
        #[arg(long, default_value = "0x4917a9746A7B6E0A57159cCb7F5a6744247f2d0d")]
        merkle_tree_hook: String,
        #[arg(long, default_value = "0xfFAEF09B3cd11D9b20d1a19bECca54EEC2884766")]
        mailbox: String,
        #[arg(long, default_value_t = 103)]
        base_slot: u64,
        #[arg(long)]
        out: Option<String>,
    },
    /// Attest one Celestia -> EVM step: gather, ask the enclave, print the result.
    ///
    /// Everything gathered here is untrusted; the enclave re-verifies all of it, so a
    /// rejection names which check failed rather than producing a wrong root.
    AttestCelestia {
        #[arg(long, default_value = "https://rpc-mocha.pops.one")]
        rpc: String,
        /// Serves reads at the ISM's trusted height, which public RPCs prune.
        #[arg(long)]
        archive: Option<String>,
        /// The enclave that attests Celestia.
        #[arg(long)]
        enclave: String,
        /// The destination ISM's current state, hex (116 bytes).
        #[arg(long)]
        trusted_state: String,
        /// Origin merkle tree hook id, 32 bytes hex.
        #[arg(long)]
        merkle_tree_hook: String,
        /// How far behind the head to attest. The app hash for H lives in H+1.
        #[arg(long, default_value_t = 8)]
        lag: u64,
        /// Write the attestation here for the prover to pick up.
        #[arg(long)]
        out: Option<String>,
    },
    /// Produce the genesis ISM state for an Ethereum-origin ISM.
    ///
    /// Anchors to a weak-subjectivity checkpoint. Whoever creates the ISM picks it, and
    /// everyone can see which one they picked, because it is committed in the state.
    BootstrapEthereum {
        #[arg(long, default_value = "https://ethereum-sepolia-beacon-api.publicnode.com")]
        beacon: String,
        #[arg(long, default_value = "https://ethereum-sepolia-rpc.publicnode.com")]
        execution: String,
        /// Checkpoint block root. Defaults to the current finalized head.
        #[arg(long)]
        checkpoint: Option<String>,
        #[arg(long)]
        identity_digest: String,
    },
    /// Prove an attestation twice: once per x/zkism public-value shape.
    ///
    /// Both proofs verify the *same* quote. They differ only in what they commit, because
    /// the two destination handlers decode with two different decoders and no single blob
    /// satisfies both.
    Prove {
        /// Attestation written by `attest-celestia`.
        #[arg(long)]
        attestation: String,
        /// Directory holding the guest ELFs.
        #[arg(long, default_value = "../tee-circuit/elf")]
        elf_dir: String,
        #[arg(long)]
        out: String,
    },
    /// Serve attestations to the bridge UI.
    Serve {
        #[arg(long, default_value = "0.0.0.0:8081")]
        listen: String,
        #[arg(long, default_value = "~/.tee-hyperlane/proofs")]
        proof_dir: String,
    },
    /// Show each route's trusted state and how far behind the origin head it is.
    Status,
    /// Report where one message stands: dispatched, authorised, or delivered.
    Verify {
        #[arg(long)]
        message_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    // Bootstrapping happens before any routes exist, so it must not need a route file.
    let load = || Config::load(&cli.config);

    match cli.command {
        Command::Run => commands::run(load()?).await,
        Command::Send { route, token, amount, to } => {
            println!("send {amount} {token} via {route} to {to}");
            println!("requires a deployed warp route; see README `Deploy`");
            Ok(())
        }
        Command::BootstrapCelestia { rpc, lag, height, identity_digest } => {
            commands::bootstrap_celestia(&rpc, lag, height, &identity_digest).await
        }
        Command::AttestEthereum {
            beacon,
            execution,
            archive,
            enclave,
            checkpoint,
            trusted_state,
            merkle_tree_hook,
            mailbox,
            base_slot,
            out,
        } => {
            commands::attest_ethereum(
                &beacon,
                &execution,
                archive.as_deref(),
                &enclave,
                checkpoint.as_deref(),
                &trusted_state,
                &merkle_tree_hook,
                &mailbox,
                base_slot,
                out,
            )
            .await
        }
        Command::AttestCelestia {
            rpc,
            archive,
            enclave,
            trusted_state,
            merkle_tree_hook,
            lag,
            out,
        } => {
            commands::attest_celestia(
                &rpc,
                archive.as_deref(),
                &enclave,
                &trusted_state,
                &merkle_tree_hook,
                lag,
                out,
            )
            .await
        }
        Command::BootstrapEthereum { beacon, execution, checkpoint, identity_digest } => {
            commands::bootstrap_ethereum(&beacon, &execution, checkpoint, &identity_digest).await
        }
        Command::Prove { attestation, elf_dir, out } => {
            commands::prove(&attestation, &elf_dir, &out).await
        }
        Command::Serve { listen, proof_dir } => {
            let routes = load().map(|config| config.routes).unwrap_or_default();
            let api =
                tee_coprocessor::api::Api::new(commands::expand_home(&proof_dir), routes);
            tee_coprocessor::api::serve(api, &listen).await
        }
        Command::Status => {
            let config = load()?;
            for route in &config.routes {
                println!(
                    "{:24} origin {} -> destination {}  ism {}",
                    route.name,
                    route.origin.domain(),
                    route.destination.domain(),
                    route.ism_id
                );
            }
            Ok(())
        }
        Command::Verify { message_id } => {
            println!("message {message_id}: requires a deployed ISM; see README `Deploy`");
            Ok(())
        }
    }
}

