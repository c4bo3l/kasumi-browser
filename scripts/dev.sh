#!/usr/bin/env bash
# Dev-mode launcher for Kasumi Browser.
#
# Equivalent of `npm run dev`: builds the debug binary (with CEF auto-fetched on
# first run) and launches it with development-friendly logging.
#
# Usage:
#   ./scripts/dev.sh            # build + run
#   ./scripts/dev.sh --clean    # remove CEF cache before launching
#   ./scripts/dev.sh --release  # build + run the optimized binary
#   ./scripts/dev.sh --watch    # auto-restart on source changes (needs cargo-watch)

set -euo pipefail

cd "$(dirname "$0")/.."

LOG_LEVEL="${RUST_LOG:-info}"
CARGO_PROFILE=""
CLEAN_CACHE=0
WATCH=0

for arg in "$@"; do
    case "$arg" in
        --clean)
            CLEAN_CACHE=1
            ;;
        --release)
            CARGO_PROFILE="--release"
            ;;
        --watch)
            WATCH=1
            ;;
        --log=*)
            LOG_LEVEL="${arg#--log=}"
            ;;
        -h|--help)
            sed -n '2,12p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *)
            echo "Unknown option: $arg" >&2
            exit 1
            ;;
    esac
done

if [ "$CLEAN_CACHE" -eq 1 ]; then
    echo "Cleaning CEF cache..."
    rm -rf target/cef_cache
fi

export RUST_LOG="$LOG_LEVEL"

if [ "$WATCH" -eq 1 ]; then
    if ! command -v cargo-watch >/dev/null 2>&1; then
        echo "cargo-watch is required for --watch. Install it with:" >&2
        echo "  cargo install cargo-watch" >&2
        exit 1
    fi
    echo "Starting Kasumi Browser in watch mode (RUST_LOG=$LOG_LEVEL)..."
    exec cargo watch -x "run --bin kasumi-browser $CARGO_PROFILE"
fi

echo "Starting Kasumi Browser (RUST_LOG=$LOG_LEVEL)..."
exec cargo run --bin kasumi-browser $CARGO_PROFILE
