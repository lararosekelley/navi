#!/bin/sh
# navi one-line installer: https://github.com/lararosekelley/navi
#
# Source of truth for the script served at https://larakelley.com/sh/navi.
# Thin wrapper: downloads the cargo-dist-generated installer
# (navi-notifier-installer.sh) from the latest GitHub release and runs it,
# forwarding any arguments. Release artifacts ship .sha256 checksums for manual
# download and verification.
set -eu
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/lararosekelley/navi/releases/latest/download/navi-notifier-installer.sh \
  | sh -s -- "$@"
