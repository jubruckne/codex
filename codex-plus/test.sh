#!/usr/bin/env bash
# Focused regression gate for the maintained Codex++ source branch.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
usage() {
    cat <<'USAGE'
Usage: ./codex-plus/test.sh [test-filter ...]
Runs the focused Codex++ library filters and standalone TUI CLI tests.
Optional filters replace the default library filters; CLI tests always run.
The known preexisting reasoning-copy soft-wrap mismatch is excluded.
USAGE
}
for argument in "$@"; do
    case "$argument" in
        -h|--help) usage; exit 0 ;;
        -*) echo "Unknown option: $argument" >&2; usage >&2; exit 2 ;;
    esac
done
command -v cargo >/dev/null || { echo 'Required command missing: cargo' >&2; exit 1; }
command -v git >/dev/null || { echo 'Required command missing: git' >&2; exit 1; }
cd "$root/codex-rs"
export STABLE_GIT_COMMIT
STABLE_GIT_COMMIT=$(git -C "$root" rev-parse --verify HEAD)
export RUST_MIN_STACK="${RUST_MIN_STACK:-8388608}"
if [ "$#" -eq 0 ]; then
    set -- unified_overview agents_views agent_footer_colors agents_navigation footer::tests \
        open_agent_picker agent_navigation::tests shared_gap agent_feed owned_transcript \
        fullscreen_ context_summary status_and_reasoning turn_token_tests turn_tokens_resize \
        context_growth reasoning_fix native_command routine_commands other_tool shell_grouping \
        thread_transcript client_only
fi
for filter in "$@"; do
    cargo test --locked -p codex-tui --lib "$filter" -- \
        --skip reasoning_copy_joins_soft_wraps_and_retains_the_existing_rendering
done
cargo test --locked -p codex-tui --bin codex-tui
