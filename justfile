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

# Install the release tooling (one-time): cargo-release + cargo-dist.
install-release-tools:
    cargo install cargo-release --locked
    cargo install cargo-dist --locked

# Preview the release artifacts cargo-dist would build
dist-plan:
    dist plan

# Preview a release without changing anything (level = patch|minor|major|<version>).
release-dry level:
    cargo release {{level}}

# Cut a release (main only): cargo-release bumps every crate's version AND the
# internal cross-crate dep requirements in lockstep, commits, tags v<version>,
# and pushes. The tag triggers the cargo-dist release workflow, which builds the
# binaries/installers and runs publish-crates.yml to publish all crates.
# level = patch|minor|major|rc|<explicit-version>.
release level:
    cargo release {{level}} --execute
