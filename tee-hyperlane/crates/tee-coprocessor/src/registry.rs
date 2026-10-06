//! Routers of tokens launched through the token factories. Every route and the tracker accept
//! these alongside the routers the config lists. The trade desk adds a token's routers once it
//! has verified its wiring on the hub; the set is kept on disk so a restart does not forget it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, RwLock};

type Routers = BTreeMap<u32, BTreeSet<[u8; 32]>>;

fn routers() -> &'static RwLock<Routers> {
    static ROUTERS: OnceLock<RwLock<Routers>> = OnceLock::new();
    ROUTERS.get_or_init(Default::default)
}

fn file() -> &'static Mutex<Option<PathBuf>> {
    static FILE: Mutex<Option<PathBuf>> = Mutex::new(None);
    &FILE
}

/// Load what an earlier run recorded, and keep `path` up to date from now on.
pub fn load(path: PathBuf) {
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(saved) = serde_json::from_str::<BTreeMap<u32, Vec<String>>>(&text) {
            let mut all = routers().write().expect("registry");
            for (domain, list) in saved {
                for r in list {
                    if let Some(bytes) = to_32(&r) {
                        all.entry(domain).or_default().insert(bytes);
                    }
                }
            }
        }
    }
    *file().lock().expect("registry file") = Some(path);
}

/// Accept `router` as a recipient on `domain`. True if it was new.
pub fn add(domain: u32, router: [u8; 32]) -> bool {
    let added = routers()
        .write()
        .expect("registry")
        .entry(domain)
        .or_default()
        .insert(router);
    if added {
        save();
    }
    added
}

pub fn on(domain: u32) -> Vec<[u8; 32]> {
    routers()
        .read()
        .expect("registry")
        .get(&domain)
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default()
}

pub fn accepts(domain: u32, recipient: &[u8; 32]) -> bool {
    routers()
        .read()
        .expect("registry")
        .get(&domain)
        .is_some_and(|s| s.contains(recipient))
}

fn save() {
    let Some(path) = file().lock().expect("registry file").clone() else {
        return;
    };
    let saved: BTreeMap<u32, Vec<String>> = routers()
        .read()
        .expect("registry")
        .iter()
        .map(|(d, s)| {
            (
                *d,
                s.iter().map(|r| format!("0x{}", hex::encode(r))).collect(),
            )
        })
        .collect();
    if let Ok(text) = serde_json::to_string_pretty(&saved) {
        let _ = std::fs::write(path, text);
    }
}

/// A 20- or 32-byte hex address as Hyperlane's 32 bytes.
pub fn to_32(text: &str) -> Option<[u8; 32]> {
    let raw = hex::decode(text.trim_start_matches("0x")).ok()?;
    if raw.len() != 20 && raw.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out[32 - raw.len()..].copy_from_slice(&raw);
    Some(out)
}
