//! Grouped command details retain terminal input in raw output without exposing hidden reasoning.

use super::*;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::model::ExecCall;
use crate::history_cell::HistoryCell;
use crate::history_cell::new_reasoning_summary_block;
use crate::history_cell::new_unified_exec_interaction;
use codex_app_server_protocol::CommandExecutionSource;
use pretty_assertions::assert_eq;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

fn completed_read(name: &str, output: &str) -> ExecCell {
    let command = vec!["cat".to_owned(), name.to_owned()];
    let parsed = codex_shell_command::parse_command::parse_command(&command);
    ExecCell::new(
        ExecCall {
            call_id: name.to_owned(),
            command,
            parsed,
            output: Some(CommandOutput::new(/*exit_code*/ 0, output.to_owned())),
            source: CommandExecutionSource::UnifiedExecStartup,
            start_time: None,
            duration: Some(Duration::from_millis(/*millis*/ 5)),
            interaction_input: None,
        },
        /*animations_enabled*/ false,
    )
}

#[test]
fn raw_grouped_history_retains_terminal_input_and_omits_reasoning() {
    let mut group = completed_read("first.txt", "first output");
    group
        .group
        .push_detail(Arc::from(new_reasoning_summary_block(
            vec!["Inspecting the next file".to_owned()],
            Path::new("."),
        ) as Box<dyn HistoryCell>));
    group
        .group
        .push_detail(Arc::new(new_unified_exec_interaction(
            Some("cat first.txt".to_owned()),
            "continue\n".to_owned(),
        )));
    group
        .append_completed(completed_read("second.txt", "second output"))
        .expect("adjacent reads stay grouped");

    let raw = group
        .raw_lines()
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>();
    let rich = group
        .transcript_lines(/*width*/ 80)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(format!("Rich\n{rich}\n\nRaw\n{}", raw.join("\n")));

    let mut earlier = completed_read("earlier.txt", "earlier output");
    earlier
        .group
        .push_detail(Arc::from(new_reasoning_summary_block(
            vec!["Inspect the first file".to_owned()],
            Path::new("."),
        ) as Box<dyn HistoryCell>));
    earlier
        .group
        .push_detail(Arc::new(new_unified_exec_interaction(
            Some("cat earlier.txt".to_owned()),
            "next\n".to_owned(),
        )));
    let mut expected_raw = earlier.raw_lines();
    expected_raw.push(Line::from(""));
    expected_raw.extend(group.raw_lines());
    let mut expected_rich = earlier.transcript_hyperlink_lines(/*width*/ 80);
    expected_rich.push(HyperlinkLine::from(""));
    expected_rich.extend(group.transcript_hyperlink_lines(/*width*/ 80));
    group.prepend(earlier);
    assert_eq!(group.raw_lines(), expected_raw);
    assert_eq!(
        group.transcript_hyperlink_lines(/*width*/ 80),
        expected_rich
    );
}

fn completed_script(script: &str, output: &str, code: i32, seconds: u64) -> ExecCell {
    let mut cell = completed_read("script", output);
    let call = &mut cell.group.calls[0];
    call.command = vec!["/bin/zsh".into(), "-lc".into(), script.into()];
    call.parsed =
        vec![codex_protocol::parse_command::ParsedCommand::Unknown { cmd: script.into() }];
    call.output = Some(CommandOutput::new(code, output.into()));
    call.duration = Some(Duration::from_secs(seconds));
    cell
}

#[test]
fn fullscreen_keeps_exact_multiline_shell_and_python_source() {
    let script = "python3 - <<'PY'\nvalue = 'two  spaces'\nprint(value)\nPY\necho \"$HOME\"";
    let cell = completed_script(script, "hidden result", 0, 1);
    for lines in [
        cell.compact_hyperlink_lines(100),
        cell.display_hyperlink_lines(100),
        cell.transcript_hyperlink_lines(100),
    ] {
        let text = lines
            .iter()
            .map(|line| line.line.to_string())
            .collect::<Vec<_>>();
        for source in script.lines() {
            assert!(
                text.iter().any(|row| row.ends_with(source)),
                "missing {source:?}: {text:?}"
            );
        }
        let python = lines
            .iter()
            .find(|row| row.line.to_string().ends_with("print(value)"))
            .unwrap();
        let expected = super::super::native::command_highlights(script);
        let expected = &expected[2];
        assert!(python.line.spans.ends_with(&expected.spans));
    }
}

