#!/usr/bin/env bash
# dev/run-server-suite.sh
#
# The live-server test invocation (see CONTRIBUTING.md) for jellyfin-api,
# jellyfin-core, and media-cache: pure-logic unit tests plus API contract
# tests against a live Jellyfin server, run twice -- once for the default
# (non-#[ignore]) suite, once for the #[ignore]-gated live tests.
#
# Assumes: a Jellyfin server is already up and reachable (see
# dev/setup-server.sh / dev/README.md) and the synthetic corpus has been
# scanned into its Movies/Shows libraries. Does not itself start or wait
# for a server; `dev/server.sh up` does that.
#
# Usage: dev/run-server-suite.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

PACKAGES=(-p jellyfin-api -p jellyfin-core -p media-cache)

log() { printf '[run-server-suite] %s\n' "$*" >&2; }

log "cargo test: default (non-#[ignore]) suite -- unit + contract tests"
cargo test "${PACKAGES[@]}" --all-targets

log "cargo test: #[ignore]-gated live-server tests (--ignored)"
cargo test "${PACKAGES[@]}" --all-targets -- --ignored --test-threads=1

log "done."
