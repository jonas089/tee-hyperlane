//! `teeism build`: every binary and contract the other steps run, built from this checkout and
//! the celestia-app checkout beside it. Always rebuilt (each tool is incremental), so nothing
//! runs from a stale build.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::shared::print;
use crate::shared::settings::Ctx;
use crate::shared::shell::Cmd;

/// celestia-app is only ever built from this branch. `main` there is someone else's
/// production code and has none of x/teeism.
pub const CELESTIA_APP_BRANCH: &str = "jonas/tee-ism";

/// Fail unless the celestia-app checkout is on the one branch this bridge runs.
pub fn check_celestia_app(ctx: &Ctx) -> Result<std::path::PathBuf> {
    let dir = ctx.celestia_app_dir();
    if !dir.join("go.mod").is_file() {
        bail!(
            "no celestia-app checkout at {}. Clone it with \
             `git clone -b {CELESTIA_APP_BRANCH} https://github.com/celestiaorg/celestia-app.git {}`, \
             or set CELESTIA_APP_DIR",
            dir.display(),
            dir.display()
        );
    }
    let branch = Cmd::new("reading celestia-app's branch", "git")
        .arg("-C")
        .arg(&dir)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()?;
    if branch.trim() != CELESTIA_APP_BRANCH {
        bail!(
            "{} is on branch `{}`; this bridge runs only `{CELESTIA_APP_BRANCH}` (git -C {} checkout {CELESTIA_APP_BRANCH})",
            dir.display(),
            branch.trim(),
            dir.display()
        );
    }
    Ok(dir)
}

/// The commit the celestia-app checkout is at, full hash.
pub fn celestia_app_commit(app: &Path) -> Result<String> {
    Ok(Cmd::new("reading celestia-app's commit", "git")
        .arg("-C")
        .arg(app)
        .args(["rev-parse", "HEAD"])
        .output()?
        .trim()
        .to_string())
}

/// The same, for the eye: `<short commit> <date> <subject>`.
pub fn celestia_app_head(app: &Path) -> Result<String> {
    Ok(Cmd::new("reading celestia-app's head", "git")
        .arg("-C")
        .arg(app)
        .args(["log", "-1", "--format=%h %as %s"])
        .output()?
        .trim()
        .to_string())
}

pub fn build(ctx: &Ctx) -> Result<()> {
    let app = check_celestia_app(ctx)?;
    print::kv(
        "celestia-app",
        &format!("{CELESTIA_APP_BRANCH} @ {}", celestia_app_head(&app)?),
    );
    let bin = ctx.bin_dir();
    std::fs::create_dir_all(&bin)?;
    for (pkg, what) in [
        ("./cmd/celestia-appd", "the chain's node and CLI"),
        (
            "./x/teeism/cmd/teeism-collateral",
            "fetches Intel collateral for Celestia-destination attestations",
        ),
        (
            "./x/teeism/cmd/teeism-identity",
            "digests an enclave identity the way x/teeism does",
        ),
    ] {
        let name = pkg.rsplit('/').next().unwrap_or(pkg);
        print::step(format!("building {name} ({what})"));
        Cmd::new(format!("building {name} from {}", app.display()), "go")
            .args(["build", "-o"])
            .arg(bin.join(name))
            .arg(pkg)
            .cwd(&app)
            .run()?;
    }

    print::step("building the relayer (tee-hyperlane) and the gas oracle");
    Cmd::new("building tee-coprocessor and gas-oracle", "cargo")
        .args([
            "build",
            "--release",
            "--locked",
            "-p",
            "tee-coprocessor",
            "-p",
            "gas-oracle",
        ])
        .cwd(ctx.repo.join("tee-hyperlane"))
        .run()?;

    print::step("compiling TeeDcapIsm and the Hyperlane contracts it deploys beside");
    let contracts = ctx.contracts_dir();
    if !contracts.join("node_modules/@hyperlane-xyz/core").is_dir() {
        Cmd::new("installing the Hyperlane Solidity packages", "npm")
            .args(["ci", "--silent"])
            .cwd(&contracts)
            .run()?;
    }
    // Our ISM, and the Hyperlane contracts teeism deploys from out/: the warp routers, and
    // Eden's mailbox and merkle tree hook. Nothing in src/ imports those, so they are named
    // here, and `--lib-paths lib` stops forge treating node_modules as a library, which it
    // would otherwise only compile when imported. Tests are skipped: they need forge-std,
    // which nothing fetches.
    Cmd::new("compiling tee-hyperlane/contracts", "forge")
        .args([
            "build",
            "--skip",
            "test",
            "--lib-paths",
            "lib",
            "src",
            "node_modules/@hyperlane-xyz/core/contracts/token/HypERC20.sol",
            "node_modules/@hyperlane-xyz/core/contracts/Mailbox.sol",
            "node_modules/@hyperlane-xyz/core/contracts/hooks/MerkleTreeHook.sol",
        ])
        .cwd(&contracts)
        .run()?;

    print::step("compiling Automata's PCCS and DCAP verifier");
    automata(ctx)?;
    print::ok(format!(
        "built into {} and the projects' out/ directories",
        bin.display()
    ));
    Ok(())
}

