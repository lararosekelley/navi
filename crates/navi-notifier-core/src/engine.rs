//! The orchestration core: poll every source, filter through the rules, route
//! survivors to destinations, and record delivery idempotently.
//!
//! The engine is transport- and provider-agnostic; it speaks only in [`Source`],
//! [`Destination`], [`StateStore`], and [`Event`]. The daemon layer owns scheduling;
//! this owns a single pass ([`Engine::run_once`]).

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{debug, error, info, warn};

use crate::config::pattern_matches;
use crate::error::{SourceError, StateError};
use crate::model::Event;
use crate::rules::{Decision, DropReason, FilterContext, RuleEngine};
use crate::traits::{Destination, Source, StateStore};

/// Connects a source to a destination, optionally scoped to certain repos. If a
/// run has no routes at all, the engine falls back to delivering every source's
/// events to every destination.
#[derive(Debug, Clone)]
pub struct Route {
    pub source: String,
    pub destination: String,
    /// Repo globs this route is limited to (matched via the shared repo matcher).
    /// Empty = every repo from `source`.
    pub repos: Vec<String>,
}

/// What happened to a single event during a run, captured for logging and
/// `--dry-run` reporting.
#[derive(Debug, Clone)]
pub enum EventOutcome {
    Delivered {
        to: Vec<String>,
    },
    Suppressed(DropReason),
    AlreadyDelivered,
    /// Buffered into the periodic digest instead of delivered now.
    Digested,
    DeliveryFailed {
        errors: Vec<String>,
    },
    /// Would have been delivered, but this was a dry run.
    WouldDeliver {
        to: Vec<String>,
    },
}

/// Per-event record pairing the event with its outcome.
#[derive(Debug, Clone)]
pub struct EventRecord {
    pub event: Event,
    pub outcome: EventOutcome,
}

/// Aggregate result of one [`Engine::run_once`] pass.
#[derive(Debug, Default, Clone)]
pub struct RunReport {
    pub records: Vec<EventRecord>,
    /// Sources whose poll failed, with the error string.
    pub source_errors: Vec<(String, String)>,
}

impl RunReport {
    pub fn delivered_count(&self) -> usize {
        self.records
            .iter()
            .filter(|r| matches!(r.outcome, EventOutcome::Delivered { .. }))
            .count()
    }
}

/// State-store keys under which the pending digest is buffered.
const DIGEST_SOURCE: &str = "__digest__";
const DIGEST_SCOPE: &str = "pending";

pub struct Engine {
    sources: Vec<Arc<dyn Source>>,
    destinations: Vec<Arc<dyn Destination>>,
    routes: Vec<Route>,
    rules: RuleEngine,
    state: Arc<dyn StateStore>,
    /// Event tags to batch into the periodic digest instead of delivering now.
    /// Empty = digest off.
    digest_kinds: HashSet<String>,
}

impl Engine {
    pub fn new(
        sources: Vec<Arc<dyn Source>>,
        destinations: Vec<Arc<dyn Destination>>,
        routes: Vec<Route>,
        rules: RuleEngine,
        state: Arc<dyn StateStore>,
    ) -> Self {
        Self {
            sources,
            destinations,
            routes,
            rules,
            state,
            digest_kinds: HashSet::new(),
        }
    }

    /// Set the event tags to batch into the periodic digest (builder-style, so
    /// `new` stays stable). Empty = digest off, the default.
    pub fn with_digest_kinds(mut self, kinds: HashSet<String>) -> Self {
        self.digest_kinds = kinds;
        self
    }

    /// Destinations that should receive this event, given its source and repo. A
    /// route matches when its source matches and its repo globs are empty or match
    /// the event's repo; every matching route's destination receives it (fan-out).
    /// With no routes configured at all, every destination receives everything.
    fn destinations_for(&self, event: &Event) -> Vec<Arc<dyn Destination>> {
        if self.routes.is_empty() {
            return self.destinations.clone();
        }
        let repo = event.pull_request.repo.full_name();
        self.destinations
            .iter()
            .filter(|n| {
                self.routes.iter().any(|r| {
                    r.source == event.source_id
                        && r.destination == n.id()
                        && (r.repos.is_empty() || r.repos.iter().any(|p| pattern_matches(p, &repo)))
                })
            })
            .cloned()
            .collect()
    }

