use super::*;
use std::io::Write;
use std::str::FromStr;

fn rollout(thread_id: ThreadId) -> tempfile::NamedTempFile {
    let mut file = tempfile::NamedTempFile::new().unwrap();
    let meta = codex_protocol::protocol::SessionMeta {
        id: thread_id,
        ..Default::default()
    };
    writeln!(file, "{}", serde_json::json!({"timestamp":"2026-10-21T12:00:00Z", "type":"session_meta", "payload":meta})).unwrap();
    writeln!(file, "{}", serde_json::json!({"timestamp":"2026-10-21T12:00:00Z", "type":"response_item", "payload":{
        "type":"message", "role":"user", "content":[{"type":"input_text", "text":"private contents never printed"}]
    }})).unwrap();
    file
}

#[test]
fn context_summary_command_is_discoverable_and_available_while_running() {
    assert_eq!(SlashCommand::from_str("context"), Ok(SlashCommand::Context));
    assert!(
        crate::slash_command::built_in_slash_commands()
            .iter()
            .any(|(name, _)| *name == "context")
    );
    assert!(SlashCommand::Context.available_during_task());
    assert!(SlashCommand::Context.available_when_thread_unavailable());
    assert!(SlashCommand::Context.available_in_side_conversation());
    assert!(SlashCommand::Context.description().contains("item type"));
}

#[tokio::test]
async fn context_summary_command_reports_missing_history_without_a_model_operation() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.dispatch_command(SlashCommand::Context);
    let rendered = match rx.try_recv().unwrap() {
        AppEvent::InsertHistoryCell(cell) => lines_to_single_string(&cell.display_lines(80)),
        other => panic!("expected unavailable report, got {other:?}"),
    };
    assert!(rendered.contains("no persisted rollout"));
    assert!(op_rx.try_recv().is_err());
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn context_summary_command_reads_viewed_thread_while_idle_or_running() {
    for running in [false, true] {
        let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
        // The viewed thread may be a subagent: its own rollout supplies the report.
        let thread_id = ThreadId::new();
        let file = rollout(thread_id);
        chat.thread_id = Some(thread_id);
        chat.current_rollout_path = Some(file.path().to_path_buf());
        chat.turn_lifecycle.agent_turn_running = running;
        chat.dispatch_command(SlashCommand::Context);
        let loaded = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        match loaded {
            AppEvent::ContextSummaryLoaded {
                thread_id: owner,
                request_id,
                result,
            } => {
                assert_eq!(owner, thread_id);
                assert!(result.is_ok(), "{result:?}");
                assert!(
                    chat.finish_context_summary(owner, request_id, result)
                        .is_none()
                );
            }
            other => panic!("expected context summary, got {other:?}"),
        };
        let rendered = render_bottom_popup(&chat, 100);
        assert!(rendered.contains("Persisted context estimate"));
        assert!(rendered.contains("User messages"));
        assert!(!rendered.contains("private contents"));
        assert!(op_rx.try_recv().is_err());
        assert!(rx.try_recv().is_err());
    }
}

#[tokio::test]
async fn context_summary_completion_is_discarded_after_thread_switch_or_new_request() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
    let thread_id = ThreadId::new();
    let file = rollout(thread_id);
    chat.thread_id = Some(thread_id);
    chat.current_rollout_path = Some(file.path().to_path_buf());
    chat.dispatch_command(SlashCommand::Context);
    let event = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    let AppEvent::ContextSummaryLoaded {
        thread_id: owner,
        request_id,
        result,
    } = event
    else {
        panic!("expected context result")
    };
    chat.thread_id = Some(ThreadId::new());
    assert!(
        chat.finish_context_summary(owner, request_id, result)
            .is_none()
    );
    assert!(rx.try_recv().is_err());
    chat.thread_id = Some(thread_id);
    chat.context_summary_request = Some(Uuid::new_v4());
    assert!(
        chat.finish_context_summary(owner, request_id, Err("stale failure".to_string()))
            .is_none()
    );
    assert!(rx.try_recv().is_err());
    assert!(op_rx.try_recv().is_err());
}

#[tokio::test]
async fn context_summary_remote_rollout_is_not_read_as_a_local_path() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.local_worktree_operations = false;
    chat.dispatch_command(SlashCommand::Context);
    let rendered = match rx.try_recv().unwrap() {
        AppEvent::InsertHistoryCell(cell) => lines_to_single_string(&cell.display_lines(80)),
        other => panic!("expected unavailable report, got {other:?}"),
    };
    assert!(rendered.contains("remote daemon"));
    assert!(op_rx.try_recv().is_err());
}
