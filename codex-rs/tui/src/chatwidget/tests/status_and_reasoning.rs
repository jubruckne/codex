use super::*;
use pretty_assertions::assert_eq;

fn reasoning_completion(id: &str, summary: Vec<String>) -> ServerNotification {
    ServerNotification::ItemCompleted(ItemCompletedNotification {
        thread_id: "thread-1".into(),
        turn_id: "turn-1".into(),
        completed_at_ms: 0,
        item: AppServerThreadItem::Reasoning {
            id: id.into(),
            summary,
            content: vec![],
        },
    })
}

#[tokio::test]
async fn live_multiline_reasoning_wraps_and_commits_once_with_identity() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(None).await;
    chat.on_task_started();
    handle_agent_reasoning_started(&mut chat, "summary-1");
    let summary = "**Checking evidence**\n\nA very long paragraph with several words that must wrap naturally without horizontal truncation.\n\n- first item\n- second item";
    chat.on_agent_reasoning_delta(summary.into());
    let cell = chat.status_state.live_reasoning.as_ref().unwrap();
    assert_eq!(cell.markdown_source(), summary);
    assert_eq!(cell.source_item_id(), Some("summary-1"));
    for width in [80, 24, 12, 80] {
        let lines = cell.display_lines(width);
        assert!(lines.len() > 4);
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        assert!(lines.iter().all(|line| !line.to_string().contains('…')));
    }
    assert!(
        chat.active_cell_transcript_lines(24)
            .unwrap()
            .iter()
            .any(|line| line.to_string().contains("Checking"))
    );
    assert_eq!(
        chat.bottom_pane.status_widget().unwrap().header(),
        "Working"
    );
    let completed = format!("{summary}\n\nComplete authoritative ending.");
    chat.handle_server_notification(
        reasoning_completion("summary-1", vec![completed.clone()]),
        None,
    );
    chat.handle_server_notification(
        reasoning_completion("summary-1", vec![completed.clone()]),
        None,
    );
    assert!(chat.status_state.live_reasoning.is_none());
    let mut found = 0;
    while let Ok(event) = rx.try_recv() {
        if let AppEvent::InsertHistoryCell(cell) = event
            && let Some(cell) = cell
                .as_any()
                .downcast_ref::<history_cell::ReasoningSummaryCell>()
        {
            found += 1;
            assert_eq!(cell.markdown_source(), completed);
            assert_eq!(cell.source_item_id(), Some("summary-1"));
            assert_eq!(cell.display_lines(40), cell.transcript_lines(40));
        }
    }
    assert_eq!(found, 1);
    assert!(chat.bottom_pane.status_indicator_visible());
    chat.on_task_complete(None, None, false);
    assert!(!chat.bottom_pane.status_indicator_visible());
}

#[tokio::test]
async fn status_survives_answer_commit_and_tools_until_interruption() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(None).await;
    chat.on_task_started();
    chat.handle_streaming_delta("Streaming answer with a newline.\n".into());
    chat.run_commit_tick_with_scope(CommitTickScope::AnyMode);
    assert!(chat.bottom_pane.status_indicator_visible());
    chat.flush_answer_stream_with_separator();
    assert!(chat.bottom_pane.status_indicator_visible());
    begin_unified_exec_startup(&mut chat, "tool-1", "process-1", "sleep 2");
    assert!(chat.bottom_pane.status_indicator_visible());
    handle_agent_reasoning_started(&mut chat, "partial");
    chat.on_agent_reasoning_delta("Available summary before interruption".into());
    chat.on_interrupted_turn(TurnAbortReason::Interrupted);
    assert!(!chat.bottom_pane.status_indicator_visible());
    assert!(chat.status_state.live_reasoning.is_none());
    assert!(chat.status_state.reasoning_item_id.is_none());
}

#[tokio::test]
async fn usage_notifications_include_reasoning_without_double_counting() {
    let (mut chat, _rx, _ops) = make_chatwidget_manual(None).await;
    chat.status_state.turn_tokens.seed_empty_thread();
    chat.status_state.turn_tokens.start("turn-1");
    for (input, output, reasoning) in [(500, 128, 100), (500, 128, 100), (1_100, 300, 240)] {
        let total = codex_app_server_protocol::TokenUsageBreakdown {
            total_tokens: input + output,
            input_tokens: input,
            cached_input_tokens: 0,
            cache_write_input_tokens: 0,
            output_tokens: output,
            reasoning_output_tokens: reasoning,
        };
        chat.handle_server_notification(
            ServerNotification::ThreadTokenUsageUpdated(
                codex_app_server_protocol::ThreadTokenUsageUpdatedNotification {
                    thread_id: "thread-1".into(),
                    turn_id: "turn-1".into(),
                    token_usage: codex_app_server_protocol::ThreadTokenUsage {
                        total: total.clone(),
                        last: total,
                        model_context_window: None,
                    },
                },
            ),
            None,
        );
    }
    assert_eq!(chat.status_state.turn_tokens.counts(), Some((1_100, 300)));
    assert_eq!(chat.token_usage().reasoning_output_tokens, 240);
}

#[test]
fn reasoning_markdown_source_preserves_indentation_and_title_only_parts() {
    let parts = vec![
        "    indented code\n\n**Heading**\n<!-- -->".to_string(),
        "Second paragraph\nwith a newline".to_string(),
    ];
    let cell = history_cell::new_reasoning_summary_block(parts.clone(), std::path::Path::new("."));
    assert_eq!(cell.markdown_source(), parts.join("\n\n"));
    let text = lines_to_single_string(&cell.display_lines(30));
    assert!(text.contains("Heading"));
    // The narrower reasoning column may wrap the code across rendered lines.
    let words = text.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(words.contains("indented code"));
}
