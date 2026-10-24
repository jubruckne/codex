//! Read-only estimates of persisted model context, independent of transcript presentation.
//!
//! This intentionally does not claim to reconstruct the daemon's complete live prompt. In
//! particular, a rollback beyond the bounded checkpoint and unknown persisted formats fail closed.

use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::path::Path;

use base64::Engine;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::ThreadId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ImageDetail;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::num_format::format_with_separators;
use codex_protocol::protocol::EventMsg;
use codex_rollout::ModelContextScan;
use codex_rollout::ModelContextScanProgress;
use codex_rollout::ReverseJsonlScanner;
use codex_rollout::ScanOutcome;
use ratatui::style::Stylize;
use ratatui::text::Line;

use crate::history_cell::HistoryCell;
use crate::token_usage::TokenUsageInfo;
use crate::wrapping::word_wrap_lines;

mod panel;
mod tool_results;

const BASE_INSTRUCTIONS_KIND: &str = "model.base_instructions";
const IMAGE_BYTES: u64 = 7373;

#[derive(Debug, Clone, Copy)]
enum Kind {
    Base,
    Developer,
    User,
    Assistant,
    Reasoning,
    Calls,
    Results,
    Compaction,
    Other,
}

impl Kind {
    const ALL: [Self; 9] = [
        Self::Base,
        Self::Developer,
        Self::User,
        Self::Assistant,
        Self::Reasoning,
        Self::Calls,
        Self::Results,
        Self::Compaction,
        Self::Other,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Base => "Base instructions",
            Self::Developer => "Developer instructions",
            Self::User => "User messages",
            Self::Assistant => "Assistant messages",
            Self::Reasoning => "Reasoning",
            Self::Calls => "Tool calls",
            Self::Results => "Tool results",
            Self::Compaction => "Compaction",
            Self::Other => "Other items",
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct Cost {
    bytes: u64,
    unknown: bool,
    opaque: bool,
    media: bool,
}

impl Cost {
    fn text(text: &str) -> Self {
        Self {
            bytes: text.len() as u64,
            ..Self::default()
        }
    }

    fn json(value: &(impl serde::Serialize + ?Sized)) -> Self {
        match serde_json::to_vec(value) {
            Ok(bytes) => Self {
                bytes: bytes.len() as u64,
                ..Self::default()
            },
            Err(_) => Self {
                unknown: true,
                ..Self::default()
            },
        }
    }

    fn add(mut self, other: Self) -> Self {
        self.bytes = self.bytes.saturating_add(other.bytes);
        self.unknown |= other.unknown;
        self.opaque |= other.opaque;
        self.media |= other.media;
        self
    }

    fn tokens(self) -> u64 {
        self.bytes.div_ceil(4)
    }
}

#[derive(Debug, Default)]
struct Row {
    items: u64,
    tokens: u64,
    unknown: bool,
}

#[derive(Debug, Default)]
pub(crate) struct ContextSummary {
    rows: [Row; 9],
    tool_results: tool_results::ToolResultsSummary,
    notices: Vec<String>,
}

impl ContextSummary {
    fn add(&mut self, kind: Kind, cost: Cost) {
        let row = &mut self.rows[kind as usize];
        row.items = row.items.saturating_add(1);
        row.tokens = row.tokens.saturating_add(cost.tokens());
        row.unknown |= cost.unknown;
    }
}

/// Freeze the logical file length before scanning, including compressed rollouts. All I/O runs
/// on a blocking worker; no history lease, writer, materialization, or model request is involved.
pub(crate) fn load(path: &Path, thread_id: ThreadId) -> Result<ContextSummary, String> {
    let mut file = codex_rollout::open_rollout_seekable_reader(path, || false).map_err(|_| {
        "Could not read the local rollout. Reconnect or resume the thread and retry /context."
            .to_string()
    })?;
    let snapshot_end = file.seek(SeekFrom::End(0)).map_err(read_error)?;
    file.rewind().map_err(read_error)?;
    let mut first_line = String::new();
    BufReader::new((&mut file).take(snapshot_end))
        .read_line(&mut first_line)
        .map_err(read_error)?;
    let meta = match codex_rollout::parse_rollout_line(&first_line)
        .map_err(|_| "The rollout's session metadata is unavailable.".to_string())?
        .item
    {
        RolloutItem::SessionMeta(line) if line.meta.id == thread_id => line.meta,
        _ => return Err("The rollout does not belong to the viewed thread.".to_string()),
    };

    // A valid record without a final newline is usable. Only an EOF parse error at the very
    // end may be a writer's partial record; malformed completed records always reject the report.
    let ends_in_newline = if snapshot_end > 0 {
        file.seek(SeekFrom::Start(snapshot_end - 1))
            .map_err(read_error)?;
        let mut byte = [0];
        file.read_exact(&mut byte).map_err(read_error)?;
        byte[0] == b'\n'
    } else {
        false
    };
    let mut scanner = ReverseJsonlScanner::new_at(file, snapshot_end).map_err(read_error)?;
    let mut scan = ModelContextScan::default();
    let mut first = true;
    let mut partial_tail = false;
    while let Some(record) = scanner.scan_next_rollout_line().map_err(read_error)? {
        match record {
            ScanOutcome::Parsed(line) => {
                if scan.push(line.item) == ModelContextScanProgress::Complete { break; }
            }
            ScanOutcome::Rejected(error) if first && !ends_in_newline && error.is_eof() => {
                partial_tail = true;
            }
            ScanOutcome::Rejected(_) => return Err("A completed rollout record is malformed or unsupported; the context breakdown is unavailable.".to_string()),
        }
        first = false;
    }
    let items = scan.finish();
    // Only the boundary selected by ModelContextScan proves that an inherited prefix is
    // fully represented. An older checkpoint in a full replay may itself be rolled back.
    let has_checkpoint = matches!(items.first(), Some(RolloutItem::Compacted(c))
        if c.replacement_history.is_some() && c.window_number.is_some());
    if meta.history_base.is_some() && !has_checkpoint {
        return Err("Inherited context is stored in another rollout and no complete local checkpoint is available.".to_string());
    }
    let history = replay(items)?;
    let mut report = summarize_at(
        &history,
        meta.base_instructions
            .as_ref()
            .map(|base| base.text.as_str()),
        &meta.cwd,
    );
    if partial_tail {
        report
            .notices
            .push("An incomplete trailing record was ignored.".to_string());
    }
    Ok(report)
}

fn read_error(_: std::io::Error) -> String {
    "The rollout changed or could not be read. Retry /context.".to_string()
}

/// Undo frames retain only history displaced by compaction, rather than copying growing history
/// for every turn. Ambiguous input/prelude boundaries fail closed instead of approximating the
/// daemon's contextual-fragment and retained-source rollback rules.
#[derive(Default)]
struct UndoFrame {
    turn_id: String,
    start_len: usize,
    user_turn: bool,
    input_items: u32,
    input_events: u32,
    has_prelude: bool,
    replaced: Vec<Vec<ResponseItemEnvelope>>,
}

fn replay(items: Vec<RolloutItem>) -> Result<Vec<ResponseItemEnvelope>, String> {
    let mut history = Vec::new();
    let mut frames: Vec<UndoFrame> = Vec::new();
    for item in items {
        match item {
            RolloutItem::ResponseItem(envelope) => {
                // Match ContextManager::is_api_message. Configuration updates carry no content
                // cost and require harness provenance; control-only and unknown items cannot be
                // interpreted as model-visible text.
                match &envelope.item {
                    ResponseItem::Message { role, .. } if role == "system" => continue,
                    ResponseItem::ConfigurationUpdate { .. } => continue,
                    ResponseItem::CompactionTrigger {} => continue,
                    ResponseItem::Other => {
                        return Err(
                            "An unknown response-item type prevents a reliable context breakdown."
                                .to_string(),
                        );
                    }
                    _ => {}
                }
                if let Some(frame) = frames.last()
                    && envelope.turn_id().is_some_and(|id| id != frame.turn_id)
                {
                    return Err("Interleaved turn history cannot be reconstructed reliably by the frontend.".to_string());
                }
                if let Some(frame) = frames.last_mut() {
                    if matches!(&envelope.item, ResponseItem::Message { role, .. } if role == "user")
                        || matches!(&envelope.item, ResponseItem::AgentMessage { .. })
                    {
                        frame.input_items = frame.input_items.saturating_add(1);
                    } else if frame.input_items == 0 {
                        frame.has_prelude = true;
                    }
                }
                history.push(envelope);
            }
            RolloutItem::InterAgentCommunication(message) => {
                if let Some(frame) = frames.last_mut() {
                    frame.user_turn = true;
                    frame.input_items = frame.input_items.saturating_add(1);
                }
                history.push(ResponseItemEnvelope::from(message.to_model_input_item()));
            }
            RolloutItem::Compacted(compacted) => {
                let Some(replacement) = compacted.replacement_history else {
                    return Err("This legacy compaction has no replacement history; its active context cannot be reconstructed reliably.".to_string());
                };
                let previous = std::mem::replace(&mut history, replacement);
                if let Some(frame) = frames.last_mut() {
                    frame.replaced.push(previous);
                }
            }
            RolloutItem::EventMsg(EventMsg::TurnStarted(start)) => {
                frames.push(UndoFrame {
                    turn_id: start.turn_id,
                    start_len: history.len(),
                    ..UndoFrame::default()
                });
            }
            RolloutItem::EventMsg(EventMsg::UserMessage(_)) => {
                if let Some(frame) = frames.last_mut() {
                    frame.user_turn = true;
                    frame.input_events = frame.input_events.saturating_add(1);
                }
            }
            RolloutItem::EventMsg(EventMsg::ThreadRolledBack(rollback)) => {
                let mut remaining = rollback.num_turns;
                while remaining > 0 {
                    let Some(frame) = frames.pop() else {
                        return Err("Rollback reaches beyond the available context checkpoint or lacks turn boundaries; the breakdown is unavailable.".to_string());
                    };
                    if frame.user_turn
                        && (frame.has_prelude || frame.input_items != 1 || frame.input_events > 1)
                    {
                        return Err("Rollback with contextual preludes or multiple inputs cannot be reconstructed reliably by the frontend.".to_string());
                    }
                    for previous in frame.replaced.into_iter().rev() {
                        history = previous;
                    }
                    history.truncate(frame.start_len);
                    if frame.user_turn {
                        remaining -= 1;
                    }
                }
            }
            // These are host-side facts or duplicate presentation events, not prompt items.
            _ => {}
        }
    }
    if history
        .iter()
        .any(|entry| matches!(entry.item, ResponseItem::Other))
    {
        return Err(
            "An unknown checkpoint item prevents a reliable context breakdown.".to_string(),
        );
    }
    Ok(history)
}

fn base_kinds(item: &ResponseItem) -> Option<&[codex_protocol::models::ContentItemKind]> {
    match item {
        ResponseItem::Message {
            internal_chat_message_metadata_passthrough: Some(meta),
            ..
        } => meta.content_item_kinds.as_deref(),
        _ => None,
    }
}

#[cfg(test)]
fn summarize(history: &[ResponseItemEnvelope], base: Option<&str>) -> ContextSummary {
    summarize_at(history, base, Path::new(""))
}

fn summarize_at(
    history: &[ResponseItemEnvelope],
    base: Option<&str>,
    directory: &Path,
) -> ContextSummary {
    let mut report = ContextSummary::default();
    let recorded_base = history.iter().any(|envelope| {
        base_kinds(&envelope.item).is_some_and(|kinds| {
            kinds
                .iter()
                .any(|kind| kind.as_str() == BASE_INSTRUCTIONS_KIND)
        })
    });
    if !recorded_base {
        match base {
            Some(base) if !base.is_empty() => report.add(Kind::Base, Cost::text(base)),
            None => report
                .notices
                .push("Base instructions were not persisted and are excluded.".to_string()),
            _ => {}
        }
    }
    let mut flags = Cost::default();
    let mut result_analysis = tool_results::Analysis::new(history).in_directory(directory);
    for envelope in history {
        let item = &envelope.item;
        let kind = match item {
            ResponseItem::Message { role, .. } => match role.as_str() {
                "developer"
                    if base_kinds(item).is_some_and(|kinds| {
                        !kinds.is_empty()
                            && kinds
                                .iter()
                                .all(|kind| kind.as_str() == BASE_INSTRUCTIONS_KIND)
                    }) =>
                {
                    Kind::Base
                }
                "developer" => Kind::Developer,
                "user" => Kind::User,
                "assistant" => Kind::Assistant,
                _ => Kind::Other,
            },
            ResponseItem::AgentMessage { .. } => Kind::Other,
            ResponseItem::Reasoning { .. } => Kind::Reasoning,
            ResponseItem::FunctionCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. } => Kind::Calls,
            ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. } => Kind::Results,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => {
                Kind::Compaction
            }
            _ => Kind::Other,
        };
        let cost = item_cost(item);
        flags = flags.add(cost);
        report.add(kind, cost);
        if matches!(kind, Kind::Results) {
            result_analysis.record(item, cost);
        }
    }
    report.tool_results = result_analysis.finish();
    if flags.opaque {
        report.notices.push("Encrypted content uses upstream size heuristics; plaintext reasoning summaries have no replay token cost.".to_string());
    }
    if flags.media {
        report.notices.push(
            "Image costs use upstream estimates; image payloads and URLs are not counted as text."
                .to_string(),
        );
    }
    if flags.unknown {
        report.notices.push("Audio or unsupported content costs are unavailable. >= marks estimates that exclude those costs.".to_string());
    }
    report
}

