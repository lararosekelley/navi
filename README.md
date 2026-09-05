<img src="https://raw.githubusercontent.com/lararosekelley/navi/main/assets/logo.svg"
     width="48" height="48" alt="navi logo" />

# navi

[![crates.io](https://img.shields.io/crates/v/navi-notifier?color=cc6699)](https://crates.io/crates/navi-notifier)
[![CI](https://github.com/lararosekelley/navi/actions/workflows/ci.yml/badge.svg)](https://github.com/lararosekelley/navi/actions/workflows/ci.yml)

> 🧚‍♀️ A friendly helper to guide you through the day-to-day noise of code review

---

`navi` is a free, open-source, locally-run service that keeps you up to date on code review. It supports GitHub,
GitLab, and Gitea/Forgejo as **sources** and Slack, Discord, and email as **destinations**.

It will notify you when:

- 👀 a **review was requested** of you (and 🔁 **re-review** requests)
- ✅/⚠️/💬 a **review was submitted** on your PR (approved / changes / comment)
- ♻️ your **review was dismissed**
- 💬 someone **replied to a comment you made** (or in a thread you're in)
- 👋 you were **@-mentioned**
- 🟣 your PR was **merged**, or 🚫 **closed**

Every alert type is individually toggle-able, filterable by repo, and mutable by author, so you control the
granularity and frequency of your notifications. `navi` was inspired by how noisy GitHub's native Slack app is, and by
emails becoming harder to manage as LLMs and bots pile onto PRs.

> **Note:** the published crate is `navi-notifier`, but the installed binary and command are just `navi`.

Read more at [larakelley.com/posts/navi](https://larakelley.com/posts/navi)!

## Reporting issues

Bugs and feature requests go in [GitHub issues](https://github.com/lararosekelley/navi/issues). Redact any tokens
before pasting output.

## How it works

`navi` normalizes activity from each **source** into one common set of events, filters them by your rules, and routes
them to your **destinations**.

```text
source activity → normalized events → filter (rules) → route → destination
```

For GitHub and Gitea it polls the notifications API as a trigger, then **diffs** each PR's reviews and comments
against a stored snapshot to derive precise events, so it can tell "reply to _my_ comment" from "a dismissal" from "a
re-review"; for GitLab it reads the Todos feed. GitHub also polls your involved open PRs directly (`track_prs`, on by
default), so reviews on your own PRs and activity in muted repos reach you even when GitHub creates no notification.
State lives in a local SQLite database, so delivery is idempotent (tracked per destination, so a retry after one
destination fails never re-pings the others) and your read/unread state on the source is never touched.

## Install

One-line install (macOS, Linux, or Git Bash on Windows):

```sh
curl https://larakelley.com/sh/navi | bash
```

Native Windows (PowerShell):

```powershell
powershell -ExecutionPolicy Bypass -c "irm https://github.com/lararosekelley/navi/releases/latest/download/navi-notifier-installer.ps1 | iex"
```

Homebrew (macOS/Linux):

```sh
brew install lararosekelley/tap/navi-notifier
```

With a Rust toolchain, `cargo install navi-notifier --locked` builds from source, or
`cargo binstall navi-notifier` fetches the prebuilt binary. Every install puts a `navi` binary on your PATH.

Both scripts pull prebuilt binaries from [GitHub Releases](https://github.com/lararosekelley/navi/releases), so they
need a published release (see [Releasing](#releasing)). Linux builds are static musl and run on any distro. `navi`
itself runs on Linux, macOS, and Windows, and `navi service install` registers a background service on all three.

## Setup

```sh
navi init      # write ~/.config/navi/config.toml, then walk through enabling providers
navi doctor    # validate the config and report what each enabled provider can see
```

Every provider ships **disabled**. `navi init` offers to enable each one in turn, printing its setup steps and storing
the token you paste in `navi.env` beside the config. To do it by hand at any point:

```sh
navi providers list              # what's on, and whether its credentials resolve
navi providers setup slack       # setup steps for one provider (incl. a Slack app manifest)
navi config set slack.enabled true
navi test --destination slack    # send a sample; --source github polls and prints what it derives
```

| Provider      | Kind        | Token env             | Notes                                                      |
| ------------- | ----------- | --------------------- | ---------------------------------------------------------- |
| GitHub        | source      | `NAVI_GITHUB_TOKEN`   | `notifications` + `repo` read; `read:org` for team review. |
| GitLab        | source      | `NAVI_GITLAB_TOKEN`   | PAT with `read_api`; set `api_base` for self-hosted.       |
| Gitea/Forgejo | source      | `NAVI_GITEA_TOKEN`    | set `api_base` to your instance (`.../api/v1`).            |
| Slack         | destination | `NAVI_SLACK_TOKEN`    | bot token (`xoxb-…`) with `chat:write` + `im:write`.       |
| Discord       | destination | `NAVI_DISCORD_TOKEN`  | or set `dm_to` to a webhook URL (no token needed).         |
| Email         | destination | `NAVI_EMAIL_PASSWORD` | SMTP host, `from`, and `to` in the `[email]` section.      |

With more than one of each, use `routes` to wire which sources feed which destinations; omit `routes` to send every
source to every destination.

## Usage

```sh
navi once --dry-run   # one poll pass; prints what WOULD be sent, changes nothing
navi once             # one poll pass; actually delivers
navi run              # run continuously on the configured interval
```

`once --dry-run` is the safe way to preview your filters: it shows each derived event and why it was delivered or
suppressed, without sending anything or advancing state.

```sh
navi doctor                            # validate the config; check what each provider can see
navi config get general.poll_interval_secs       # read a value by dotted key
navi config set general.poll_interval_secs 120   # write one in place, comments preserved
navi config edit                       # open config.toml in $VISUAL/$EDITOR
navi env                               # open navi.env (the token file) in $VISUAL/$EDITOR
```

### As a background service

`navi init` offers to set this up for you; you can also do it any time:

```sh
navi service install     # generate + enable a login service for your OS
navi service status      # is it installed and running?
navi service restart     # re-read config and navi.env
navi service uninstall   # stop and remove it
navi logs -f             # tail the service's logs (journald on Linux, a log file on macOS/Windows)
```

The service is generated from your own binary and config paths and runs on login:

- **Linux:** a systemd user unit at `~/.config/systemd/user/navi.service`. Paths use systemd's `%h` specifier rather
  than a hard-coded home directory, so the unit is safe to check into dotfiles.
- **macOS:** a launchd agent at `~/Library/LaunchAgents/dev.navi.navi.plist`.
- **Windows:** a Task Scheduler logon task named `Navi`, run hidden (no console window).

A background service does not inherit your shell environment, so tokens reach it separately. On Linux and macOS,
`install` writes a `navi.env` file next to your config (chmod 600); put your tokens there. navi loads `navi.env`
automatically at startup (foreground or service), and an already-set shell variable still wins over the file. On
Windows, the task inherits user-scope variables, so set them once with `setx NAVI_GITHUB_TOKEN ...`.

The hand-written templates in [`deploy/`](deploy) remain for reference or manual setup.

### Shell completions and upgrades

```sh
navi setup                 # install the man page + wire completions into your shell rc (idempotent)
navi completions zsh       # or print the script yourself (bash/zsh/fish/powershell/elvish)
navi upgrade               # update an installer-managed copy to the latest release
navi downgrade --to 0.1.4  # step back to an earlier release (or bare `downgrade` for the previous one)
navi uninstall             # reverse setup + the installer (completions, man page, config); reports how to remove the binary
```

`upgrade`/`downgrade` re-run the release installer, so they apply to copies installed via the shell/PowerShell
installer or Homebrew; a `cargo install` copy should update through cargo. Both restart the background service onto the
new binary (`--no-restart` to skip), since a running daemon otherwise stays on the old build. A once-a-day check prints
a one-line nudge when a newer release exists (silence it with `NAVI_NO_UPDATE_CHECK=1`).

## Configuration

`navi init` documents every field inline, and `navi config set <key> <value>` edits one in place. The fields worth
knowing about:

| Section              | Key                      | Meaning                                                               |
| -------------------- | ------------------------ | --------------------------------------------------------------------- |
| `general`            | `poll_interval_secs`     | Seconds between poll passes (`run`).                                  |
| `general`            | `utc_offset_minutes`     | Your UTC offset, used only for quiet hours.                           |
| `general`            | `comment_min_age_secs`   | Hold comments back this long so bots that edit in place settle first. |
| `general`            | `backfill`               | First-poll behavior: `review_requests`, `none`, or `all_open`.        |
| `general`            | `log_level`              | `tracing` filter, e.g. `info` or `navi=debug`.                        |
| `github`             | `token_env` / `api_base` | Source. Token env var; API base for GitHub Enterprise.                |
| `github`             | `track_prs`              | Also poll your involved open PRs, not just the notifications inbox.   |
| `github`             | `mark_read`              | Mark a notification thread read once delivered. Off by default.       |
| `gitlab`             | `enabled` / `token_env`  | Source. `read_api` token; `api_base` for self-hosted.                 |
| `gitea`              | `enabled` / `api_base`   | Source. Gitea or Forgejo instance.                                    |
| `slack`              | `dm_to`                  | Destination. `"self"`, a user id `U…`, `C…`, or `#name`.              |
| `slack`              | `broadcast`              | Event tags that surface at top level, not just in the PR thread.      |
| `discord`            | `enabled` / `dm_to`      | Destination. Webhook URL or user id.                                  |
| `email`              | `smtp_host` / `to`       | Destination. SMTP delivery, threaded per PR.                          |
| `digest`             | `enabled` / `kinds`      | Batch low-signal kinds into a periodic summary instead of alerting.   |
| `rules.events.*`     |                          | Per-event-kind on/off toggles.                                        |
| `rules.repos`        | `allow` / `deny`         | `owner/name` or `owner/*` patterns; `deny` wins.                      |
| `rules.mute_authors` |                          | Logins whose actions never notify (e.g. bots).                        |
| `rules.quiet_hours`  |                          | Hold events during a local time window; delivered once it ends.       |
| `rules.merge_close`  | `author` / `reviewer`    | Whose merges/closes to report.                                        |
| `routes`             | `repos` / `fallback`     | Which sources feed which destinations, optionally scoped by repo.     |

It works across **all repos your token can see**. There's no repo list to maintain; narrow the firehose with
`rules.repos`.

## Architecture

A Cargo workspace with a provider-agnostic core and thin provider crates:

| Crate                   | Role                                                                                                                                                      |
| ----------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `navi-notifier-core`    | Normalized event model, the `Source`/`Destination`/`StateStore` traits, the rule/filter layer, and the poll→filter→deliver engine. No provider specifics. |
| `navi-notifier-forge`   | Shared diff engine + model for GitHub-shaped forges (used by the github and gitea sources).                                                               |
| `navi-notifier-github`  | Source: notifications polling + PR-timeline diffing.                                                                                                      |
| `navi-notifier-gitlab`  | Source: review-request and mention alerts from the Todos API.                                                                                             |
| `navi-notifier-gitea`   | Source: Gitea/Forgejo, reusing the forge diff engine.                                                                                                     |
| `navi-notifier-slack`   | Destination: Block Kit DMs via a bot token.                                                                                                               |
| `navi-notifier-discord` | Destination: embed DMs via a bot token, or a channel webhook.                                                                                             |
| `navi-notifier-email`   | Destination: SMTP delivery, one message per event, threaded per PR.                                                                                       |
| `navi-notifier`         | The binary (`navi`): config, SQLite state store, provider registry, daemon loop, CLI.                                                                     |

Adding a provider is "implement a trait, register a constructor", with no engine changes.

## Development

```sh
just install          # fetch Rust + JS dev deps
just test             # workspace test suite (mock-based; no network)
just lint             # rustfmt --check, clippy -D warnings, markdownlint
just check            # format + lint + test
just e2e              # live smoke test (needs NAVI_GITHUB_TOKEN + NAVI_SLACK_TOKEN)
```

Commits follow [Conventional Commits](https://www.conventionalcommits.org) with a required scope, enforced by
commitlint via a git hook; run `just install` once to wire the hooks. The interesting logic (the forge diff engine and
the rule filter) is pure and covered by fixture tests; HTTP wiring is covered by
[wiremock](https://docs.rs/wiremock) integration tests under each provider crate's `tests/`.

## Releasing

Versioning is driven by [cargo-release](https://github.com/crate-ci/cargo-release) and artifact building by
[cargo-dist](https://github.com/axodotdev/cargo-dist) ([`dist-workspace.toml`](dist-workspace.toml)). Every crate
shares one version; cargo-release keeps that version _and_ the internal cross-crate dependency requirements in lockstep
on every bump (see [`[workspace.metadata.release]`](Cargo.toml)), so they can never drift.

One-time setup: install the tooling and generate the (not-hand-written) release workflow:

```sh
just install-release-tools    # cargo install cargo-release + cargo-dist (--locked)
dist init                     # reads dist-workspace.toml, writes .github/workflows/release.yml
```

Cutting a release (from `main`):

```sh
just release-dry minor        # preview the bump, commit, and tag; changes nothing
just release minor            # bump all crates + internal deps, commit, tag v<version>, push
```

`just release` only bumps/commits/tags/pushes; it does **not** publish. The tag push triggers the cargo-dist release
workflow, which builds the binaries and installers, gates on the [e2e workflow](.github/workflows/e2e.yml), and then
runs [`publish-crates.yml`](.github/workflows/publish-crates.yml) to publish every crate to crates.io in dependency
order. See [`docs/SMOKE_TEST.md`](docs/SMOKE_TEST.md) for the manual pre-release checklist.

## License

[MIT License](LICENSE). Copyright (c) 2026 Lara Kelley.
