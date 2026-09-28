//! The coprocessor service: every route in the config, and the dashboard API.
//!
//! ```text
//! tee-hyperlane --config coprocessor.toml
//! ```
//!
//! Two setup modes, both used by the deploy scripts before any route exists:
//!
//! ```text
//! tee-hyperlane --config coprocessor.toml genesis --chain sepolia --identity 0x... [--height N]
//! tee-hyperlane identity --url https://<app-id>-8080.<gateway> --json identity.json
//! ```

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
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
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
