//! Desktop handoff output and clear-screen history reset behavior.

use super::*;
use crate::app::tests::make_test_app_with_channels;
use crate::history_cell;
use crate::history_cell::HistoryCell;
use crate::history_cell::PlainHistoryCell;
use pretty_assertions::assert_eq;

#[test]
fn desktop_thread_opened_history_snapshot() {
    let cell = history_cell::new_info_event(
        DESKTOP_THREAD_OPENED_MESSAGE.to_string(),
        /*hint*/ None,
    );

    insta::assert_snapshot!("desktop_thread_opened_history", render_cell(&cell));
}

#[test]
fn desktop_thread_open_error_history_snapshot() {
    let cell = history_cell::new_error_event(desktop_thread_open_error_message("launch failed"));

    insta::assert_snapshot!("desktop_thread_open_error_history", render_cell(&cell));
}

fn render_cell(cell: &impl HistoryCell) -> String {
    let lines = cell.display_lines(/*width*/ 80);
    lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn owned_clear_resets_navigation_and_retains_one_fresh_header() -> Result<()> {
    let (mut app, _rx, _op_rx) = make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(/*owned*/ true)?;
    app.insert_history_cell(
        &mut tui,
        Box::new(PlainHistoryCell::new(vec!["old".into()])),
    );
    app.transcript_view
        .jump_to_entry(&app.transcript_cells, /*index*/ 0);
    app.reset_transcript_state_after_clear();
    app.queue_clear_ui_header(&mut tui);
    app.queue_clear_ui_header(&mut tui);

    assert_eq!(app.transcript_cells.len(), 1);
    assert_eq!(
        app.transcript_cells[0].display_lines(/*width*/ 80),
        app.clear_ui_header_lines(/*width*/ 80),
    );
    app.chat_widget.set_raw_output_mode(/*enabled*/ true);
    assert_eq!(
        app.transcript_cells[0].raw_lines(),
        app.clear_ui_header_lines(/*width*/ 80),
    );
    assert!(app.transcript_view.is_following());
    assert!(tui.pending_history_lines_for_test().is_empty());
    tui.set_owned_screen(/*owned*/ false)?;
    Ok(())
}

#[tokio::test]
async fn routine_commands_history_insertion_keeps_summary_and_full_transcript() -> Result<()> {
    let (mut app, _rx, _ops) = make_test_app_with_channels().await;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    let script = "git status --short; rg -n needle file; sed -n '1,10p' file";
    let mut cell = crate::exec_cell::new_active_exec_command(
        "inspection".into(),
        vec![script.into()],
        vec![],
        codex_app_server_protocol::CommandExecutionSource::Agent,
        None,
        false,
    );
    cell.remember_original_command("inspection", script);
    cell.complete_call(
        "inspection",
        crate::exec_cell::CommandOutput::new(0, "retained inspection output".into()),
        std::time::Duration::from_secs(1),
    );
    app.insert_history_cell(&mut tui, Box::new(cell));
    assert!(!tui.pending_history_lines_for_test().is_empty());
    assert!(!app.render_transcript_lines_for_reflow(160).lines.is_empty());
    let retained = app.transcript_cells.last().unwrap();
    let summary = retained.display_lines(160).iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
    assert!(summary.contains("Searched") && summary.contains("Read file"));
    assert!(!summary.contains("git status") && !summary.contains("1–10"));
    let full = retained
        .transcript_lines(160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(full.contains(script) && full.contains("retained inspection output"));
    Ok(())
}

#[tokio::test]
async fn native_command_history_insertion_keeps_output_only_in_full_transcript() -> Result<()> {
    for owned in [false, true] {
        for completed in [false, true] {
            let (mut app, _rx, _op_rx) = make_test_app_with_channels().await;
            let mut tui = crate::tui::test_support::make_test_tui()?;
            tui.set_owned_screen(owned)?;
            let script = "echo 'original quoting'\n    echo next";
            let mut cell = crate::exec_cell::new_active_exec_command(
                "call".into(),
                vec![script.into()],
                vec![],
                codex_app_server_protocol::CommandExecutionSource::Agent,
                None,
                false,
            );
            cell.remember_original_command("call", script);
            cell.append_output("call", "secret output");
            if completed {
                cell.complete_call(
                    "call",
                    crate::exec_cell::CommandOutput::new(1, "secret output".into()),
                    std::time::Duration::from_secs(1),
                );
            }
            app.insert_history_cell(&mut tui, Box::new(cell));
            let retained = app.transcript_cells.last().unwrap();
            let visible = retained
                .display_lines(120)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            let full = retained
                .transcript_lines(120)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(full.contains("secret output"));
            if !owned {
                assert!(visible.contains("echo 'original quoting'"));
                assert!(visible.contains("    echo next"));
                assert!(!visible.contains("secret output"));
                assert!(
                    !tui.pending_history_lines_for_test()
                        .iter()
                        .any(|line| line.line.to_string().contains("secret output"))
                );
            } else {
                assert_eq!(visible.contains("secret output"), !completed);
                assert!(tui.pending_history_lines_for_test().is_empty());
            }
            tui.set_owned_screen(false)?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn other_tool_native_history_commits_invocations_without_payloads() -> Result<()> {
    for owned in [false, true] {
        let (mut app, _rx, _ops) = make_test_app_with_channels().await;
        let mut tui = crate::tui::test_support::make_test_tui()?;
        tui.set_owned_screen(owned)?;
        let mut mcp = history_cell::new_active_mcp_tool_call(
            "mcp".into(),
            history_cell::McpInvocation {
                server: "example".into(),
                tool: "lookup".into(),
                arguments: Some(serde_json::json!({"path": "file"})),
            },
            false,
        );
        mcp.complete(
            std::time::Duration::ZERO,
            Ok(codex_protocol::mcp::CallToolResult {
                content: vec![serde_json::json!({"type": "text", "text": "MCP RESULT PAYLOAD"})],
                is_error: None,
                structured_content: None,
                meta: None,
            }),
        );
        app.insert_history_cell(&mut tui, Box::new(mcp));
        let mut calls = Vec::new();
        for id in ["first", "second"] {
            let cell = history_cell::DynamicToolCallCell::from_item(
                codex_app_server_protocol::ThreadItem::DynamicToolCall {
                    id: id.into(),
                    namespace: Some("host".into()),
                    tool: "lookup".into(),
                    arguments: serde_json::json!({"id": id}),
                    status: codex_app_server_protocol::DynamicToolCallStatus::InProgress,
                    content_items: None,
                    success: None,
                    duration_ms: None,
                },
            )
            .unwrap();
            calls.push(cell.clone());
            app.insert_history_cell(&mut tui, Box::new(cell));
        }
        let before = tui.pending_history_lines_for_test();
        for call in calls.iter().rev() {
            assert!(call.update_from_item(
                codex_app_server_protocol::ThreadItem::DynamicToolCall {
                    id: call.call_id().into(),
                    namespace: Some("host".into()),
                    tool: "lookup".into(),
                    arguments: serde_json::json!({"id": call.call_id()}),
                    status: codex_app_server_protocol::DynamicToolCallStatus::Completed,
                    content_items: Some(vec![
                        codex_app_server_protocol::DynamicToolCallOutputContentItem::InputText {
                            text: "DYNAMIC RESULT PAYLOAD".into()
                        }
                    ]),
                    success: Some(true),
                    duration_ms: Some(10),
                }
            ));
            app.flush_native_history(&mut tui);
        }
        let inserted = tui.pending_history_lines_for_test();
        if owned {
            assert!(inserted.is_empty());
        } else {
            let text = inserted
                .iter()
                .map(|line| line.line.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("example.lookup") && text.contains("host.lookup"));
            assert!(!text.contains("RESULT PAYLOAD"));
            assert_eq!(text.matches("host.lookup").count(), 2);
            assert!(inserted.len() > before.len());
            app.flush_native_history(&mut tui);
            assert_eq!(tui.pending_history_lines_for_test(), inserted);
        }
        let calls: Vec<_> = app
            .transcript_cells
            .iter()
            .filter(|cell| {
                cell.as_any().is::<history_cell::McpToolCallCell>()
                    || cell.as_any().is::<history_cell::DynamicToolCallCell>()
            })
            .collect();
        assert_eq!(calls.len(), 3);
        for cell in calls {
            assert!(
                cell.transcript_hyperlink_lines(120)
                    .iter()
                    .any(|line| line.line.to_string().contains("RESULT PAYLOAD"))
            );
        }
        tui.set_owned_screen(false)?;
    }
    Ok(())
}
