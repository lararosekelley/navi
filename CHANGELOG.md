# Changelog

Notable changes per release. The section matching the version being tagged is
published into that release's GitHub notes by `dist`, so the headings must stay
`## <version>`.

## 0.3.5

### Fixed

- **source:** don't record a pull request as seen when it couldn't be fetched.
  A momentary API failure used to advance the per-PR cursor anyway, so the PR
  was skipped until something moved its timestamp again, and its cursor was left
  sitting ahead of its snapshot. A PR the forge reports as gone still advances,
  so a deleted or now-invisible one isn't re-fetched on every poll
- **core:** release digest events a quiet window held back as soon as the window
  ends, rather than at the next digest interval. With a long `interval_secs` and
  a per-repo quiet-hours override the held part of a batch could otherwise wait
  a further interval each time, or never go out at all if the interval's phase
  fell inside the window

## 0.3.4

### Added

- **state:** sweep the poll cursors for PRs navi hasn't diffed in
  `general.state_retention_days` (default 90) once a day while `navi run` is
  going, so they stop accumulating one row per PR it has ever seen. Snapshots,
  delivery records and merge-queue state are never touched, so this can cost a
  re-fetch but never a duplicate

### Fixed

- **cli:** let `config set` write a known key that isn't in config.toml yet,
  creating its section if needed, so `Option` fields and fields added after your
  `navi init` no longer need hand-editing. Unknown keys and wrongly typed values
  are still refused, and an unknown one now suggests the valid siblings
- **cli:** `config set` accepts a numeric-looking value for a string field (a
  Discord user id in `discord.dm_to`), and no longer fails a valid write because
  something unrelated in config.toml doesn't parse
- **core:** hold events that land during quiet hours and deliver them once the
  window ends, instead of dropping them for good
- **core:** hold the digest inside a quiet window too, so batching a kind no
  longer opts it out of quiet hours
- **core:** track delivery per destination, so a retry after one destination
  fails no longer re-notifies the ones that already succeeded. The digest flush
  follows the same rule, replacing its "may re-send" caveat
- **cli:** `once --dry-run` reports events it has already delivered as such,
  rather than previewing them as outgoing

## 0.3.3

### Changed

- **readme:** sync with the current CLI and config surface, trim prose
- **code:** remove em dashes that crept back into comments

## 0.3.2

### Changed

- **service:** write the generated systemd unit's paths with the `%h` specifier
  instead of a hard-coded home directory, so it can be checked into dotfiles

## 0.3.1

### Fixed

- **github:** set connect/read/write timeouts on the API client, so a half-open
  connection can no longer park a poll forever
- **cli:** abandon and retry a poll pass that overruns, so one stuck source
  can't silently wedge the daemon

## 0.3.0

### Added

- **source:** warn when gitea/gitlab pagination hits the page cap

### Fixed

- **discord:** retry transient delivery failures with backoff, like slack

### Changed

- **cli:** only start a Tokio runtime for commands that await
- **destination:** share headline/thread_key/escape across slack, discord, email
- **forge:** share is_settled/ts_key/excerpt with the gitlab source
- **cli:** dedupe provider id lists and enabled/creds across config, providers,
  wiring, doctor
- **core:** share headline/thread_key/html_escape and add model tests
- **e2e:** share mailpit helpers and Mailpit email wiring across the source
  slices

## 0.2.9

### Added

- **cli:** validate config in `navi doctor` (static checks + `--offline`)
- **email:** link "navi" in the footer to the project
- **slack:** only broadcast review actions on your own PRs

### Fixed

- **core:** name the PR author in merge-queue headlines, not "their own PR"

## 0.2.8

### Fixed

- **cli:** register the Windows logon task via XML to dodge schtasks' `/TR` limit

### Changed

- **e2e:** cross-OS background-service lifecycle matrix
- **e2e:** live GitHub, GitLab, and Discord read-back slices in place of the
  Slack smoke test
- **e2e:** share env/json_ok/MemState across the e2e harnesses

## 0.2.7

### Added

- **cli:** add `navi service restart`
- **engine:** fallback route for events no other route claims

## 0.2.6

### Added

- **cli:** add `navi config edit` and `navi env` to open files in `$EDITOR`
- **cli:** embed the Slack app manifest and a prefilled Discord bot invite URL
  in `navi providers setup`
- **slack:** broadcast approvals and change requests by default

## 0.2.5

### Added

- **slack:** broadcast configurable terminal events out of the thread

### Fixed

- **source:** defer involved-sweep `pr:` cursors until delivery

## 0.2.4

### Added