/// Automata's on-chain PCCS, at the commit the live PCCS matches.
const AUTOMATA_PCCS: (&str, &str) = (
    "https://github.com/automata-network/automata-on-chain-pccs",
    "fd799ede07bcfde6d571e0b10680a9ca5364783c",
);

/// Automata's DCAP verifier. Every later upstream commit belongs to their DCAP V2 path, which
/// rewrites the shared bases and the v4 and v5 quote verifiers we have deployed: taking one is a
/// new PCCS deployment, not a refresh.
const AUTOMATA_DCAP: (&str, &str) = (
    "https://github.com/automata-network/automata-dcap-attestation",
    "41aedff9e95d6736f6feb8529d005b54b4fb3c42",
);

/// The one source file not taken from `AUTOMATA_PCCS`: the PCK helper as the live PCCS was
/// deployed, before upstream `f1406ef` added `parsePckExtensionWithIdentity`. The helper's
/// address goes into the constructors of the DAOs and the router, so the newer helper would make
/// `teeism pccs deploy` stand up a second PCCS instead of finding the live one. Only the DCAP V2
/// path, which we do not deploy, calls the new entry point. Drop this when the PCCS is
/// redeployed anyway.
const PCK_HELPER: (&str, &str) = (
    "src/helpers/PCKHelper.sol",
    "4105c6fcc941ba214d57aeacc5582a09777df1d5",
);

