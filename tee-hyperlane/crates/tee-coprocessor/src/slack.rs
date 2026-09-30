//! Posting the monitor to a Slack channel: every problem as it opens and as it resolves, and a
//! status post every `SLACK_EVERY_SECS` (30 minutes by default) whether or not anything is
//! wrong, so a quiet channel means the relayer has stopped, not that all is well.
//!
//! Off unless `SLACK_BOT_TOKEN` and `SLACK_CHANNEL` are set, in `devnet/.env` on the host, which
//! the service loads. The token is never written anywhere else. `EXPLORER_URL`, when set, turns
//! each problem into a link to its page.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use crate::monitor::{duration, Change, Notification, Severity, Subject, Summary};

const DEFAULT_EVERY: u64 = 30 * 60;
/// Slack allows about one post a second per channel.
const SPACING: Duration = Duration::from_millis(1100);

pub struct Slack {
    token: String,
    channel: String,
    explorer: Option<String>,
    pub every: u64,
    http: reqwest::Client,
}

impl Slack {
    /// The configured channel, or `None` when Slack is not set up.
    pub fn from_env() -> Option<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Some(Self {
            token: var("SLACK_BOT_TOKEN")?,
            channel: var("SLACK_CHANNEL")?,
            explorer: var("EXPLORER_URL").map(|u| u.trim_end_matches('/').to_string()),
            every: var("SLACK_EVERY_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(DEFAULT_EVERY)
                .max(60),
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .expect("http client"),
        })
    }

    pub async fn post(&self, text: &str) -> Result<()> {
        let reply: Value = self
            .http
            .post("https://slack.com/api/chat.postMessage")
            .bearer_auth(&self.token)
            .json(&json!({ "channel": self.channel, "text": text, "unfurl_links": false }))
            .send()
            .await
            .context("slack unreachable")?
            .json()
            .await
            .context("slack answered with something other than JSON")?;
        anyhow::ensure!(
            reply["ok"].as_bool() == Some(true),
            "slack refused the post: {}",
            reply["error"].as_str().unwrap_or("no reason given")
        );
        Ok(())
    }

    /// Post, retrying a few times: a problem that never reaches the channel is the one post
    /// that matters most.
    async fn deliver(&self, text: &str, streak: &mut crate::Streak) {
        for attempt in 1..=RETRIES {
            match self.post(text).await {
                Ok(()) => {
                    streak.ok();
                    return;
                }
                Err(e) => {
                    let error = crate::brief(&format!("{e:#}"));
                    if streak.fail(&error).is_some() {
                        tracing::warn!(error, attempt, "cannot post to slack");
                    }
                    tokio::time::sleep(SPACING * 5 * attempt).await;
                }
            }
        }
    }
}

const RETRIES: u32 = 3;

/// Post every change as it arrives, and the status every `every` seconds, until the process
/// stops.
pub async fn run(
    slack: Slack,
    tracker: std::sync::Arc<crate::tracker::Tracker>,
    mut changes: tokio::sync::mpsc::UnboundedReceiver<Change>,
) {
    let mut streak = crate::Streak::default();
    tracing::info!(every_secs = slack.every, "posting to slack");
    let started = format!(
        ":arrow_forward: Relayer started. A status post follows every {}.",
        duration(slack.every)
    );
    slack.deliver(&started, &mut streak).await;
    let mut status = tokio::time::interval(Duration::from_secs(slack.every));
    status.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires at once; the first status waits one interval, like the log's.
    status.tick().await;
    loop {
        tokio::select! {
            change = changes.recv() => {
                let Some(change) = change else { return };
                slack.deliver(&change_text(&change, slack.explorer.as_deref()), &mut streak).await;
                tokio::time::sleep(SPACING).await;
            }
            _ = status.tick() => {
                let summary = crate::monitor::summary(&tracker);
                slack.deliver(&summary_text(&summary, slack.explorer.as_deref()), &mut streak).await;
            }
        }
    }
}

