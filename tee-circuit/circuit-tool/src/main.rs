//! Build the guest ELFs, emit their verifying keys, and measure what proving actually costs.
//!
//! `vkeys` produces the three values an ISM is created with: the two program vkey
//! commitments and the SP1 Groth16 wrap key. `bench` answers "how long does a proof take on
//! this machine", using a real third-party TDX quote so the DCAP work is genuine.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use sp1_sdk::{HashableKey, Prover, ProverClient, SP1Stdin};
use tee_attestation::AttestationInputs;

const STATE_TRANSITION: &str = "tee-state-transition";
const STATE_MEMBERSHIP: &str = "tee-state-membership";
const BENCH: &str = "tee-bench-attestation";

#[derive(Parser)]
#[command(about = "tee-circuit build and measurement tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile the guest programs with SP1's own build flags.
    Build,
    /// Print the vkeys an ISM is created with.
    Vkeys,
    /// Turn a live enclave's /identity response into the pinned enclave-identity.toml.
    Identity {
        /// The enclave to read measurements from.
        #[arg(long)]
        url: String,
        /// Write the result rather than printing it.
        #[arg(long)]
        write: bool,
    },
    /// Measure cycle counts, and optionally produce a real Groth16 proof on CPU.
    Bench {
        /// Also prove, not just execute. Slow: this is the number that matters.
        #[arg(long)]
        prove: bool,
    },
}

fn elf(name: &str) -> Result<Vec<u8>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        root.join("../elf").join(name),
        root.join("../programs/target/riscv32im-succinct-zkvm-elf/release").join(name),
    ];
    for path in &candidates {
        if let Ok(bytes) = std::fs::read(path) {
            return Ok(bytes);
        }
    }
    anyhow::bail!("{name} not built; run `cargo run -p circuit-tool -- build`")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Build => build_programs(),
        Command::Vkeys => vkeys(),
        Command::Identity { url, write } => identity(&url, write),
        Command::Bench { prove } => bench(prove),
    }
}

/// SP1 v5's guest compiler flags, applied directly.
///
/// `sp1_build` 5.2.2 would normally supply these, but it also passes `--remap-path-scope`,
/// which the installed (v6-era) `succinct` toolchain rejects. These are exactly the flags
/// it emits, minus that one; getting `-Ttext` and `--image-base` right is what avoids SP1's
/// "detected old compiler flags" warning at proving time.
const GUEST_RUSTFLAGS: &[&str] = &[
    "-C passes=lower-atomic",
    "-C link-arg=-Ttext=0x00201000",
    "-C link-arg=--image-base=0x00200800",
    "-C panic=abort",
    "--cfg getrandom_backend=\"custom\"",
    "-C llvm-args=-misched-prera-direction=bottomup",
    "-C llvm-args=-misched-postra-direction=bottomup",
];

fn build_programs() -> Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../programs");
    let status = std::process::Command::new("cargo")
        .current_dir(&root)
        .env("RUSTFLAGS", GUEST_RUSTFLAGS.join(" "))
        .args([
            "+succinct",
            "build",
            "--release",
            "--target",
            "riscv32im-succinct-zkvm-elf",
        ])
        .status()
        .context("failed to run cargo for the guest programs")?;
    anyhow::ensure!(status.success(), "guest build failed");

    let out = PathBuf::from(elf_dir()?);
    for name in [STATE_TRANSITION, STATE_MEMBERSHIP, BENCH] {
        let built = root
            .join("target/riscv32im-succinct-zkvm-elf/release")
            .join(name);
        std::fs::copy(&built, out.join(name))
            .with_context(|| format!("copying {}", built.display()))?;
        println!("built {name}");
    }
    Ok(())
}

fn elf_dir() -> Result<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../elf");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.to_str().context("non-utf8 path")?.to_string())
}

fn vkeys() -> Result<()> {
    let client = ProverClient::builder().cpu().build();
    for name in [STATE_TRANSITION, STATE_MEMBERSHIP] {
        let (_, vk) = client.setup(&elf(name)?);
        println!("{name:24} vkey {}", vk.bytes32());
    }

    let wrap = groth16_wrap_vk()?;
    println!("{:24} {} bytes, sha256 {}", "groth16 wrap vk", wrap.len(), hex::encode(sha256(&wrap)));
    println!(
        "{:24} {}",
        "identity digest",
        hex::encode(tee_attestation::build_identity_digest())
    );
    println!(
        "{:24} {}",
        "identity pinned",
        tee_attestation::enclave_identity::REQUIRES_SPECIFIC_ENCLAVE
    );
    Ok(())
}

/// The SP1 v5 Groth16 verifying key, which is what `x/zkism` stores as `groth16_vkey`.
fn groth16_wrap_vk() -> Result<Vec<u8>> {
    let path = dirs_home()?.join(".sp1/circuits/groth16/v5.0.0/groth16_vk.bin");
    std::fs::read(&path).with_context(|| format!("missing {}", path.display()))
}

fn dirs_home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME unset")?))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(data).into()
}

