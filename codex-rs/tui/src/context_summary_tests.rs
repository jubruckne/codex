use super::*;
use codex_protocol::protocol::SessionMeta;
use serde_json::json;
use std::io::Write;

fn response(value: serde_json::Value) -> ResponseItemEnvelope {
    serde_json::from_value::<ResponseItem>(value)
        .unwrap()
        .into()
}

fn message(role: &str, text: &str) -> ResponseItemEnvelope {
    response(json!({"type":"message", "role":role, "content":[{"type":"input_text", "text":text}]}))
}

fn record(kind: &str, payload: serde_json::Value) -> RolloutItem {
    codex_rollout::decode_rollout_line(
        json!({"timestamp":"2026-10-21T12:00:00Z", "type":kind, "payload":payload}),
    )
    .unwrap()
    .item
}

fn start(id: &str) -> RolloutItem {
    record(
        "event_msg",
        json!({"type":"task_started", "turn_id":id, "model_context_window":100_000}),
    )
}

fn user_event() -> RolloutItem {
    record(
        "event_msg",
        json!({"type":"user_message", "message":"request", "images":null, "local_images":[]}),
    )
}

fn rollback(n: u32) -> RolloutItem {
    record(
        "event_msg",
        json!({"type":"thread_rolled_back", "num_turns":n}),
    )
}

fn compact(text: &str) -> RolloutItem {
    record(
        "compacted",
        json!({"message":"summary", "window_number":1,
        "replacement_history":[{"type":"message", "role":"user", "content":[{"type":"input_text", "text":text}]}]}),
    )
}

pub(super) fn write_fixture(items: Vec<RolloutItem>) -> (tempfile::NamedTempFile, ThreadId) {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let meta = SessionMeta::default();
    let id = meta.id;
    let line = json!({"timestamp":"2026-10-21T12:00:00Z", "type":"session_meta", "payload":meta});
    writeln!(file, "{line}").unwrap();
    for item in items {
        let mut line = serde_json::to_value(item).unwrap();
        line.as_object_mut()
            .unwrap()
            .insert("timestamp".into(), json!("2026-10-21T12:00:00Z"));
        writeln!(file, "{line}").unwrap();
    }
    (file, id)
}

#[test]
fn context_summary_groups_content_once_and_excludes_transport_metadata() {
    let mut user = message("user", "🦀"); // Four UTF-8 bytes, one estimated token.
    user.item
        .set_id(Some(codex_protocol::ResponseItemId::from_server(
            "extremely-long-transport-id-that-must-not-count".to_string(),
        )));
    let history = vec![
        message("developer", "rules!!!"),
        user,
        message("assistant", "done"),
        response(json!({"type":"function_call", "name":"run", "arguments":"{}", "call_id":"id"})),
        response(json!({"type":"function_call_output", "call_id":"id", "output":"result"})),
    ];
    let report = summarize(&history, Some("base"));
    assert_eq!(report.rows[Kind::Base as usize].tokens, 1);
    assert_eq!(report.rows[Kind::Developer as usize].tokens, 2);
    assert_eq!(report.rows[Kind::User as usize].tokens, 1);
    assert_eq!(report.rows[Kind::Assistant as usize].tokens, 1);
    assert_eq!(report.rows[Kind::Calls as usize].tokens, 4);
    assert_eq!(report.rows[Kind::Results as usize].tokens, 2);
    assert_eq!(report.rows.iter().map(|row| row.items).sum::<u64>(), 6);
}

#[test]
fn context_summary_deduplicates_recorded_base_instructions() {
    let mut item = serde_json::to_value(message("developer", "persisted base").item).unwrap();
    item["internal_chat_message_metadata_passthrough"] =
        json!({"content_item_kinds":["model.base_instructions"]});
    let history = vec![response(item)];
    let report = summarize(&history, Some("do not count this second copy"));
    assert_eq!(report.rows[Kind::Base as usize].items, 1);
    assert_eq!(report.rows[Kind::Base as usize].tokens, 4);
    assert_eq!(report.rows[Kind::Developer as usize].items, 0);
}

#[test]
fn context_summary_mixed_base_and_developer_content_is_counted_once() {
    let mut item = serde_json::to_value(message("developer", "baserule").item).unwrap();
    item["internal_chat_message_metadata_passthrough"] =
        json!({"content_item_kinds":["model.base_instructions", "other"]});
    let report = summarize(&[response(item)], Some("base"));
    assert_eq!(report.rows[Kind::Base as usize].items, 0);
    assert_eq!(report.rows[Kind::Developer as usize].tokens, 2);
}

#[test]
fn context_summary_compaction_replaces_old_history_and_keeps_new_suffix() {
    let (file, id) = write_fixture(vec![
        RolloutItem::ResponseItem(message("user", "discard me")),
        compact("kept"),
        RolloutItem::ResponseItem(message("assistant", "also kept")),
    ]);
    let report = load(file.path(), id).unwrap();
    assert_eq!(report.rows[Kind::User as usize].items, 1);
    assert_eq!(report.rows[Kind::User as usize].tokens, 1);
    assert_eq!(report.rows[Kind::Assistant as usize].items, 1);
}

