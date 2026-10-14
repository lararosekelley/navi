//! Snoozes: user-requested pauses that hold events until a deadline, for
//! everything, one org, or one repo.
//!
//! A snooze is stored as an absolute deadline rather than a duration, so it
//! survives the daemon restarting. The duration the user typed is kept alongside
//! it only to label the batch released when the snooze ends.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::model::Event;

/// What a snooze covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SnoozeScope {
    Global,
    /// Every repo under `org` on one provider. On GitLab this includes subgroups.
    Org {
        provider: String,
        org: String,
    },
    /// One `owner/name` repo on one provider.
    Repo {
        provider: String,
        repo: String,
    },
}

impl SnoozeScope {
    /// Stable key identifying the scope, so snoozing the same scope twice replaces
    /// the first snooze instead of adding a second. Case-folded, since forges
    /// treat owner and repo names case-insensitively.
    pub fn key(&self) -> String {
        match self {
            Self::Global => "global".into(),
            Self::Org { provider, org } => format!("org:{provider}:{}", org.to_lowercase()),
            Self::Repo { provider, repo } => format!("repo:{provider}:{}", repo.to_lowercase()),
        }
    }

    pub fn matches(&self, event: &Event) -> bool {
        let repo = &event.pull_request.repo;
        match self {
            Self::Global => true,
            Self::Org { provider, org } => {
                *provider == event.source_id
                    && (repo.owner.eq_ignore_ascii_case(org)
                        || repo
                            .owner
                            .to_lowercase()
                            .starts_with(&format!("{}/", org.to_lowercase())))
            }
            Self::Repo {
                provider,
                repo: name,
            } => *provider == event.source_id && repo.full_name().eq_ignore_ascii_case(name),
        }
    }
}

impl fmt::Display for SnoozeScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Global => write!(f, "everything"),
            Self::Org { provider, org } => write!(f, "{provider} org {org}"),
            Self::Repo { provider, repo } => write!(f, "{provider} repo {repo}"),
        }
    }
}

/// An active or expired snooze.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snooze {
    pub scope: SnoozeScope,
    pub duration: SnoozeDuration,
    #[serde(with = "time::serde::rfc3339")]
    pub until: OffsetDateTime,
}

impl Snooze {
    pub fn is_active(&self, now: OffsetDateTime) -> bool {
        now < self.until
    }
}

/// Of the snoozes covering `event` at `now`, the one that ends last, since that
/// is the one that decides when the event is released.
pub fn covering<'a>(
    snoozes: &'a [Snooze],
    event: &Event,
    now: OffsetDateTime,
) -> Option<&'a Snooze> {
    snoozes
        .iter()
        .filter(|s| s.is_active(now) && s.scope.matches(event))
        .max_by_key(|s| s.until)
}

/// What a batch of snoozed events reports besides the events themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnoozeRelease {
    /// The length of the snooze that held the batch.
    pub duration: SnoozeDuration,
    /// Older snoozed events dropped because the buffer filled up.
    pub dropped: usize,
}

/// A snooze length such as `45s`, `4h` or `2mo`. The unit is required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SnoozeDuration {
    count: u32,
    unit: Unit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Seconds,
    Minutes,
    Hours,
    Days,
    Weeks,
    Months,
    Years,
}

impl Unit {
    const ALL: [(Unit, &'static str); 7] = [
        (Unit::Seconds, "s"),
        (Unit::Minutes, "m"),
        (Unit::Hours, "h"),
        (Unit::Days, "d"),
        (Unit::Weeks, "w"),
        (Unit::Months, "mo"),
        (Unit::Years, "y"),
    ];

    fn suffix(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(u, _)| *u == self)
            .map(|(_, s)| *s)
            .unwrap()
    }

    /// Months and years are fixed lengths (30 and 365 days): a snooze is a rough
    /// "leave me alone for a while", not a calendar appointment.
    fn seconds(self) -> i64 {
        match self {
            Self::Seconds => 1,
            Self::Minutes => 60,
            Self::Hours => 60 * 60,
            Self::Days => 24 * 60 * 60,
            Self::Weeks => 7 * 24 * 60 * 60,
            Self::Months => 30 * 24 * 60 * 60,
            Self::Years => 365 * 24 * 60 * 60,
        }
    }
}

impl SnoozeDuration {
    pub fn as_duration(self) -> time::Duration {
        time::Duration::seconds(i64::from(self.count) * self.unit.seconds())
    }

    /// When a snooze of this length that starts at `now` ends, or `None` if that
    /// is past the end of representable time.
    pub fn end_from(self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        now.checked_add(self.as_duration())
    }
}

/// Why a duration string was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidDuration(String);

impl fmt::Display for InvalidDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid duration `{}`; use a number and a unit: s, m, h, d, w, mo, y (e.g. 45s, 4h, 2mo)",
            self.0
        )
    }
}

impl std::error::Error for InvalidDuration {}

impl FromStr for SnoozeDuration {
    type Err = InvalidDuration;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let invalid = || InvalidDuration(s.to_string());
        let trimmed = s.trim();
        let split = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .ok_or_else(invalid)?;
        let (digits, suffix) = trimmed.split_at(split);
        let count: u32 = digits.parse().map_err(|_| invalid())?;
        if count == 0 {
            return Err(invalid());
        }
        let suffix = suffix.to_ascii_lowercase();
        let unit = Unit::ALL
            .iter()
            .find(|(_, s)| *s == suffix)
            .map(|(u, _)| *u)
            .ok_or_else(invalid)?;
        Ok(Self { count, unit })
    }
}

