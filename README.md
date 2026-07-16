# navi

[![crates.io](https://img.shields.io/crates/v/navi-notifier?color=cc6699)](https://crates.io/crates/navi-notifier)
[![CI](https://github.com/lararosekelley/navi/actions/workflows/ci.yml/badge.svg)](https://github.com/lararosekelley/navi/actions/workflows/ci.yml)

> 🧚‍♀️ A friendly helper to guide you through the day-to-day noise of code review

---

`navi` is a free, open-source, and locally-run service for keeping you up-to-date with code review requests. It supports
GitHub and Slack as of now, with planned support for GitLab, Discord, and email transports.

It will notify you when:

- 👀 a **review was requested** of you (and 🔁 **re-review** requests)
- ✅/⚠️/💬 a **review was submitted** on your PR (approved / changes / comment)
- ♻️ your **review was dismissed**
- 💬 someone **replied to a comment you made** (or in a thread you're in)
- 👋 you were **@-mentioned**
- 🟣 your PR was **merged**, or 🚫 **closed**

Every alert kind is individually toggleable, filterable by repo, and mutable by author — so you keep the signal and
drop the noise. It ships for **GitHub → Slack** today, but the core is provider-agnostic so GitLab, Discord, etc. are
additive.

> **Naming:** the published crate is `navi-notifier` (the short name was taken on crates.io), but the installed
> binary and command are just `navi`.

## Reporting issues

Please report bugs and feature requests in
[GitHub issues](https://github.com/lararosekelley/navi/issues). Redact any tokens before pasting output.

## How it works

navi polls GitHub's Notifications API as a cheap trigger to learn _which_ PRs have new activity, then fetches each
such PR's reviews and comments and **diffs** them against a stored snapshot to derive precise events. The notification
`reason` alone can't distinguish "reply to _my_ comment" from "a dismissal" from "a re-review" — the diff can. State
lives in a local SQLite database, so delivery is idempotent (you're never pinged twice) and it never touches your
GitHub read/unread state.

```text
notifications (trigger) → fetch PR timeline → diff vs snapshot → filter (rules) → Slack DM
```

## Install

With a Rust toolchain:

```sh
cargo install navi-notifier --locked     # installs a binary called `navi`
```

Or build from a checkout:

```sh
cargo build --release                     # ./target/release/navi
```

Prebuilt binaries and installers (shell/PowerShell/Homebrew) are published per release once the release workflow is
set up — see [Releasing](#releasing).

## Setup

### 1. GitHub token

Create a Personal Access Token that can read your notifications and PRs:

- **Classic PAT:** scopes `notifications` + `repo` (read access to the repos you care about).
- **Fine-grained PAT:** read access to _Pull requests_ and _Notifications_ on the relevant repos.

Export it as `NAVI_GITHUB_TOKEN`.

### 2. Slack app

1. Create an app at <https://api.slack.com/apps> → _From scratch_.
2. Under **OAuth & Permissions**, add bot scopes: `chat:write` and `im:write`.
3. Install the app to your workspace and copy the **Bot User OAuth Token** (`xoxb-…`).
4. Export it as `NAVI_SLACK_TOKEN`.

`dm_to = "self"` DMs whoever the token authenticates as. If that resolves to the bot rather than you, set `dm_to` to
your Slack user id (`U…`) — find it via your Slack profile → _Copy member ID_.

### 3. Configure

```sh
navi init                 # writes ~/.config/navi/config.toml with commented defaults
$EDITOR ~/.config/navi/config.toml
navi test-slack           # DMs you a sample message to confirm credentials
```

## Usage

```sh
navi once --dry-run   # one poll pass; prints what WOULD be sent, changes nothing
navi once             # one poll pass; actually delivers
navi run              # run continuously on the configured interval
```

Preview your filters safely with `once --dry-run` — it shows each derived event and why it was delivered or
suppressed, without sending anything or advancing state.

### As a background service

- **Linux (systemd):** see [`deploy/navi.service`](deploy/navi.service).
- **macOS (launchd):** see [`deploy/dev.navi.navi.plist`](deploy/dev.navi.navi.plist).

## Configuration

`navi init` documents every field inline. Highlights:

| Section              | Key                      | Meaning                                               |
| -------------------- | ------------------------ | ----------------------------------------------------- |
| `general`            | `poll_interval_secs`     | Seconds between poll passes (`run`).                  |
| `general`            | `utc_offset_minutes`     | Your UTC offset, used only for quiet hours.           |
| `github`             | `token_env` / `api_base` | Token env var; API base for GitHub Enterprise.        |
| `slack`              | `dm_to`                  | `"self"`, a user id `U…`, a channel `C…`, or `#name`. |
| `rules.events.*`     |                          | Per-event-kind on/off toggles.                        |
| `rules.repos`        | `allow` / `deny`         | `owner/name` or `owner/*` patterns; `deny` wins.      |
| `rules.mute_authors` |                          | Logins whose actions never notify (e.g. bots).        |
| `rules.quiet_hours`  |                          | Suppress during a local time window.                  |
| `rules.merge_close`  | `author` / `reviewer`    | Whose merges/closes to report.                        |
| `routes`             |                          | Which sources feed which notifiers.                   |

It works across **all repos your token can see** — there's no repo list to maintain; narrow the firehose with
`rules.repos`.

## Architecture

A Cargo workspace with a provider-agnostic core and thin provider crates:

| Crate                  | Role                                                                                                                                                   |
| ---------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `navi-notifier-core`   | Normalized event model, the `Source`/`Notifier`/`StateStore` traits, the rule/filter layer, and the poll→filter→deliver engine. No provider specifics. |
| `navi-notifier-github` | `Source`: notifications polling + PR-timeline diffing.                                                                                                 |
| `navi-notifier-slack`  | `Notifier`: Block Kit DMs via a bot token.                                                                                                             |
| `navi-notifier`        | The binary (`navi`): config, SQLite state store, provider registry, daemon loop, CLI.                                                                  |

Adding a provider is "implement a trait, register a constructor" — no engine changes.

## Development

```sh
just install          # fetch Rust + JS dev deps
just test             # workspace test suite (mock-based; no network)
just lint             # rustfmt --check, clippy -D warnings, markdownlint
just check            # format + lint + test
just e2e              # live smoke test (needs NAVI_GITHUB_TOKEN + NAVI_SLACK_TOKEN)
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org) with a required scope (enforced by
commitlint via a git hook); run `just install` once to wire the hooks. The interesting logic — the GitHub diff engine
and the rule filter — is pure and covered by fixture tests; the HTTP wiring is covered by
[wiremock](https://docs.rs/wiremock) integration tests under each provider crate's `tests/`.

## Releasing

Versioning is driven by [cargo-release](https://github.com/crate-ci/cargo-release) and artifact building by
[cargo-dist](https://github.com/axodotdev/cargo-dist) ([`dist-workspace.toml`](dist-workspace.toml)). All four crates
share one version; cargo-release keeps that version _and_ the internal cross-crate dependency requirements in lockstep
on every bump (see [`[workspace.metadata.release]`](Cargo.toml)), so they can never drift.

One-time setup — install the tooling and generate the (not-hand-written) release workflow:

```sh
just install-release-tools    # cargo install cargo-release + cargo-dist (--locked)
dist init                     # reads dist-workspace.toml, writes .github/workflows/release.yml
```

Cutting a release (from `main`):

```sh
just release-dry minor        # preview the bump, commit, and tag — changes nothing
just release minor            # bump all crates + internal deps, commit, tag v<version>, push
```

`just release` only bumps/commits/tags/pushes — it does **not** publish. The tag push triggers the cargo-dist release
workflow, which builds the binaries and installers, runs the [e2e workflow](.github/workflows/e2e.yml) as a
pre-release gate, and then runs [`publish-crates.yml`](.github/workflows/publish-crates.yml) to publish all crates to
crates.io in dependency order — so publishing only happens after the release builds pass. See
[`docs/SMOKE_TEST.md`](docs/SMOKE_TEST.md) for the manual pre-release checklist.

## License

[MIT](LICENSE).
