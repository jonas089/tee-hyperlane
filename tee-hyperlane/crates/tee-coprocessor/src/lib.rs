//! The coprocessor: everything the bridge does that does not need to be trusted.
//!
//! It finds what the enclave needs, asks it to attest, and delivers the result. A dishonest
//! coprocessor can stall the bridge; it cannot make a chain accept a message the enclave did
//! not attest.
//!
//! Two directories, one per side of a route. `origin/` holds the `Indexer` trait and one file
//! per chain that implements it, under the chain it rides on and mirroring the enclave:
//! `origin/ethereum/base.rs` finds what the enclave's `ethereum/base.rs` verifies.
//! `destination/` holds the `Destination` trait and its two implementations, EVM and Celestia.

pub mod api;
pub mod bech32;
pub mod config;
pub mod destination;
pub mod identity;
pub mod monitor;
pub mod origin;
pub mod route;
pub mod slack;
pub mod tracker;
pub mod v1;
pub mod wallets;

/// A run of failures of one thing, so the log says it failed once, then again only every
/// `STREAK_REPEAT` or when the error changes, then that it recovered: one line per event
/// instead of one per retry.
#[derive(Default)]
pub struct Streak {
    started: Option<std::time::Instant>,
    count: u32,
    last_logged: Option<std::time::Instant>,
    last_error: String,
}

pub const STREAK_REPEAT: std::time::Duration = std::time::Duration::from_secs(10 * 60);

impl Streak {
    /// Record a failure. `Some(n)` when it should be logged: the first of a run, a different
    /// error, or the run still going after `STREAK_REPEAT`; `n` is the failures so far.
    pub fn fail(&mut self, error: &str) -> Option<u32> {
        let now = std::time::Instant::now();
        // Heights and hashes change from one retry to the next; the error does not.
        let shape: String = error.chars().filter(|c| !c.is_ascii_hexdigit()).collect();
        self.count += 1;
        let first = self.started.is_none();
        self.started.get_or_insert(now);
        let changed = shape != self.last_error;
        let due = self
            .last_logged
            .is_none_or(|t| now.duration_since(t) >= STREAK_REPEAT);
        self.last_error = shape;
        (first || changed || due).then(|| {
            self.last_logged = Some(now);
            self.count
        })
    }

    /// Record a success. `Some((failures, for))` when it ends a run.
    pub fn ok(&mut self) -> Option<(u32, std::time::Duration)> {
        let started = self.started.take()?;
        let count = std::mem::take(&mut self.count);
        self.last_logged = None;
        self.last_error.clear();
        Some((count, started.elapsed()))
    }

    pub fn failures(&self) -> u32 {
        self.count
    }
}

/// Error text from an endpoint, shortened to one line for a log. An endpoint that fails behind
/// a CDN answers with a whole HTML page, which is reduced to its `<title>`; anything else loses
/// its line breaks and is cut at 300 characters.
pub fn brief(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    if lower.contains("<html") || lower.contains("<!doctype") {
        let title = lower
            .find("<title>")
            .and_then(|a| {
                lower[a + 7..]
                    .find("</title>")
                    .map(|b| &text[a + 7..a + 7 + b])
            })
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "));
        return format!(
            "an HTML error page: {}",
            title.unwrap_or_else(|| "no title".into())
        );
    }
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match one_line.char_indices().nth(300) {
        Some((cut, _)) => format!("{}...", &one_line[..cut]),
        None => one_line,
    }
}

#[cfg(test)]
mod streak_tests {
    use super::Streak;

    #[test]
    fn a_run_of_failures_is_logged_once_then_recovers() {
        let mut s = Streak::default();
        assert!(s.ok().is_none(), "nothing to recover from");
        assert_eq!(s.fail("rpc 429 at block 100"), Some(1));
        assert_eq!(
            s.fail("rpc 429 at block 101"),
            None,
            "same error, new height"
        );
        assert_eq!(s.fail("enclave unreachable"), Some(3), "a different error");
        assert_eq!(s.ok().map(|(n, _)| n), Some(3));
        assert_eq!(s.fail("rpc 429"), Some(1), "a new run");
    }
}

#[cfg(test)]
mod tests {
    use super::brief;

    #[test]
    fn an_html_error_page_becomes_its_title() {
        let page = "<!DOCTYPE html>\n<html><head>\n<title>rpc.sepolia.ethpandaops.io | 521: Web server is down</title>\n</head><body>...</body></html>";
        assert_eq!(
            brief(page),
            "an HTML error page: rpc.sepolia.ethpandaops.io | 521: Web server is down"
        );
    }

    #[test]
    fn long_text_is_one_line_and_cut() {
        assert_eq!(brief("a\n  b\tc"), "a b c");
        assert_eq!(brief(&"x".repeat(1000)).len(), 303);
    }
}