- **discord:** group a PR's events into a reply chain in bot mode
- **source:** honor `general.backfill` on gitlab and gitea too
- **source:** log a per-poll summary of what each poll examined
- **gitea:** sweep involved PRs to catch self-merges and own-PR activity

### Fixed

- **github:** catch self-merges/closes via a recently-closed PR sweep
- **gitlab:** honor `general.comment_min_age_secs` when diffing MR notes

## 0.2.3

### Added

- **github:** detect merge-queue enter/exit via GraphQL
- **github:** opt-in first-run backfill via `general.backfill`
- **slack:** thread a PR's events under one Slack message
- **gitlab:** diff MR notes for merged/closed/comment-reply/ready

## 0.2.2

### Added

- **config:** default providers off with a guided `navi init` opt-in
- **config:** make `navi.env` authoritative over process env
- **cli:** `navi providers list` and `navi providers setup <name>`
- **cli:** `navi test --source/--destination`, dropping `test-slack`
- **cli:** add `navi config get/set` with in-place edits
- **cli:** roll the Windows service log over a size cap

### Fixed

- **cli:** compare against the latest published release, not tags

### Changed

- **tooling:** run clippy in the pre-commit hook

## 0.2.1

### Added

- **engine:** digest mode to batch low-signal events into a periodic summary
- **engine:** route events by repo pattern, not just source
- **rules:** per-repo rule overrides

### Fixed

- **cli:** fail `navi upgrade` when the installer download fails

## 0.2.0

### Added

- **core:** say "their own PR" when the author acts on their own PR
- **core:** defer snapshot commit until after delivery
- **rules:** multi-condition mute rules with per-field regex

### Fixed

- **github:** mark a PR thread read once per poll, not per event

## 0.1.10

### Added

- **config:** support repo name-prefix filter patterns

### Fixed

- **cli:** skip `navi upgrade` re-download when already on the latest

## 0.1.9

### Added

- **cli:** add `navi doctor` to report provider visibility
- **core:** headline uses "<author>'s PR" for PRs you only review, and "you"
  when the actor is yourself
- **config:** default `ready_for_review` alerts on
- **github:** opt-in mark-read after delivery
- **rules:** pattern and regex mute filters

### Fixed

- **cli:** escape single quotes in the windows schtasks action

## 0.1.8

### Added

- **cli:** add `navi logs` to tail the service log
- **cli:** restart the service after upgrade/downgrade
- **github:** poll your involved open PRs directly, not just notifications

### Fixed

- **github:** page notifications deeper and warn on truncation

## 0.1.7

### Added

- **config:** auto-load `navi.env` beside the config

### Fixed

- **cli:** redirect windows service output to a log file
- **forge:** detect review requests routed to your teams
- **forge:** surface the triggering event on a PR's first sighting

### Changed

- **e2e:** consolidate e2e workflows into one

## 0.1.6

### Added

- **cli:** add `navi service` to manage the background service

### Changed

- **docs:** drop stale GitHub-to-Slack-only phrasing

## 0.1.5

### Added

- **cli:** add completions, setup, upgrade, downgrade, uninstall
- **docs:** document shell completions and upgrades

### Fixed

- **cli:** floor downgrade at 0.1.5, the first release with the command

### Changed

- **deps:** swap axoupdater for clap_complete and clap_mangen

## 0.1.4

### Added

- **e2e:** add hermetic gitea to mailpit e2e harness

### Fixed

- **gitea:** drop username alias that broke Gitea `/user` parsing
- **release:** remove backticks from install-success-msg

## 0.1.3

### Added

- **email:** add SMTP email destination with per-PR threading

### Changed

- **release:** publish all crates in dependency order

## 0.1.2

### Added

- **gitlab:** add gitlab source via the todos api
- **gitea:** add gitea/forgejo source reusing the forge engine
- **discord:** add discord notifier with webhook or bot dm
- **cli:** register gitlab and discord providers

### Fixed

- **github:** reject empty token and classify 403 vs rate limit
- **slack:** reject empty token with a clear error
- **e2e:** treat an empty token env var as missing

### Changed

- **core:** rename the Notifier concept to Destination
- **docs:** unify source/destination language and cover all providers
- **release:** block release on e2e failure via host gate

## 0.1.1

### Added

- **core:** domain model, provider traits, rule filter, and engine
- **github:** github source with notifications poll and timeline diff
- **slack:** slack notifier with block kit dms
- **cli:** daemon, config, sqlite state store, and provider registry
- **e2e:** live smoke-test harness for real github and slack
- **build:** systemd and launchd service units
- **docs:** readme, one-line install, platform support, and smoke-test guide
