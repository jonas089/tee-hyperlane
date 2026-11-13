//! `teeism check-secrets`: refuse to let a credential reach a tracked file.
//!
//! The repository is public, and an Alchemy key was committed once already. A key in a public
//! repository is scraped within hours. Live values belong in devnet/.env or devnet/.state/,
//! both gitignored; a tracked file carries a placeholder instead.

use anyhow::{bail, Context, Result};
use regex::Regex;

use crate::shared::print;
use crate::shared::settings::Ctx;
use crate::shared::shell::Cmd;

pub fn check(ctx: &Ctx, staged: bool) -> Result<()> {
    let files = if staged {
        Cmd::new("listing staged files", "git")
            .arg("-C")
            .arg(&ctx.repo)
            .args(["diff", "--cached", "--name-only", "--diff-filter=ACM"])
            .output()?
    } else {
        Cmd::new("listing tracked files", "git")
            .arg("-C")
            .arg(&ctx.repo)
            .arg("ls-files")
            .output()?
    };
    // A provider host followed anywhere on the line by a long opaque path segment: matching
    // the segment rather than one provider's URL shape is what catches the next provider too.
    let provider = Regex::new(
        r#"(alchemy|infura|quicknode|blastapi|ankr|zan\.top|drpc|tenderly)\.[a-z.]+/[^" ]*[A-Za-z0-9_-]{24,}"#,
    )?;
    // OpenAI keys, which no provider host gives away: `sk-`, `sk-proj-`, `sk-svcacct-`.
    let openai = Regex::new(r"\bsk-(?:proj-|svcacct-|admin-)?[A-Za-z0-9_-]{32,}")?;
    let placeholder = Regex::new(r"ALCHEMY_KEY|INFURA_KEY|<[A-Za-z_]+>|YOUR_|KEY_HERE|\$\{|\{k\}|\{key\}")?;
    let hex64 = Regex::new(r"\b(0x)?[0-9a-fA-F]{64}\b")?;
    let key_words = Regex::new(r"(?i)priv|secret|mnemonic|private_key")?;
    let phrase = Regex::new(r"([a-z]+ ){11,}[a-z]+")?;
    let phrase_words = Regex::new(r"(?i)mnemonic|seed phrase")?;

    let mut found = Vec::new();
    for f in files.lines().filter(|f| !f.is_empty()) {
        let path = ctx.repo.join(f);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            let at = format!("{f}:{}", n + 1);
            if provider.is_match(line) && !placeholder.is_match(line) {
                found.push(format!("provider key  {at}"));
            }
            if openai.is_match(line) {
                found.push(format!("openai key    {at}"));
            }
            if key_words.is_match(line) {
                for m in hex64.find_iter(line) {
                    if m.as_str().trim_start_matches("0x").chars().any(|c| c != '0') {
                        found.push(format!("private key   {at}"));
                    }
                }
            }
            if phrase_words.is_match(line) && phrase.is_match(line) {
                found.push(format!("mnemonic      {at}"));
            }
        }
    }
    if found.is_empty() {
        print::ok("no credentials in tracked files");
        return Ok(());
    }
    for f in &found {
        print::bad(f);
    }
    bail!(
        "{} possible credential(s) in tracked files. Put the live value in devnet/.env and leave a placeholder; \
         if one was already pushed, rotate it: deleting it from the tip does not remove it from history",
        found.len()
    )
}

/// Run `check-secrets --staged` before every commit.
pub fn install_hook(ctx: &Ctx) -> Result<()> {
    let exe = std::env::current_exe().context("locating this binary")?;
    let hook = ctx.repo.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        format!("#!/bin/sh\nexec \"{}\" check-secrets --staged\n", exe.display()),
    )
    .with_context(|| format!("writing {}", hook.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))?;
    }
    print::ok(format!("installed {}", hook.display()));
    Ok(())
}