/// Slack's own escaping, so a revert reason or an RPC error cannot inject a link or a mention.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn link(n: &Notification, explorer: Option<&str>) -> String {
    let title = escape(&n.title);
    let Some(base) = explorer.map(|e| e.trim_end_matches('/')) else {
        return format!("*{title}*");
    };
    let path = match n.subject {
        Subject::Message => format!("#/message/{}", n.subject_id),
        Subject::Route => format!("#/route/{}", n.subject_id),
        Subject::Chain => format!("#/chain/{}", n.subject_id),
        Subject::Monitor => "#/inbox".into(),
    };
    format!("*<{base}/{path}|{title}>*")
}

pub fn change_text(change: &Change, explorer: Option<&str>) -> String {
    match change {
        Change::Opened(n) => {
            let (icon, label) = match n.severity {
                Severity::Critical => (":rotating_light:", "Critical"),
                Severity::Warning => (":warning:", "Warning"),
            };
            format!(
                "{icon} {label}: {}\n{}",
                link(n, explorer),
                escape(&n.detail)
            )
        }
        Change::Resolved(n) => format!(
            ":white_check_mark: Resolved after {}: {}",
            duration(
                n.resolved_at
                    .unwrap_or(n.updated_at)
                    .saturating_sub(n.opened_at)
            ),
            link(n, explorer)
        ),
    }
}

pub fn summary_text(s: &Summary, explorer: Option<&str>) -> String {
    let traffic = format!(
        "{} transfer{} in flight, {} delivered in the last hour",
        s.in_flight,
        if s.in_flight == 1 { "" } else { "s" },
        s.delivered_last_hour
    );
    let gas = s
        .lowest_gas
        .as_ref()
        .map(|(chain, days)| format!(" Lowest gas: {chain}, about {days:.0} days left."))
        .unwrap_or_default();
    if s.open.is_empty() {
        return format!(
            ":large_green_circle: *All good.* {} routes healthy, {traffic}.{gas}",
            s.routes
        );
    }
    let icon = if s.critical() {
        ":red_circle:"
    } else {
        ":large_yellow_circle:"
    };
    let list: Vec<String> = s
        .open
        .iter()
        .map(|n| {
            format!(
                "• {} (open {})",
                link(n, explorer),
                duration(crate::route::now().saturating_sub(n.opened_at))
            )
        })
        .collect();
    format!(
        "{icon} *{} problem{} open.* {traffic}.{gas}\n{}",
        s.open.len(),
        if s.open.len() == 1 { "" } else { "s" },
        list.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(severity: Severity) -> Notification {
        Notification {
            id: "k@1".into(),
            key: "message-overdue:0x01".into(),
            severity,
            subject: Subject::Message,
            subject_id: "0x01".into(),
            title: "Transfer stuck on base-to-celestia".into(),
            detail: "reverted: <script> & friends".into(),
            opened_at: 100,
            updated_at: 100,
            resolved_at: None,
        }
    }

    #[test]
    fn a_problem_links_to_its_page_and_cannot_inject_markup() {
        let text = change_text(
            &Change::Opened(note(Severity::Critical)),
            Some("http://h:3001/"),
        );
        assert!(text.starts_with(":rotating_light: Critical: *<http://h:3001/#/message/0x01|"));
        assert!(text.contains("&lt;script&gt; &amp; friends"));
        assert!(!text.contains('—'), "no em-dashes");
    }

    #[test]
    fn a_resolution_says_how_long_it_lasted() {
        let mut n = note(Severity::Warning);
        n.resolved_at = Some(100 + 12 * 60);
        let text = change_text(&Change::Resolved(n), None);
        assert_eq!(
            text,
            ":white_check_mark: Resolved after 12m: *Transfer stuck on base-to-celestia*"
        );
    }

    #[test]
    fn the_status_post_says_all_good_or_lists_what_is_open() {
        let mut s = Summary {
            routes: 8,
            in_flight: 1,
            delivered_last_hour: 3,
            open: vec![],
            lowest_gas: Some(("eden".into(), 9.9)),
        };
        assert_eq!(
            summary_text(&s, None),
            ":large_green_circle: *All good.* 8 routes healthy, 1 transfer in flight, 3 delivered in the last hour. Lowest gas: eden, about 10 days left."
        );
        s.open = vec![note(Severity::Critical)];
        let text = summary_text(&s, None);
        assert!(text.starts_with(":red_circle: *1 problem open.*"));
        assert!(text.contains("• *Transfer stuck on base-to-celestia* (open"));
    }
}