#[test]
fn context_summary_rollback_removes_turn_items_without_counting_duplicate_events() {
    let history = replay(vec![
        start("a"),
        user_event(),
        RolloutItem::ResponseItem(message("user", "keep")),
        start("b"),
        user_event(),
        RolloutItem::ResponseItem(message("user", "remove")),
        RolloutItem::ResponseItem(message("assistant", "remove answer")),
        rollback(1),
    ])
    .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].item, message("user", "keep").item);
}

#[test]
fn context_summary_replay_undoes_compaction_in_rolled_back_turn() {
    let history = replay(vec![
        start("a"),
        user_event(),
        RolloutItem::ResponseItem(message("user", "keep")),
        start("b"),
        user_event(),
        RolloutItem::ResponseItem(message("user", "remove")),
        compact("discard summary"),
        rollback(1),
    ])
    .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].item, message("user", "keep").item);
}

#[test]
fn context_summary_rollback_skips_automatic_turns_without_user_boundaries() {
    let history = replay(vec![
        start("a"),
        user_event(),
        RolloutItem::ResponseItem(message("user", "keep")),
        start("b"),
        user_event(),
        RolloutItem::ResponseItem(message("user", "remove")),
        start("automatic"),
        RolloutItem::ResponseItem(message("assistant", "remove too")),
        rollback(1),
    ])
    .unwrap();
    assert_eq!(history.len(), 1);
}

#[test]
fn context_summary_rollback_beyond_checkpoint_is_unavailable() {
    let (file, id) = write_fixture(vec![
        start("old"),
        user_event(),
        compact("summary"),
        rollback(1),
    ]);
    assert!(
        load(file.path(), id)
            .unwrap_err()
            .contains("Rollback reaches beyond")
    );
}

#[test]
fn context_summary_ambiguous_rollback_preludes_and_multiple_inputs_are_unavailable() {
    for prelude in [
        message("developer", "persistent instructions"),
        message("user", "<environment_context>context</environment_context>"),
    ] {
        let error = replay(vec![
            start("a"),
            user_event(),
            RolloutItem::ResponseItem(prelude),
            RolloutItem::ResponseItem(message("user", "input")),
            rollback(1),
        ])
        .unwrap_err();
        assert!(error.contains("preludes or multiple inputs"));
    }
}

#[test]
fn context_summary_unknown_checkpoint_items_and_interleaved_turns_are_unavailable() {
    let checkpoint = record(
        "compacted",
        json!({"message":"summary", "window_number":1,
        "replacement_history":[{"type":"future_unknown_item"}]}),
    );
    assert!(
        replay(vec![checkpoint])
            .unwrap_err()
            .contains("unknown checkpoint item")
    );
    let mut late = message("assistant", "late message");
    late.item.set_turn_id_if_missing("old");
    assert!(
        replay(vec![start("new"), RolloutItem::ResponseItem(late)])
            .unwrap_err()
            .contains("Interleaved")
    );
}

#[test]
fn context_summary_legacy_compaction_without_replacement_is_unavailable() {
    assert!(
        replay(vec![record("compacted", json!({"message":"summary"}))])
            .unwrap_err()
            .contains("legacy compaction")
    );
}

#[test]
fn context_summary_system_messages_and_control_events_are_not_prompt_items() {
    let history = replay(vec![
        RolloutItem::ResponseItem(message("system", "not retained upstream")),
        record("response_item", json!({"type":"compaction_trigger"})),
        RolloutItem::ResponseItem(message("user", "keep")),
    ])
    .unwrap();
    assert_eq!(history.len(), 1);
    assert!(
        replay(vec![record(
            "response_item",
            json!({"type":"future_unknown_item"})
        )])
        .is_err()
    );
}

#[test]
fn context_summary_partial_tail_is_ignored_but_completed_corruption_is_rejected() {
    let (mut file, id) = write_fixture(vec![RolloutItem::ResponseItem(message("user", "keep"))]);
    writeln!(file, "{{bad json}}").unwrap();
    assert!(load(file.path(), id).unwrap_err().contains("malformed"));
    let (mut file, id) = write_fixture(vec![RolloutItem::ResponseItem(message("user", "keep"))]);
    write!(file, "{{\"timestamp\":").unwrap();
    let report = load(file.path(), id).unwrap();
    assert_eq!(report.rows[Kind::User as usize].items, 1);
    assert!(
        report
            .notices
            .iter()
            .any(|notice| notice.contains("trailing"))
    );
}

#[test]
fn context_summary_valid_last_record_needs_no_newline() {
    let (file, id) = write_fixture(vec![RolloutItem::ResponseItem(message("user", "keep"))]);
    let length = file.as_file().metadata().unwrap().len();
    file.as_file().set_len(length - 1).unwrap();
    assert_eq!(
        load(file.path(), id).unwrap().rows[Kind::User as usize].items,
        1
    );
}

