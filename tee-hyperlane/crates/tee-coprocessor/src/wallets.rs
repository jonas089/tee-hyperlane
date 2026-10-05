//! Tracks the relayer's gas balance on each destination chain and estimates how long it lasts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::destination::Destination;

pub const CHECK_EVERY: u64 = 5 * 60;
/// Readings older than this are dropped, and the spend rate is taken over what is left.
const HISTORY: u64 = 7 * 24 * 60 * 60;
/// Less history than this gives no spend rate: one attestation in the first minutes would read
/// as a burn rate fit to empty the wallet by lunchtime.
const MIN_SPAN: u64 = 6 * 60 * 60;
/// Warn at this many days of gas left, and go critical at the second.
pub const WARN_DAYS: f64 = 7.0;
pub const CRITICAL_DAYS: f64 = 2.0;

pub struct WatchedWallet {
    pub chain: String,
    pub symbol: String,
    pub decimals: u32,
    /// Below this, in whole tokens, warn whatever the spend rate says.
    pub low: f64,
    pub destination: Box<dyn Destination>,
}

/// A gas token and a warning level for a chain of `kind`, unless its config says otherwise.
pub fn defaults(kind: &str) -> (&'static str, u32, f64) {
    match kind {
        "celestia" => ("TIA", 6, 5.0),
        // Eden's gas token is TIA, with EVM decimals.
        "eden" => ("TIA", 18, 1.0),
        "ethereum" => ("ETH", 18, 0.05),
        _ => ("ETH", 18, 0.01),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WalletReading {
    pub address: Option<String>,
    /// Base units, decimal.
    pub balance: Option<String>,
    pub checked_at: Option<u64>,
    pub failing_since: Option<u64>,
    pub error: Option<String>,
    /// `(at, balance)`, oldest first. Persisted so a restart does not forget the spend rate.
    pub history: Vec<(u64, u128)>,
}

impl WalletReading {
    pub fn record(&mut self, address: String, balance: u128, at: u64) {
        self.address = Some(address);
        self.balance = Some(balance.to_string());
        self.checked_at = Some(at);
        self.failing_since = None;
        self.error = None;
        self.history.push((at, balance));
        self.history
            .retain(|(t, _)| at.saturating_sub(*t) <= HISTORY);
    }

    /// Base units spent per day over the history, counting only drops, or `None` without
    /// enough history to say.
    pub fn spend_per_day(&self) -> Option<f64> {
        let (first, last) = (self.history.first()?, self.history.last()?);
        let span = last.0.saturating_sub(first.0);
        if span < MIN_SPAN {
            return None;
        }
        let spent: u128 = self
            .history
            .windows(2)
            .map(|w| w[0].1.saturating_sub(w[1].1))
            .sum();
        Some(spent as f64 * 86_400.0 / span as f64)
    }
}

pub fn whole(amount: u128, decimals: u32) -> f64 {
    amount as f64 / 10f64.powi(decimals as i32)
}

/// A balance for people: at most four decimals, no trailing zeros.
pub fn show(amount: u128, decimals: u32) -> String {
    let text = format!("{:.4}", whole(amount, decimals));
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text.is_empty() {
        "0".into()
    } else {
        text.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Low,
    Critical,
    /// No reading yet, or the last one failed.
    Unknown,
}

/// How a wallet stands: its level, days left if known, and why.
pub fn judge(w: &WatchedWallet, r: &WalletReading) -> (Level, Option<f64>, Option<String>) {
    let Some(balance) = r.balance.as_ref().and_then(|b| b.parse::<u128>().ok()) else {
        return (Level::Unknown, None, None);
    };
    let days = r
        .spend_per_day()
        .filter(|s| *s > 0.0)
        .map(|s| balance as f64 / s);
    let held = whole(balance, w.decimals);
    let shown = format!("{} {}", show(balance, w.decimals), w.symbol);
    if balance == 0 {
        return (
            Level::Critical,
            days,
            Some(format!("the relayer's wallet is empty ({shown})")),
        );
    }
    if let Some(d) = days {
        if d < CRITICAL_DAYS {
            return (
                Level::Critical,
                days,
                Some(format!(
                    "{shown} left, about {d:.1} days at the current spend"
                )),
            );
        }
        if d < WARN_DAYS {
            return (
                Level::Low,
                days,
                Some(format!(
                    "{shown} left, about {d:.1} days at the current spend"
                )),
            );
        }
    }
    if held < w.low {
        return (
            Level::Low,
            days,
            Some(format!(
                "{shown} left, below the {} {} warning level",
                w.low, w.symbol
            )),
        );
    }
    (Level::Ok, days, None)
}

/// Read every wallet once.
pub async fn check(
    wallets: &[WatchedWallet],
) -> BTreeMap<String, anyhow::Result<crate::destination::Wallet>> {
    let mut out = BTreeMap::new();
    for w in wallets {
        out.insert(w.chain.clone(), w.destination.wallet().await);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spend_counts_drops_only_and_needs_enough_history() {
        let mut r = WalletReading::default();
        r.record("a".into(), 1000, 0);
        r.record("a".into(), 900, 3600);
        assert_eq!(r.spend_per_day(), None, "an hour is not enough to say");
        r.record("a".into(), 5000, 4 * 3600); // a top-up
        r.record("a".into(), 4900, 12 * 3600);
        // 100 + 100 spent over twelve hours: 400 a day.
        assert_eq!(r.spend_per_day(), Some(400.0));
    }

    #[test]
    fn old_readings_fall_out() {
        let mut r = WalletReading::default();
        r.record("a".into(), 10, 0);
        r.record("a".into(), 9, HISTORY + 10);
        assert_eq!(r.history.len(), 1);
    }

    #[test]
    fn balances_read_like_money() {
        assert_eq!(show(1_234_500_000_000_000_000, 18), "1.2345");
        assert_eq!(show(50_000_000_000_000_000, 18), "0.05");
        assert_eq!(show(0, 18), "0");
        assert_eq!(show(5_000_000, 6), "5");
    }
}