/// Fetch Automata's two repositories and their Solidity dependencies at pinned commits and
/// compile both projects. Nothing of theirs is committed here: the checkouts are gitignored, and
/// apart from `PCK_HELPER` and the two compiler settings passed to the DCAP build, what is
/// compiled is upstream at those commits.
///
/// Changing any pin changes initcode, so CREATE2 addresses: compare every artifact's `bytecode`
/// and `deployedBytecode` before and after, and expect ISMs (which hold the verifier's address
/// immutably) to need replacing if anything the deploy step creates moved.
pub fn automata(ctx: &Ctx) -> Result<()> {
    use crate::shared::shell::Cmd;
    let root = crate::steps::pccs::automata_dir(ctx);
    let pccs = crate::steps::pccs::pccs_dir(ctx);
    let dcap = crate::steps::pccs::dcap_dir(ctx);
    pin_dependency(&pccs, AUTOMATA_PCCS.0, AUTOMATA_PCCS.1)?;
    pin_dependency(&root.join("dcap"), AUTOMATA_DCAP.0, AUTOMATA_DCAP.1)?;
    Cmd::new("taking the PCK helper the live PCCS was deployed from", "git")
        .arg("-C")
        .arg(&pccs)
        .args(["checkout", "-q", PCK_HELPER.1, "--", PCK_HELPER.0])
        .output()?;
    let deps: [(&Path, &str, &str, &str); 6] = [
        (
            &pccs,
            "lib/forge-std",
            "https://github.com/foundry-rs/forge-std",
            "155d547c449afa8715f538d69454b83944117811",
        ),
        (
            &pccs,
            "lib/openzeppelin-contracts",
            "https://github.com/OpenZeppelin/openzeppelin-contracts",
            "442886ed5ff8a0b9ab477b191f5238541ee6d772",
        ),
        (
            &pccs,
            "lib/solady",
            "https://github.com/Vectorized/solady",
            "772a49d91b004eb55dc9129ae57dd36fbb8417ee",
        ),
        (
            &dcap,
            "lib/forge-std",
            "https://github.com/foundry-rs/forge-std",
            "3b20d60d14b343ee4f908cb8079495c07f5e8981",
        ),
        (
            &dcap,
            "lib/risc0-ethereum",
            "https://github.com/risc0/risc0-ethereum",
            "365e7b2db4f620fa256580c27558d2623362b9ae",
        ),
        (
            &dcap,
            "lib/sp1-contracts",
            "https://github.com/succinctlabs/sp1-contracts",
            "512b5e029abc27f6e46a3c7eba220dac83ecc306",
        ),
    ];
    for (project, dir, url, rev) in deps {
        pin_dependency(&project.join(dir), url, rev)?;
    }
    // risc0-ethereum carries the OpenZeppelin that Automata's remappings name.
    Cmd::new("fetching risc0-ethereum's OpenZeppelin", "git")
        .arg("-C")
        .arg(dcap.join("lib/risc0-ethereum"))
        .args([
            "submodule",
            "update",
            "-q",
            "--init",
            "lib/openzeppelin-contracts",
        ])
        .output()?;
    // Upstream has the PCCS as a submodule here; ours is the checkout above, so the DCAP
    // verifier is compiled against exactly the PCCS sources that are deployed.
    let link = dcap.join("lib/automata-on-chain-pccs");
    let linked = link
        .symlink_metadata()
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !linked {
        if link.exists() {
            std::fs::remove_dir_all(&link).with_context(|| format!("removing {}", link.display()))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink("../../../pccs", &link)
            .with_context(|| format!("linking {}", link.display()))?;
    }
    Cmd::new("compiling the Automata PCCS", "forge")
        .args(["build", "--skip", "test", "--skip", "script"])
        .cwd(&pccs)
        .run()?;
    // No metadata hash, as the PCCS's own foundry.toml already has it: the hash covers every
    // source in the import graph, so with it the bytecode, and the CREATE2 addresses, depend on
    // the build environment.
    Cmd::new("compiling the Automata DCAP verifier", "forge")
        .args(["build", "--skip", "test", "--skip", "script", "--remappings"])
        .arg("@openzeppelin/contracts/=lib/automata-on-chain-pccs/lib/openzeppelin-contracts/contracts/")
        .env("FOUNDRY_BYTECODE_HASH", "none")
        .env("FOUNDRY_CBOR_METADATA", "false")
        .cwd(&dcap)
        .run()?;
    Ok(())
}

/// Put a gitignored dependency checkout at exactly `rev`. The commit is fetched by its hash,
/// which works on a shallow or stale clone too. A checkout that still cannot reach it, or a
/// directory that is not a checkout at all (an uninitialised submodule, or build output left
/// where a vendored copy used to be), is deleted and cloned again: the directory only ever holds
/// a fetched copy.
fn pin_dependency(path: &Path, url: &str, rev: &str) -> Result<()> {
    let git = |what: &str| Cmd::new(what, "git").arg("-C").arg(path);
    if path.join(".git").exists() {
        let head = git("reading a dependency's revision")
            .args(["rev-parse", "HEAD"])
            .output()?;
        if head.trim() == rev {
            return Ok(());
        }
        let fetched = git(&format!("fetching {rev} of {url}"))
            .args(["fetch", "-q", "origin", rev])
            .output();
        let checked = fetched.and_then(|_| {
            git(&format!("checking out {rev} of {url}"))
                .args(["checkout", "-q", rev])
                .output()
        });
        if checked.is_ok() {
            return Ok(());
        }
        print::info(format!("{} cannot reach {rev}; cloning it again", path.display()));
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    } else if path.exists() {
        std::fs::remove_dir_all(path).with_context(|| format!("removing {}", path.display()))?;
    }
    print::info(format!("fetching {url} at {rev}"));
    Cmd::new(format!("cloning {url}"), "git")
        .args(["clone", "-q", url])
        .arg(path)
        .output()?;
    git(&format!("checking out {rev} of {url}"))
        .args(["checkout", "-q", rev])
        .output()?;
    Ok(())
}
