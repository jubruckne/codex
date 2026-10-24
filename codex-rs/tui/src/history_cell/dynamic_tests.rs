//! Dynamic tool projections keep truthful status and bounded previews without losing detail.

use super::*;
use crate::test_support::PathBufExt;
use crate::test_support::test_path_buf;
use crate::thread_transcript::RawReasoningVisibility;
use crate::thread_transcript::thread_items_to_transcript_cells;
use pretty_assertions::assert_eq;
use serde_json::json;

fn item(status: DynamicToolCallStatus, output: Option<&str>) -> ThreadItem {
    ThreadItem::DynamicToolCall {
        id: "dynamic-1".to_string(),
        namespace: Some("example".to_string()),
        tool: "inspect".to_string(),
        arguments: json!({"path": "src/main.rs"}),
        status,
        content_items: output.map(|text| {
            vec![DynamicToolCallOutputContentItem::InputText {
                text: text.to_string(),
            }]
        }),
        success: None,
        duration_ms: None,
    }
}

#[test]
fn dynamic_status_and_output_match_persisted_presentations() {
    let cwd = test_path_buf("/workspace").abs();
    let mut snapshots = Vec::new();
    for (label, status, output) in [
        ("pending", DynamicToolCallStatus::InProgress, None),
        (
            "success",
            DynamicToolCallStatus::Completed,
            Some("Found the definition"),
        ),
        (
            "failure",
            DynamicToolCallStatus::Failed,
            Some("Permission denied"),
        ),
        ("unavailable", DynamicToolCallStatus::Completed, None),
    ] {
        let replayed = thread_items_to_transcript_cells(
            /*thread_id*/ None,
            &cwd,
            [item(status, output)],
            RawReasoningVisibility::Hidden,
            /*config*/ None,
        );
        assert_eq!(replayed.len(), 1);
        let cell = &replayed[0];
        for (mode, lines) in [
            ("compact", cell.display_lines(/*width*/ 80)),
            (
                "full",
                visible_lines(cell.transcript_hyperlink_lines(/*width*/ 80)),
            ),
            ("raw", cell.raw_lines()),
        ] {
            let text = lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            if mode == "compact" || label == "success" {
                snapshots.push(format!("{label}, {mode}\n{text}"));
            }
        }
    }
    insta::assert_snapshot!(snapshots.join("\n\n"));
}

#[test]
fn dynamic_preview_reports_hidden_lines_and_retains_full_output() {
    let mut snapshots = Vec::new();
    for count in [3, 4] {
        let output = (1..=count)
            .map(|index| format!("Result line {index}: retained content"))
            .collect::<Vec<_>>()
            .join("\n");
        let cell =
            DynamicToolCallCell::from_item(item(DynamicToolCallStatus::Completed, Some(&output)))
                .unwrap();
        for width in [20, 80] {
            let compact = cell.display_lines(width);
            assert!(
                compact
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            let text = compact
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            snapshots.push(format!("lines={count}, width={width}\n{text}"));
        }
        let raw = cell
            .raw_lines()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(raw.ends_with(&output));
        let detailed = visible_lines(cell.transcript_hyperlink_lines(/*width*/ 80))
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(output.lines().all(|line| detailed.contains(line)));
    }
    insta::assert_snapshot!(snapshots.join("\n\n"));
}

#[test]
fn other_tool_dynamic_native_motion_cancellation_late_response_and_owned_preservation() {
    let mut call =
        DynamicToolCallCell::from_item(item(DynamicToolCallStatus::InProgress, None)).unwrap();
    let (requester, mut frames) = crate::tui::FrameRequester::test_channel();
    call.configure_native(true, Some(requester), true);
    let live = call
        .display_lines(80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(live.contains("example.inspect") && live.contains("src/main.rs"));
    assert!(frames.try_recv().is_ok());
    call.configure_native(true, None, false);
    assert_eq!(call.display_lines(80).last().unwrap().to_string(), "  └ ◌");
    call.mark_interrupted();
    assert!(
        call.display_lines(80)
            .iter()
            .any(|line| line.to_string().contains("Cancelled"))
    );
    assert!(call.update_from_item(item(
        DynamicToolCallStatus::Completed,
        Some("late result payload")
    )));
    let full = call.transcript_hyperlink_lines(80);
    assert!(
        full.iter()
            .any(|line| line.line.to_string().contains("late result payload"))
    );
    for width in [1, 8, 20, 80] {
        for mode in [
            super::super::HistoryRenderMode::Rich,
            super::super::HistoryRenderMode::Raw,
        ] {
            let compact = call.display_lines_for_mode(width, mode);
            assert!(
                compact
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
            assert!(
                !compact
                    .iter()
                    .any(|line| line.to_string().contains("late result"))
            );
        }
    }
    call.configure_native(false, None, false);
    assert!(
        call.display_lines(80)
            .iter()
            .any(|line| line.to_string().contains("late result payload"))
    );
    assert_eq!(call.transcript_hyperlink_lines(80), full);
    call.configure_native(true, None, false);
    call.update_from_item(item(
        DynamicToolCallStatus::Failed,
        Some("sensitive failure payload"),
    ));
    assert!(
        call.display_lines(80)
            .iter()
            .any(|line| line.to_string().contains("Error:"))
    );
    assert!(
        !call
            .display_lines(80)
            .iter()
            .any(|line| line.to_string().contains("sensitive failure"))
    );
}
