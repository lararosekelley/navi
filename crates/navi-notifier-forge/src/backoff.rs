//! Per-PR retry backoff for fetches that keep failing.
//!
//! A source holds a per-PR cursor so an unchanged pull request is skipped cheaply,
//! and only advances it past one it actually fetched. That is what stops a momentary
//! failure hiding a PR until its timestamp moves again, but it also means a fetch
//! that *never* succeeds is retried on every poll for as long as the daemon runs:
//! the involved-PR search has no date bound, so the PR comes back every pass, is
//! re-fetched, fails, and logs. At a 60 second interval that is on the order of a
//! thousand wasted requests a day for one PR.
//!
//! This bounds that without ever abandoning the PR. A repeatedly failing scope is
//! skipped for a growing interval, and the first success clears it. Unlike degrading
//! the PR to "gone", nothing is given up: the PR is still fetched, just less often
//! while it is failing.
//!
//! Deliberately in memory rather than in the state store. The cost being avoided is
//! within a single long-running daemon, so losing the state on restart is harmless -
//! one attempt, then backed off again. Persisting it would mean another per-PR row
//! to grow, prune and migrate, for no benefit the process lifetime doesn't already
//! provide.

use std::collections::HashMap;
use std::sync::Mutex;

use time::{Duration, OffsetDateTime};

/// Wait after the first failure. Doubles per consecutive failure, up to [`MAX_WAIT`].
const BASE_WAIT: Duration = Duration::minutes(2);

/// Ceiling on the wait, so a PR that recovers is picked up within the hour.
///
/// Purely a pickup-latency choice, with no correctness constraint on it, because
/// the caller only ever defers a pull request it already holds a snapshot for. The
/// diff applies its age watermark on first sight alone, so for those a deferred
/// fetch is late and never lossy however long the wait. Tying the length of a
/// backoff to the first-sight window instead would be bounding the wrong thing:
/// what matters is cumulative elapsed time, and successive waits add up past any
/// single cap.
const MAX_WAIT: Duration = Duration::hours(1);

/// Overflow guard only. [`MAX_WAIT`] is what actually bounds the wait; this just
/// stops the exponent running away for a scope that has failed thousands of times.
const MAX_DOUBLINGS: u32 = 10;

#[derive(Debug, Clone, Copy)]
struct Failing {
    consecutive: u32,
    retry_at: OffsetDateTime,
}

/// Tracks which PRs are in a fetch-failure backoff.
#[derive(Debug, Default)]
pub struct FetchBackoff {
    failing: Mutex<HashMap<String, Failing>>,
}

impl FetchBackoff {
    /// Whether `scope` should be attempted on this pass.
    ///
    /// True for anything not currently failing, so the common path is one map lookup.
    pub fn ready(&self, scope: &str, now: OffsetDateTime) -> bool {
        self.failing
            .lock()
            .map(|f| f.get(scope).is_none_or(|s| now >= s.retry_at))
            .unwrap_or(true)
    }

    /// Stop tracking `scope`.
    ///
    /// Called both when a fetch succeeds and when the PR turns out to be permanently
    /// gone. Those mean different things to the caller but the same thing here:
    /// there is no longer a failing fetch to slow down.
    pub fn clear(&self, scope: &str) {
        if let Ok(mut f) = self.failing.lock() {
            f.remove(scope);
        }
    }

    /// Record a failed fetch, and return how long `scope` will now be skipped.
    pub fn failed(&self, scope: &str, now: OffsetDateTime) -> Duration {
        let Ok(mut failing) = self.failing.lock() else {
            // Poisoned: skip the bookkeeping and let the next poll retry normally.
            // This is a cache for saving requests, so losing it costs requests, not
            // correctness, and panicking a notifier daemon over one is worse.
            return Duration::ZERO;
        };
        let entry = failing.entry(scope.to_string()).or_insert(Failing {
            consecutive: 0,
            retry_at: now,
        });
        entry.consecutive = entry.consecutive.saturating_add(1);
        let wait = BASE_WAIT * 2i32.pow(entry.consecutive.min(MAX_DOUBLINGS) - 1);
        let wait = wait.min(MAX_WAIT);
        entry.retry_at = now + wait;
        wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }

    #[test]
    fn an_untracked_scope_is_always_ready() {
        let b = FetchBackoff::default();
        assert!(b.ready("acme/w#1", t0()));
    }

    #[test]
    fn a_failure_defers_the_next_attempt_and_the_wait_doubles() {
        let b = FetchBackoff::default();
        assert_eq!(b.failed("acme/w#1", t0()), BASE_WAIT);
        assert!(!b.ready("acme/w#1", t0()));
        assert!(!b.ready("acme/w#1", t0() + BASE_WAIT - Duration::seconds(1)));
        assert!(b.ready("acme/w#1", t0() + BASE_WAIT));

        assert_eq!(b.failed("acme/w#1", t0()), BASE_WAIT * 2);
        assert_eq!(b.failed("acme/w#1", t0()), BASE_WAIT * 4);
    }

    #[test]
    fn the_wait_is_capped_so_a_recovered_pr_is_picked_up() {
        let b = FetchBackoff::default();
        for _ in 0..20 {
            b.failed("acme/w#1", t0());
        }
        assert_eq!(b.failed("acme/w#1", t0()), MAX_WAIT);
        assert!(b.ready("acme/w#1", t0() + MAX_WAIT));
    }

    /// The point of backing off rather than giving up: one success and the PR is
    /// back on the normal cadence, so a flaky fetch is never permanently degraded.
    #[test]
    fn one_success_clears_the_backoff() {
        let b = FetchBackoff::default();
        b.failed("acme/w#1", t0());
        b.failed("acme/w#1", t0());
        b.clear("acme/w#1");
        assert!(b.ready("acme/w#1", t0()));
        // And the next failure starts from the base wait, not where it left off.
        assert_eq!(b.failed("acme/w#1", t0()), BASE_WAIT);
    }

    #[test]
    fn scopes_are_tracked_independently() {
        let b = FetchBackoff::default();
        b.failed("acme/w#1", t0());
        assert!(b.ready("acme/w#2", t0()));
        assert!(!b.ready("acme/w#1", t0()));
    }
}
