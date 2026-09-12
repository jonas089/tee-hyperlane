//! A quiet route still proves twice a day.
//!
//! Routes only prove for their own destination, which is correct but leaves a quiet route
//! standing still - and a route that stands still has to scan further every time it looks.
//! Four days of that put every Celestia route past what its RPC would answer and they stopped
//! dead, which is how a real transfer went unnoticed. The heartbeat bounds that distance.

use std::time::{Duration, SystemTime};
use tee_coprocessor::commands::{heartbeat_due, record_advanced};

/// `out` is the staging path the attest paths are handed; the marker sits two levels up.
fn staging(dir: &std::path::Path) -> String {
    let staging = dir.join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    staging
        .join("attestation.json")
        .to_string_lossy()
        .into_owned()
}

#[test]
fn a_route_that_has_never_advanced_takes_the_heartbeat() {
    let dir = tempdir();
    // No marker at all: better to prove once than to wait another twelve hours to find out
    // the route is stuck.
    assert!(heartbeat_due(Some(&staging(&dir))));
}

#[test]
fn a_route_that_just_advanced_does_not() {
    let dir = tempdir();
    let out = staging(&dir);
    record_advanced(Some(&out));
    assert!(!heartbeat_due(Some(&out)));
}

#[test]
fn a_route_quiet_for_over_twelve_hours_does() {
    let dir = tempdir();
    let out = staging(&dir);
    record_advanced(Some(&out));
    let marker = dir.join("advanced");
    let stale = SystemTime::now() - Duration::from_secs(13 * 60 * 60);
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(stale)).unwrap();
    assert!(
        heartbeat_due(Some(&out)),
        "twelve hours is the bound, thirteen is past it"
    );
}

#[test]
fn no_output_path_means_no_heartbeat() {
    // The one-shot CLI has nowhere to keep state; it should not invent a reason to prove.
    assert!(!heartbeat_due(None));
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "tee-heartbeat-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
