#!/usr/bin/env bash
# Generate realistic navi notifications from fake repos, for screenshots.
#
# Spins up a throwaway Gitea + Mailpit in Docker, seeds a fictional "acme" org
# with a few teammates, then drives every alert kind step by step with a real
# `navi once` in between. Config and state live in $NAVI_DEMO_DIR, so your own
# navi config, tokens, and state are never read or touched.
#
# Usage: scripts/demo.sh <up|run|dry-run|poll|digest|init|down>
#   up       start Gitea + Mailpit and seed users, org, and repos
#   run      drive the scenarios and deliver each alert (leaves a pending tail)
#   dry-run  preview the pending tail without sending (repeatable)
#   poll     deliver whatever is pending
#   digest   merge/close a few PRs and let `navi run` flush them as one digest
#   init     run `navi init` against a scratch config dir
#   down     remove the containers and $NAVI_DEMO_DIR
#
# Destinations: email always goes to Mailpit (http://localhost:8325). Slack and
# Discord are opt-in: set NAVI_SLACK_TOKEN + NAVI_DEMO_SLACK_DM_TO, and/or
# NAVI_DISCORD_TOKEN + NAVI_DEMO_DISCORD_DM_TO (user id or webhook URL), before
# `run` and `digest`.
#
# Runs navi from this checkout (built on `up`) unless NAVI_BIN points elsewhere.
# NAVI_DEMO_NAME, NAVI_DEMO_GITEA_PORT, NAVI_DEMO_MAILPIT_PORT, and
# NAVI_DEMO_SMTP_PORT let a second copy run alongside the first.

set -euo pipefail

DEMO_DIR=${NAVI_DEMO_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/navi-demo}
REPO=$(cd "$(dirname "$0")/.." && pwd)
NAVI=${NAVI_BIN:-$REPO/target/debug/navi}
NAME=${NAVI_DEMO_NAME:-navi-demo}
GITEA_PORT=${NAVI_DEMO_GITEA_PORT:-3300}
MAILPIT_HTTP_PORT=${NAVI_DEMO_MAILPIT_PORT:-8325}
MAILPIT_SMTP_PORT=${NAVI_DEMO_SMTP_PORT:-1325}
GITEA="http://localhost:$GITEA_PORT"
API="$GITEA/api/v1"
PASSWORD="navi-demo-pass-1"
ADMIN=navi-admin

# The user navi runs as. Teammates (priya, marcus, sofia, renovate-bot) act on PRs.
VIEWER=jamie
ORG=acme

die() { echo "demo: $*" >&2; exit 1; }
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

token_of() { cat "$DEMO_DIR/tokens/$1"; }

# api METHOD PATH USER [JSON] -> response body on stdout
api() {
  local method=$1 path=$2 user=$3 body=${4:-}
  local args=(-fsS -X "$method" "$API$path" -H "Authorization: token $(token_of "$user")")
  [[ -n $body ]] && args+=(-H 'Content-Type: application/json' -d "$body")
  curl "${args[@]}" || die "$method $path as $user failed"
}

navi_env() {
  env XDG_DATA_HOME="$DEMO_DIR/data" XDG_CONFIG_HOME="$DEMO_DIR/config-home" \
    NAVI_NO_UPDATE_CHECK=1 NAVI_GITEA_TOKEN="$(token_of "$VIEWER")" "$@"
}

poll() {
  # Gitea writes notifications from a queue; give it a moment to settle.
  sleep "${1:-3}"
  navi_env "$NAVI" -c "$DEMO_DIR/config.toml" once
}

# --- seeding ----------------------------------------------------------------

create_user() {
  local user=$1 full=$2
  curl -fsS -o /dev/null -X POST "$API/admin/users" -H "Authorization: token $(token_of $ADMIN)" \
    -H 'Content-Type: application/json' \
    -d "$(jq -n --arg u "$user" --arg f "$full" --arg p "$PASSWORD" \
      '{username: $u, full_name: $f, email: "\($u)@acme.test", password: $p,
        must_change_password: false, visibility: "public"}')"
  curl -fsS -u "$user:$PASSWORD" -X POST "$API/users/$user/tokens" -H 'Content-Type: application/json' \
    -d '{"name":"demo","scopes":["write:repository","write:issue","write:organization","read:notification","read:user"]}' \
    | jq -r .sha1 >"$DEMO_DIR/tokens/$user"
}