    /// Run a single poll→filter→deliver pass over all sources.
    ///
    /// `dry_run` reports what would happen without sending, marking delivery, or
    /// advancing provider cursors, so the user can preview their config safely.
    pub async fn run_once(&self, ctx: FilterContext, dry_run: bool) -> RunReport {
        let mut report = RunReport::default();

        for source in &self.sources {
            let events = match source.poll(self.state.as_ref()).await {
                Ok(events) => events,
                Err(err) => {
                    Self::log_source_error(source.id(), &err);
                    report
                        .source_errors
                        .push((source.id().to_string(), err.to_string()));
                    continue;
                }
            };

            debug!(source = source.id(), count = events.len(), "polled events");

            let mut source_records = Vec::new();
            for event in events {
                // Resolved per event: a route may scope to specific repos.
                let targets = self.destinations_for(&event);
                let record = self
                    .process_event(source.as_ref(), &targets, event, &ctx, dry_run)
                    .await;
                source_records.push(record);
            }

            // Flush the source's deferred per-PR snapshots, holding back any PR that
            // had a delivery failure so its events re-derive next pass (dedup stops
            // the ones that did send from re-sending). A dry run persists nothing.
            if !dry_run {
                let failed_scopes: HashSet<String> = source_records
                    .iter()
                    .filter(|r| matches!(r.outcome, EventOutcome::DeliveryFailed { .. }))
                    .map(|r| r.event.scope())
                    .collect();
                if let Err(err) = source
                    .commit_snapshots(self.state.as_ref(), &failed_scopes)
                    .await
                {
                    warn!(source = source.id(), %err, "committing snapshots failed");
                }
            }
            report.records.extend(source_records);
        }

        info!(
            delivered = report.delivered_count(),
            total = report.records.len(),
            source_errors = report.source_errors.len(),
            dry_run,
            "run complete"
        );
        report
    }