// Keep these estimates aligned with core/context_manager/history.rs. Count content, never the
// serialized transport envelope. Opaque payloads use the same distinct ciphertext heuristics.
fn item_cost(item: &ResponseItem) -> Cost {
    match item {
        ResponseItem::Message {
            content,
            encrypted_content,
            ..
        } => {
            let mut cost = content
                .iter()
                .map(content_cost)
                .fold(Cost::default(), Cost::add);
            cost.unknown |= encrypted_content.is_some();
            cost
        }
        ResponseItem::AgentMessage {
            author,
            recipient,
            content,
            ..
        } => content
            .iter()
            .map(|part| match part {
                AgentMessageInputContent::InputText { text } => Cost::text(text),
                AgentMessageInputContent::EncryptedContent { encrypted_content } => {
                    encrypted_output_cost(encrypted_content)
                }
            })
            .fold(Cost::text(author).add(Cost::text(recipient)), Cost::add),
        ResponseItem::Reasoning {
            encrypted_content: Some(content),
            ..
        } => Cost {
            bytes: (content.len() as u64)
                .saturating_mul(3)
                .checked_div(4)
                .unwrap_or(0)
                .saturating_sub(650),
            opaque: true,
            ..Cost::default()
        },
        ResponseItem::Reasoning { .. } => Cost::default(),
        ResponseItem::Compaction {
            encrypted_content, ..
        }
        | ResponseItem::ContextCompaction {
            encrypted_content: Some(encrypted_content),
            ..
        } => Cost {
            bytes: codex_protocol::protocol::estimate_compaction_content_length(
                encrypted_content.len(),
            ) as u64,
            opaque: true,
            ..Cost::default()
        },
        ResponseItem::ContextCompaction { .. } => Cost::default(),
        ResponseItem::FunctionCall {
            name,
            namespace,
            arguments: input,
            encrypted_function_args,
            encrypted_content,
            ..
        } => {
            let mut cost = Cost::text(name)
                .add(Cost::text(namespace.as_deref().unwrap_or("functions")))
                .add(Cost::text(input));
            cost.unknown = encrypted_function_args.is_some() || encrypted_content.is_some();
            cost
        }
        ResponseItem::CustomToolCall {
            name,
            namespace,
            input,
            ..
        } => Cost::text(name)
            .add(Cost::text(namespace.as_deref().unwrap_or("functions")))
            .add(Cost::text(input)),
        ResponseItem::FunctionCallOutput {
            call_id,
            name,
            namespace,
            output,
            ..
        } => output_cost(&output.body)
            .add(Cost::text(call_id.as_deref().unwrap_or_default()))
            .add(Cost::text(name.as_deref().unwrap_or_default()))
            .add(Cost::text(namespace.as_deref().unwrap_or_default())),
        ResponseItem::CustomToolCallOutput {
            call_id,
            name,
            output,
            ..
        } => output_cost(&output.body)
            .add(Cost::text(call_id))
            .add(Cost::text(name.as_deref().unwrap_or_default())),
        ResponseItem::AdditionalTools { tools, .. } => Cost::json(tools),
        ResponseItem::ToolSearchOutput { tools, .. } => Cost::json(tools),
        ResponseItem::ToolSearchCall { arguments, .. } => Cost::json(arguments),
        ResponseItem::LocalShellCall { action, .. } => Cost::json(action),
        ResponseItem::WebSearchCall { action, .. } => {
            action.as_ref().map(Cost::json).unwrap_or_default()
        }
        ResponseItem::ImageGenerationCall {
            revised_prompt,
            result,
            ..
        } => Cost::text(revised_prompt.as_deref().unwrap_or_default()).add(Cost {
            bytes: if result.is_empty() { 0 } else { IMAGE_BYTES },
            media: !result.is_empty(),
            ..Cost::default()
        }),
        _ => Cost {
            unknown: true,
            ..Cost::default()
        },
    }
}

