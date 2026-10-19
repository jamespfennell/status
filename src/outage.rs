//! Module outage groups projects that are down at the same time into a single outage, so that
//! each outage results in one email thread rather than separate emails for every project.
//!
//! An outage opens when a project goes down and no outage is open. Any project that goes down
//! while the outage is open joins it. Once every project in the outage has recovered, the
//! outage stays open for a cool-down period so that flapping projects don't start new threads.
//!
//! Emails only describe projects. Changes to the checks within a project that is already
//! down don't send emails; the status page has the details.

use crate::email;
use std::collections::BTreeMap;

type Time = chrono::DateTime<chrono::Utc>;

/// How long an outage stays open after all of its projects have recovered.
const COOLDOWN_MINUTES: i64 = 5;

/// A project that is currently down.
pub struct Down {
    pub name: String,
    /// Time of the first failed check.
    pub since: Time,
    /// Which checks are down and why; e.g. "3 of 7 checks down".
    pub summary: String,
}

/// Tracks the current outage, if any, and builds the emails for it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct Tracker {
    outage: Option<Outage>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct Outage {
    start: Time,
    subject: String,
    /// Message ID of the first email, which all later emails reply to.
    root_message_id: String,
    emails_sent: u32,
    /// Every project that has been down during the outage, keyed by name.
    projects: BTreeMap<String, AffectedProject>,
    /// When the last project recovered. None while any project is down.
    all_recovered_at: Option<Time>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct AffectedProject {
    since: Time,
    /// The latest summary, which is updated while the project is down.
    summary: String,
    /// None if the project is still down.
    recovered: Option<Time>,
}

impl Tracker {
    /// Updates the outage given the projects that are currently down, returning an email
    /// to send if anything changed.
    ///
    /// Emails link to projects on the status page at `ui_url`, which ends in a slash.
    pub fn update(
        &mut self,
        hostname: &str,
        ui_url: &str,
        now: Time,
        mut down: Vec<Down>,
    ) -> Option<email::Email> {
        down.sort_by(|a, b| a.name.cmp(&b.name));
        let Some(outage) = &mut self.outage else {
            if down.is_empty() {
                return None;
            }
            let mut outage = Outage::open(hostname, down);
            let body = outage.initial_body(ui_url);
            let email = outage.email(hostname, body);
            self.outage = Some(outage);
            return Some(email);
        };
        if let Some(changes) = outage.apply(now, down) {
            let body = outage.update_body(ui_url, now, &changes);
            return Some(outage.email(hostname, body));
        }
        match outage.all_recovered_at {
            Some(recovered_at) if now - recovered_at >= cooldown() => {
                let body = outage.closed_body(recovered_at);
                let email = outage.email(hostname, body);
                self.outage = None;
                Some(email)
            }
            _ => None,
        }
    }
}

struct Changes {
    went_down: Vec<String>,
    /// Name and how long the check was down for.
    recovered: Vec<(String, chrono::Duration)>,
    /// Whether the outage was in its cool-down period before these changes.
    was_cooling_down: bool,
}

impl Outage {
    fn open(hostname: &str, down: Vec<Down>) -> Self {
        let start = down.iter().map(|d| d.since).min().unwrap();
        let projects = down
            .into_iter()
            .map(|d| {
                (
                    d.name,
                    AffectedProject {
                        since: d.since,
                        summary: d.summary,
                        recovered: None,
                    },
                )
            })
            .collect();
        Self {
            start,
            subject: format!("[{hostname}] Outage started {}", format_time(start)),
            root_message_id: format!("<outage-{}@{hostname}>", start.format("%Y%m%dT%H%M%SZ")),
            emails_sent: 0,
            projects,
            all_recovered_at: None,
        }
    }

    /// Records which projects are down now, returning the changes if there were any.
    ///
    /// The summaries of projects that are still down are updated without counting as a change.
    fn apply(&mut self, now: Time, down: Vec<Down>) -> Option<Changes> {
        let mut changes = Changes {
            went_down: vec![],
            recovered: vec![],
            was_cooling_down: self.all_recovered_at.is_some(),
        };
        let down_names: std::collections::HashSet<String> =
            down.iter().map(|d| d.name.clone()).collect();
        for (name, project) in &mut self.projects {
            if project.recovered.is_none() && !down_names.contains(name) {
                project.recovered = Some(now);
                changes.recovered.push((name.clone(), now - project.since));
            }
        }
        for d in down {
            match self.projects.get_mut(&d.name) {
                Some(project) if project.recovered.is_none() => {
                    project.summary = d.summary;
                }
                _ => {
                    changes.went_down.push(d.name.clone());
                    self.projects.insert(
                        d.name,
                        AffectedProject {
                            since: d.since,
                            summary: d.summary,
                            recovered: None,
                        },
                    );
                }
            }
        }
        if changes.went_down.is_empty() && changes.recovered.is_empty() {
            return None;
        }
        self.all_recovered_at = if self.still_down().next().is_none() {
            Some(now)
        } else {
            None
        };
        Some(changes)
    }

    fn still_down(&self) -> impl Iterator<Item = (&String, &AffectedProject)> {
        self.projects.iter().filter(|(_, p)| p.recovered.is_none())
    }

    /// Builds the next email in the outage's thread.
    fn email(&mut self, hostname: &str, body: String) -> email::Email {
        let (message_id, in_reply_to) = if self.emails_sent == 0 {
            (self.root_message_id.clone(), None)
        } else {
            (
                format!(
                    "<outage-{}-{}@{hostname}>",
                    self.start.format("%Y%m%dT%H%M%SZ"),
                    self.emails_sent
                ),
                Some(self.root_message_id.clone()),
            )
        };
        self.emails_sent += 1;
        email::Email {
            subject: self.subject.clone(),
            body,
            message_id,
            in_reply_to,
        }
    }

    fn initial_body(&self, ui_url: &str) -> String {
        let mut body = format!("Affected projects ({}):\n", self.projects.len());
        for (name, project) in &self.projects {
            body.push_str(&down_line(ui_url, name, project));
        }
        body.push_str(&format!("\nDetails: {ui_url}\n"));
        body
    }

    fn update_body(&self, ui_url: &str, now: Time, changes: &Changes) -> String {
        let mut body = String::new();
        if !changes.went_down.is_empty() {
            body.push_str(&format!("Went down ({}):\n", changes.went_down.len()));
            for name in &changes.went_down {
                body.push_str(&down_line(ui_url, name, &self.projects[name]));
            }
            if changes.was_cooling_down {
                body.push_str("\nThe outage is no longer closing.\n");
            }
            body.push('\n');
        }
        if !changes.recovered.is_empty() {
            body.push_str(&format!("Recovered ({}):\n", changes.recovered.len()));
            for (name, down_for) in &changes.recovered {
                body.push_str(&format!(
                    "- {name} (down for {})\n",
                    format_duration(*down_for)
                ));
            }
            body.push('\n');
        }
        let still_down: Vec<_> = self.still_down().collect();
        if still_down.is_empty() {
            body.push_str(&format!(
                "All {} affected projects have recovered. The outage will close at {} if nothing else goes down in the next {}.\n\n",
                self.projects.len(),
                format_time(now + cooldown()),
                format_duration(cooldown()),
            ));
        } else {
            body.push_str(&format!("Still down ({}):\n", still_down.len()));
            for (name, project) in still_down {
                body.push_str(&down_line(ui_url, name, project));
            }
            body.push('\n');
        }
        body.push_str(&format!("Details: {ui_url}\n"));
        body
    }

    fn closed_body(&self, recovered_at: Time) -> String {
        let mut body = format!(
            "Outage closed. Nothing has gone down since the last affected project recovered.\n\n\
            Started: {}\nRecovered: {}\nDuration: {}\n\nAffected projects ({}):\n",
            format_time(self.start),
            format_time(recovered_at),
            format_duration(recovered_at - self.start),
            self.projects.len(),
        );
        for name in self.projects.keys() {
            body.push_str(&format!("- {name}\n"));
        }
        body
    }
}

/// A line describing a project that is down, with a link to it on the status page.
fn down_line(ui_url: &str, name: &str, project: &AffectedProject) -> String {
    format!(
        "- {name}: {}\n  {ui_url}#/projects/{}\n",
        project.summary,
        encode_uri_component(name)
    )
}

/// Percent-encodes the string like JavaScript's encodeURIComponent, which the status page
/// uses for its links.
fn encode_uri_component(s: &str) -> String {
    let mut encoded = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    encoded
}

fn format_time(t: Time) -> String {
    t.format("%Y-%m-%d %H:%M UTC").to_string()
}

fn format_duration(d: chrono::Duration) -> String {
    let minutes = d.num_minutes();
    if minutes < 1 {
        format!("{}s", d.num_seconds())
    } else if minutes < 60 {
        format!("{minutes}m")
    } else if minutes < 60 * 24 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{}d {}h", minutes / (60 * 24), (minutes / 60) % 24)
    }
}