/// A genuine third-party TDX quote, so the DCAP path is the real one.
fn sample_inputs() -> Result<AttestationInputs> {
    let root = sample_dir()?;
    let quote = std::fs::read(root.join("tdx_quote")).context("sample quote")?;
    let collateral: dcap_qvl::QuoteCollateralV3 =
        serde_json::from_slice(&std::fs::read(root.join("tdx_quote_collateral.json"))?)?;
    Ok(AttestationInputs {
        quote,
        event_log: b"[]".to_vec(),
        collateral: AttestationInputs::encode_collateral(&collateral),
        now: sample_now(),
        payload: Vec::new(),
    })
}

/// Inside the sample collateral's validity window (issued 2025-06-19, expires 2025-07-19).
/// DCAP checks certificate, CRL and TCB validity against this, so it is not free to choose.
fn sample_now() -> u64 {
    std::env::var("BENCH_NOW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1750852800)
}

fn sample_dir() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("DCAP_SAMPLE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let base = dirs_home()?.join(".cargo/registry/src");
    for entry in std::fs::read_dir(&base)? {
        let candidate = entry?.path().join("dcap-qvl-0.5.3/sample");
        if candidate.is_dir() {
            return Ok(candidate);
        }
    }
    anyhow::bail!("dcap-qvl sample directory not found; set DCAP_SAMPLE_DIR")
}

/// Read the measurements out of a running enclave.
///
/// This is the bootstrap step the ordering constraint forces: the circuit pins an enclave's
/// identity, but the identity only exists once an enclave is running. Everything pinned here
/// comes from a signed quote or from an event log the quote's RTMRs commit to, so a wrong
/// answer from the enclave is not silently usable - it just fails to verify later.
fn identity(url: &str, write: bool) -> Result<()> {
    let body: serde_json::Value = ureq_get(&format!("{}/identity", url.trim_end_matches('/')))?;
    let quote_hex = body["quote"].as_str().context("no quote in response")?;
    let quote_bytes = hex::decode(quote_hex.trim_start_matches("0x"))?;
    let quote = dcap_qvl::quote::Quote::parse(&quote_bytes)
        .map_err(|e| anyhow::anyhow!("quote does not parse: {e:?}"))?;

    let td = quote
        .report
        .as_td10()
        .context("not a TDX quote; this bridge does not run on SGX")?;

    let log_text = body["event_log"].as_str().context("no event log")?;
    let events: Vec<tee_attestation::EventLog> = serde_json::from_str(log_text)?;

    // Cross-check before trusting any of it: the log must reproduce the signed RTMRs.
    let replayed = tee_attestation::replay_event_logs(&events);
    let signed = [td.rt_mr0, td.rt_mr1, td.rt_mr2, td.rt_mr3];
    anyhow::ensure!(replayed == signed, "event log does not replay to the quote's RTMRs");

    let read = |name: &str| -> Result<String> {
        let value = tee_attestation::get_event_value(&events, name)
            .with_context(|| format!("event `{name}` missing or not self-consistent"))?;
        Ok(hex::encode(value))
    };

    let toml = format!(
        "# Captured from a live CVM at {url}\n\
         # Every value below is either signed into the quote or committed by its RTMRs.\n\n\
         require_enclave = true\n\n\
         # Platform\n\
         mr_td         = \"{mr_td}\"\n\
         os_image_hash = \"{os}\"\n\
         # Application\n\
         compose_hash  = \"{compose}\"\n\
         # Key management\n\
         mr_kms        = \"{kms}\"\n\
         key_provider  = \"{kp}\"\n",
        url = url,
        mr_td = hex::encode(td.mr_td),
        os = read("os-image-hash")?,
        compose = read("compose-hash")?,
        kms = read("mr-kms")?,
        kp = read("key-provider")?,
    );

    if write {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../crates/tee-attestation/enclave-identity.toml");
        std::fs::write(&path, &toml)?;
        println!("wrote {}", path.display());
        println!(
            "rebuild the circuits: cargo run -p circuit-tool -- build && \
             cargo run -p circuit-tool -- vkeys"
        );
    } else {
        print!("{toml}");
    }
    Ok(())
}

fn ureq_get(url: &str) -> Result<serde_json::Value> {
    let output = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "60", url])
        .output()
        .context("curl")?;
    anyhow::ensure!(output.status.success(), "GET {url} failed");
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn bench(prove: bool) -> Result<()> {
    sp1_sdk::utils::setup_logger();
    let inputs = sample_inputs()?;
    let mut stdin = SP1Stdin::new();
    stdin.write(&inputs);

    let client = ProverClient::builder().cpu().build();

    let (_, report) = client.execute(&elf(BENCH)?, &stdin).run()?;
    println!("cycles (DCAP verification + event log replay): {}", report.total_instruction_count());

    if prove {
        let (pk, vk) = client.setup(&elf(BENCH)?);
        let start = Instant::now();
        let proof = client.prove(&pk, &stdin).groth16().run()?;
        let elapsed = start.elapsed();
        client.verify(&proof, &vk)?;
        println!("groth16 proof on CPU:                      {:.1}s", elapsed.as_secs_f64());
        println!("proof bytes:                               {}", proof.bytes().len());
    }
    Ok(())
}