fn content_cost(part: &ContentItem) -> Cost {
    match part {
        ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => Cost::text(text),
        ContentItem::InputImage { image, detail } => image_cost(image, *detail),
        ContentItem::InputAudio { .. } => Cost {
            unknown: true,
            ..Cost::default()
        },
    }
}

fn output_cost(body: &FunctionCallOutputBody) -> Cost {
    match body {
        FunctionCallOutputBody::Text(text) => Cost::text(text),
        FunctionCallOutputBody::ContentItems(parts) => parts
            .iter()
            .map(|part| match part {
                FunctionCallOutputContentItem::InputText { text } => Cost::text(text),
                FunctionCallOutputContentItem::InputImage { image, detail } => {
                    image_cost(image, *detail)
                }
                FunctionCallOutputContentItem::InputAudio { .. } => Cost {
                    unknown: true,
                    ..Cost::default()
                },
                FunctionCallOutputContentItem::EncryptedContent { encrypted_content } => {
                    encrypted_output_cost(encrypted_content)
                }
            })
            .fold(Cost::default(), Cost::add),
    }
}

fn encrypted_output_cost(content: &str) -> Cost {
    Cost {
        bytes: (content.len() as u64).saturating_mul(9).div_ceil(16),
        opaque: true,
        ..Cost::default()
    }
}