#[test]
fn context_summary_missing_wrong_thread_and_inherited_rollouts_fail_safely() {
    assert!(
        load(
            Path::new("/nonexistent/context-summary-test.jsonl"),
            ThreadId::new()
        )
        .is_err()
    );
    let (file, _) = write_fixture(vec![]);
    assert!(
        load(file.path(), ThreadId::new())
            .unwrap_err()
            .contains("viewed thread")
    );
    // A fork's local suffix is not its entire inherited model context.
    let mut lines: Vec<serde_json::Value> = std::fs::read_to_string(file.path())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    lines[0]["payload"]["history_base"] =
        json!({"thread_id":ThreadId::new(), "end_ordinal_exclusive":10, "end_byte_offset":100});
    std::fs::write(file.path(), format!("{}\n", lines[0])).unwrap();
    let id = ThreadId::from_string(lines[0]["payload"]["id"].as_str().unwrap()).unwrap();
    assert!(
        load(file.path(), id)
            .unwrap_err()
            .contains("Inherited context")
    );
}

#[test]
fn context_summary_opaque_costs_do_not_count_ciphertext_as_plaintext() {
    let reasoning =
        response(json!({"type":"reasoning", "summary":[], "encrypted_content":"a".repeat(1000)}));
    assert_eq!(item_cost(&reasoning.item).tokens(), 25); // (1000 * 3/4 - 650) / 4
    let compact = response(json!({"type":"compaction", "encrypted_content":"a".repeat(1200)}));
    assert_eq!(item_cost(&compact.item).tokens(), 100);
    let output = response(
        json!({"type":"function_call_output", "output":[{"type":"encrypted_content", "encrypted_content":"a".repeat(1600)}]}),
    );
    assert_eq!(item_cost(&output.item).tokens(), 225);
    let plain_reasoning = response(
        json!({"type":"reasoning", "summary":[{"type":"summary_text", "text":"visible summary"}]}),
    );
    assert_eq!(item_cost(&plain_reasoning.item).tokens(), 0);
}

#[test]
fn context_summary_inherited_context_requires_the_selected_complete_checkpoint() {
    for complete in [false, true] {
        let checkpoint = if complete {
            compact("inherited and local context")
        } else {
            record(
                "compacted",
                json!({"message":"summary", "replacement_history":[]}),
            )
        };
        let (file, id) = write_fixture(vec![checkpoint]);
        let contents = std::fs::read_to_string(file.path()).unwrap();
        let mut lines: Vec<serde_json::Value> = contents
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        lines[0]["payload"]["history_base"] =
            json!({"thread_id":ThreadId::new(), "end_ordinal_exclusive":10, "end_byte_offset":100});
        let text = lines
            .iter()
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        std::fs::write(file.path(), text).unwrap();
        let result = load(file.path(), id);
        if complete {
            assert!(result.is_ok(), "{result:?}");
        } else {
            assert!(result.unwrap_err().contains("Inherited context"));
        }
    }
}

#[test]
fn context_summary_media_costs_exclude_base64_and_mark_audio_unknown() {
    let image = ImageReference::Inline {
        image_url: format!("data:image/png;base64,{}", "A".repeat(10000)),
    };
    assert_eq!(image_cost(&image, None).tokens(), 1844);
    assert_eq!(
        image_cost(&image, Some(ImageDetail::Original)).tokens(),
        1844
    );
    let audio = response(
        json!({"type":"message", "role":"user", "content":[{"type":"input_audio", "audio_url":"data:audio/wav;base64,AAAA"}]}),
    );
    let report = summarize(&[audio], Some(""));
    assert!(report.rows[Kind::User as usize].unknown);
    assert!(report.notices.iter().any(|notice| notice.contains("Audio")));
}

#[test]
fn context_summary_original_image_uses_patch_dimensions() {
    let image = image::DynamicImage::new_rgb8(64, 32);
    let mut bytes = std::io::Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png).unwrap();
    let url = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    );
    assert_eq!(
        image_cost(
            &ImageReference::Inline { image_url: url },
            Some(ImageDetail::Original)
        )
        .tokens(),
        2
    );
}

#[test]
fn context_summary_rendering_separates_reported_totals_and_wraps_narrow_views() {
    let cell = ContextSummaryCell {
        summary: summarize(&[message("user", "test")], Some("base")),
        usage: Some(TokenUsageInfo {
            total_token_usage: Default::default(),
            last_token_usage: crate::token_usage::TokenUsage {
                total_tokens: 12345,
                ..Default::default()
            },
            model_context_window: Some(100000),
        }),
    };
    let text = cell
        .raw_lines()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Estimated total: ~2 tokens (2 items)"));
    assert!(text.contains("12,345"));
    assert!(text.contains("100,000"));
    assert!(!text.contains("Developer instructions"));
    for width in [1, 20, 51, 80] {
        for line in cell.display_lines(width) {
            assert!(line.width() <= usize::from(width), "{width}: {line}");
        }
    }
}