    async fn process_event(
        &self,
        source: &dyn Source,
        targets: &[Arc<dyn Destination>],
        event: Event,
        ctx: &FilterContext,
        dry_run: bool,
    ) -> EventRecord {
        // 1. Rule filter.
        if let Decision::Drop(reason) = self.rules.decide(&event, ctx) {
            debug!(dedup_key = %event.dedup_key, ?reason, "event suppressed");
            return EventRecord {
                event,
                outcome: EventOutcome::Suppressed(reason),
            };
        }

        // 2. Dedup: never ping twice for the same underlying action.
        match self.state.was_delivered(&event.dedup_key).await {
            Ok(true) => {
                return EventRecord {
                    event,
                    outcome: EventOutcome::AlreadyDelivered,
                };
            }
            Ok(false) => {}
            Err(err) => {
                // Fail safe: if we can't check dedup, treat as a delivery failure
                // so it is retried next pass rather than risk spamming.
                warn!(dedup_key = %event.dedup_key, %err, "dedup check failed");
                return EventRecord {
                    event,
                    outcome: EventOutcome::DeliveryFailed {
                        errors: vec![format!("dedup check failed: {err}")],
                    },
                };
            }
        }

        let target_ids: Vec<String> = targets.iter().map(|n| n.id().to_string()).collect();

        if dry_run {
            return EventRecord {
                event,
                outcome: EventOutcome::WouldDeliver { to: target_ids },
            };
        }

        if targets.is_empty() {
            // Routes exist but none cover this repo: an intentional filter, not a
            // failure. Treating it as failed would hold the snapshot back and
            // re-derive the same events every poll (a loop).
            if !self.routes.is_empty() {
                debug!(source = %event.source_id, "no route matches this repo; suppressing");
                return EventRecord {
                    event,
                    outcome: EventOutcome::Suppressed(DropReason::NoMatchingRoute),
                };
            }
            warn!(source = %event.source_id, "no destination configured; event undeliverable");
            return EventRecord {
                event,
                outcome: EventOutcome::DeliveryFailed {
                    errors: vec!["no destination configured".into()],
                },
            };
        }

        // Digest kinds are buffered for the periodic flush rather than sent now.
        // Marked delivered so they don't re-derive; the flush handles routing.
        if self.digest_kinds.contains(event.kind.tag()) {
            if let Err(err) = self.enqueue_digest(&event).await {
                warn!(dedup_key = %event.dedup_key, %err, "failed to buffer digest event");
                return EventRecord {
                    event,
                    outcome: EventOutcome::DeliveryFailed {
                        errors: vec![format!("digest buffer: {err}")],
                    },
                };
            }
            if let Err(err) = self.state.mark_delivered(&event.dedup_key).await {
                warn!(dedup_key = %event.dedup_key, %err, "failed to persist dedup key");
            }
            return EventRecord {
                event,
                outcome: EventOutcome::Digested,
            };
        }

        // 3. Deliver to every routed destination.
        let mut errors = Vec::new();
        let mut delivered_to = Vec::new();
        for destination in targets {
            match destination.send(&event).await {
                Ok(()) => delivered_to.push(destination.id().to_string()),
                Err(err) => {
                    error!(destination = destination.id(), %err, "delivery failed");
                    errors.push(format!("{}: {err}", destination.id()));
                }
            }
        }

        // Only consider the event delivered (and advance provider cursors) if every
        // routed destination succeeded. A partial failure stays undelivered so the next
        // pass retries; dedup guards against double-sends to destinations that did work
        // via provider-side idempotency where available.
        if errors.is_empty() {
            if let Err(err) = self.state.mark_delivered(&event.dedup_key).await {
                warn!(dedup_key = %event.dedup_key, %err, "failed to persist dedup key");
            }
            if let Err(err) = source.commit(self.state.as_ref(), &event).await {
                warn!(%err, "source commit hook failed");
            }
            EventRecord {
                event,
                outcome: EventOutcome::Delivered { to: delivered_to },
            }
        } else {
            EventRecord {
                event,
                outcome: EventOutcome::DeliveryFailed { errors },
            }
        }
    }

    /// The events currently buffered for the next digest flush.
    async fn read_digest(&self) -> Result<Vec<Event>, StateError> {
        match self.state.get_snapshot(DIGEST_SOURCE, DIGEST_SCOPE).await? {
            Some(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| StateError::Serde(format!("digest buffer: {e}"))),
            None => Ok(Vec::new()),
        }
    }

    /// Append an event to the persisted digest buffer.
    async fn enqueue_digest(&self, event: &Event) -> Result<(), StateError> {
        let mut pending = self.read_digest().await?;
        pending.push(event.clone());
        let bytes = serde_json::to_vec(&pending).map_err(|e| StateError::Serde(e.to_string()))?;
        self.state
            .put_snapshot(DIGEST_SOURCE, DIGEST_SCOPE, &bytes)
            .await
    }

    /// Flush the buffered digest: one batched message per destination (only the
    /// events routed to it), then clear the buffer. Called by the daemon on the
    /// digest interval. Returns how many events were flushed. If any destination
    /// fails, the buffer is kept for the next interval (which may re-send to
    /// destinations that already succeeded - acceptable for a low-priority digest).
    pub async fn flush_digest(&self) -> usize {
        let pending = match self.read_digest().await {
            Ok(p) => p,
            Err(err) => {
                warn!(%err, "could not read digest buffer; leaving it in place");
                return 0;
            }
        };
        if pending.is_empty() {
            return 0;
        }

        let mut all_ok = true;
        for dest in &self.destinations {
            let batch: Vec<Event> = pending
                .iter()
                .filter(|e| self.destinations_for(e).iter().any(|d| d.id() == dest.id()))
                .cloned()
                .collect();
            if batch.is_empty() {
                continue;
            }
            if let Err(err) = dest.send_digest(&batch).await {
                error!(destination = dest.id(), %err, "digest flush failed");
                all_ok = false;
            }
        }

        if !all_ok {
            return 0;
        }
        // If the buffer can't be cleared, don't report success: the events are still
        // buffered and would re-send next flush, so surface it as a non-clean flush.
        if let Err(err) = self
            .state
            .put_snapshot(DIGEST_SOURCE, DIGEST_SCOPE, b"[]")
            .await
        {
            warn!(%err, "digest sent but the buffer could not be cleared; it may re-send next flush");
            return 0;
        }
        info!(count = pending.len(), "digest flushed");
        pending.len()
    }