impl TryFrom<String> for SnoozeDuration {
    type Error = InvalidDuration;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<SnoozeDuration> for String {
    fn from(d: SnoozeDuration) -> Self {
        d.to_string()
    }
}

impl fmt::Display for SnoozeDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.count, self.unit.suffix())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, EventKind, PullRequest, Repo, ViewerRelationship};

    fn event(source: &str, owner: &str, name: &str) -> Event {
        Event {
            source_id: source.into(),
            kind: EventKind::Mentioned,
            pull_request: PullRequest {
                repo: Repo::new(owner, name),
                number: 1,
                title: "t".into(),
                url: "https://example.test/pr/1".into(),
                author: Actor::new("a"),
                draft: false,
            },
            viewer: ViewerRelationship::default(),
            actor: Actor::new("b"),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            target_url: None,
            excerpt: None,
            dedup_key: "k".into(),
        }
    }

    #[test]
    fn parses_every_unit() {
        for (input, secs) in [
            ("45s", 45),
            ("1m", 60),
            ("4h", 4 * 3600),
            ("3d", 3 * 86400),
            ("1w", 7 * 86400),
            ("2mo", 60 * 86400),
            ("1y", 365 * 86400),
        ] {
            let d: SnoozeDuration = input.parse().unwrap();
            assert_eq!(d.as_duration().whole_seconds(), secs, "{input}");
            assert_eq!(d.to_string(), input);
        }
    }

    #[test]
    fn unit_is_case_insensitive() {
        assert_eq!("4H".parse::<SnoozeDuration>().unwrap().to_string(), "4h");
        assert_eq!("2Mo".parse::<SnoozeDuration>().unwrap().to_string(), "2mo");
    }

    #[test]
    fn rejects_missing_unknown_or_zero() {
        for input in [
            "",
            "45",
            "h",
            "4x",
            "4 h",
            "-1h",
            "0h",
            "1.5h",
            "4hh",
            "99999999999h",
        ] {
            assert!(input.parse::<SnoozeDuration>().is_err(), "{input}");
        }
    }

    #[test]
    fn end_overflow_is_none() {
        let d: SnoozeDuration = "4294967295y".parse().unwrap();
        assert!(d.end_from(OffsetDateTime::now_utc()).is_none());
    }

    #[test]
    fn scope_matching() {
        let gh = |repo: &str| SnoozeScope::Repo {
            provider: "github".into(),
            repo: repo.into(),
        };
        let org = |org: &str| SnoozeScope::Org {
            provider: "gitlab".into(),
            org: org.into(),
        };
        let e = event("github", "Acme", "widgets");
        assert!(SnoozeScope::Global.matches(&e));
        assert!(gh("acme/widgets").matches(&e));
        assert!(!gh("acme/gadgets").matches(&e));
        // The provider is part of the scope.
        assert!(!gh("acme/widgets").matches(&event("gitea", "acme", "widgets")));

        assert!(org("acme").matches(&event("gitlab", "acme", "w")));
        assert!(org("acme").matches(&event("gitlab", "acme/infra", "w")));
        assert!(!org("acme").matches(&event("gitlab", "acmecorp", "w")));
        assert!(!org("acme").matches(&event("github", "acme", "w")));
    }

    #[test]
    fn scope_key_ignores_case() {
        let a = SnoozeScope::Repo {
            provider: "github".into(),
            repo: "Acme/Widgets".into(),
        };
        let b = SnoozeScope::Repo {
            provider: "github".into(),
            repo: "acme/widgets".into(),
        };
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn covering_picks_the_latest_active_snooze() {
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(10);
        let snooze = |scope, hours: i64, d: &str| Snooze {
            scope,
            duration: d.parse().unwrap(),
            until: OffsetDateTime::UNIX_EPOCH + time::Duration::hours(hours),
        };
        let snoozes = vec![
            snooze(SnoozeScope::Global, 12, "2h"),
            snooze(
                SnoozeScope::Repo {
                    provider: "github".into(),
                    repo: "acme/widgets".into(),
                },
                20,
                "10h",
            ),
            // Expired, despite ending latest of all had it been active.
            snooze(SnoozeScope::Global, 5, "1h"),
        ];
        let e = event("github", "acme", "widgets");
        assert_eq!(
            covering(&snoozes, &e, now).unwrap().duration.to_string(),
            "10h"
        );
        let other = event("github", "acme", "gadgets");
        assert_eq!(
            covering(&snoozes, &other, now)
                .unwrap()
                .duration
                .to_string(),
            "2h"
        );
        let later = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(30);
        assert!(covering(&snoozes, &e, later).is_none());
    }

    #[test]
    fn snooze_round_trips_through_json() {
        let s = Snooze {
            scope: SnoozeScope::Org {
                provider: "gitea".into(),
                org: "acme".into(),
            },
            duration: "3d".parse().unwrap(),
            until: OffsetDateTime::UNIX_EPOCH,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Snooze>(&json).unwrap(), s);
    }
}
