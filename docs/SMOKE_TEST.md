# Pre-release manual checks

CI covers the unit + mock integration tests, and the live
[`e2e workflow`](../.github/workflows/e2e.yml) proves real GitHub auth, a real notifications poll, and a real Slack DM.
This doc is only the things CI **structurally can't** exercise. Run them by hand before announcing a release.

## 1. Install methods

The e2e builds from source and runs the binary directly; it never touches the published installers. After a release,
verify each lands a working binary (`navi --version`):

```sh
cargo install navi-notifier --locked
brew install lararosekelley/tap/navi          # once the tap formula is published
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/lararosekelley/navi/releases/latest/download/navi-notifier-installer.sh | sh
```

```powershell
# native Windows
powershell -ExecutionPolicy Bypass -c "irm https://github.com/lararosekelley/navi/releases/latest/download/navi-notifier-installer.ps1 | iex"
```

## 2. The daemon over time

The e2e runs a single poll pass. Confirm the long-running path behaves:

```sh
navi run          # leave it running
```

- Trigger a real event (have someone request your review, or reply to a comment you made) and confirm exactly one DM
  arrives within a poll interval.
- Confirm no duplicate DM on subsequent polls (dedup).
- `Ctrl-C` (and, under systemd/launchd, a `stop`) shuts it down cleanly.

## 3. Multi-repo + filtering

- With `rules.repos.allow = []`, confirm events surface across more than one repo your token can see.
- Set an `allow`/`deny` pattern and a `mute_authors` entry, then use `navi once --dry-run` to confirm the expected
  events show `suppressed (...)` for the right reason.

## 4. Service units

- Install [`deploy/navi.service`](../deploy/navi.service) (Linux) or
  [`deploy/dev.navi.navi.plist`](../deploy/dev.navi.navi.plist) (macOS), start it, and confirm logs flow and the DM
  path works end-to-end from the managed process.