fn image_cost(image: &ImageReference, detail: Option<ImageDetail>) -> Cost {
    let bytes = if detail == Some(ImageDetail::Original) {
        match image {
            ImageReference::File { .. } => 40_000,
            ImageReference::Inline { image_url } => {
                original_image_bytes(image_url).unwrap_or(IMAGE_BYTES)
            }
        }
    } else {
        IMAGE_BYTES
    };
    Cost {
        bytes,
        media: true,
        ..Cost::default()
    }
}

fn original_image_bytes(url: &str) -> Option<u64> {
    let (metadata, payload) = url.split_once(',')?;
    if !metadata.to_ascii_lowercase().starts_with("data:image/")
        || !metadata
            .split(';')
            .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    Some(
        (u64::from(image.width()).div_ceil(32) * u64::from(image.height()).div_ceil(32))
            .min(10_000)
            * 4,
    )
}

#[derive(Debug)]
pub(crate) struct ContextSummaryCell {
    pub(crate) summary: ContextSummary,
    pub(crate) usage: Option<TokenUsageInfo>,
}

impl ContextSummaryCell {
    fn item_lines(&self, narrow: bool) -> Vec<Line<'static>> {
        let mut lines = vec![
            Line::from("Persisted context estimate".bold()),
            Line::default(),
        ];
        if !narrow {
            lines.push(
                "Item type                 Items   Estimated tokens"
                    .bold()
                    .into(),
            );
        }
        let mut total = Row::default();
        for (kind, row) in Kind::ALL.iter().zip(&self.summary.rows) {
            if row.items == 0 {
                continue;
            }
            let tokens = token_label(row.tokens, row.unknown);
            let count = count_label(row.items);
            let line = if narrow {
                format!("{}: {} items, {} tokens", kind.label(), count, tokens)
            } else {
                format!("{:<24} {:>6} {:>18}", kind.label(), count, tokens)
            };
            lines.push(line.into());
            total.items = total.items.saturating_add(row.items);
            total.tokens = total.tokens.saturating_add(row.tokens);
            total.unknown |= row.unknown;
        }
        lines.push(
            format!(
                "Estimated total: {} tokens ({} items)",
                token_label(total.tokens, total.unknown),
                count_label(total.items)
            )
            .into(),
        );
        if let Some(usage) = &self.usage {
            lines.push(
                format!(
                    "Latest daemon-reported context: {} tokens",
                    format_with_separators(usage.last_token_usage.total_tokens)
                )
                .into(),
            );
            if let Some(limit) = usage.model_context_window.filter(|limit| *limit > 0) {
                lines.push(
                    format!(
                        "Model context limit: {} tokens",
                        format_with_separators(limit)
                    )
                    .into(),
                );
            }
        }

        lines.push(Line::default());
        lines.push("Snapshot of persisted context; estimates use roughly 4 bytes per token. Tool definitions, runtime injections, truncation, and unflushed activity may differ from the live prompt.".into());
        lines.extend(self.summary.notices.iter().cloned().map(Line::from));
        lines
    }
}

impl ContextSummaryCell {
    fn lines(&self, narrow: bool) -> Vec<Line<'static>> {
        let mut lines = self.item_lines(narrow);
        lines.extend(self.summary.tool_results.lines(narrow));
        lines
    }
}

fn count_label(value: u64) -> String {
    format_with_separators(i64::try_from(value).unwrap_or(i64::MAX))
}

fn token_label(tokens: u64, unknown: bool) -> String {
    format!(
        "{}{}",
        if unknown { ">=" } else { "~" },
        count_label(tokens)
    )
}

impl HistoryCell for ContextSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        word_wrap_lines(self.lines(width < 52), usize::from(width.max(1)))
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        self.lines(false)
    }
}

#[cfg(test)]
#[path = "context_summary_tests.rs"]
mod tests;