create_repo() {
  local repo=$1 desc=$2
  api POST "/orgs/$ORG/repos" priya \
    "$(jq -n --arg r "$repo" --arg d "$desc" '{name: $r, description: $d, auto_init: true, default_branch: "main"}')" >/dev/null
  for user in "$VIEWER" marcus sofia renovate-bot; do
    api PUT "/repos/$ORG/$repo/collaborators/$user" priya '{"permission":"admin"}' >/dev/null
  done
}

cmd_up() {
  command -v jq >/dev/null || die "jq is required"
  if [[ -z ${NAVI_BIN:-} ]]; then
    say "building navi"
    cargo build -q -p navi-notifier --manifest-path "$REPO/Cargo.toml"
  fi
  mkdir -p "$DEMO_DIR/tokens" "$DEMO_DIR/data" "$DEMO_DIR/config-home"

  say "starting Gitea ($GITEA) and Mailpit (http://localhost:$MAILPIT_HTTP_PORT)"
  docker run -d --name "$NAME-mailpit" \
    -p "127.0.0.1:$MAILPIT_SMTP_PORT:1025" -p "127.0.0.1:$MAILPIT_HTTP_PORT:8025" axllent/mailpit:latest >/dev/null
  docker run -d --name "$NAME-gitea" -p "127.0.0.1:$GITEA_PORT:3000" \
    -e GITEA__security__INSTALL_LOCK=true \
    -e "GITEA__server__ROOT_URL=$GITEA/" \
    -e GITEA__service__DEFAULT_KEEP_EMAIL_PRIVATE=true \
    gitea/gitea:1.22 >/dev/null
  for _ in $(seq 1 90); do
    curl -fsS "$API/version" >/dev/null 2>&1 && break
    sleep 1
  done
  curl -fsS "$API/version" >/dev/null || die "Gitea did not come up"

  docker exec -u git "$NAME-gitea" gitea admin user create --username $ADMIN \
    --password "$PASSWORD" --email admin@acme.test --admin --must-change-password=false >/dev/null
  curl -fsS -u "$ADMIN:$PASSWORD" -X POST "$API/users/$ADMIN/tokens" -H 'Content-Type: application/json' \
    -d '{"name":"admin","scopes":["write:admin","write:user","write:organization","write:repository"]}' \
    | jq -r .sha1 >"$DEMO_DIR/tokens/$ADMIN"

  say "seeding users, org, and repos"
  create_user "$VIEWER" "Jamie Rivera"
  create_user priya "Priya Nair"
  create_user marcus "Marcus Chen"
  create_user sofia "Sofia Alvarez"
  create_user renovate-bot "Renovate Bot"
  api POST /orgs priya '{"username":"acme","full_name":"Acme Co.","visibility":"public"}' >/dev/null
  create_repo storefront "Customer-facing web store"
  create_repo payments-api "Charges, refunds, and ledgers"
  create_repo sandbox "Scratch space for experiments"

  write_config
  echo "Gitea users all share the password $PASSWORD (log in as $VIEWER to browse)."
}

