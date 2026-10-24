# Codex++

Codex++ is the customized standalone Codex TUI maintained in
[`jubruckne/codex`](https://github.com/jubruckne/codex/tree/codex-plus).
Edit and commit Rust source directly on `codex-plus`; numbered patches are no
longer the development workflow. The initial import combines the final tested
customizations on official `rust-v0.164.0`
(`6aaac694803004a63c6f393d153c96d22521a6b9`). The earlier patch-wrapper repository
is retained at commit `fe94b76` as a recovery snapshot.

## Build and run

Use the repository's pinned Rust toolchain and locked dependencies. Building the
Apple Foundation Models integration on Apple Silicon requires an Xcode 26+ SDK
and Swift toolchain. Other supported targets disable that integration.

From the repository root:

```bash
./codex-plus/build.sh --fast --no-install  # development build
./codex-plus/test.sh                     # focused customization and CLI tests
./codex-plus/build.sh --test --force     # validate, release-build and install
codex++ -c tui.fullscreen_transcript=true
```

The build helper builds only `codex-tui` and installs it as
`~/.local/bin/codex++`. `--no-install` keeps the artifact in the Cargo target
directory; `--force` accepts replacement of an existing custom installation.
It builds the current checkout without fetching, switching revisions, or
resetting source. Run `codex++ --version` after installation to verify the build.

Codex++ attaches to an already-running compatible official Codex service.
Manage that service, authentication and saved settings using official `codex`.
This frontend does not start, update, restart, replace or stop the service;
its saved configuration surfaces are read-only. Ordinary task and session
operations still run through the official service. A successful connection does
not guarantee that every newer frontend RPC is supported by an older service.

## Custom behavior

- Fullscreen subagent windows start at 11 lines and eight columns wider than the
  original quarter-width default, tile from the right, and retain
  session positions and sizes. Drag a title to move, side/bottom borders to resize,
  and click to bring forward. New windows start collapsed until summary/fallback
  text is ready, then expand automatically. Explicit collapse/expand choices take
  precedence. **▲** collapses to title and a one-line caption; **▼** expands.
  **F6** toggles all windows. The title carries a status glyph (spinner while working, ○ idle, ? approval, ◇ input, ! error); reported
  tokens sit in the bottom border. All nine
  interior rows are available for summary text. Animation settings are respected.
  The composer and main status bar stay unobscured.
- On resume, only agents actively generating are admitted to the floating view;
  idle/unloaded/error states and agents waiting for input or approval stay hidden,
  regardless of recency. A hidden agent appears automatically when it begins
  generating. Once admitted in this TUI run, a window stays visible. Agents created
  during this run remain visible even after closing; archive/delete removes them.
  After ten minutes of inactivity, border, name and border counters turn gray
  together; fresh activity restores the stable agent color. Polling, summarization
  and resume notifications do not reset the inactivity clock. Empty resumed trees
  show no automatic windows; unknown token counts stay hidden. Manual F6 choices
  remain session-local.
- On supported Macs, Apple Foundation Models summarize only the current turn's
  assistant messages and public reasoning summaries while an agent is active, or
  its most recent finished turn while idle. Failed/interrupted turns retain their
  explicit outcome. Older turns never provide background to a current summary;
  a new turn keeps the previous displayed summary until a valid replacement is
  ready, while excluding all previous-turn text from the new model input.
  The former three-response quota is replaced by bounded turn input (32 source
  entries and a model-context byte budget), retaining the 15-minute source cutoff.
  The prompt requests 30–40 words total in first person, in one or two short
  paragraphs, plus a separate single-sentence caption of at most ten words for
  collapsed windows in place of status text. Both are generated in one request
  through Apple's schema-guided generation and replaced together after validation;
  no caption placeholder is shown. Concise instructions and two simple examples
  distinguish research/proposals from implementation/verification. Tense is selected
  in code for the turn's lifecycle. While idle, a completed final assistant report
  supersedes earlier thinking and updates; thinking remains available when no such
  report exists. Summaries over 40 words or captions over ten words are retried
  once, then rejected while the previous text remains visible. Sparse evidence may
  yield fewer words than the 30-word target.
  Live source updates use received event times. Running paginated turns recover
  completed items and their timestamps through passive item-history reads; open
  items remain excluded. Legacy history contains completed assistant/reasoning
  events and can also trigger summaries before turn completion. Where item-level
  timing is unavailable, turn-start timestamps are used conservatively, so recent
  output inside a long-running older turn can be omitted. Inference runs
  asynchronously with serialized requests, cancellation, input-hash caching and
  stale-result rejection, including lifecycle changes. Completed assistant messages
  and public reasoning items trigger inference immediately; streaming fragments do
  not. Turn completion also triggers an immediate final summary, without debounce.
  A clean prepared session per visible subagent prewarms the fixed instructions
  and prompt prefix ahead of demand. Used sessions are discarded and replenished
  with fresh prewarmed sessions, so prior outputs never become conversation context.
  Already-ready jobs run without waiting for prewarming; sessions are released when
  their agents leave the active view. Lifecycle wording follows the stable prefix
  in each request, rather than changing the cached system instructions. Startup unavailability
  disables semantic summaries and keeps the latest response/thinking excerpt;
  there is no cloud fallback.
- Fullscreen contextual feedback and Find/reverse-search queries share the gap
  above the composer, leaving drafts untouched. The separate passive hint row is
  removed; the configured model/usage/path/context bar remains below the composer.
- Numbered footer destinations and Ctrl+0–9 navigate root/subagent threads.
  `/subagents` opens the current-tree overview; `/agents` retains the upstream
  command center. `/context` inspects local persisted context estimates.
- Command presentation, public reasoning styling, per-turn token/context counters
  and interactive `codex++ resume` behavior are carried forward from the custom
  frontend. Full tool/command details remain available through Ctrl+T.

## Branches and everyday changes

Use `origin` for `https://github.com/jubruckne/codex.git` and `upstream` for
`https://github.com/openai/codex.git`.

Keep `main` as an unmodified upstream mirror. Keep all customization work on
`codex-plus`, with small feature commits or short-lived topic branches. Run the
relevant focused tests and review UI snapshots before committing. Regenerate
schemas when changing upstream configuration types, following repository guidance.
Use the committed toolchain and Cargo.lock; keep unrelated dependency updates out
of UI changes. Published branch history is retained during upgrades.

The former wrapper's build cache is not the editable source of truth. A normal
clone of the fork plus its `codex-plus` branch contains the implementation and
helpers needed for development and recovery.

## Upgrade to an official release

Choose a published stable release and use a clean working tree. If the clone is
shallow, run `git fetch --unshallow upstream` first so Git can find the release
merge base. For example,
replace the version below with the release being adopted:

```bash
version=0.165.0
git fetch upstream "tag" "rust-v$version"
git switch codex-plus
git switch -c "upgrade/$version"
git merge --no-ff "rust-v$version"
./codex-plus/test.sh
./codex-plus/build.sh --no-install
```

Resolve conflicts against the final behavior, especially daemon attachment guards,
read-only settings, event routing, transcript/composer geometry and local-model
lifecycle. Check new upstream configuration/event types and service RPC needs.
Review snapshot changes for actual layout or behavior changes. On supported Macs,
exercise both available-model and unavailable-model paths, plus a window
move/resize/collapse and draft/search smoke check. Use the official CLI separately
if the chosen release requires a newer service.

After validation, merge the upgrade branch into `codex-plus` and push that branch.
Record the adopted upstream release in this document. Keep the previous known-good
commit available for rollback. Updating the upstream-mirror `main` is a separate
fast-forward operation and does not change the custom release base.

## Validation and CI

`test.sh` runs focused customization filters and standalone TUI CLI tests rather
than the full upstream suite. The existing upstream test
`reasoning_copy_joins_soft_wraps_and_retains_the_existing_rendering` has a known
expectation mismatch with the custom 60% reasoning width; broader transcript runs
must account for that separately. Saved-config persistence tests can also conflict
with the intentional read-only frontend policy.

Before enabling GitHub Actions on the fork, review the inherited workflows and
limit CI to build/test jobs for this branch. Release, publishing and privileged
upstream automation need separate configuration. Local-model availability is not
guaranteed on CI runners; ordinary checks should exercise the unavailable fallback,
with real Apple-model smoke tests run explicitly on a supported Mac.
