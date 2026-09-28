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
pub mod tracker;
pub mod v1;

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