#[test]
fn fullscreen_completed_output_is_disclosed_even_for_one_line() {
    for output in ["one line", "one\ntwo\nthree\nfour\nfive", ""] {
        let cell = completed_script("echo result", output, 7, 10);
        for lines in [
            cell.compact_hyperlink_lines(100),
            cell.display_hyperlink_lines(100),
        ] {
            let rows = lines
                .iter()
                .map(|line| line.line.to_string())
                .collect::<Vec<_>>();
            assert_eq!(rows.len(), 1);
            assert!(rows[0].contains("(exit 7)"));
            assert!(rows[0].contains("10.00s"));
        }
        assert_eq!(
            cell.activity_disclosure(100),
            (!output.is_empty()).then_some(crate::history_cell::ActivityDisclosure::OutputLines(
                output.lines().count()
            ))
        );
        let detailed = cell
            .transcript_lines(100)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>();
        for row in output.lines() {
            assert!(detailed.iter().any(|line| line == row));
        }
    }
    let short = completed_script("echo result", "", 0, 9);
    assert_eq!(
        short.compact_hyperlink_lines(100)[0].line.to_string(),
        "• Ran echo result"
    );
}

#[test]
fn fullscreen_active_output_remains_visible() {
    let mut cell = completed_script("echo result", "live result", 0, 1);
    cell.group.calls[0].duration = None;
    for lines in [
        cell.compact_hyperlink_lines(100),
        cell.display_hyperlink_lines(100),
    ] {
        assert!(
            lines
                .iter()
                .any(|line| line.line.to_string().contains("live result"))
        );
    }
}

#[test]
fn fullscreen_reasoning_has_one_blank_before_next_command_and_no_trailing_gap() {
    let mut cell = completed_read("first.txt", "first output");
    cell.group
        .push_detail(Arc::from(new_reasoning_summary_block(
            vec!["Inspecting the next file".into()],
            Path::new("."),
        ) as Box<dyn HistoryCell>));
    let visible = cell.compact_hyperlink_lines(100);
    assert!(!visible.last().unwrap().line.to_string().trim().is_empty());
    cell.append_completed(completed_read("second.txt", "second output"))
        .unwrap();
    let rows = cell
        .transcript_lines(100)
        .iter()
        .map(Line::to_string)
        .collect::<Vec<_>>();
    let reasoning = rows
        .iter()
        .position(|row| row.contains("Inspecting the next file"))
        .unwrap();
    assert_eq!(rows[reasoning + 1], "");
    assert_eq!(rows[reasoning + 2], "$ cat second.txt");
}

#[test]
fn fullscreen_metadata_uses_reported_exit_code_and_unknown_failure() {
    for (reported, expected) in [(Some(23), "(exit 23)"), (None, "(failed)")] {
        let mut cell = completed_script("silent-command", "", 1, 10);
        cell.remember_exit_code("script", reported);
        for width in [24, 80] {
            for lines in [
                cell.compact_hyperlink_lines(width),
                cell.display_hyperlink_lines(width),
            ] {
                assert!(
                    lines
                        .iter()
                        .all(|line| line.line.width() <= usize::from(width))
                );
                let words = lines
                    .iter()
                    .map(|line| {
                        let text = line.line.to_string();
                        text.trim_start_matches([' ', '│', '└']).to_owned()
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                let text = words.split_whitespace().collect::<Vec<_>>().join(" ");
                assert!(text.contains(expected), "{text}");
                assert!(text.contains("10.00s"), "{text}");
                assert!(!text.contains("(exit 1)"), "{text}");
            }
        }
    }
}

#[test]
fn fullscreen_duration_threshold_and_fully_visible_source_need_no_disclosure() {
    for millis in [9_999, 10_000] {
        let mut cell = completed_script("echo first\necho second", "", 0, 0);
        cell.group.calls[0].duration = Some(Duration::from_millis(millis));
        for width in [24, 80] {
            assert_eq!(cell.activity_disclosure(width), None);
            for lines in [
                cell.compact_hyperlink_lines(width),
                cell.display_hyperlink_lines(width),
            ] {
                let text = lines
                    .iter()
                    .map(|line| line.line.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                assert_eq!(text.contains("10.00s"), millis >= 10_000, "{text}");
                assert!(text.contains("echo first"), "{text}");
                assert!(text.contains("echo second"), "{text}");
            }
        }
    }
}
