//! `teeism enclaves verify`: is the enclave running the code in this checkout?

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::quote::*;
use crate::shared::settings::Ctx;
use crate::shared::shell::Cmd;
use crate::shared::{chains, print};
use crate::steps::enclaves::images::compose_file;

pub struct Args {
    pub target: String,
    pub app_id: Option<String>,
    pub ism_chain: Option<String>,
    pub rebuild: bool,
}

/// `teeism enclaves verify`: is the enclave running the code in this checkout? Each link is
/// checked against the thing downstream of it, never against this repo's own claims:
///
///   this checkout's compose file -> the app_compose dstack measured -> compose_hash
///     -> mr_config_id inside the quote Intel signed -> what the ISM on chain accepts
///
/// `info` in the enclave's reply is unsigned and could say anything; the quote is signed, so
/// where they disagree the quote wins.
pub async fn run(ctx: &Ctx, a: Args) -> Result<()> {
    let d = ctx.deployment()?;
    let app_id = match a.app_id {
        Some(id) => id,
        None => d
            .enclaves
            .cvms
            .get(&a.target)
            .map(|c| c.app_id.clone())
            .with_context(|| format!("no recorded {} CVM; pass --app-id", a.target))?,
    };
    // A slot (`all`, `celestia`, `all#2`) names a CVM; its compose and chains are its target's.
    let target = crate::shared::record::slot_target(&a.target);
    let first = *chains_in(target).first().context("unknown target")?;
    let gateway = match d
        .enclaves
        .cvms
        .get(&a.target)
        .filter(|c| c.app_id == app_id)
        .and_then(|c| c.gateway.clone())
    {
        Some(g) => g,
        None => super::up::cvm_info(&app_id)?.gateway,
    };
    let url = chains::enclave_url(&app_id, &gateway, first)?;
    print::step(format!("enclave {url}"));
    let id = fetch_identity(&url).await?;
    let mut failed = Vec::new();

    print::step("1. compose_hash is the hash of what dstack measured");
    let ac = app_compose(&id)?;
    let hash = hex::encode(Sha256::digest(ac.as_bytes()));
    let claimed = id.raw["info"]["compose_hash"]
        .as_str()
        .unwrap_or("")
        .trim_start_matches("0x")
        .to_string();
    print::kv("sha256(app_compose)", &hash);
    if hash == claimed {
        print::ok("reproduced from the preimage the enclave supplied");
    } else {
        print::bad(format!("the enclave claims {claimed}"));
        failed.push("compose_hash");
    }

    print::step("2. that hash is inside the signed quote, not only in the unsigned info");
    let body = report_body(&id.quote)?;
    let cfg = &body[184..232];
    match mr_config_binds(cfg, &id, &hash) {
        Ok(what) => print::ok(format!("mr_config_id binds it: {what}")),
        Err(e) => {
            print::bad(format!("mr_config_id is {}: {e:#}", hex::encode(cfg)));
            failed.push("mr_config_id");
        }
    }

    print::step("3. the measured compose is this checkout's compose");
    let ours = std::fs::read_to_string(compose_file(ctx, target))?;
    let measured = measured_compose(&id)?;
    if measured == ours {
        print::ok(format!(
            "byte for byte identical to deploy/docker-compose.{target}.yml"
        ));
    } else {
        print::bad("the enclave measured a different compose file");
        failed.push("compose");
    }

    print::step("4. the images that compose pins");
    let mut service = String::new();
    let mut services = Vec::new();
    for line in measured.lines() {
        if line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':') {
            service = line.trim().trim_end_matches(':').to_string();
        } else if let Some(img) = line.trim().strip_prefix("image:") {
            services.push((service.clone(), img.trim().to_string()));
            print::info(format!("{service}: {}", img.trim()));
        }
    }

    print::step("5. what the chain will accept");
    print::kv("mr_td", mr_td(&id.quote)?);
    print::kv("measurements", measurements(&id.quote)?);
    if let Some(chain) = &a.ism_chain {
        let chain = chains::evm(chain)?;
        let ism = d
            .evm(chain.name)
            .and_then(|e| e.ism)
            .with_context(|| format!("no ISM recorded on {}", chain.label))?;
        let pinned = crate::steps::isms::pinned_measurements(&chain.rpc(ctx), ism).await?;
        if pinned.contains(&measurements(&id.quote)?) {
            print::ok(format!(
                "the ISM {ism} on {} pins this enclave ({} pinned in all)",
                chain.label,
                pinned.len()
            ));
        } else {
            print::bad(format!(
                "the ISM {ism} on {} pins {}: it would reject this enclave",
                chain.label,
                pinned.join(", ")
            ));
            failed.push("ism");
        }
    } else {
        print::info("skipped; pass --ism-chain <chain> to compare with an EVM ISM");
    }

    print::step("6. the images are what this source builds");
    if a.rebuild {
        for (chain, image) in &services {
            let digest = image.rsplit('@').next().unwrap_or("");
            Cmd::new(format!("building the {chain} image"), "nix")
                .args([
                    "build",
                    &format!(".#image-{chain}"),
                    "-o",
                    &format!("result-verify-{chain}"),
                ])
                .cwd(&ctx.repo)
                .run()?;
            let manifest = Cmd::new("reading the built image's manifest", "tar")
                .args(["-xOf"])
                .arg(ctx.repo.join(format!("result-verify-{chain}")))
                .arg("manifest.json")
                .output()?;
            let m: Value =
                serde_json::from_str(&manifest).context("the image's manifest.json is not JSON")?;
            let built = m[0]["Config"]
                .as_str()
                .unwrap_or("")
                .trim_end_matches(".json")
                .rsplit('/')
                .next()
                .unwrap_or("")
                .trim_start_matches("sha256:")
                .to_string();
            Cmd::new("pulling the pinned image", "docker")
                .args(["pull", "-q", &format!("{}@{digest}", chains::IMAGE_REPO)])
                .output()?;
            let pinned = Cmd::new("reading the pinned image's config digest", "docker")
                .args([
                    "image",
                    "inspect",
                    "--format",
                    "{{.Id}}",
                    &format!("{}@{digest}", chains::IMAGE_REPO),
                ])
                .output()?
                .trim()
                .trim_start_matches("sha256:")
                .to_string();
            if !built.is_empty() && built == pinned {
                print::ok(format!(
                    "{chain}: rebuilt from this source, and it is the image the compose pins"
                ));
            } else {
                print::bad(format!(
                    "{chain}: rebuilt {built}, but the pinned image is {pinned}"
                ));
                failed.push("image");
            }
        }
    } else {
        print::info("not rebuilt: the digests are the registry's word until you pass --rebuild");
        print::info("(--rebuild reproduces the image only on x86_64 Linux with Nix's sandbox on)");
    }

    if failed.is_empty() {
        print::ok("every checked link holds");
        Ok(())
    } else {
        bail!("these links failed: {}", failed.join(", "))
    }
}
