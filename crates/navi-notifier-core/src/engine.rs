//! The orchestration core: poll every source, filter through the rules, route
//! survivors to notifiers, and record delivery idempotently.
//!
//! The engine is transport- and provider-agnostic; it speaks only in [`Source`],
//! [`Notifier`], [`StateStore`], and [`Event`]. The daemon layer owns scheduling;
//! this owns a single pass ([`Engine::run_once`]).

use std::sync::Arc;

use tracing::{debug, error, info, warn};

use crate::error::SourceError;
use crate::model::Event;
use crate::rules::{Decision, DropReason, FilterContext, RuleEngine};
use crate::traits::{Notifier, Source, StateStore};

/// Connects a source to a notifier. If a run has no routes at all, the engine
/// falls back to delivering every source's events to every notifier.
#[derive(Debug, Clone)]
pub struct Route {
    pub source: String,
    pub notifier: String,
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

pub struct Engine {
    sources: Vec<Arc<dyn Source>>,
    notifiers: Vec<Arc<dyn Notifier>>,
    routes: Vec<Route>,
    rules: RuleEngine,
    state: Arc<dyn StateStore>,
}

impl Engine {
    pub fn new(
        sources: Vec<Arc<dyn Source>>,
        notifiers: Vec<Arc<dyn Notifier>>,
        routes: Vec<Route>,
        rules: RuleEngine,
        state: Arc<dyn StateStore>,
    ) -> Self {
        Self {
            sources,
            notifiers,
            routes,
            rules,
            state,
        }
    }

    /// Notifiers that should receive events from `source_id`.
    fn notifiers_for(&self, source_id: &str) -> Vec<Arc<dyn Notifier>> {
        if self.routes.is_empty() {
            return self.notifiers.clone();
        }
        self.notifiers
            .iter()
            .filter(|n| {
                self.routes
                    .iter()
                    .any(|r| r.source == source_id && r.notifier == n.id())
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
            let targets = self.notifiers_for(source.id());

            for event in events {
                let record = self
                    .process_event(source.as_ref(), &targets, event, &ctx, dry_run)
                    .await;
                report.records.push(record);
            }
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
        targets: &[Arc<dyn Notifier>],
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
            warn!(source = %event.source_id, "no notifier routed for source; event undeliverable");
            return EventRecord {
                event,
                outcome: EventOutcome::DeliveryFailed {
                    errors: vec!["no notifier routed for this source".into()],
                },
            };
        }

        // 3. Deliver to every routed notifier.
        let mut errors = Vec::new();
        let mut delivered_to = Vec::new();
        for notifier in targets {
            match notifier.send(&event).await {
                Ok(()) => delivered_to.push(notifier.id().to_string()),
                Err(err) => {
                    error!(notifier = notifier.id(), %err, "delivery failed");
                    errors.push(format!("{}: {err}", notifier.id()));
                }
            }
        }

        // Only consider the event delivered (and advance provider cursors) if every
        // routed notifier succeeded. A partial failure stays undelivered so the next
        // pass retries; dedup guards against double-sends to notifiers that did work
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
    use crate::error::{NotifyError, StateError};
    use crate::model::{Actor, EventKind, PullRequest, Repo, ViewerRelationship};
    use crate::traits::{Notifier, Source, StateStore};
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

    struct MockSource {
        events: Vec<Event>,
    }
    #[async_trait]
    impl Source for MockSource {
        fn id(&self) -> &str {
            "mock"
        }
        async fn poll(&self, _state: &dyn StateStore) -> Result<Vec<Event>, SourceError> {
            Ok(self.events.clone())
        }
    }

    #[derive(Default)]
    struct MockNotifier {
        sent: Mutex<Vec<String>>,
        fail: bool,
    }
    #[async_trait]
    impl Notifier for MockNotifier {
        fn id(&self) -> &str {
            "mock-notify"
        }
        async fn send(&self, event: &Event) -> Result<(), NotifyError> {
            if self.fail {
                return Err(NotifyError::Delivery("boom".into()));
            }
            self.sent.lock().unwrap().push(event.dedup_key.clone());
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
        notifier: Arc<MockNotifier>,
    ) -> (Engine, Arc<MemState>) {
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource { events })],
            vec![notifier],
            vec![],
            RuleEngine::new(rules),
            state.clone(),
        );
        (engine, state)
    }

    #[tokio::test]
    async fn delivers_then_dedupes_across_runs() {
        let notifier = Arc::new(MockNotifier::default());
        let (engine, _state) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            RuleConfig::default(),
            notifier.clone(),
        );

        let r1 = engine.run_once(FilterContext::default(), false).await;
        assert_eq!(r1.delivered_count(), 1);
        assert_eq!(
            notifier.sent.lock().unwrap().as_slice(),
            &["k1".to_string()]
        );

        // Second pass: same event is already delivered → suppressed, not re-sent.
        let r2 = engine.run_once(FilterContext::default(), false).await;
        assert_eq!(r2.delivered_count(), 0);
        assert!(matches!(
            r2.records[0].outcome,
            EventOutcome::AlreadyDelivered
        ));
        assert_eq!(notifier.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn rules_suppress_disabled_kind() {
        let notifier = Arc::new(MockNotifier::default());
        let mut rules = RuleConfig::default();
        rules.events.mentioned = false;
        let (engine, _s) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            rules,
            notifier.clone(),
        );
        let r = engine.run_once(FilterContext::default(), false).await;
        assert_eq!(r.delivered_count(), 0);
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::Suppressed(DropReason::EventKindDisabled)
        ));
        assert!(notifier.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn dry_run_sends_nothing_and_leaves_state() {
        let notifier = Arc::new(MockNotifier::default());
        let (engine, state) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            RuleConfig::default(),
            notifier.clone(),
        );
        let r = engine.run_once(FilterContext::default(), true).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::WouldDeliver { .. }
        ));
        assert!(notifier.sent.lock().unwrap().is_empty());
        // Not marked delivered → a real run afterwards would still deliver.
        assert!(!state.was_delivered("k1").await.unwrap());
    }

    #[tokio::test]
    async fn failed_delivery_is_not_marked_delivered() {
        let notifier = Arc::new(MockNotifier {
            fail: true,
            ..Default::default()
        });
        let (engine, state) = engine_with(
            vec![ev(EventKind::Mentioned, "k1")],
            RuleConfig::default(),
            notifier,
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
