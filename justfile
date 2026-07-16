# navi-notifier task runner. Run `just` to list recipes.

# Development
# -----------

# Install Rust + JS dev dependencies
install: install-rust install-js

install-rust:
    cargo fetch

install-js:
    npm install

# Build the release binary
build:
    cargo build --release

clean:
    cargo clean

# Testing
# -------

# Run the whole workspace test suite (mock-based; no network)
test:
    cargo test --workspace

# Run the live e2e smoke test against real GitHub + Slack.
# Needs NAVI_GITHUB_TOKEN + NAVI_SLACK_TOKEN (and optionally NAVI_SLACK_DM_TO).
e2e:
    cargo run -p navi-notifier --features e2e --bin navi-e2e

# Formatting & linting
# --------------------

# Format, lint, and test everything
check: format lint test

format:
    cargo fmt --all

# Lint Rust + Markdown
lint: lint-rust lint-md

lint-rust:
    cargo fmt --all --check
    cargo clippy --workspace --all-targets -- -D warnings

lint-md:
    npx --yes markdownlint-cli2 "**/*.md"

# Release
# -------

# Preview the release artifacts cargo-dist would build
dist-plan:
    dist plan

# Cut a release (main only): tag the current workspace version and push it,
# which triggers the dist release workflow. Bump the version in the root
# Cargo.toml [workspace.package] first, then commit, then run this.
release:
    #!/usr/bin/env bash
    set -euo pipefail
    branch="$(git rev-parse --abbrev-ref HEAD)"
    if [ "$branch" != "main" ]; then
        echo "release: must be on 'main' (currently on '$branch')" >&2
        exit 1
    fi
    if [ -n "$(git status --porcelain --untracked-files=no)" ]; then
        echo "release: tracked changes are uncommitted; commit first" >&2
        exit 1
    fi
    # Read the workspace version from the [workspace.package] table.
    version="$(awk -F'"' '/^\[workspace.package\]/{p=1} p && /^[[:space:]]*version[[:space:]]*=/{print $2; exit}' Cargo.toml)"
    if [ -z "$version" ]; then
        echo "release: could not read version from Cargo.toml" >&2
        exit 1
    fi
    git tag -a "v${version}" -m "v${version}"
    git push --follow-tags
    echo "tagged and pushed v${version}"