write_config() {
  local slack=false discord=false
  [[ -n ${NAVI_SLACK_TOKEN:-} && -n ${NAVI_DEMO_SLACK_DM_TO:-} ]] && slack=true
  # A user id needs a bot token; a webhook URL doesn't.
  [[ ${NAVI_DEMO_DISCORD_DM_TO:-} == *://* || (-n ${NAVI_DEMO_DISCORD_DM_TO:-} && -n ${NAVI_DISCORD_TOKEN:-}) ]] &&
    discord=true
  cat >"$DEMO_DIR/config.toml" <<EOF
[general]
poll_interval_secs = 5
comment_min_age_secs = 0

[gitea]
enabled = true
api_base = "$API"
track_prs = true

[slack]
enabled = $slack
dm_to = "${NAVI_DEMO_SLACK_DM_TO:-self}"

[discord]
enabled = $discord
dm_to = "${NAVI_DEMO_DISCORD_DM_TO:-}"

[email]
enabled = true
smtp_host = "localhost"
smtp_port = $MAILPIT_SMTP_PORT
tls = "none"
from = "navi <navi@acme.test>"
to = "Jamie Rivera <jamie@acme.test>"

[rules]
mute_authors = ["renovate-bot"]

[rules.repos]
deny = ["$ORG/sandbox"]
EOF
  echo "destinations: email (Mailpit), slack=$slack, discord=$discord"
}

# --- scenario helpers -------------------------------------------------------

# open_pr AUTHOR REPO BRANCH TITLE BODY [REVIEWER...] -> PR number
open_pr() {
  local author=$1 repo=$2 branch=$3 title=$4 body=$5
  shift 5
  local file="${branch//\//-}.md"
  api POST "/repos/$ORG/$repo/contents/$file" "$author" \
    "$(jq -n --arg b "$branch" --arg c "$(printf '%s\n' "$title" | base64 -w0)" --arg m "$title" \
      '{content: $c, message: $m, branch: "main", new_branch: $b}')" >/dev/null
  local number
  number=$(api POST "/repos/$ORG/$repo/pulls" "$author" \
    "$(jq -n --arg h "$branch" --arg t "$title" --arg d "$body" '{head: $h, base: "main", title: $t, body: $d}')" \
    | jq -r .number)
  if (($#)); then request_review "$author" "$repo" "$number" "$@"; fi
  echo "$number"
}

push_commit() {
  local author=$1 repo=$2 branch=$3 message=$4
  local file="${branch//\//-}.md" sha
  sha=$(api GET "/repos/$ORG/$repo/contents/$file?ref=$branch" "$author" | jq -r .sha)
  api PUT "/repos/$ORG/$repo/contents/$file" "$author" \
    "$(jq -n --arg b "$branch" --arg s "$sha" --arg c "$(printf '%s\n' "$message" | base64 -w0)" --arg m "$message" \
      '{content: $c, sha: $s, message: $m, branch: $b}')" >/dev/null
}

request_review() {
  local author=$1 repo=$2 number=$3
  shift 3
  api POST "/repos/$ORG/$repo/pulls/$number/requested_reviewers" "$author" \
    "$(jq -n '{reviewers: $ARGS.positional}' --args "$@")" >/dev/null
}

# review USER REPO NUMBER APPROVED|REQUEST_CHANGES|COMMENT BODY -> review id
review() {
  api POST "/repos/$ORG/$2/pulls/$3/reviews" "$1" \
    "$(jq -n --arg e "$4" --arg b "$5" '{event: $e, body: $b}')" | jq -r .id
}

comment() {
  api POST "/repos/$ORG/$2/issues/$3/comments" "$1" "$(jq -n --arg b "$4" '{body: $b}')" >/dev/null
}

merge() {
  api POST "/repos/$ORG/$2/pulls/$3/merge" "$1" '{"Do":"squash"}' >/dev/null
}

close_pr() {
  api PATCH "/repos/$ORG/$2/pulls/$3" "$1" '{"state":"closed"}' >/dev/null
}

save() { echo "$2" >"$DEMO_DIR/prs/$1"; }
load() { cat "$DEMO_DIR/prs/$1"; }

# --- commands ---------------------------------------------------------------

cmd_run() {
  [[ -f $DEMO_DIR/config.toml ]] || die "run \`$0 up\` first"
  write_config
  mkdir -p "$DEMO_DIR/prs"
  local n rid

  say "initial poll (first run sets navi's watermark)"
  poll 0

  # PRs the digest step merges or closes later. Opened now so navi has seen them.
  save digest-1 "$(open_pr "$VIEWER" storefront chore/analytics-events \
    "Tidy up checkout analytics events" "Drops three events nobody has queried since March.")"
  save digest-2 "$(open_pr "$VIEWER" storefront ci/node-22 \
    "Bump Node to 22 in CI" "Matches what we run in production now.")"
  save digest-3 "$(open_pr "$VIEWER" payments-api chore/legacy-refunds \
    "Remove legacy refund endpoint" "The v1 refund route has had zero traffic for 90 days.")"

  say "review requested: acme/storefront, saved carts"
  n=$(open_pr priya storefront feat/saved-carts "Add saved carts to checkout" \
    "Lets signed-in shoppers park a cart and come back to it later." "$VIEWER")
  poll

  say "(you request changes; no alert for your own action)"
  review "$VIEWER" storefront "$n" REQUEST_CHANGES \
    "A couple of edge cases around expired sessions, see inline notes." >/dev/null
  poll

  say "re-review requested"
  push_commit priya storefront feat/saved-carts "Handle carts that outlive the session"
  request_review priya storefront "$n" "$VIEWER"
  poll

  say "your PR: acme/payments-api, idempotent retries"
  n=$(open_pr "$VIEWER" payments-api feat/idempotent-retries "Retry idempotent charge requests" \
    "Retries charges that fail with a network error, keyed on the idempotency token." marcus sofia)
  poll

  say "review submitted: comment"
  review marcus payments-api "$n" COMMENT \
    "Looks reasonable overall. Left a question about the backoff ceiling." >/dev/null
  poll

  say "review submitted: changes requested"
  review sofia payments-api "$n" REQUEST_CHANGES \
    "We need to skip retries for card-declined errors, those aren't transient." >/dev/null
  poll

  say "reply in a thread you're in"
  comment "$VIEWER" payments-api "$n" "Good catch. Pushed a fix that never retries a 4xx decline."
  comment marcus payments-api "$n" "Nice, that matches how the card networks recommend handling it."
  poll

  say "review submitted: approved"
  review sofia payments-api "$n" APPROVED "LGTM, thanks for the quick turnaround!" >/dev/null
  poll

  say "merged"
  merge marcus payments-api "$n"
  poll

  say "review dismissed"
  n=$(open_pr marcus storefront perf/lazy-images "Lazy-load product images" \
    "Defers offscreen product images; LCP drops about 400ms on the category page." "$VIEWER")
  poll
  rid=$(review "$VIEWER" storefront "$n" APPROVED "Looks great, ship it.")
  poll
  api POST "/repos/$ORG/storefront/pulls/$n/reviews/$rid/dismissals" marcus \
    '{"message":"Rebased onto the new image pipeline, needs another look."}' >/dev/null
  poll

  say "mentioned"
  n=$(open_pr sofia storefront feat/flags-config-service "Move feature flags to the config service" \
    "Reads flags from the config service instead of the env, with a local fallback.")
  comment priya storefront "$n" "@$VIEWER can you sanity-check the fallback when the config service is down?"
  poll

  say "closed"
  n=$(open_pr "$VIEWER" payments-api spike/ledger-sharding "Experiment with ledger sharding" \
    "Spike only, not meant to merge as-is.")
  poll
  comment priya payments-api "$n" "Let's park this until after the Q4 freeze."
  close_pr priya payments-api "$n"
  poll

  say "pending tail for \`$0 dry-run\` (not delivered yet)"
  open_pr renovate-bot storefront renovate/vite-7 "Update dependency vite to v7" \
    "This PR contains the following updates: vite ^6.3.0 -> ^7.0.0" "$VIEWER" >/dev/null
  open_pr priya sandbox try/new-router "Try the new router" "Throwaway, ignore." "$VIEWER" >/dev/null
  n=$(load digest-1)
  comment marcus storefront "$n" "@$VIEWER do we still need the cart_viewed event for the funnel dashboard?"

  echo
  echo "Done. Email: http://localhost:$MAILPIT_HTTP_PORT  Gitea: $GITEA (user $VIEWER / $PASSWORD)"
  echo "Next: \`$0 dry-run\` to preview the tail, \`$0 poll\` to deliver it, then \`$0 digest\`."
}

cmd_dry_run() {
  sleep 2
  navi_env "$NAVI" -c "$DEMO_DIR/config.toml" once --dry-run
}

cmd_digest() {
  [[ -d $DEMO_DIR/prs ]] || die "run \`$0 run\` first"
  write_config
  cat >>"$DEMO_DIR/config.toml" <<EOF

[digest]
enabled = true
interval_secs = 20
kinds = ["merged", "closed"]
EOF
  say "deliver anything still pending, so the digest only holds what follows"
  poll 2

  say "merging and closing PRs for the digest"
  merge marcus storefront "$(load digest-1)"
  merge marcus storefront "$(load digest-2)"
  close_pr sofia payments-api "$(load digest-3)"

  say "running navi for 45s; the digest flushes after 20s"
  # `timeout` exits 124 when it stops navi, which is the expected way out.
  navi_env timeout 45 "$NAVI" -c "$DEMO_DIR/config.toml" run || [[ $? -eq 124 ]] || die "navi run failed"
  write_config
}

cmd_init() {
  local dir="$DEMO_DIR/init"
  rm -rf "$dir"
  mkdir -p "$dir"
  echo "Scratch config at $dir. Decline the background service prompt; it would install a real one."
  env XDG_DATA_HOME="$dir/data" NAVI_NO_UPDATE_CHECK=1 "$NAVI" -c "$dir/config.toml" init
}

cmd_down() {
  docker rm -f "$NAME-gitea" "$NAME-mailpit" >/dev/null 2>&1 || true
  # Only what `up`, `run`, and `init` create, so an overridden NAVI_DEMO_DIR keeps
  # anything else that lives there.
  if [[ -d $DEMO_DIR ]]; then
    (cd "$DEMO_DIR" && rm -rf tokens data config-home prs init config.toml)
    rmdir "$DEMO_DIR" 2>/dev/null || true
  fi
  echo "removed containers and demo state in $DEMO_DIR"
}

case ${1:-} in
  up) cmd_up ;;
  run) cmd_run ;;
  dry-run) cmd_dry_run ;;
  poll) poll 2 ;;
  digest) cmd_digest ;;
  init) cmd_init ;;
  down) cmd_down ;;
  *) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 1 ;;
esac
