#!/usr/bin/env bash
# Build only Codex++; official Codex owns the shared service and saved settings.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
profile=release
install=true
force=false
run_tests=false
usage() {
    cat <<'USAGE'
Usage: ./codex-plus/build.sh [--fast] [--test] [--no-install] [--force]
  --fast         Build the incremental development profile instead of release.
  --test         Run focused Codex++ regressions before building.
  --no-install   Build without installing.
  --force        Replace an existing codex++ without prompting.

Builds the current checkout with its pinned Rust toolchain and Cargo.lock.
Does not fetch, switch branches, reset source, or manage the official Codex service.
USAGE
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --fast) profile=dev ;;
        --test) run_tests=true ;;
        --no-install) install=false ;;
        --force) force=true ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done
for command in git cargo rustup; do
    command -v "$command" >/dev/null || { echo "Required command missing: $command" >&2; exit 1; }
done
mkdir -p "$root/.cache"
lock="$root/.cache/codex-plus-build.lock"
if ! mkdir "$lock" 2>/dev/null; then
    echo "Build lock exists: $lock. If no build is running, remove it and retry." >&2
    exit 1
fi
staged=''
cleanup() {
    [ -z "$staged" ] || rm -f "$staged"
    rmdir "$lock"
}
trap cleanup EXIT
cd "$root/codex-rs"
export STABLE_GIT_COMMIT
STABLE_GIT_COMMIT=$(git -C "$root" rev-parse --verify HEAD)
export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"
echo "Building Codex++ at $STABLE_GIT_COMMIT with the checkout's pinned Rust toolchain."
if rustup run nightly rustfmt --version >/dev/null 2>&1; then
    cargo +nightly fmt --package codex-tui -- --check
else
    echo 'Nightly rustfmt unavailable; formatting check skipped (no toolchain downloaded).' >&2
fi
if $run_tests; then
    "$root/codex-plus/test.sh"
fi
cargo build --locked --profile "$profile" -p codex-tui --bin codex-tui
# Cargo resolves a relative CARGO_TARGET_DIR against codex-rs, our working directory.
target_dir=${CARGO_TARGET_DIR:-target}
case "$target_dir" in /*) ;; *) target_dir="$PWD/$target_dir" ;; esac
profile_dir="$profile"
[ "$profile" != dev ] || profile_dir=debug
binary="$target_dir/$profile_dir/codex-tui"
[ -x "$binary" ] || { echo "Built binary not found: $binary" >&2; exit 1; }
if ! $install; then
    echo "Built $binary"
    exit 0
fi
destination="$HOME/.local/bin/codex++"
if [ -d "$destination" ]; then
    echo "Cannot install over a directory: $destination" >&2
    exit 1
fi
if [ -e "$destination" ] || [ -L "$destination" ]; then
    echo "Warning: replacing existing $destination (official codex is untouched)." >&2
    if ! $force; then
        if [ ! -t 0 ]; then
            echo 'Rerun with --force to accept replacement, or --no-install.' >&2
            exit 1
        fi
        read -r -p 'Replace codex++? [y/N] ' answer
        case "$answer" in y|Y|yes) ;; *) echo 'Build succeeded; installation skipped.'; exit 0 ;; esac
    fi
fi
mkdir -p "$(dirname "$destination")"
staged=$(mktemp "$(dirname "$destination")/.codex-plus-plus.XXXXXX")
cp "$binary" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$destination"
staged=''
echo "Installed $destination"
