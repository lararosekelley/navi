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
use crate::rules::{Decision, DeferReason, DropReason, FilterContext, RuleEngine};
use crate::traits::{Destination, Source, StateStore};

/// Connects a source to a destination, optionally scoped to certain repos. If a
/// run has no routes at all, the engine falls back to delivering every source's
/// events to every destination.
#[derive(Debug, Clone, Default)]
pub struct Route {
    pub source: String,
    pub destination: String,
    /// Repo globs this route is limited to (matched via the shared repo matcher).
    /// Empty = every repo from `source`.
    pub repos: Vec<String>,
    /// When true, this route only acts on events that no normal (non-fallback)
    /// route claimed: a catch-all for "everything else". Lets a config send some
    /// repos to one destination and route the remainder to another without listing
    /// every owner. A fallback route may still set `source`/`repos` to narrow when
    /// it acts, but note an unclaimed event that doesn't match this route's own
    /// `source`/`repos` is still suppressed, not caught, so a scoped fallback is not
    /// a universal safety net.
    pub fallback: bool,
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
    /// Held back by a deferring rule (quiet hours) and buffered for release once
    /// that rule stops applying. Unlike [`EventOutcome::Suppressed`], the event is
    /// not lost.
    Deferred(DeferReason),
    DeliveryFailed {
        errors: Vec<String>,
    },
    /// Would have been delivered, but this was a dry run.
    WouldDeliver {
        to: Vec<String>,
    },
    /// Would have been deferred, but this was a dry run.
    WouldDefer(DeferReason),
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

/// Where the digest parks events a deferring rule held back.
///
/// Separate from `pending` because the two are on different clocks. `pending`
/// accumulates and is emptied once per `digest.interval_secs`; what the window held
/// back is retried every poll pass, so it goes out as soon as the window ends rather
/// than waiting for the next interval. Sharing one row forced a single clock to do
/// both jobs, which either stranded the held events or emitted a one-event digest
/// every pass.
const DIGEST_HELD_SCOPE: &str = "held";

/// State-store keys under which deferred (quiet-hours) events are buffered. A
/// separate bucket from the digest: these are ordinary alerts waiting for the
/// window to end, not events the user asked to have batched.
const DEFERRED_SOURCE: &str = "__deferred__";
const DEFERRED_SCOPE: &str = "pending";

/// Dedup sinks standing for "accepted into a buffer" rather than "sent to a
/// destination". Taking an event into a buffer is recorded so a re-derived copy is
/// not buffered twice; the real per-destination sinks are recorded later, when the
/// flush actually sends it. Both are `__`-wrapped, which no destination id can be.
const DIGEST_SINK: &str = "__digest__";
const DEFERRED_SINK: &str = "__deferred__";

/// What one digest flush did, so the caller can tell an attempt that had nothing
/// to send from one a quiet window prevented from sending.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DigestFlush {
    /// Events that reached at least one destination.
    pub sent: usize,
    /// Events left in the buffer because a deferring rule still applies to them.
    pub held: usize,
    /// A destination or the buffer write failed, so the batch is kept to retry.
    /// Distinct from `held`, which is the window doing its job rather than an error.
    pub failed: bool,
}