fn cooldown() -> chrono::Duration {
    chrono::Duration::minutes(COOLDOWN_MINUTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOST: &str = "status.b.example.com";
    const UI: &str = "https://status.example.com/";

    fn time(minute: i64) -> Time {
        chrono::DateTime::parse_from_rfc3339("2026-10-07T14:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc)
            + chrono::Duration::minutes(minute)
    }

    /// Projects that are down, as (name, minute of first failure).
    fn down(projects: &[(&str, i64)]) -> Vec<Down> {
        projects
            .iter()
            .map(|(name, since)| Down {
                name: name.to_string(),
                since: time(*since),
                summary: "HTTP 502".into(),
            })
            .collect()
    }

    #[test]
    fn nothing_down() {
        let mut tracker = Tracker::default();
        assert!(tracker.update(HOST, UI, time(0), vec![]).is_none());
    }

    #[test]
    fn full_lifecycle() {
        let mut tracker = Tracker::default();

        let first = tracker
            .update(HOST, UI, time(2), down(&[("b", 1), ("a", 0)]))
            .unwrap();
        assert_eq!(
            first.subject,
            "[status.b.example.com] Outage started 2026-10-07 14:00 UTC"
        );
        assert_eq!(first.in_reply_to, None);
        assert_eq!(
            first.body,
            "Affected projects (2):\n\
            - a: HTTP 502\n  https://status.example.com/#/projects/a\n\
            - b: HTTP 502\n  https://status.example.com/#/projects/b\n\
            \nDetails: https://status.example.com/\n"
        );

        // No changes, no email.
        assert!(tracker
            .update(HOST, UI, time(3), down(&[("a", 0), ("b", 1)]))
            .is_none());

        let joined = tracker
            .update(HOST, UI, time(4), down(&[("a", 0), ("b", 1), ("c", 3)]))
            .unwrap();
        assert_eq!(joined.subject, first.subject);
        assert_eq!(joined.in_reply_to.as_ref(), Some(&first.message_id));
        assert_ne!(joined.message_id, first.message_id);
        assert!(joined.body.starts_with(
            "Went down (1):\n- c: HTTP 502\n  https://status.example.com/#/projects/c\n"
        ));
        assert!(joined.body.contains("Still down (3):"));

        let partial = tracker
            .update(HOST, UI, time(10), down(&[("c", 3)]))
            .unwrap();
        assert!(partial
            .body
            .starts_with("Recovered (2):\n- a (down for 10m)\n- b (down for 9m)\n"));
        assert!(partial.body.contains("Still down (1):\n- c: HTTP 502\n"));

        let recovered = tracker.update(HOST, UI, time(20), vec![]).unwrap();
        assert_eq!(recovered.in_reply_to.as_ref(), Some(&first.message_id));
        assert!(recovered.body.contains(
            "All 3 affected projects have recovered. The outage will close at 2026-10-07 14:25 UTC if nothing else goes down in the next 5m."
        ));

        // Still cooling down.
        assert!(tracker.update(HOST, UI, time(24), vec![]).is_none());

        let closed = tracker.update(HOST, UI, time(25), vec![]).unwrap();
        assert_eq!(closed.subject, first.subject);
        assert_eq!(closed.in_reply_to.as_ref(), Some(&first.message_id));
        assert!(closed.body.contains(
            "Started: 2026-10-07 14:00 UTC\nRecovered: 2026-10-07 14:20 UTC\nDuration: 20m\n"
        ));
        assert!(closed
            .body
            .contains("Affected projects (3):\n- a\n- b\n- c\n"));
        assert!(tracker.outage.is_none());

        // The next failure starts a new thread.
        let next = tracker
            .update(HOST, UI, time(31), down(&[("a", 30)]))
            .unwrap();
        assert_eq!(next.in_reply_to, None);
        assert_ne!(next.message_id, first.message_id);
        assert_eq!(
            next.subject,
            "[status.b.example.com] Outage started 2026-10-07 14:30 UTC"
        );
    }

    #[test]
    fn summary_changes_do_not_send_emails() {
        let mut tracker = Tracker::default();
        tracker
            .update(HOST, UI, time(1), down(&[("a", 0)]))
            .unwrap();

        let mut more_checks_down = down(&[("a", 0)]);
        more_checks_down[0].summary = "2 of 3 checks down".into();
        assert!(tracker
            .update(HOST, UI, time(2), more_checks_down)
            .is_none());

        // The latest summary is shown in the next email.
        let joined = tracker
            .update(HOST, UI, time(3), {
                let mut d = down(&[("a", 0), ("b", 2)]);
                d[0].summary = "2 of 3 checks down".into();
                d
            })
            .unwrap();
        assert!(joined
            .body
            .contains("Still down (2):\n- a: 2 of 3 checks down\n"));
    }

    #[test]
    fn links_are_encoded() {
        let mut tracker = Tracker::default();
        let email = tracker
            .update(HOST, UI, time(1), down(&[("a.example.com (frontend)", 0)]))
            .unwrap();
        assert!(email
            .body
            .contains("https://status.example.com/#/projects/a.example.com%20(frontend)\n"));
    }

    #[test]
    fn down_during_cooldown_keeps_outage_open() {
        let mut tracker = Tracker::default();
        let first = tracker
            .update(HOST, UI, time(1), down(&[("a", 0)]))
            .unwrap();
        tracker.update(HOST, UI, time(5), vec![]).unwrap();

        // The recovered project goes down again, so it's reported as going down.
        let reopened = tracker
            .update(HOST, UI, time(8), down(&[("a", 7)]))
            .unwrap();
        assert_eq!(reopened.in_reply_to.as_ref(), Some(&first.message_id));
        assert!(reopened.body.starts_with(
            "Went down (1):\n- a: HTTP 502\n  https://status.example.com/#/projects/a\n\nThe outage is no longer closing.\n"
        ));

        // The original cool-down has been cancelled.
        assert!(tracker
            .update(HOST, UI, time(11), down(&[("a", 7)]))
            .is_none());
        let recovered = tracker.update(HOST, UI, time(12), vec![]).unwrap();
        assert!(recovered
            .body
            .contains("will close at 2026-10-07 14:17 UTC"));
        assert!(tracker.update(HOST, UI, time(16), vec![]).is_none());
        assert!(tracker.update(HOST, UI, time(17), vec![]).is_some());
        assert!(tracker.outage.is_none());
    }
}
