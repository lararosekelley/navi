# Repo notes - navi

Facts the review bot cannot infer from one pass over the diff. The philosophy, the finding
categories, the workflow, the scoring, and the output format all live in the bot's base prompt; this
file only refines them for navi.

## The stack

A Rust workspace under `crates/` that polls code-forge activity and delivers focused PR-review
alerts. navi's promise to its user is that it stays **quiet and precise**; most real bugs here are a
layer violation or a cross-crate inconsistency.

- `navi-notifier-core`: the normalized `Event` / `PullRequest` model, the `Source` / `Destination` /
  `StateStore` traits, the `rules` filter layer, and the `engine` that turns a poll into filtered,
  deduplicated delivery. Knows nothing about any provider, SQLite, or HTTP.
- `navi-notifier-forge`: what forge-shaped sources genuinely share. `model::PrData` in,
  `diff::diff` out, compared against a persisted `PrSnapshot`.
- Sources: `navi-notifier-github` and `navi-notifier-gitea` both map their payloads into
  `forge::model` and call `diff`. `navi-notifier-gitlab` has two paths: the Todos API feed, plus its
  own `mr_diff` for everything todos cannot express.
- Destinations: `navi-notifier-slack`, `navi-notifier-discord`, `navi-notifier-email`.
- `navi-notifier`: the binary. CLI, config, guided setup, service install, `doctor`, and the SQLite
  `StateStore`.

## What CI already covers, so never mention it

On every PR: `rustfmt`, `clippy -D warnings`, `markdownlint`, `commitlint`, and the workspace test
matrix. The live e2e suite is **not** a PR check - it runs on pushes to main, on dispatch, and as a
`workflow_call` from the release.

## What CI cannot cover, so it is yours

**The diff engines.** `navi-notifier-forge/src/diff.rs` and `navi-notifier-gitlab/src/mr_diff.rs`
turn fetched PR/MR state into events by comparing against a snapshot. Watch for:

- an event that would fire on **first sight** of a PR - history back-fill must not happen, except
  for outstanding review requests (see `first_sight_watermark` and `FIRST_SIGHT_LEEWAY`);
- an event that could fire **twice** for one underlying action, via an unstable dedup key or a
  snapshot that advances at the wrong moment;
- for GitLab, the todo path and the `mr_diff` path covering overlapping event kinds, so one action
  fires from both - they must stay disjoint;
- an edge transition handled wrong: draft to ready, merged versus closed, review dismissed versus
  re-requested;
- a login comparison that is not case-insensitive.

**Noise.** Any change that makes navi ping more often by default is suspect. A new high-volume event
kind should default off. The filters in `core/src/rules.rs` - event toggles, mute rules, the repo
allowlist, quiet hours, per-repo overrides - must fail **closed**, not open, and a malformed rule is
a config error rather than a silently ignored one.

**Exactly-once delivery.** In the engine, an event is marked delivered only after every routed
destination succeeds, and a source that defers snapshot writes flushes them in `commit_snapshots`
for the scopes **not** in `failed_scopes`. The ordering between `mark_delivered`, the source
`commit` hook, `commit_snapshots`, and the snapshot writes is what makes delivery exactly-once
rather than at-most-once. The digest buffer is a separate path with looser semantics - buffered
events are marked delivered on enqueue and a flush failure is tolerated - and that looseness must
not leak into the immediate path.

## Seams

- Provider-specific logic belongs in the source or destination crate, never in `navi-notifier-core`.
  A GitHub, GitLab, or Slack concept leaking into the core traits, model, or engine is a violation.
- `navi-notifier-forge` is the seam for what forge sources genuinely share. Something only GitHub
  does does not belong there either.
- SQLite calls in `navi-notifier/src/state.rs` go through `spawn_blocking`. New synchronous I/O on
  the async path is a finding.
- Tokens and SMTP credentials come from environment variables. They must never be logged, put in an
  error message, or written to state - flag any `tracing` call or `format!` that could include one.

## Coverage matrix

A new event kind or config knob usually needs all of: the `EventKind` tag, a config default,
rule and per-repo override handling, and rendering in **every** destination. A change that touches
one and not the rest is a finding.

- Diff or rule behavior → a fixture test in `forge/src/diff_tests.rs`,
  `gitlab/src/mr_diff_tests.rs`, `gitlab/src/todo_tests.rs`, or the inline tests in
  `core/src/rules.rs`.
- Source or destination wiring → a wiremock integration test: each source crate's `tests/poll.rs`,
  each destination crate's `tests/deliver.rs`.
- A new CLI flag, config field, or default → reflected in `README.md`, and where relevant in
  `doctor` and the guided setup. Silent surface drift is a real finding.
- A state migration that could re-notify is high risk; say so.

## Repo-specific non-findings

- `.unwrap()` / `.expect()` inside `#[cfg(test)]` or a `mod tests` is the norm here, and is never a
  finding.