/// Most events either buffer will hold. Generous enough that a normal quiet window
/// never reaches it, low enough that a wedged destination can't grow one state row
/// without limit. See [`Engine::enqueue`].
const MAX_BUFFERED: usize = 1000;

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
    /// the event's repo; every matching normal route's destination receives it
    /// (fan-out). Fallback routes act only when no normal route matched the event, so a
    /// repo a normal route claims never also reaches the fallback, even if that
    /// route's destination is disabled. With no routes at all, every destination
    /// receives everything.
    fn destinations_for(&self, event: &Event) -> Vec<Arc<dyn Destination>> {
        if self.routes.is_empty() {
            return self.destinations.clone();
        }
        let repo = event.pull_request.repo.full_name();
        let matches = |r: &Route| {
            r.source == event.source_id
                && (r.repos.is_empty() || r.repos.iter().any(|p| pattern_matches(p, &repo)))
        };
        // A normal route claiming this event locks out the fallback bucket.
        let claimed = self.routes.iter().any(|r| !r.fallback && matches(r));
        self.destinations
            .iter()
            .filter(|n| {
                self.routes
                    .iter()
                    // Normal bucket when claimed, fallback bucket otherwise.
                    .filter(|r| r.fallback != claimed)
                    .any(|r| r.destination == n.id() && matches(r))
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
        // 1. Rule filter. A Defer is carried past the routing checks rather than
        // acted on here: there is no point buffering an event that dedup already
        // covers, or that no route would ever deliver.
        let deferred = match self.rules.decide(&event, ctx) {
            Decision::Deliver => None,
            Decision::Drop(reason) => {
                debug!(dedup_key = %event.dedup_key, ?reason, "event suppressed");
                return EventRecord {
                    event,
                    outcome: EventOutcome::Suppressed(reason),
                };
            }
            Decision::Defer(reason) => Some(reason),
        };

        let target_ids: Vec<String> = targets.iter().map(|n| n.id().to_string()).collect();

        if dry_run {
            // Ask the same dedup question the live path asks, against the same sink,
            // or the preview claims it would send things that are already handled.
            // Sources legitimately re-derive delivered events every pass - the GitLab
            // todos feed has no snapshot at all - so without this a dry run against a
            // live database reports the whole backlog as outgoing.
            //
            // Which sink depends on where the live path would put it, so this mirrors
            // the branch order below: a digest kind goes to the digest buffer, an
            // event a rule defers goes to the deferred buffer, and anything else is
            // checked per destination.
            let digested = self.digest_kinds.contains(event.kind.tag());
            let buffer_sink = if digested {
                Some(DIGEST_SINK)
            } else {
                deferred.as_ref().map(|_| DEFERRED_SINK)
            };
            if let Some(sink) = buffer_sink {
                match self.state.was_delivered(&event.dedup_key, sink).await {
                    Ok(true) => {
                        return EventRecord {
                            event,
                            outcome: EventOutcome::AlreadyDelivered,
                        };
                    }
                    Ok(false) => {}
                    Err(err) => {
                        warn!(dedup_key = %event.dedup_key, %err, "dedup check failed; previewing as deliverable");
                    }
                }
            }
            // A digest kind is reported by where it ends up, not by the window: the
            // live path buffers it for the digest regardless of the deferral.
            if let Some(reason) = deferred {
                if !digested {
                    return EventRecord {
                        event,
                        outcome: EventOutcome::WouldDefer(reason),
                    };
                }
            }
            let outcome = match self.undelivered_targets(&event, targets).await {
                Ok(pending) if pending.is_empty() && !targets.is_empty() => {
                    EventOutcome::AlreadyDelivered
                }
                // The pending subset, not every routed destination: after a partial
                // failure the live retry sends to the ones that missed it, so naming
                // all of them would preview a send that will not happen.
                Ok(pending) => EventOutcome::WouldDeliver {
                    to: pending.iter().map(|d| d.id().to_string()).collect(),
                },
                Err(err) => {
                    warn!(dedup_key = %event.dedup_key, %err, "dedup check failed; previewing as deliverable");
                    EventOutcome::WouldDeliver { to: target_ids }
                }
            };
            return EventRecord { event, outcome };
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

        // 2. Buffered paths. Both hand the event to a flush that will route it
        // later, so neither delivers here.
        //
        // Digest is checked first: a kind the user chose to batch is already
        // deferred by their own choice, so it keeps the digest's cadence rather than
        // being double-handled. The window is still honoured, by `flush_digest`
        // re-deciding each event before it sends.
        if self.digest_kinds.contains(event.kind.tag()) {
            return self
                .take_into_buffer(
                    event,
                    DIGEST_SINK,
                    DIGEST_SOURCE,
                    DIGEST_SCOPE,
                    "digest",
                    |_| EventOutcome::Digested,
                )
                .await;
        }

        // Quiet hours: buffer for release rather than discard, so the window costs
        // the user silence and not the notification.
        if let Some(reason) = deferred {
            return self
                .take_into_buffer(
                    event,
                    DEFERRED_SINK,
                    DEFERRED_SOURCE,
                    DEFERRED_SCOPE,
                    "deferred",
                    move |_| EventOutcome::Deferred(reason.clone()),
                )
                .await;
        }

        // 3. Dedup, per destination: an event that reached Slack but not email last
        // pass must retry email alone. Keyed on the pair, so a retry never re-pings
        // a destination that already took it.
        let pending = match self.undelivered_targets(&event, targets).await {
            Ok(p) => p,
            Err(err) => {
                // Fail safe: if we can't check dedup, treat as a delivery failure so
                // it is retried next pass rather than risk spamming.
                warn!(dedup_key = %event.dedup_key, %err, "dedup check failed");
                return EventRecord {
                    event,
                    outcome: EventOutcome::DeliveryFailed {
                        errors: vec![format!("dedup check failed: {err}")],
                    },
                };
            }
        };
        if pending.is_empty() {
            return EventRecord {
                event,
                outcome: EventOutcome::AlreadyDelivered,
            };
        }

        // 4. Deliver to every destination that still needs it.
        let (delivered_to, errors) = self.fan_out(&event, &pending).await;

        // Successes were already recorded per destination by `fan_out`. Provider
        // cursors only advance once every routed destination has the event, so a
        // partial failure still re-derives next pass - but the dedup sinks mean that
        // retry reaches only the destinations that actually failed.
        if errors.is_empty() {
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

    /// Send one event to each destination, recording every success against that
    /// destination's own dedup sink before moving on. Returns the ids that took it
    /// and the errors from those that didn't.
    ///
    /// Marking here, per destination and as each one lands, is what keeps a retry
    /// from re-pinging destinations that already took the event. Shared by the live
    /// path and the deferred flush so both behave identically.
    async fn fan_out(
        &self,
        event: &Event,
        targets: &[Arc<dyn Destination>],
    ) -> (Vec<String>, Vec<String>) {
        let mut delivered_to = Vec::new();
        let mut errors = Vec::new();
        for destination in targets {
            match destination.send(event, self.state.as_ref()).await {
                Ok(()) => {
                    if let Err(err) = self
                        .state
                        .mark_delivered(&event.dedup_key, destination.id())
                        .await
                    {
                        // The send happened; only the record of it failed. Warn
                        // rather than report failure, so the pass doesn't retry a
                        // destination that already has the message.
                        warn!(dedup_key = %event.dedup_key, destination = destination.id(), %err, "delivered but failed to persist the dedup key; it may re-send");
                    }
                    delivered_to.push(destination.id().to_string());
                }
                Err(err) => {
                    error!(destination = destination.id(), %err, "delivery failed");
                    errors.push(format!("{}: {err}", destination.id()));
                }
            }
        }
        (delivered_to, errors)
    }

    /// The subset of `targets` that has not already received `event`.
    async fn undelivered_targets(
        &self,
        event: &Event,
        targets: &[Arc<dyn Destination>],
    ) -> Result<Vec<Arc<dyn Destination>>, StateError> {
        let mut pending = Vec::new();
        for destination in targets {
            if !self
                .state
                .was_delivered(&event.dedup_key, destination.id())
                .await?
            {
                pending.push(destination.clone());
            }
        }
        Ok(pending)
    }

    /// Hand an event to one of the buffers, recording it against that buffer's sink
    /// so a re-derived copy is not buffered twice. Buffering before marking means a
    /// write failure leaves the event to be re-derived next pass rather than lost.
    async fn take_into_buffer(
        &self,
        event: Event,
        sink: &str,
        source: &str,
        scope: &str,
        label: &str,
        outcome: impl FnOnce(&Event) -> EventOutcome,
    ) -> EventRecord {
        match self.state.was_delivered(&event.dedup_key, sink).await {
            Ok(true) => {
                return EventRecord {
                    event,
                    outcome: EventOutcome::AlreadyDelivered,
                };
            }
            Ok(false) => {}
            Err(err) => {
                warn!(dedup_key = %event.dedup_key, %err, "dedup check failed");
                return EventRecord {
                    event,
                    outcome: EventOutcome::DeliveryFailed {
                        errors: vec![format!("dedup check failed: {err}")],
                    },
                };
            }
        }
        if let Err(err) = self.enqueue(source, scope, label, &event).await {
            warn!(dedup_key = %event.dedup_key, buffer = label, %err, "failed to buffer event");
            return EventRecord {
                event,
                outcome: EventOutcome::DeliveryFailed {
                    errors: vec![format!("{label} buffer: {err}")],
                },
            };
        }
        if let Err(err) = self.state.mark_delivered(&event.dedup_key, sink).await {
            warn!(dedup_key = %event.dedup_key, %err, "failed to persist dedup key");
        }
        debug!(dedup_key = %event.dedup_key, buffer = label, "event buffered");
        let outcome = outcome(&event);
        EventRecord { event, outcome }
    }

    /// The events currently sitting in one of the persisted buffers.
    ///
    /// A payload that can't be parsed is treated as empty, since nothing can be
    /// recovered from it. A *backend* failure is propagated instead: the two are not
    /// interchangeable, because `enqueue` reads, appends and writes back, so
    /// answering "empty" to a transient read error would overwrite the whole buffer
    /// with the one event being added.
    async fn read_buffer(
        &self,
        source: &str,
        scope: &str,
        label: &str,
    ) -> Result<Vec<Event>, StateError> {
        match self.state.get_snapshot(source, scope).await? {
            Some(bytes) => Ok(serde_json::from_slice(&bytes).unwrap_or_else(|err| {
                error!(%err, buffer = label, "buffer is unreadable; discarding it");
                Vec::new()
            })),
            None => Ok(Vec::new()),
        }
    }

    /// Append an event to one of the persisted buffers, unless it is already there.
    ///
    /// Idempotent on `dedup_key`, because `take_into_buffer` marks the buffer's sink
    /// only after this succeeds: if that mark fails, the event re-derives next pass
    /// and arrives here a second time. Appending it twice would mean two identical
    /// pings on release.
    ///
    /// Capped, because the buffer is one state row rewritten in full on every
    /// append. Over a long window with a wedged destination it would otherwise grow
    /// without bound, and quadratically in write cost. At the cap the oldest event
    /// is dropped, which is a loss, but a bounded one that is loudly logged.
    async fn enqueue(
        &self,
        source: &str,
        scope: &str,
        label: &str,
        event: &Event,
    ) -> Result<(), StateError> {
        let mut pending = self.read_buffer(source, scope, label).await?;
        if pending.iter().any(|e| e.dedup_key == event.dedup_key) {
            debug!(dedup_key = %event.dedup_key, buffer = label, "already buffered");
            return Ok(());
        }
        pending.push(event.clone());
        while pending.len() > MAX_BUFFERED {
            let dropped = pending.remove(0);
            error!(
                dedup_key = %dropped.dedup_key,
                buffer = label,
                cap = MAX_BUFFERED,
                "buffer is full; dropping the oldest held event"
            );
        }
        self.write_buffer(source, scope, &pending).await
    }

    /// Overwrite one of the persisted buffers with `events`.
    async fn write_buffer(
        &self,
        source: &str,
        scope: &str,
        events: &[Event],
    ) -> Result<(), StateError> {
        let bytes = serde_json::to_vec(events).map_err(|e| StateError::Serde(e.to_string()))?;
        self.state.put_snapshot(source, scope, &bytes).await
    }

    /// The events currently buffered for the next digest flush.
    async fn read_digest(&self) -> Result<Vec<Event>, StateError> {
        self.read_buffer(DIGEST_SOURCE, DIGEST_SCOPE, "digest")
            .await
    }

    /// Flush the buffered digest: one batched message per destination (only the
    /// events routed to it), then rewrite the buffer with whatever is left. Called
    /// by the daemon once per `digest.interval_secs`. Events a deferring rule holds
    /// back move to the held row, which [`Engine::release_digest`] retries every poll
    /// pass, so the interval never gates their release.
    ///
    /// If any destination fails, the batch is kept and retried on the next interval;
    /// the per-destination dedup sinks mean that retry only reaches the destinations
    /// that actually missed it.
    ///
    /// Events a deferring rule currently holds back are left in the buffer, so a
    /// digest never breaks a quiet window. Without that, batching a kind would opt
    /// it out of quiet hours: the flush runs on its own interval and would happily
    /// fire at 02:00.
    pub async fn flush_digest(&self, ctx: &FilterContext) -> DigestFlush {
        let buffered = match self.read_digest().await {
            Ok(p) => p,
            Err(err) => {
                warn!(%err, "could not read the digest buffer; leaving it in place");
                return DigestFlush::default();
            }
        };
        if buffered.is_empty() {
            return DigestFlush::default();
        }
        // Re-decided exactly as `flush_deferred` does, and for the same reasons: a
        // rule the user changed while events sat in the buffer applies on the way
        // out. Deferred events wait for the window; dropped ones are discarded, so
        // an overnight mute is honoured here too and not only on the deferred path.
        let mut pending = Vec::new();
        let mut hold = Vec::new();
        let mut dropped = 0usize;
        for event in buffered {
            match self.rules.decide(&event, ctx) {
                Decision::Deliver => pending.push(event),
                Decision::Defer(_) => hold.push(event),
                Decision::Drop(reason) => {
                    debug!(dedup_key = %event.dedup_key, ?reason, "digest event dropped on flush");
                    dropped += 1;
                }
            }
        }
        if !hold.is_empty() {
            debug!(
                count = hold.len(),
                "holding digest events inside a quiet window"
            );
        }
        if pending.is_empty() {
            // Nothing to send this interval, but the buffer still has to be rewritten:
            // a rule may have dropped events, and anything the window held moves to
            // the held row, which `release_digest` retries every pass.
            if let Err(err) = self.park_held(&hold).await {
                warn!(%err, "could not park the held digest events");
                return DigestFlush {
                    sent: 0,
                    held: hold.len(),
                    failed: true,
                };
            }
            return DigestFlush {
                sent: 0,
                held: hold.len(),
                failed: false,
            };
        }

        let (sent, all_ok) = self.send_digest_batches_checked(&pending).await;

        if !all_ok {
            return DigestFlush {
                sent: 0,
                held: hold.len(),
                failed: true,
            };
        }
        // If the buffer can't be rewritten, don't report success: the events are
        // still buffered and would re-send next flush, so surface it as a non-clean
        // flush. What was held back for quiet hours stays.
        if let Err(err) = self.park_held(&hold).await {
            warn!(%err, "digest sent but the buffer could not be cleared; it may re-send next flush");
            return DigestFlush {
                sent: 0,
                held: hold.len(),
                failed: true,
            };
        }
        info!(
            count = sent.len(),
            held = hold.len(),
            dropped,
            "digest flushed"
        );
        // `sent`, not `pending`: an event every routed destination already had is
        // dropped from the batch, so counting the buffer would report a send that
        // never happened.
        DigestFlush {
            sent: sent.len(),
            held: hold.len(),
            failed: false,
        }
    }

    /// Batch `events` per destination and send. Shared by the interval flush and the
    /// held-event release so both route, dedup and record identically.
    ///
    /// Returns the keys that reached at least one destination, and whether every
    /// attempt succeeded.
    async fn send_digest_batches_checked(&self, events: &[Event]) -> (HashSet<String>, bool) {
        let mut all_ok = true;
        // Keys that reached at least one destination, so callers count what was
        // actually sent rather than what happened to be in the buffer.
        let mut sent: HashSet<String> = HashSet::new();
        for dest in &self.destinations {
            // Routed here, and not already sent here. The second half matters
            // because the digest branch runs ahead of the per-destination dedup
            // check: an event delivered live, then added to `digest.kinds` by the
            // user, re-derives and buffers with only its `__digest__` sink marked.
            // Without this filter the flush would send it a second time.
            let mut batch = Vec::new();
            for event in events {
                if !self
                    .destinations_for(event)
                    .iter()
                    .any(|d| d.id() == dest.id())
                {
                    continue;
                }
                // Exact, not the wildcard-tolerant check: an event already in the
                // buffer when an older database was migrated carries a record that
                // matches every sink, because the pre-per-sink code marked a digest
                // event delivered as soon as it was enqueued. Folding that in here
                // would empty every batch and then clear the buffer, silently losing
                // exactly the events the user asked to have batched.
                match self
                    .state
                    .was_delivered_exact(&event.dedup_key, dest.id())
                    .await
                {
                    Ok(false) => batch.push(event.clone()),
                    Ok(true) => {}
                    Err(err) => {
                        // Fail safe: skip it this flush rather than risk a duplicate.
                        warn!(dedup_key = %event.dedup_key, %err, "dedup check failed for the digest");
                        all_ok = false;
                    }
                }
            }
            if batch.is_empty() {
                continue;
            }
            match dest.send_digest(&batch, self.state.as_ref()).await {
                Ok(()) => {
                    // Recorded per destination as the live path does, so a retry
                    // after another destination fails doesn't re-send this batch.
                    for event in &batch {
                        if let Err(err) =
                            self.state.mark_delivered(&event.dedup_key, dest.id()).await
                        {
                            warn!(dedup_key = %event.dedup_key, destination = dest.id(), %err, "digest sent but the dedup key did not persist");
                        }
                        sent.insert(event.dedup_key.clone());
                    }
                }
                Err(err) => {
                    error!(destination = dest.id(), %err, "digest flush failed");
                    all_ok = false;
                }
            }
        }
        (sent, all_ok)
    }

    /// [`Engine::send_digest_batches_checked`] without the success flag.
    async fn send_digest_batches(&self, events: &[Event]) -> HashSet<String> {
        self.send_digest_batches_checked(events).await.0
    }

    /// Empty `pending` and move what the window held onto the held row, appending to
    /// whatever is already parked there.
    async fn park_held(&self, hold: &[Event]) -> Result<(), StateError> {
        if !hold.is_empty() {
            let mut parked = self
                .read_buffer(DIGEST_SOURCE, DIGEST_HELD_SCOPE, "digest-held")
                .await?;
            let known: HashSet<&str> = parked.iter().map(|e| e.dedup_key.as_str()).collect();
            let fresh: Vec<Event> = hold
                .iter()
                .filter(|e| !known.contains(e.dedup_key.as_str()))
                .cloned()
                .collect();
            if !fresh.is_empty() {
                parked.extend(fresh);
                self.write_buffer(DIGEST_SOURCE, DIGEST_HELD_SCOPE, &parked)
                    .await?;
            }
        }
        self.write_buffer(DIGEST_SOURCE, DIGEST_SCOPE, &[]).await
    }

    /// Send digest events a quiet window held back, as soon as it stops applying.
    ///
    /// Called every poll pass, unlike [`Engine::flush_digest`], which runs on
    /// `digest.interval_secs`. The interval governs how often a *new* batch is sent;
    /// it is the wrong clock for releasing one the window has already delayed. With a
    /// daily interval whose phase falls inside the window, one clock for both jobs
    /// left the held events waiting a further day each time, or indefinitely.
    ///
    /// Returns how many events were released.
    pub async fn release_digest(&self, ctx: &FilterContext) -> usize {
        let parked = match self
            .read_buffer(DIGEST_SOURCE, DIGEST_HELD_SCOPE, "digest-held")
            .await
        {
            Ok(p) => p,
            Err(err) => {
                warn!(%err, "could not read the held digest events; leaving them in place");
                return 0;
            }
        };
        if parked.is_empty() {
            return 0;
        }
        let before = parked.len();
        let mut keep = Vec::new();
        let mut ready = Vec::new();
        for event in parked {
            match self.rules.decide(&event, ctx) {
                Decision::Deliver => ready.push(event),
                Decision::Defer(_) => keep.push(event),
                Decision::Drop(reason) => {
                    debug!(dedup_key = %event.dedup_key, ?reason, "held digest event dropped on release")
                }
            }
        }
        if ready.is_empty() {
            if keep.len() != before {
                if let Err(err) = self
                    .write_buffer(DIGEST_SOURCE, DIGEST_HELD_SCOPE, &keep)
                    .await
                {
                    warn!(%err, "could not rewrite the held digest events");
                }
            }
            return 0;
        }

        let sent = self.send_digest_batches(&ready).await;
        if sent.is_empty() {
            // Nothing landed, so keep everything for the next pass.
            return 0;
        }
        keep.extend(ready.into_iter().filter(|e| !sent.contains(&e.dedup_key)));
        if let Err(err) = self
            .write_buffer(DIGEST_SOURCE, DIGEST_HELD_SCOPE, &keep)
            .await
        {
            warn!(%err, "released held digest events but could not rewrite the row; they may re-send");
        }
        info!(count = sent.len(), "released held digest events");
        sent.len()
    }

    /// Release events held by a deferring rule (quiet hours) now that the rule may
    /// no longer apply. Called by the daemon each pass; cheap when the buffer is
    /// empty.
    ///
    /// Every buffered event is re-decided against the current rules and `ctx`
    /// rather than against whatever was true when it was buffered. That keeps
    /// per-repo quiet-hours overrides working without recording which window
    /// deferred what, and means a rule the user changed overnight (a new mute, a
    /// repo deny) still applies on the way out. An event a rule now drops is
    /// discarded; one still inside its window stays buffered.
    ///
    /// Returns how many events were delivered.
    ///
    /// Note the source's `commit` hook does not run for a released event, so with
    /// `github.mark_read = true` a deferred notification's thread is not marked read
    /// on the forge. The digest path has always behaved the same way. Deliberate,
    /// not an oversight: `commit` needs the `Source`, which a flush does not have.
    pub async fn flush_deferred(&self, ctx: &FilterContext) -> usize {
        let pending = match self
            .read_buffer(DEFERRED_SOURCE, DEFERRED_SCOPE, "deferred")
            .await
        {
            Ok(p) => p,
            Err(err) => {
                warn!(%err, "could not read the deferred buffer; leaving it in place");
                return 0;
            }
        };
        if pending.is_empty() {
            return 0;
        }

        let buffered = pending.len();
        let mut keep = Vec::new();
        let mut delivered = 0usize;
        let mut dropped = 0usize;
        for event in pending {
            match self.rules.decide(&event, ctx) {
                // Still inside the window: hold it for the next attempt.
                Decision::Defer(_) => keep.push(event),
                // A rule that changed while it sat in the buffer now drops it.
                Decision::Drop(reason) => {
                    debug!(dedup_key = %event.dedup_key, ?reason, "deferred event dropped on flush");
                    dropped += 1;
                }
                Decision::Deliver => {
                    let targets = self.destinations_for(&event);
                    // Only the destinations that don't have it yet: a previous flush
                    // may have delivered to some of them before failing on the rest.
                    let targets = match self.undelivered_targets(&event, &targets).await {
                        Ok(t) => t,
                        Err(err) => {
                            warn!(dedup_key = %event.dedup_key, %err, "dedup check failed on flush; keeping it buffered");
                            keep.push(event);
                            continue;
                        }
                    };
                    if targets.is_empty() {
                        debug!(dedup_key = %event.dedup_key, "deferred event has no destination left to reach");
                        dropped += 1;
                        continue;
                    }
                    // The event was recorded against the buffer's sink when it was
                    // taken in, so a failure here can't be recovered by re-deriving
                    // it. Keep it buffered and retry on the next flush; `fan_out`
                    // marked whatever did land, so that retry skips those.
                    let (_, errors) = self.fan_out(&event, &targets).await;
                    if errors.is_empty() {
                        delivered += 1;
                    } else {
                        warn!(dedup_key = %event.dedup_key, errors = ?errors, "deferred event partially delivered; keeping it buffered for the rest");
                        keep.push(event);
                    }
                }
            }
        }

        // Rewrite only when something actually left the buffer. A write failure
        // leaves released events in it, so they would send again next flush: loud,
        // because that is a duplicate ping.
        if keep.len() != buffered {
            if let Err(err) = self
                .write_buffer(DEFERRED_SOURCE, DEFERRED_SCOPE, &keep)
                .await
            {
                // Same contract as `flush_digest`: the events did send, but they are
                // still in the buffer and will send again, so this is not a clean
                // flush and the caller must not report it as one.
                error!(%err, "deferred events were released but the buffer could not be updated; they may re-send");
                return 0;
            }
            info!(
                delivered,
                dropped,
                still_deferred = keep.len(),
                "released deferred events"
            );
        }
        delivered
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
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex;
    use time::OffsetDateTime;

    /// The sentinel the SQLite store writes for records migrated from before
    /// deliveries were tracked per sink. Mirrored here so the doubles answer the two
    /// lookups the way the real store does.
    const ANY: &str = "*";

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
        async fn was_delivered(&self, k: &str, sink: &str) -> Result<bool, StateError> {
            let d = self.delivered.lock().unwrap();
            // Mirrors the SQLite store: a record written before deliveries were
            // tracked per sink stands for every sink.
            Ok(d.contains(&format!("{k}@{sink}")) || d.contains(&format!("{k}@{ANY}")))
        }
        async fn was_delivered_exact(&self, k: &str, sink: &str) -> Result<bool, StateError> {
            Ok(self
                .delivered
                .lock()
                .unwrap()
                .contains(&format!("{k}@{sink}")))
        }
        async fn mark_delivered(&self, k: &str, sink: &str) -> Result<(), StateError> {
            self.delivered.lock().unwrap().insert(format!("{k}@{sink}"));
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

    /// Wraps a store and fails every `get_snapshot`, to exercise the difference
    /// between "buffer is empty" and "the store could not be read".
    struct ReadFails(Arc<MemState>);

    #[async_trait]
    impl StateStore for ReadFails {
        async fn get_snapshot(&self, _: &str, _: &str) -> Result<Option<Vec<u8>>, StateError> {
            Err(StateError::Backend("database is locked".into()))
        }
        async fn put_snapshot(&self, s: &str, scope: &str, b: &[u8]) -> Result<(), StateError> {
            self.0.put_snapshot(s, scope, b).await
        }
        async fn was_delivered(&self, k: &str, sink: &str) -> Result<bool, StateError> {
            self.0.was_delivered(k, sink).await
        }
        async fn was_delivered_exact(&self, k: &str, sink: &str) -> Result<bool, StateError> {
            self.0.was_delivered_exact(k, sink).await
        }
        async fn mark_delivered(&self, k: &str, sink: &str) -> Result<(), StateError> {
            self.0.mark_delivered(k, sink).await
        }
        async fn get_cursor(&self, s: &str, k: &str) -> Result<Option<String>, StateError> {
            self.0.get_cursor(s, k).await
        }
        async fn put_cursor(&self, s: &str, k: &str, v: &str) -> Result<(), StateError> {
            self.0.put_cursor(s, k, v).await
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
        /// Whether `send` errors. Settable mid-test so a destination can recover and
        /// the retry behaviour after a partial failure can be observed.
        fail: AtomicBool,
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
        async fn send(
            &self,
            event: &Event,
            _state: &dyn StateStore,
        ) -> Result<(), DestinationError> {
            if self.fail.load(Ordering::Relaxed) {
                return Err(DestinationError::Delivery("boom".into()));
            }
            self.sent.lock().unwrap().push(event.dedup_key.clone());
            Ok(())
        }
        async fn send_digest(
            &self,
            events: &[Event],
            _state: &dyn StateStore,
        ) -> Result<(), DestinationError> {
            if self.fail.load(Ordering::Relaxed) {
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

    /// The dedup keys parked on the digest's held row.
    async fn digest_held(engine: &Engine) -> Vec<String> {
        engine
            .read_buffer(DIGEST_SOURCE, DIGEST_HELD_SCOPE, "digest-held")
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.dedup_key)
            .collect()
    }

    /// Like [`ev`], but in a named repo, so per-repo rule overrides can be exercised.
    fn ev_in(kind: EventKind, owner: &str, name: &str, key: &str) -> Event {
        let mut e = ev(kind, key);
        e.pull_request.repo = Repo::new(owner, name);
        e
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
        assert!(!state.was_delivered("k1", "mock-notify").await.unwrap());
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
        let flushed = engine.flush_digest(&FilterContext::default()).await;
        assert_eq!(flushed.sent, 1);
        assert_eq!(
            dest.digests.lock().unwrap().as_slice(),
            &[vec!["k1".to_string()]]
        );

        // A second flush finds an empty buffer and does nothing.
        assert_eq!(engine.flush_digest(&FilterContext::default()).await.sent, 0);
        assert_eq!(dest.digests.lock().unwrap().len(), 1);
    }

    /// The dedup keys sitting in the deferred buffer, for asserting on what a flush
    /// kept versus cleared (a flush returning 0 alone can't tell those apart).
    async fn buffered(state: &MemState) -> Vec<String> {
        let bytes = state
            .get_snapshot(DEFERRED_SOURCE, DEFERRED_SCOPE)
            .await
            .unwrap()
            .unwrap_or_else(|| b"[]".to_vec());
        serde_json::from_slice::<Vec<Event>>(&bytes)
            .unwrap()
            .into_iter()
            .map(|e| e.dedup_key)
            .collect()
    }

    /// A quiet window of 22:00-08:00, and the two clock readings either side of it.
    fn quiet_rules() -> RuleConfig {
        RuleConfig {
            quiet_hours: crate::config::QuietHours {
                enabled: true,
                start: "22:00".into(),
                end: "08:00".into(),
            },
            ..Default::default()
        }
    }
    const INSIDE_WINDOW: FilterContext = FilterContext {
        local_minutes: Some(23 * 60),
    };
    const OUTSIDE_WINDOW: FilterContext = FilterContext {
        local_minutes: Some(12 * 60),
    };

    #[tokio::test]
    async fn quiet_hours_defers_then_releases_after_the_window() {
        let dest = Arc::new(MockDestination::default());
        let (engine, state) = engine_with(
            vec![ev(EventKind::ReviewRequested, "k1")],
            quiet_rules(),
            dest.clone(),
        );

        // Inside the window: held, not sent, and not lost.
        let r = engine.run_once(INSIDE_WINDOW, false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::Deferred(DeferReason::QuietHours)
        ));
        assert!(dest.sent.lock().unwrap().is_empty());

        // Still inside: the flush is a no-op and the event stays buffered.
        assert_eq!(engine.flush_deferred(&INSIDE_WINDOW).await, 0);
        assert!(dest.sent.lock().unwrap().is_empty());

        // Window over: released exactly once.
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(dest.sent.lock().unwrap().as_slice(), &["k1".to_string()]);
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
        assert_eq!(dest.sent.lock().unwrap().len(), 1);
        assert!(buffered(&state).await.is_empty());
    }

    /// The regression this whole path exists for: the snapshot advances past a
    /// deferred event (so it never re-derives), which is exactly why dropping it
    /// used to lose it for good.
    #[tokio::test]
    async fn deferred_event_survives_the_snapshot_advancing() {
        let dest = Arc::new(MockDestination::default());
        let source = Arc::new(MockSource {
            events: vec![ev(EventKind::ReviewRequested, "k1")],
            ..Default::default()
        });
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![source.clone()],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            state.clone(),
        );

        engine.run_once(INSIDE_WINDOW, false).await;
        // Nothing was held back, so the source committed the event's scope.
        let committed = source.committed.lock().unwrap().clone();
        assert_eq!(committed, vec![HashSet::new()]);

        // A later pass re-polls the same event; dedup keeps it from buffering twice.
        let r = engine.run_once(INSIDE_WINDOW, false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::AlreadyDelivered
        ));

        // One copy comes out when the window ends.
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(dest.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deferred_event_that_fails_to_deliver_stays_buffered() {
        let dest = Arc::new(MockDestination {
            fail: AtomicBool::new(true),
            ..Default::default()
        });
        let (engine, state) = engine_with(
            vec![ev(EventKind::ReviewRequested, "k1")],
            quiet_rules(),
            dest.clone(),
        );
        engine.run_once(INSIDE_WINDOW, false).await;

        // Delivery fails on release, so nothing is reported and it is kept. It was
        // already marked delivered when buffered, so losing it here would lose it
        // for good.
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
        assert_eq!(buffered(&state).await, vec!["k1".to_string()]);
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
        assert_eq!(buffered(&state).await, vec!["k1".to_string()]);
        assert!(dest.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn deferred_event_is_re_decided_against_current_rules() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::ReviewRequested, "k1")],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            state.clone(),
        );
        engine.run_once(INSIDE_WINDOW, false).await;

        // Rebuild the engine as if the user disabled the kind overnight, keeping the
        // same state (and so the same buffer).
        let mut rules = quiet_rules();
        rules.events.review_requested = false;
        let engine = Engine::new(
            vec![Arc::new(MockSource::default())],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(rules).unwrap(),
            state.clone(),
        );

        // The new rule drops it on the way out; it is not delivered, and the buffer
        // is cleared rather than retrying forever.
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
        assert!(dest.sent.lock().unwrap().is_empty());
        assert!(buffered(&state).await.is_empty());
    }

    #[tokio::test]
    async fn dry_run_reports_deferral_without_buffering() {
        let dest = Arc::new(MockDestination::default());
        let (engine, _state) = engine_with(
            vec![ev(EventKind::ReviewRequested, "k1")],
            quiet_rules(),
            dest.clone(),
        );

        let r = engine.run_once(INSIDE_WINDOW, true).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::WouldDefer(DeferReason::QuietHours)
        ));
        // A dry run advances nothing, so there is nothing to release.
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
        assert!(dest.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rule_that_drops_wins_over_quiet_hours() {
        let dest = Arc::new(MockDestination::default());
        let mut rules = quiet_rules();
        rules.events.review_requested = false;
        let (engine, _state) = engine_with(
            vec![ev(EventKind::ReviewRequested, "k1")],
            rules,
            dest.clone(),
        );

        // Suppressed outright, not parked in the buffer: the user said never, not later.
        let r = engine.run_once(INSIDE_WINDOW, false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::Suppressed(DropReason::EventKindDisabled)
        ));
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
    }

    /// Build an engine fanning one event out to two destinations, with no routes
    /// (so both receive everything).
    fn engine_fanning_out(
        a: Arc<MockDestination>,
        b: Arc<MockDestination>,
    ) -> (Engine, Arc<MemState>) {
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::ReviewRequested, "k1")],
                ..Default::default()
            })],
            vec![a, b],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state.clone(),
        );
        (engine, state)
    }

    /// The bug this change fixes: with two destinations and one of them failing,
    /// every later pass used to re-send to the one that had already succeeded.
    #[tokio::test]
    async fn partial_failure_retries_only_the_failed_destination() {
        let good = Arc::new(MockDestination {
            id: "good".into(),
            ..Default::default()
        });
        let bad = Arc::new(MockDestination {
            id: "bad".into(),
            fail: AtomicBool::new(true),
            ..Default::default()
        });
        let (engine, _state) = engine_fanning_out(good.clone(), bad.clone());

        // Pass 1: good takes it, bad fails, so the event is not fully delivered.
        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::DeliveryFailed { .. }
        ));
        assert_eq!(good.sent.lock().unwrap().as_slice(), &["k1".to_string()]);

        // Pass 2: the source re-derives the event (the scope was held back). good
        // must not hear about it a second time.
        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::DeliveryFailed { .. }
        ));
        assert_eq!(
            good.sent.lock().unwrap().len(),
            1,
            "a destination that already took the event must not be re-sent to"
        );

        // Pass 3: bad recovers and gets exactly one copy; good still has one.
        bad.fail.store(false, Ordering::Relaxed);
        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(
            matches!(&r.records[0].outcome, EventOutcome::Delivered { to } if to == &["bad".to_string()]),
            "only the destination that still needed it should be reported: {:?}",
            r.records[0].outcome
        );
        assert_eq!(good.sent.lock().unwrap().len(), 1);
        assert_eq!(bad.sent.lock().unwrap().as_slice(), &["k1".to_string()]);
    }

    /// Once every destination has it, the event is fully deduped again.
    #[tokio::test]
    async fn event_delivered_everywhere_is_already_delivered() {
        let a = Arc::new(MockDestination {
            id: "a".into(),
            ..Default::default()
        });
        let b = Arc::new(MockDestination {
            id: "b".into(),
            ..Default::default()
        });
        let (engine, _state) = engine_fanning_out(a.clone(), b.clone());

        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::Delivered { .. }
        ));

        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::AlreadyDelivered
        ));
        assert_eq!(a.sent.lock().unwrap().len(), 1);
        assert_eq!(b.sent.lock().unwrap().len(), 1);
    }

    /// A deferred event released while one destination is down must not re-send to
    /// the other when the flush retries. Same guarantee as the live path, on the
    /// path added for quiet hours.
    #[tokio::test]
    async fn deferred_release_retries_only_the_failed_destination() {
        let good = Arc::new(MockDestination {
            id: "good".into(),
            ..Default::default()
        });
        let bad = Arc::new(MockDestination {
            id: "bad".into(),
            fail: AtomicBool::new(true),
            ..Default::default()
        });
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::ReviewRequested, "k1")],
                ..Default::default()
            })],
            vec![good.clone(), bad.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            state.clone(),
        );

        engine.run_once(INSIDE_WINDOW, false).await;
        assert!(good.sent.lock().unwrap().is_empty());

        // First release: good takes it, bad fails, so it stays buffered for bad.
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
        assert_eq!(good.sent.lock().unwrap().as_slice(), &["k1".to_string()]);
        assert_eq!(buffered(&state).await, vec!["k1".to_string()]);

        // Second release with bad still down: good must not get it again.
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 0);
        assert_eq!(good.sent.lock().unwrap().len(), 1);

        // bad recovers: it gets one copy and the buffer finally empties.
        bad.fail.store(false, Ordering::Relaxed);
        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(good.sent.lock().unwrap().len(), 1);
        assert_eq!(bad.sent.lock().unwrap().as_slice(), &["k1".to_string()]);
        assert!(buffered(&state).await.is_empty());
    }

    /// A transient store read must not be mistaken for an empty buffer: `enqueue`
    /// reads, appends and writes back, so treating a read failure as empty would
    /// overwrite everything held with the one event being added.
    #[tokio::test]
    async fn a_read_failure_does_not_clobber_the_buffer() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());

        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![
                    ev(EventKind::ReviewRequested, "k1"),
                    ev(EventKind::Mentioned, "k2"),
                ],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            state.clone(),
        );
        engine.run_once(INSIDE_WINDOW, false).await;
        assert_eq!(buffered(&state).await.len(), 2);

        // A third event arrives while reads are failing.
        let failing = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::ReviewRequested, "k3")],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            Arc::new(ReadFails(state.clone())),
        );
        let r = failing.run_once(INSIDE_WINDOW, false).await;
        assert!(
            matches!(r.records[0].outcome, EventOutcome::DeliveryFailed { .. }),
            "a read failure must surface, so the scope is held back and re-derives"
        );
        assert_eq!(
            buffered(&state).await,
            vec!["k1".to_string(), "k2".to_string()],
            "the held events must survive a failed enqueue"
        );
    }

    /// If marking the buffer's sink fails, the event re-derives and reaches
    /// `enqueue` again. Buffering it twice would mean two identical pings on release.
    #[tokio::test]
    async fn buffering_the_same_event_twice_holds_one_copy() {
        let dest = Arc::new(MockDestination::default());
        let (engine, state) = engine_with(vec![], quiet_rules(), dest.clone());

        for _ in 0..2 {
            engine
                .enqueue(
                    DEFERRED_SOURCE,
                    DEFERRED_SCOPE,
                    "deferred",
                    &ev(EventKind::ReviewRequested, "k1"),
                )
                .await
                .unwrap();
        }
        assert_eq!(buffered(&state).await, vec!["k1".to_string()]);

        assert_eq!(engine.flush_deferred(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(dest.sent.lock().unwrap().len(), 1, "one ping, not two");
    }

    /// A wedged destination plus a long window must not grow one state row for ever.
    #[tokio::test]
    async fn the_buffer_is_capped() {
        let dest = Arc::new(MockDestination::default());
        let (engine, state) = engine_with(vec![], quiet_rules(), dest);

        // Seeded directly: appending one at a time rewrites the whole row each
        // time, which is exactly the cost the cap exists to bound.
        let seed: Vec<Event> = (0..MAX_BUFFERED)
            .map(|i| ev(EventKind::Mentioned, &format!("k{i}")))
            .collect();
        engine
            .write_buffer(DEFERRED_SOURCE, DEFERRED_SCOPE, &seed)
            .await
            .unwrap();
        for i in MAX_BUFFERED..(MAX_BUFFERED + 5) {
            engine
                .enqueue(
                    DEFERRED_SOURCE,
                    DEFERRED_SCOPE,
                    "deferred",
                    &ev(EventKind::Mentioned, &format!("k{i}")),
                )
                .await
                .unwrap();
        }
        let held = buffered(&state).await;
        assert_eq!(held.len(), MAX_BUFFERED);
        // The oldest went, the newest stayed.
        assert_eq!(held.first().unwrap(), "k5");
        assert_eq!(held.last().unwrap(), &format!("k{}", MAX_BUFFERED + 4));
    }

    /// Batching a kind must not opt it out of quiet hours. The digest flush runs on
    /// its own interval, so without this it would fire inside the window.
    #[tokio::test]
    async fn digest_does_not_flush_inside_a_quiet_window() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::Mentioned, "k1")],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));

        let r = engine.run_once(INSIDE_WINDOW, false).await;
        assert!(matches!(r.records[0].outcome, EventOutcome::Digested));

        // A flush inside the window sends nothing and parks the batch on the held
        // row, which is retried every pass rather than on the digest interval.
        assert_eq!(engine.flush_digest(&INSIDE_WINDOW).await.sent, 0);
        assert!(
            dest.digests.lock().unwrap().is_empty(),
            "a digest must not break the quiet window"
        );
        assert_eq!(digest_held(&engine).await, vec!["k1".to_string()]);
        assert_eq!(engine.release_digest(&INSIDE_WINDOW).await, 0);

        // Once the window is over it goes out, exactly once.
        assert_eq!(engine.release_digest(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(
            dest.digests.lock().unwrap().as_slice(),
            &[vec!["k1".to_string()]]
        );
        assert_eq!(engine.release_digest(&OUTSIDE_WINDOW).await, 0);
        assert!(digest_held(&engine).await.is_empty());
    }

    /// A flush that sends some of the buffer and holds the rest must write back what
    /// it held, not clear the row. A per-repo override makes one event deliverable
    /// and the other quiet at the same instant.
    #[tokio::test]
    async fn a_partial_digest_flush_keeps_the_held_events() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let rules = RuleConfig {
            quiet_hours: crate::config::QuietHours {
                enabled: true,
                start: "22:00".into(),
                end: "08:00".into(),
            },
            overrides: vec![crate::config::RuleOverride {
                repos: vec!["loud/*".into()],
                quiet_hours: crate::config::QuietHoursOverride {
                    enabled: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![
                    ev_in(EventKind::Mentioned, "loud", "repo", "loud1"),
                    ev_in(EventKind::Mentioned, "quiet", "repo", "quiet1"),
                ],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(rules).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));

        engine.run_once(INSIDE_WINDOW, false).await;

        // Inside the window: only the override'd repo's event goes out.
        assert_eq!(engine.flush_digest(&INSIDE_WINDOW).await.sent, 1);
        assert_eq!(
            dest.digests.lock().unwrap().as_slice(),
            &[vec!["loud1".to_string()]]
        );

        // The quiet one moves to the held row rather than being wiped with the
        // batch, and `pending` is emptied so the next interval starts clean.
        assert!(engine.read_digest().await.unwrap().is_empty());
        assert_eq!(digest_held(&engine).await, vec!["quiet1".to_string()]);

        // And it goes out once the window ends, without waiting for an interval.
        assert_eq!(engine.release_digest(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(
            dest.digests.lock().unwrap().last().unwrap(),
            &vec!["quiet1".to_string()]
        );
    }

    /// A dry run must answer the same dedup question the live pass answers. Sources
    /// re-derive delivered events every poll, so without this the preview reports a
    /// whole backlog as outgoing.
    #[tokio::test]
    async fn dry_run_reports_already_delivered() {
        let dest = Arc::new(MockDestination::default());
        let (engine, _state) = engine_with(
            vec![ev(EventKind::ReviewRequested, "k1")],
            RuleConfig::default(),
            dest.clone(),
        );

        // Deliver for real, then preview the same event again.
        assert_eq!(
            engine
                .run_once(FilterContext::default(), false)
                .await
                .delivered_count(),
            1
        );
        let r = engine.run_once(FilterContext::default(), true).await;
        assert!(
            matches!(r.records[0].outcome, EventOutcome::AlreadyDelivered),
            "preview must not claim it would re-send: {:?}",
            r.records[0].outcome
        );
        assert_eq!(dest.sent.lock().unwrap().len(), 1);
    }

    /// Turning on `digest.kinds` for a kind already delivered live must not re-notify
    /// when the re-derived event lands in the digest buffer.
    #[tokio::test]
    async fn digest_does_not_resend_what_was_already_delivered() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let events = vec![ev(EventKind::Mentioned, "k1")];

        // Delivered live first.
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: events.clone(),
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state.clone(),
        );
        assert_eq!(
            engine
                .run_once(FilterContext::default(), false)
                .await
                .delivered_count(),
            1
        );

        // The user now batches that kind; the source re-derives the same event.
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events,
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));
        engine.run_once(FilterContext::default(), false).await;

        // The flush must find nothing left to send to a destination that has it.
        assert_eq!(engine.flush_digest(&FilterContext::default()).await.sent, 0);
        assert!(
            dest.digests.lock().unwrap().is_empty(),
            "already-delivered event must not come back as a digest"
        );
        assert_eq!(dest.sent.lock().unwrap().len(), 1);
    }

    /// A digest that reaches one destination and fails at another must not re-send
    /// to the first when the buffer is retried.
    #[tokio::test]
    async fn digest_retry_skips_destinations_that_got_the_batch() {
        let good = Arc::new(MockDestination {
            id: "good".into(),
            ..Default::default()
        });
        let bad = Arc::new(MockDestination {
            id: "bad".into(),
            fail: AtomicBool::new(true),
            ..Default::default()
        });
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::Mentioned, "k1")],
                ..Default::default()
            })],
            vec![good.clone(), bad.clone()],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));
        engine.run_once(FilterContext::default(), false).await;

        // First flush: good takes the batch, bad fails, so the buffer is kept.
        assert_eq!(engine.flush_digest(&FilterContext::default()).await.sent, 0);
        assert_eq!(good.digests.lock().unwrap().len(), 1);

        // Retry with bad still down: good must not get the batch again.
        assert_eq!(engine.flush_digest(&FilterContext::default()).await.sent, 0);
        assert_eq!(good.digests.lock().unwrap().len(), 1);

        // bad recovers and gets it once.
        bad.fail.store(false, Ordering::Relaxed);
        assert_eq!(engine.flush_digest(&FilterContext::default()).await.sent, 1);
        assert_eq!(good.digests.lock().unwrap().len(), 1);
        assert_eq!(
            bad.digests.lock().unwrap().as_slice(),
            &[vec!["k1".to_string()]]
        );
    }

    /// The daemon resets its digest interval only when nothing is still held, so a
    /// flush that sent nothing *because of the window* must report `held`. Without
    /// that signal a digest phased inside the window burns its whole period on an
    /// attempt that could never send, and at a daily interval never flushes at all.
    #[tokio::test]
    async fn a_held_digest_flush_reports_what_it_is_holding() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::Mentioned, "k1")],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));
        engine.run_once(INSIDE_WINDOW, false).await;

        // Held by the window: nothing sent, and the caller can tell why.
        let flush = engine.flush_digest(&INSIDE_WINDOW).await;
        assert_eq!(
            flush,
            DigestFlush {
                sent: 0,
                held: 1,
                failed: false
            }
        );

        // An empty buffer also sends nothing, but holds nothing, so the daemon may
        // start its next interval. That is the distinction the daemon needs.
        let empty = Engine::new(
            vec![Arc::new(MockSource::default())],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            Arc::new(MemState::default()),
        );
        assert_eq!(
            empty.flush_digest(&INSIDE_WINDOW).await,
            DigestFlush::default()
        );

        // Once the window ends the release sends it. The interval flush is not
        // involved, which is the point of keeping the two rows on separate clocks.
        assert_eq!(engine.release_digest(&OUTSIDE_WINDOW).await, 1);
    }

    /// Both buffers re-decide through the same call, so they must agree: a mute the
    /// user adds while events sit in a buffer has to be honoured on the digest path
    /// too, not only on the deferred one.
    #[tokio::test]
    async fn digest_drops_events_a_rule_now_rejects() {
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
        engine.run_once(FilterContext::default(), false).await;

        // The user disables the kind overnight; same state, so the same buffer.
        let mut rules = RuleConfig::default();
        rules.events.mentioned = false;
        let engine = Engine::new(
            vec![Arc::new(MockSource::default())],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(rules).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));

        assert_eq!(
            engine.flush_digest(&FilterContext::default()).await,
            DigestFlush::default(),
            "muted event must not be sent"
        );
        assert!(dest.digests.lock().unwrap().is_empty());
        // Discarded, not left to retry for ever.
        assert!(engine.read_digest().await.unwrap().is_empty());
    }

    /// The preview has to check the sink the live path would use, not just the
    /// destinations. An event sitting in the digest buffer has only `__digest__`
    /// marked, so a per-destination check alone still previews it as outgoing.
    #[tokio::test]
    async fn dry_run_sees_an_event_already_in_a_buffer() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let events = vec![ev(EventKind::Mentioned, "k1")];
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: events.clone(),
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));

        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(r.records[0].outcome, EventOutcome::Digested));

        // The live pass now answers AlreadyDelivered, so the preview must too.
        let r = engine.run_once(FilterContext::default(), true).await;
        assert!(
            matches!(r.records[0].outcome, EventOutcome::AlreadyDelivered),
            "buffered event previewed as outgoing: {:?}",
            r.records[0].outcome
        );
        let live = engine.run_once(FilterContext::default(), false).await;
        assert!(matches!(
            live.records[0].outcome,
            EventOutcome::AlreadyDelivered
        ));
    }

    /// After a partial failure the preview must name only the destinations the retry
    /// will actually reach.
    #[tokio::test]
    async fn dry_run_names_only_the_destinations_still_pending() {
        let good = Arc::new(MockDestination {
            id: "good".into(),
            ..Default::default()
        });
        let bad = Arc::new(MockDestination {
            id: "bad".into(),
            fail: AtomicBool::new(true),
            ..Default::default()
        });
        let (engine, _state) = engine_fanning_out(good.clone(), bad.clone());

        // good takes it, bad fails.
        engine.run_once(FilterContext::default(), false).await;
        assert_eq!(good.sent.lock().unwrap().len(), 1);

        let r = engine.run_once(FilterContext::default(), true).await;
        match &r.records[0].outcome {
            EventOutcome::WouldDeliver { to } => {
                assert_eq!(to, &["bad".to_string()], "preview must not re-list good")
            }
            other => panic!("expected WouldDeliver, got {other:?}"),
        }
    }

    /// Same for the deferred buffer.
    #[tokio::test]
    async fn dry_run_sees_an_event_already_deferred() {
        let dest = Arc::new(MockDestination::default());
        let (engine, _state) = engine_with(
            vec![ev(EventKind::ReviewRequested, "k1")],
            quiet_rules(),
            dest.clone(),
        );

        let r = engine.run_once(INSIDE_WINDOW, false).await;
        assert!(matches!(
            r.records[0].outcome,
            EventOutcome::Deferred(DeferReason::QuietHours)
        ));

        let r = engine.run_once(INSIDE_WINDOW, true).await;
        assert!(
            matches!(r.records[0].outcome, EventOutcome::AlreadyDelivered),
            "already-held event previewed as new: {:?}",
            r.records[0].outcome
        );
    }

    /// The other half of the migration contract: a record carried over from before
    /// per-sink tracking must still suppress live delivery, or upgrading re-notifies
    /// the entire dedup history on the first poll.
    #[tokio::test]
    async fn a_migrated_record_still_suppresses_live_delivery() {
        let dest = Arc::new(MockDestination::default());
        let (engine, state) = engine_with(
            vec![ev(EventKind::ReviewRequested, "k1")],
            RuleConfig::default(),
            dest.clone(),
        );
        state.delivered.lock().unwrap().insert(format!("k1@{ANY}"));

        let r = engine.run_once(FilterContext::default(), false).await;
        assert!(
            matches!(r.records[0].outcome, EventOutcome::AlreadyDelivered),
            "upgrade must not re-notify: {:?}",
            r.records[0].outcome
        );
        assert!(dest.sent.lock().unwrap().is_empty());
    }

    /// Upgrading with a non-empty digest buffer must not empty it.
    ///
    /// Before deliveries were tracked per sink, a digest event was marked delivered
    /// as soon as it was buffered, and the migration has to read those records as
    /// "reached every destination" or an upgrade would re-notify. Inside the digest
    /// flush that reading is wrong: the event was buffered, not sent. Folding it in
    /// there empties every batch and the clean rewrite then clears the buffer, so
    /// the events are lost rather than delayed - and the same record stops them
    /// re-deriving.
    #[tokio::test]
    async fn a_migrated_record_does_not_empty_the_digest_buffer() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource::default())],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));

        // A database as an upgrade leaves it: the event is in the buffer, and its
        // only delivery record is the migrated one that matches every sink.
        let event = ev(EventKind::Mentioned, "k1");
        engine
            .write_buffer(DIGEST_SOURCE, DIGEST_SCOPE, std::slice::from_ref(&event))
            .await
            .unwrap();
        state.delivered.lock().unwrap().insert(format!("k1@{ANY}"));
        // The live path still reads it as delivered everywhere, which is what keeps
        // an upgrade from re-notifying.
        assert!(state.was_delivered("k1", "mock-notify").await.unwrap());

        let flush = engine.flush_digest(&FilterContext::default()).await;
        assert_eq!(flush.sent, 1, "the buffered event must still go out");
        assert_eq!(
            dest.digests.lock().unwrap().as_slice(),
            &[vec!["k1".to_string()]]
        );
        assert!(engine.read_digest().await.unwrap().is_empty());
    }

    /// A poll that produces no events must still flush snapshots.
    ///
    /// The cursor sweep in `navi-notifier` dates off `snapshots.updated_at` and
    /// relies on this: deleting a quiet PR's cursor causes one event-free re-diff,
    /// and that re-diff has to refresh the column or the row is eligible again
    /// tomorrow and the PR is re-fetched daily for ever. `commit_snapshots` is
    /// called once per source outside the per-event loop, which is what makes that
    /// true - and from in here, skipping the flush when a pass found nothing reads
    /// like a free optimisation.
    #[tokio::test]
    async fn snapshots_are_committed_even_when_a_poll_finds_nothing() {
        let source = Arc::new(MockSource {
            events: vec![],
            ..Default::default()
        });
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![source.clone()],
            vec![Arc::new(MockDestination::default())],
            vec![],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            state,
        );

        let report = engine.run_once(FilterContext::default(), false).await;
        assert!(report.records.is_empty());
        assert_eq!(
            source.committed.lock().unwrap().as_slice(),
            &[HashSet::new()],
            "an event-free pass must still commit, or the cursor sweep re-fetches \
             every quiet PR daily"
        );
    }

    /// #158: the interval must not be the clock for releasing what the window held.
    ///
    /// The scenario the issue describes: a per-repo override makes one repo loud and
    /// another quiet, so an interval flush sends the loud events and holds the quiet
    /// ones. With one clock for both jobs, the held remainder waited for the *next*
    /// interval, which at `interval_secs = 86400` is a day later and, if the phase
    /// falls inside the window, never. Releasing is now checked every pass instead.
    #[tokio::test]
    async fn a_held_batch_is_released_without_waiting_for_the_next_interval() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let rules = RuleConfig {
            quiet_hours: crate::config::QuietHours {
                enabled: true,
                start: "22:00".into(),
                end: "08:00".into(),
            },
            overrides: vec![crate::config::RuleOverride {
                repos: vec!["loud/*".into()],
                quiet_hours: crate::config::QuietHoursOverride {
                    enabled: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        };
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![
                    ev_in(EventKind::Mentioned, "loud", "repo", "loud1"),
                    ev_in(EventKind::Mentioned, "quiet", "repo", "quiet1"),
                ],
                ..Default::default()
            })],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(rules).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));
        engine.run_once(INSIDE_WINDOW, false).await;

        // The one interval flush this scenario gets: loud goes, quiet is parked.
        let flush = engine.flush_digest(&INSIDE_WINDOW).await;
        assert_eq!((flush.sent, flush.held), (1, 1));
        assert_eq!(digest_held(&engine).await, vec!["quiet1".to_string()]);

        // No further flush happens for a day. Releases run every pass, and do
        // nothing until the window lifts.
        assert_eq!(engine.release_digest(&INSIDE_WINDOW).await, 0);
        assert_eq!(dest.digests.lock().unwrap().len(), 1);

        // The moment it lifts, the held batch goes, with no interval in between.
        assert_eq!(engine.release_digest(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(
            dest.digests.lock().unwrap().last().unwrap(),
            &vec!["quiet1".to_string()]
        );
        assert!(digest_held(&engine).await.is_empty());
    }

    /// The other half of the split: a released batch must not drag the *next*
    /// interval's events out with it. `pending` and the held row are separate, so
    /// events buffered after a flush wait for the interval as configured.
    #[tokio::test]
    async fn releasing_held_events_does_not_send_the_next_batch_early() {
        let dest = Arc::new(MockDestination::default());
        let state = Arc::new(MemState::default());
        let engine = Engine::new(
            vec![Arc::new(MockSource::default())],
            vec![dest.clone()],
            vec![],
            RuleEngine::new(quiet_rules()).unwrap(),
            state.clone(),
        )
        .with_digest_kinds(HashSet::from(["mentioned".to_string()]));

        // One event parked by the window, one freshly buffered for the next interval.
        engine
            .write_buffer(
                DIGEST_SOURCE,
                DIGEST_HELD_SCOPE,
                &[ev(EventKind::Mentioned, "held1")],
            )
            .await
            .unwrap();
        engine
            .write_buffer(
                DIGEST_SOURCE,
                DIGEST_SCOPE,
                &[ev(EventKind::Mentioned, "new1")],
            )
            .await
            .unwrap();

        assert_eq!(engine.release_digest(&OUTSIDE_WINDOW).await, 1);
        assert_eq!(
            dest.digests.lock().unwrap().as_slice(),
            &[vec!["held1".to_string()]],
            "a release must not pull the accumulating batch forward"
        );
        assert_eq!(
            engine.read_digest().await.unwrap().len(),
            1,
            "the next interval's batch is untouched"
        );
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
                    ..Default::default()
                },
                Route {
                    source: "mock".into(),
                    destination: "dest-b".into(),
                    repos: vec![],
                    ..Default::default()
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
    async fn fallback_route_catches_only_unclaimed_events() {
        // slack is scoped to acme/*; email is the fallback for everything else. An
        // acme event goes to slack only (never the fallback); an other/* event, which
        // no normal route claims, goes to email only.
        let slack = Arc::new(MockDestination {
            id: "slack".into(),
            ..Default::default()
        });
        let email = Arc::new(MockDestination {
            id: "email".into(),
            ..Default::default()
        });
        let mut other = ev(EventKind::Mentioned, "k-other");
        other.pull_request.repo = Repo::new("other", "thing");
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::Mentioned, "k-acme"), other],
                ..Default::default()
            })],
            vec![slack.clone(), email.clone()],
            vec![
                Route {
                    source: "mock".into(),
                    destination: "slack".into(),
                    repos: vec!["acme/*".into()],
                    ..Default::default()
                },
                Route {
                    source: "mock".into(),
                    destination: "email".into(),
                    fallback: true,
                    ..Default::default()
                },
            ],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        engine.run_once(FilterContext::default(), false).await;
        assert_eq!(
            slack.sent.lock().unwrap().as_slice(),
            &["k-acme".to_string()]
        );
        assert_eq!(
            email.sent.lock().unwrap().as_slice(),
            &["k-other".to_string()],
            "fallback must catch the unclaimed event and only that one"
        );
    }

    #[tokio::test]
    async fn fallback_does_not_fire_when_a_normal_route_matched_a_disabled_destination() {
        // slack claims acme/* but is not among the built destinations. The claim
        // still locks out the fallback, so the acme event is delivered nowhere rather
        // than leaking to email.
        let email = Arc::new(MockDestination {
            id: "email".into(),
            ..Default::default()
        });
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![ev(EventKind::Mentioned, "k-acme")],
                ..Default::default()
            })],
            vec![email.clone()],
            vec![
                Route {
                    source: "mock".into(),
                    destination: "slack".into(),
                    repos: vec!["acme/*".into()],
                    ..Default::default()
                },
                Route {
                    source: "mock".into(),
                    destination: "email".into(),
                    fallback: true,
                    ..Default::default()
                },
            ],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        engine.run_once(FilterContext::default(), false).await;
        assert!(
            email.sent.lock().unwrap().is_empty(),
            "a claimed event must not reach the fallback even if its destination is off"
        );
    }

    #[tokio::test]
    async fn scoped_fallback_only_catches_within_its_own_repos() {
        // A fallback narrowed to billing/* is not a universal net: an unclaimed
        // acme/* event matches neither the (absent) normal routes nor this fallback's
        // repo filter, so it's suppressed rather than emailed.
        let email = Arc::new(MockDestination {
            id: "email".into(),
            ..Default::default()
        });
        let mut acme = ev(EventKind::Mentioned, "k-acme");
        acme.pull_request.repo = Repo::new("acme", "thing");
        let mut billing = ev(EventKind::Mentioned, "k-billing");
        billing.pull_request.repo = Repo::new("billing", "thing");
        let engine = Engine::new(
            vec![Arc::new(MockSource {
                events: vec![acme, billing],
                ..Default::default()
            })],
            vec![email.clone()],
            vec![Route {
                source: "mock".into(),
                destination: "email".into(),
                repos: vec!["billing/*".into()],
                fallback: true,
            }],
            RuleEngine::new(RuleConfig::default()).unwrap(),
            Arc::new(MemState::default()),
        );
        engine.run_once(FilterContext::default(), false).await;
        assert_eq!(
            email.sent.lock().unwrap().as_slice(),
            &["k-billing".to_string()],
            "a scoped fallback catches only repos matching its own filter"
        );
    }

    #[tokio::test]
    async fn event_with_no_matching_route_is_suppressed_not_failed() {
        // A scoped route that this event's repo doesn't match must suppress the
        // event, not fail it, else its snapshot is held back and it re-derives
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
                ..Default::default()
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
                fail: AtomicBool::new(true),
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
            fail: AtomicBool::new(true),
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
        assert!(!state.was_delivered("k1", "mock-notify").await.unwrap());
    }
}