    fn log_source_error(source_id: &str, err: &SourceError) {
        match err {
            SourceError::RateLimited { retry_after_secs } => {
                warn!(source = source_id, retry_after_secs, "source rate limited");
            }
            other => error!(source = source_id, %other, "source poll failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuleConfig;
    use crate::error::{DestinationError, StateError};
    use crate::model::{Actor, EventKind, PullRequest, Repo, ViewerRelationship};
    use crate::traits::{Destination, Source, StateStore};
    use async_trait::async_trait;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;
    use time::OffsetDateTime;

    /// Minimal in-memory state store for exercising the engine.
    #[derive(Default)]
    struct MemState {
        delivered: Mutex<HashSet<String>>,
        snapshots: Mutex<HashMap<String, Vec<u8>>>,
        cursors: Mutex<HashMap<String, String>>,
    }

    #[async_trait]
    impl StateStore for MemState {
        async fn get_snapshot(&self, s: &str, scope: &str) -> Result<Option<Vec<u8>>, StateError> {
            Ok(self
                .snapshots
                .lock()
                .unwrap()
                .get(&format!("{s}:{scope}"))
                .cloned())
        }
        async fn put_snapshot(&self, s: &str, scope: &str, b: &[u8]) -> Result<(), StateError> {
            self.snapshots
                .lock()
                .unwrap()
                .insert(format!("{s}:{scope}"), b.to_vec());
            Ok(())
        }
        async fn was_delivered(&self, k: &str) -> Result<bool, StateError> {
            Ok(self.delivered.lock().unwrap().contains(k))
        }
        async fn mark_delivered(&self, k: &str) -> Result<(), StateError> {
            self.delivered.lock().unwrap().insert(k.to_string());
            Ok(())
        }
        async fn get_cursor(&self, s: &str, k: &str) -> Result<Option<String>, StateError> {
            Ok(self
                .cursors
                .lock()
                .unwrap()
                .get(&format!("{s}:{k}"))
                .cloned())
        }
        async fn put_cursor(&self, s: &str, k: &str, v: &str) -> Result<(), StateError> {
            self.cursors
                .lock()
                .unwrap()
                .insert(format!("{s}:{k}"), v.to_string());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockSource {
        events: Vec<Event>,
        /// Records the `failed_scopes` each `commit_snapshots` call received.
        committed: Mutex<Vec<HashSet<String>>>,
    }
    #[async_trait]
    impl Source for MockSource {
        fn id(&self) -> &str {
            "mock"
        }
        async fn poll(&self, _state: &dyn StateStore) -> Result<Vec<Event>, SourceError> {
            Ok(self.events.clone())
        }
        async fn commit_snapshots(
            &self,
            _state: &dyn StateStore,
            failed_scopes: &HashSet<String>,
        ) -> Result<(), SourceError> {
            self.committed.lock().unwrap().push(failed_scopes.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MockDestination {
        id: String,
        sent: Mutex<Vec<String>>,
        /// Batches received via `send_digest` (each is the dedup keys in the batch).
        digests: Mutex<Vec<Vec<String>>>,
        fail: bool,
    }
    #[async_trait]
    impl Destination for MockDestination {
        fn id(&self) -> &str {
            if self.id.is_empty() {
                "mock-notify"
            } else {
                &self.id
            }
        }
        async fn send(&self, event: &Event) -> Result<(), DestinationError> {
            if self.fail {
                return Err(DestinationError::Delivery("boom".into()));
            }
            self.sent.lock().unwrap().push(event.dedup_key.clone());
            Ok(())
        }
        async fn send_digest(&self, events: &[Event]) -> Result<(), DestinationError> {
            if self.fail {
                return Err(DestinationError::Delivery("boom".into()));
            }
            self.digests
                .lock()
                .unwrap()
                .push(events.iter().map(|e| e.dedup_key.clone()).collect());
            Ok(())
        }
    }

    fn ev(kind: EventKind, key: &str) -> Event {
        Event {
            source_id: "mock".into(),
            kind,
            pull_request: PullRequest {
                repo: Repo::new("acme", "widgets"),
                number: 1,
                title: "t".into(),
                url: "u".into(),
                author: Actor::new("a"),
                draft: false,
            },
            viewer: ViewerRelationship::default(),
            actor: Actor::new("b"),
            occurred_at: OffsetDateTime::UNIX_EPOCH,
            target_url: None,
            excerpt: None,
            dedup_key: key.into(),
        }
    }

    fn engine_with(
        events: Vec<Event>,
        rules: RuleConfig,
        destination: Arc<MockDestination>,
    ) -> (Engine, Arc<MemState>) {
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events,
                ..Default::default()
            })],
            vec![destination],
            vec![],
            RuleEngine::new(rules).expect("valid test rules"),
            state.clone(),
        );
        (engine, state)
    }

    #[tokio::test]
    async fn delivers_then_dedupes_across_runs() {
        let destination = Arc::new(MockDestination::default());
        let (engine, _state) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            RuleConfig::default(),
            destination.clone(),
        );

        let r1 = engine.run_once(FilterContext::default(), false).await;
        assert_eq!(r1.delivered_count(), 1);
        assert_eq!(
            destination.sent.lock().unwrap().as_slice(),
            &["k1".to_string()]
        );

        // Second pass: same event is already delivered → suppressed, not re-sent.
        let r2 = engine.run_once(FilterContext::default(), false).await;
        assert_eq!(r2.delivered_count(), 0);
        assert!(matches!(
            r2.records[0].outcome,
            EventOutcome::AlreadyDelivered
        ));
        assert_eq!(destination.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rules_suppress_disabled_kind() {
        let destination = Arc::new(MockDestination::default());
        let mut rules = RuleConfig::default();
        rules.events.mentioned = false;
        let (engine, _s) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            rules,
            destination.clone(),
        );
        let r = engine.run_once(FilterContext::default(), false).await;
        assert_eq!(r.delivered_count(), 0);
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::Suppressed(DropReason::EventKindDisabled)
        ));
        assert!(destination.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dry_run_sends_nothing_and_leaves_state() {
        let destination = Arc::new(MockDestination::default());
        let (engine, state) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            RuleConfig::default(),
            destination.clone(),
        );
        let r = engine.run_once(FilterContext::default(), true).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::WouldDeliver { .. }
        ));
        assert!(destination.sent.lock().unwrap().is_empty());
        // Not marked delivered → a real run afterwards would still deliver.
        assert!(!state.was_delivered("k1").await.unwrap());
    }

    #[tokio::test]
    async fn digest_kinds_are_buffered_then_flushed() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::Mentioned, "k1")],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));

        // The mentioned event is a digest kind → buffered, not sent immediately.
        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(r.records[0].outcome, EventOutcome::Digested));
        assert!(
            dest.sent.lock().unwrap().is_empty(),
            "nothing sent immediately"
        );
        assert!(dest.digests.lock().unwrap().is_empty(), "not flushed yet");

        // Flushing sends the batch via send_digest, once.
        let flushed = engine.flush_digest().await;
        assert_eq!(flushed, 1);
        assert_eq!(
            dest.digests.lock().unwrap().as_slice(),
            &[vec!["k1".to_string()]]
        );

        // A second flush finds an empty buffer and does nothing.
        assert_eq!(engine.flush_digest().await, 0);
        assert_eq!(dest.digests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn routes_scope_by_repo() {
        // dest-a is limited to acme/*; dest-b takes everything.
        let a = Arc::new(MockDestination {
            id: "dest-a".into(),
            ..Default::default()
        });
        let b = Arc::new(MockDestination {
            id: "dest-b".into(),
            ..Default::default()
        });
        let mut other = ev(EventKind::Mentioned, "k-other");
        other.pull_request.repo = Repo::new("other", "thing");
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::Mentioned, "k-acme"), other],
                ..Default::default()
            })],
            vec![a.clone(), b.clone()],
            vec![
                Route {
                    source: "mock".into(),
                    destination: "dest-a".into(),
                    repos: vec!["acme/*".into()],
                },
                Route {
                    source: "mock".into(),
                    destination: "dest-b".into(),
                    repos: vec![],
                },
            ],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        engine.run_once(FilterContext::default(), false).await;
        // dest-a only got the acme event; dest-b got both (fan-out + catch-all).
        assert_eq!(a.sent.lock().unwrap().as_slice(), &["k-acme".to_string()]);
        assert_eq!(b.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn event_with_no_matching_route_is_suppressed_not_failed() {
        // A scoped route that this event's repo doesn't match must suppress the
        // event, not fail it — else its snapshot is held back and it re-derives
        // every poll (a loop).
        let dest = Arc::new(MockDestination {
            id: "dest-a".into(),
            ..Default::default()
        });
        let mut out = ev(EventKind::Mentioned, "k1");
        out.pull_request.repo = Repo::new("other", "thing");
        let src = Arc::new(MockSource {
            events: vec![out],
            ..Default::default()
        });
        let engine = Engine::new(
            vec![src.clone()],
            vec![dest.clone()],
            vec![Route {
                source: "mock".into(),
                destination: "dest-a".into(),
                repos: vec!["acme/*".into()],
            }],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::Suppressed(DropReason::NoMatchingRoute)
        ));
        assert!(dest.sent.lock().unwrap().is_empty());
        // Not counted as a failed scope, so its snapshot can advance.
        assert!(src.committed.lock().unwrap()[0].is_empty());
    }

    #[tokio::test]
    async fn commit_snapshots_holds_back_only_failed_scopes() {
        // A clean delivery: commit_snapshots runs with no failed scopes, so the
        // source is free to persist everything it deferred.
        let ok = Arc::new(MockSource {
            events: vec![ev(EventKind::Mentioned, "k1")],
            ..Default::default()
        });
        let engine = Engine::new(
            vec![ok.clone()],
            vec![Arc::new(MockDestination::default())],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        engine.run_once(FilterContext::default(), false).await;
        let calls = ok.committed.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].is_empty(),
            "clean run should report no failed scopes"
        );
    }

    #[tokio::test]
    async fn commit_snapshots_reports_the_failed_pr_scope() {
        // A failed delivery: the event's PR scope must be reported so the source
        // holds its snapshot back and the event re-derives next pass.
        let src = Arc::new(MockSource {
            events: vec![ev(EventKind::Mentioned, "k1")],
            ..Default::default()
        });
        let engine = Engine::new(
            vec![src.clone()],
            vec![Arc::new(MockDestination {
                fail: true,
                ..Default::default()
            })],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        engine.run_once(FilterContext::default(), false).await;
        let calls = src.committed.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].contains("acme/widgets#1"), "got {:?}", calls[0]);
    }

    #[tokio::test]
    async fn dry_run_does_not_commit_snapshots() {
        let src = Arc::new(MockSource {
            events: vec![ev(EventKind::Mentioned, "k1")],
            ..Default::default()
        });
        let engine = Engine::new(
            vec![src.clone()],
            vec![Arc::new(MockDestination::default())],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        engine.run_once(FilterContext::default(), true).await;
        assert!(
            src.committed.lock().unwrap().is_empty(),
            "dry run must not flush snapshots"
        );
    }

    #[tokio::test]
    async fn failed_delivery_is_not_marked_delivered() {
        let destination = Arc::new(MockDestination {
            fail: true,
            ..Default::default()
        });
        let (engine, state) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            RuleConfig::default(),
            destination,
        );
        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::DeliveryFailed { .. }
        ));
        // Must remain undelivered so the next pass retries.
        assert!(!state.was_delivered("k1").await.unwrap());
    }
}
