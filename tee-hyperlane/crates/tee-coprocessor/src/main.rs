//! Entry point: runs the relayer, or the `genesis` and `identity` setup commands.

use anyhow::Result;
use clap::{Parser, Subcommand};
use tee_coprocessor::config::Config;

#[derive(Parser)]
struct Cli {
    #[arg(long, default_value = "coprocessor.toml")]
    config: String,
    #[command(subcommand)]
    mode: Option<Mode>,
}

#[derive(Subcommand)]
enum Mode {
    /// Print the genesis state for a new ISM whose origin is `chain`.
    Genesis {
        #[arg(long)]
        chain: String,
        /// The identity digest of the enclave family that attests `chain`.
        #[arg(long)]
        identity: String,
        /// Anchor at this origin height instead of the current head, where the chain allows it.
        #[arg(long)]
        height: Option<u64>,
    },
    /// Write the identity a new ISM pins for the enclave at `url`, read from its quote.
    Identity {
        #[arg(long)]
        url: String,
        #[arg(long)]
        json: std::path::PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr: `genesis` prints the state on stdout, and the ISM scripts capture it.
    // Under systemd, journald stamps each line itself, so the time and colours are left out
    // there; `RUST_LOG=debug` brings back every step of every pass.
    use std::io::IsTerminal;
    let terminal = std::io::stderr().is_terminal();
    let mut filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    // Peer discovery for Base's p2p reports every misbehaving stranger on the network as a
    // warning. None of it is ours to act on, so only its errors get through unless RUST_LOG
    // names discv5 or libp2p itself, as `RUST_LOG=info,discv5=debug` does.
    if !std::env::var("RUST_LOG").is_ok_and(|v| v.contains("discv5") || v.contains("libp2p")) {
        for quiet in [
            "discv5=error",
            "libp2p=error",
            "libp2p_gossipsub=error",
            "libp2p_swarm=error",
        ] {
            filter = filter.add_directive(quiet.parse().expect("a static directive"));
        }
    }
    let builder = tracing_subscriber::fmt()
        .with_target(false)
        .with_ansi(terminal)
        .with_writer(std::io::stderr)
        .with_env_filter(filter);
    if terminal {
        builder.init();
    } else {
        builder.without_time().init();
    }
    tee_coprocessor::install_tls_provider();
    let cli = Cli::parse();
    if let Some(Mode::Identity { url, json }) = &cli.mode {
        let identity = tee_coprocessor::identity::Identity::fetch(url).await?;
        std::fs::write(json, serde_json::to_vec_pretty(&identity)?)?;
        eprintln!("compose hash {} from {url}", identity.compose_hash);
        return Ok(());
    }
    let config = Config::load(&cli.config)?;
    match cli.mode {
        None => tee_coprocessor::route::serve(config).await,
        Some(Mode::Identity { .. }) => unreachable!("handled above"),
        Some(Mode::Genesis {
            chain,
            identity,
            height,
        }) => {
            let identity: [u8; 32] = hex::decode(identity.trim_start_matches("0x"))?
                .try_into()
                .map_err(|_| anyhow::anyhow!("the identity digest must be 32 bytes"))?;
            let state = config.indexer(&chain)?.bootstrap(identity, height).await?;
            eprintln!(
                "origin {chain}, height {}, state root 0x{}",
                state.height,
                hex::encode(state.state_root)
            );
            println!("0x{}", hex::encode(state.encode()));
            Ok(())
        }
    }
}
