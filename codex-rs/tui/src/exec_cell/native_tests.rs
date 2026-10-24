use super::*;
use crate::exec_cell::CommandOutput;
use crate::exec_cell::new_active_exec_command;
use crate::history_cell::HistoryCell;
use codex_app_server_protocol::CommandExecutionSource;
use std::time::Duration;

fn command(script: &str) -> ExecCell {
    let mut cell = new_active_exec_command(
        "call".into(),
        vec![script.into()],
        vec![],
        CommandExecutionSource::Agent,
        None,
        false,
    );
    let (requester, _) = crate::tui::FrameRequester::test_channel();
    cell.configure_native(true, Some(requester), false);
    cell.remember_original_command("call", script);
    cell
}
fn text(cell: &ExecCell, width: u16) -> Vec<String> {
    cell.display_lines(width)
        .iter()
        .map(ToString::to_string)
        .collect()
}

#[test]
fn routine_commands_live_visibility_scrollback_filter_and_full_transcript() {
    for script in [
        "rg -n 'needle' file",
        "sed -n '1,10p' file",
        "cat file | head -25",
        "git status --short; git diff --stat; pwd; cargo --version",
    ] {
        let mut cell = command(script);
        cell.append_output("call", "inspection output\n");
        assert!(!text(&cell, 160).is_empty());
        assert!(text(&cell, 160).join("\n").contains("inspection output"));
        cell.complete_call(
            "call",
            CommandOutput::new(0, "inspection output\n".into()),
            Duration::from_secs(12),
        );
        // Finished calls remain visible until the live cell is committed.
        assert!(!text(&cell, 160).is_empty());
        let full = cell.transcript_lines(160);
        cell.configure_native(true, None, true);
        assert_eq!(text(&cell, 160).is_empty(), script.starts_with("git "), "{script}");
        assert_eq!(cell.display_lines_for_mode(160, HistoryRenderMode::Raw).iter().map(ToString::to_string).collect::<Vec<_>>(), text(&cell, 160));
        assert_eq!(cell.transcript_lines(160), full);
        let full_text = full
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(full_text.contains(script) && full_text.contains("inspection output"));
        cell.configure_native(false, None, true);
        assert!(!text(&cell, 160).is_empty());
    }
}

#[test]
fn routine_commands_failures_search_no_matches_and_changes_stay_distinct() {
    for (script, exit_code, hidden) in [
        ("rg missing file", 1, false),
        ("grep missing file", 1, true),
        ("rg missing file", 2, false),
        ("cat missing", 1, false),
        ("git diff --exit-code", 1, false),
        ("cargo test", 0, false),
        ("sed -i 's/a/b/' file", 0, false),
        ("rg missing file; cat missing", 1, false),
        ("cmp .build/expected.patch .build/current.patch", 0, true),
        ("cmp .build/expected.patch .build/current.patch", 1, false),
        ("cmp .build/expected.patch .build/current.patch", 2, false),
        ("git diff > .build/diff.patch; cmp .build/expected.patch .build/diff.patch", 0, true),
    ] {
        let mut cell = command(script);
        cell.complete_call(
            "call",
            CommandOutput::new(exit_code, "details".into()),
            Duration::from_secs(1),
        );
        cell.configure_native(true, None, true);
        let rows = text(&cell, 160);
        assert_eq!(rows.is_empty(), hidden, "{script}: {rows:?}");
        if exit_code != 0 && !hidden && !(script == "rg missing file" && exit_code == 1) {
            assert!(rows.join("\n").contains(&format!("exit {exit_code}")));
        }
        assert!(cell
            .transcript_lines(160)
            .iter()
            .any(|line| line.to_string().contains(script)));
    }
    let mut user = command("cat file");
    user.group.calls[0].source = CommandExecutionSource::UserShell;
    user.complete_call(
        "call",
        CommandOutput::new(0, "contents".into()),
        Duration::from_secs(1),
    );
    user.configure_native(true, None, true);
    assert!(!text(&user, 80).is_empty());
    let mut unknown_failure = command("rg missing file");
    unknown_failure.complete_call(
        "call",
        CommandOutput::new(1, String::new()),
        Duration::from_secs(1),
    );
    unknown_failure.remember_exit_code("call", None);
    unknown_failure.configure_native(true, None, true);
    assert!(!text(&unknown_failure, 80).is_empty());
}

#[test]
fn routine_commands_mixed_groups_preserve_visible_header_and_reasoning_order() {
    let mut cell = command("cat file");
    cell.group.calls[0].parsed = vec![codex_protocol::parse_command::ParsedCommand::Read {
        name: "file".into(),
        cmd: "cat file".into(),
        path: "file".into(),
    }];
    let summary = crate::history_cell::new_reasoning_summary_block(
        vec!["**Thinking**\n\nInspection complete.".into()],
        std::path::Path::new("."),
    );
    assert!(cell.append_reasoning(summary).is_ok());
    assert!(cell.add_call(
        "build".into(),
        vec!["cargo build".into()],
        vec![],
        CommandExecutionSource::Agent,
        None,
    ));
    cell.remember_original_command("build", "cargo build");
    cell.complete_call(
        "call",
        CommandOutput::new(0, "contents".into()),
        Duration::from_secs(1),
    );
    cell.complete_call(
        "build",
        CommandOutput::new(0, "built".into()),
        Duration::from_secs(1),
    );
    cell.configure_native(true, None, true);
    let rows = text(&cell, 80);
    assert!(!rows.iter().any(|row| row.contains("cat file") || row == "• Ran"));
    let reasoning = rows
        .iter()
        .position(|row| row.contains("Inspection complete."))
        .unwrap();
    assert_eq!(rows[reasoning + 1], "");
    assert_eq!(rows[reasoning + 2], "• Ran cargo build");
    let full = cell
        .transcript_lines(80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(full.find("cat file").unwrap() < full.find("Inspection complete.").unwrap());
    assert!(full.find("Inspection complete.").unwrap() < full.find("cargo build").unwrap());
    cell.remember_original_command("build", "pwd");
    let rows = text(&cell, 80);
    assert!(rows.iter().any(|row| row.contains("Inspection complete.")));
    assert!(!rows.iter().any(|row| row.contains("Ran") || row.contains("pwd")));
}

#[test]
fn routine_commands_live_rendering_compacts_reads_and_keeps_path_labels_unique() {
    let script = "cat a/style.rs | head -25; sed -n '150,260p' app/events.rs";
    let mut cell = command(script);
    assert!(cell.add_call(
        "other".into(),
        vec!["cat b/style.rs".into()],
        vec![],
        CommandExecutionSource::Agent,
        None,
    ));
    cell.remember_original_command("other", "cat b/style.rs");
    cell.append_output("call", "live read output");
    let live = text(&cell, 120).join("\n");
    assert!(live.contains("Read a/style.rs:1–25, events.rs:150–260"));
    assert!(live.contains("Read b/style.rs"));
    assert!(live.contains("live read output"));
    assert!(!live.contains("sed -n"));
    let full = cell
        .transcript_lines(160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(full.contains(script) && full.contains("cat b/style.rs"));
}

#[test]
fn routine_commands_live_readmes_keep_directory_context() {
    let cell = command("rg -n 'routine' README.md patches/readme.md; cat README.md");
    let live = text(&cell, 120).join("\n");
    assert!(live.contains("Search 'routine' in README.md, patches/readme.md"));
    assert!(live.contains("Read README.md"));

    let cell = command("cat docs/README.md patches/readme.md");
    let live = text(&cell, 120).join("\n");
    assert!(live.contains("Read docs/README.md, patches/readme.md"));
}

#[test]
fn routine_commands_scrollback_merges_reads_and_searches_without_ranges() {
    let mut cell = command("sed -n '1,85p' routine.rs; sed -n '86,165p' routine.rs; cat native.rs");
    let scripts = [
        ("docs", "cat README.md patches/readme.md upstream-revision"),
        ("reads", "cat preview.rs; cat build.sh | head -45"),
        ("search", "rg golden_goose README.md; rg valhalla README.md; rg gold README.md; rg gold README.md"),
        ("other", "rg gold native.rs"),
    ];
    for (id, script) in scripts {
        assert!(cell.add_call(id.into(), vec![script.into()], vec![], CommandExecutionSource::Agent, None));
        cell.remember_original_command(id, script);
    }
    for id in ["call", "docs", "reads", "search", "other"] {
        cell.complete_call(id, CommandOutput::new(0, "exact output".into()), Duration::from_secs(1));
    }
    cell.configure_native(true, None, true);
    assert_eq!(text(&cell, 200), [
        "• Explored",
        "  └ Read routine.rs, native.rs, README.md, patches/readme.md, upstream-revision, preview.rs, build.sh",
        "    Searched 'golden_goose', 'valhalla', 'gold' in README.md",
        "    Searched 'gold' in native.rs",
    ]);
    let full = cell.transcript_lines(200).iter().map(ToString::to_string).collect::<Vec<_>>().join("\n");
    assert!(full.contains("86,165p") && full.contains("exact output"));
    assert!(text(&cell, 30).iter().all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 30));
}

#[test]
fn routine_commands_explored_preserves_search_read_order() {
    let mut cell = command("rg first a.rs; rg second b.rs; cat a.rs; cat b.rs; rg third a.rs; cat a.rs");
    cell.complete_call("call", CommandOutput::new(0, String::new()), Duration::from_secs(1));
    cell.configure_native(true, None, true);
    assert_eq!(text(&cell, 120), [
        "• Explored",
        "  └ Searched 'first' in a.rs",
        "    Searched 'second' in b.rs",
        "    Read a.rs, b.rs",
        "    Searched 'third' in a.rs",
        "    Read a.rs",
    ]);
    let lines = cell.display_lines(120);
    for line in lines.iter().skip(1) {
        assert!(!line.style.add_modifier.contains(Modifier::DIM));
        let action = line.spans.iter().find(|span| {
            matches!(span.content.as_ref(), "Searched " | "Read ")
        }).unwrap();
        assert_eq!(action.style.fg, Some(crate::style::accent_color()));
        for span in line.spans.iter().filter(|span| {
            span.content.contains("a.rs") || span.content.contains("b.rs")
        }) {
            assert_eq!(span.style.fg, None);
            assert!(!span.style.add_modifier.intersects(Modifier::DIM | Modifier::BOLD));
        }
    }
}

#[test]
fn routine_commands_hidden_middle_call_does_not_add_spacing_or_alter_group_heading() {
    let mut cell = command("cargo build");
    for (id, script) in [("read", "cat file"), ("test", "cargo test")] {
        assert!(cell.add_call(
            id.into(),
            vec![script.into()],
            vec![],
            CommandExecutionSource::Agent,
            None,
        ));
        cell.remember_original_command(id, script);
    }
    for id in ["call", "read", "test"] {
        cell.complete_call(
            id,
            CommandOutput::new(0, String::new()),
            Duration::from_secs(1),
        );
    }
    cell.configure_native(true, None, true);
    assert_eq!(text(&cell, 80), ["• Ran", "  │ cargo build", "• Explored", "  └ Read file", "  │ cargo test"]);
}

#[test]
fn native_command_spinner_only_until_output_and_respects_motion() {
    let mut cell = command("echo hello");
    assert_eq!(text(&cell, 80).last().unwrap(), "  └ ◌");
    let mut animated = new_active_exec_command(
        "call".into(),
        vec!["echo".into()],
        vec![],
        CommandExecutionSource::Agent,
        None,
        true,
    );
    let (requester, mut frames) = crate::tui::FrameRequester::test_channel();
    animated.configure_native(true, Some(requester), false);
    let rendered = text(&animated, 80);
    assert!(
        rendered.last().unwrap().starts_with("  └ ⠋")
            || rendered.last().unwrap().chars().last().unwrap() as u32 >= 0x2800
    );
    assert!(frames.try_recv().is_ok());
    cell.append_output("call", "hello");
    assert_eq!(text(&cell, 80).last().unwrap(), "  └ hello");
    assert!(!text(&cell, 80).join("\n").contains('◌'));
}

#[test]
fn native_command_rolling_tail_is_exactly_eight_rows() {
    let mut cell = command("echo numbers");
    for line in 0..20 {
        cell.append_output("call", &format!("line-{line}\n"));
    }
    let rendered = text(&cell, 80);
    let rows = &rendered[rendered.len() - 8..];
    assert_eq!(rows[0], "  └ line-12");
    assert_eq!(rows[7], "    line-19");
    assert!(!rendered.join("\n").contains("line-11"));
    assert!(
        cell.transcript_lines(80)
            .iter()
            .any(|line| line.to_string() == "line-0")
    );
    let mut shorter = command("echo short");
    shorter.append_output("call", "one\ntwo\npartial");
    let rendered = text(&shorter, 80);
    assert_eq!(
        &rendered[rendered.len() - 3..],
        &["  └ one", "    two", "    partial"]
    );
    shorter.append_output("call", "-rest\n");
    assert_eq!(text(&shorter, 80).last().unwrap(), "    partial-rest");
}

#[test]
fn native_command_carriage_return_and_split_crlf() {
    let mut cell = command("progress");
    cell.append_output("call", "old\r");
    cell.append_output("call", "new\x1b[K\r\n10%\r");
    cell.append_output("call", "20%\x1b[K");
    let rendered = text(&cell, 80);
    assert_eq!(&rendered[rendered.len() - 2..], &["  └ new", "    20%"]);
    assert!(
        cell.iter_calls()
            .next()
            .unwrap()
            .output
            .as_ref()
            .unwrap()
            .transcript_lines()
            .any(|line| line.contains("20%"))
    );
}

#[test]
fn native_command_unicode_ansi_truncation_and_resize_never_wrap_output() {
    let mut cell = command("echo");
    cell.append_output("call", "\x1b[31m界界界界e\u{301}🌍hello\x1b[0m\n");
    for width in [1, 3, 4, 5, 8, 12, 40] {
        let lines = cell.display_lines(width);
        let output = lines.last().unwrap();
        assert!(crate::line_truncation::line_width(output) <= usize::from(width));
        let raw = cell.display_lines_for_mode(width, HistoryRenderMode::Raw);
        assert!(crate::line_truncation::line_width(raw.last().unwrap()) <= usize::from(width));
        assert_eq!(
            text(&cell, width)
                .iter()
                .filter(|line| line.contains('界'))
                .count(),
            usize::from(width >= 8)
        );
        if (5..20).contains(&width) {
            assert!(output.to_string().ends_with('…'));
        }
    }
    let wide = cell.display_lines(40).pop().unwrap();
    assert!(
        wide.spans
            .iter()
            .any(|span| span.style.fg == Some(ratatui::style::Color::Red))
    );
}

#[test]
fn native_command_completed_keeps_every_command_line_and_full_transcript_output() {
    let script =
        "mkdir -p 'a b'\npython3 - <<'CUSTOM'\n    print(\"hello\")\n\nCUSTOM\necho done\n";
    let mut cell = command(script);
    cell.append_output("call", "live output\n");
    assert!(text(&cell, 120).join("\n").contains("live output"));
    cell.complete_call(
        "call",
        CommandOutput::new(0, "all available output\nmore output\n".into()),
        Duration::from_secs(2),
    );
    let rendered = text(&cell, 120).join("\n");
    for line in script.split('\n').filter(|line| !line.is_empty()) {
        assert!(rendered.contains(line), "{rendered}");
    }
    assert!(rendered.starts_with("• Ran mkdir -p 'a b'"));
    assert!(!rendered.contains("available output") && !rendered.contains('◌'));
    assert!(!rendered.contains("2.00s"));
    assert!(
        cell.transcript_lines(120)
            .iter()
            .any(|line| line.to_string() == "all available output")
    );
    assert_eq!(
        cell.original_command(cell.iter_calls().next().unwrap()),
        script
    );
}

#[test]
fn native_command_completed_duration_starts_at_ten_seconds() {
    for (millis, show_duration) in [(0, false), (9_999, false), (10_000, true), (10_001, true)] {
        let mut cell = command("echo hello");
        cell.complete_call(
            "call",
            CommandOutput::new(0, "hello".into()),
            Duration::from_millis(millis),
        );
        let rendered = text(&cell, 80);
        assert_eq!(rendered.len(), 1);
        assert_eq!(
            rendered[0],
            if show_duration {
                "• Ran echo hello · 10.00s"
            } else {
                "• Ran echo hello"
            }
        );
    }
}

#[test]
fn native_command_completed_silent_nonzero_exit_keeps_exit_label() {
    for output in [
        "",
        " \t\n",
        "\x1b[31m\x1b[0m\n",
        "\x1b[31m \x1b[0m\n",
        "old\r\x1b[K",
    ] {
        for millis in [0, 200, 10_000] {
            let mut cell = command("rg missing README.md");
            cell.complete_call(
                "call",
                CommandOutput::new(1, output.into()),
                Duration::from_millis(millis),
            );
            let rendered = text(&cell, 80);
            assert_eq!(rendered.len(), 1);
            assert!(rendered[0].starts_with("• Ran rg missing README.md (exit 1)"));
            assert_eq!(rendered[0].contains("10.00s"), millis >= 10_000);
            assert_eq!(
                cell.iter_calls()
                    .next()
                    .unwrap()
                    .output
                    .as_ref()
                    .unwrap()
                    .exit_code,
                1
            );
            assert!(
                cell.transcript_lines(80)
                    .iter()
                    .any(|line| line.to_string().starts_with("✗ (1)"))
            );
        }
    }
}

#[test]
fn native_command_committed_active_cell_never_inserts_output_and_failures_stay_visible() {
    let mut cell = command("false\necho next");
    cell.append_output("call", "sensitive tool output");
    cell.configure_native(true, None, true);
    assert!(!text(&cell, 80).join("\n").contains("tool output"));
    assert!(!text(&cell, 80).join("\n").contains('◌'));
    assert!(
        cell.transcript_lines(80)
            .iter()
            .any(|line| line.to_string().contains("sensitive tool output"))
    );
    cell.complete_call(
        "call",
        CommandOutput::new(7, "failed output".into()),
        Duration::from_millis(200),
    );
    let lines = cell.display_lines(80);
    assert!(
        lines[0]
            .spans
            .iter()
            .any(|span| span.style.fg == Some(ratatui::style::Color::Red))
    );
    assert!(text(&cell, 80).join("\n").contains("exit 7"));
    assert!(!text(&cell, 80).join("\n").contains("200ms"));
    assert!(!text(&cell, 80).join("\n").contains("failed output"));
}

#[test]
fn native_command_owned_mode_hides_completed_output_and_retains_full_transcript() {
    let mut cell = command("python3 - <<'END'\nprint('x')\nEND");
    cell.complete_call(
        "call",
        CommandOutput::new(0, "owned output\n".into()),
        Duration::from_secs(1),
    );
    let full = cell.transcript_lines(80);
    cell.configure_native(false, None, false);
    let owned = cell.display_lines(80);
    let compact = cell.compact_hyperlink_lines(80);
    assert!(full.iter().any(|line| line.to_string().contains("owned output")));
    assert!(!owned.iter().any(|line| line.to_string().contains("owned output")));
    assert!(!compact.iter().any(|line| line.line.to_string().contains("owned output")));
    assert_eq!(
        cell.activity_disclosure(80),
        Some(crate::history_cell::ActivityDisclosure::OutputLines(1)),
    );
    cell.configure_native(true, None, true);
    assert_eq!(cell.transcript_lines(80), full);
    cell.configure_native(false, None, true);
    assert_eq!(cell.display_lines(80), owned);
    assert_eq!(cell.compact_hyperlink_lines(80), compact);
}

#[test]
fn native_command_python_highlights_exact_source_multiple_delimiters_and_shell_context() {
    for opening in [
        "python - <<'PY'",
        "python3 - <<PY",
        "python3 <<'CUSTOM_2'",
        "python3 - <<\"END-PY\"",
    ] {
        let delimiter = python_delimiter(opening).unwrap();
        let body = "from pathlib import Path\n\n    result = 42  # comment\nprint(\"hello\")  ";
        let script = format!(
            "mkdir -p results\n{opening}\n{body}\n{delimiter}\necho done\npython3 - <<SECOND\nprint(1)\nSECOND\n"
        );
        let lines = command_highlights(&script);
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            script.split('\n').collect::<Vec<_>>()
        );
        assert_eq!(lines[2..6], exact_highlight(body, "python"));
        assert_eq!(lines[0], exact_highlight("mkdir -p results", "bash")[0]);
        assert_eq!(lines[6].to_string(), delimiter);
        assert_eq!(lines[9], exact_highlight("print(1)", "python")[0]);
    }
}

#[test]
fn native_command_ambiguous_or_malformed_heredocs_fall_back_verbatim() {
    for script in [
        "cat <<END\npython3 - <<PY\nprint(1)\nPY\nEND",
        "python3 - <<PY\nprint(1)",
        "python3 - <<$END\nprint(1)\nEND",
        "python3 - <<PY | cat\nprint(1)\nPY",
        "python3 -c 'x' <<PY\nprint(1)\nPY",
        "python3 - <<-PY\n\tprint(1)\nPY",
    ] {
        assert_eq!(
            command_highlights(script),
            exact_highlight(script, "bash"),
            "{script}"
        );
        assert_eq!(
            command_highlights(script)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            script.split('\n').collect::<Vec<_>>()
        );
    }
}

#[test]
fn shell_grouping_commands_order_multiline_metadata_and_full_records() {
    let script = "python3 - <<'PY'\n    print('one')\n\nPY";
    let mut cell = command(script);
    assert!(cell.add_call(
        "two".into(),
        vec!["echo second".into()],
        vec![],
        CommandExecutionSource::Agent,
        None
    ));
    cell.remember_original_command("two", "echo second");
    cell.complete_call(
        "call",
        CommandOutput::new(1, "first output".into()),
        Duration::from_secs(12),
    );
    cell.complete_call(
        "two",
        CommandOutput::new(0, "second output".into()),
        Duration::from_millis(1),
    );
    let rows = text(&cell, 120);
    assert_eq!(rows[0], "• Ran");
    assert!(rows.join("\n").contains("    print('one')"));
    assert!(rows.iter().any(|row| row == "  │ PY (exit 1) · 12.00s"));
    assert_eq!(rows.last().unwrap(), "  │ echo second");
    assert!(!rows.join("\n").contains("output"));
    assert!(cell.should_flush()); // no delay waiting for another command
    let full = cell
        .transcript_lines(120)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(full.contains("first output") && full.contains("second output"));
    assert_eq!(cell.activity_ids(), ["exec:call", "exec:two"]);
    cell.configure_native(false, None, false);
    assert!(!cell.add_call(
        "owned".into(),
        vec!["echo".into()],
        vec![],
        CommandExecutionSource::Agent,
        None
    ));
}

#[test]
fn shell_grouping_metadata_narrow_missing_values_and_committed_boundary() {
    let mut cell = command(&"界".repeat(100));
    cell.complete_call(
        "call",
        CommandOutput::new(7, String::new()),
        Duration::from_secs(15),
    );
    for width in [20, 30, 80] {
        let lines = cell.display_lines(width);
        assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
        assert!(lines.last().unwrap().to_string().contains("exit 7"));
        assert!(lines.last().unwrap().to_string().contains("15.00s"));
    }
    cell.remember_exit_code("call", None);
    cell.group.calls[0].duration = None;
    let rows = text(&cell, 120);
    assert!(!rows.join("\n").contains("exit") && !rows.join("\n").contains("15.00s"));
    cell.configure_native(true, None, true);
    assert!(!cell.add_call(
        "later".into(),
        vec!["echo".into()],
        vec![],
        CommandExecutionSource::Agent,
        None
    ));
}

#[test]
fn reasoning_fix_native_commands_separate_interleaved_summaries() {
    use codex_protocol::parse_command::ParsedCommand;
    let parsed = |name: &str| {
        vec![ParsedCommand::Read {
            name: name.into(),
            cmd: format!("cat {name}"),
            path: name.into(),
        }]
    };
    for with_reasoning in [false, true] {
        for with_next_command in [false, true] {
            let mut cell = command("cat first");
            cell.group.calls[0].parsed = parsed("first");
            if with_reasoning {
                let summary = crate::history_cell::new_reasoning_summary_block(
                    vec!["**Thinking**\n\nAvailable summary.".into()],
                    std::path::Path::new("."),
                );
                assert!(cell.append_reasoning(summary).is_ok());
            }
            if with_next_command {
                assert!(cell.add_call(
                    "two".into(),
                    vec!["cat second".into()],
                    parsed("second"),
                    CommandExecutionSource::Agent,
                    None,
                ));
                cell.remember_original_command("two", "cat second");
                cell.complete_call(
                    "two",
                    CommandOutput::new(0, String::new()),
                    Duration::from_secs(1),
                );
            }
            cell.complete_call(
                "call",
                CommandOutput::new(0, String::new()),
                Duration::from_secs(1),
            );
            for width in [24, 80, 120] {
                let rows = text(&cell, width);
                if with_reasoning {
                    let words = rows.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
                    assert!(words.contains("Available summary."));
                    let paragraph = rows
                        .iter()
                        .position(|row| row.contains("summary."))
                        .unwrap();
                    if with_next_command {
                        assert_eq!(rows[paragraph + 1], "");
                        assert_eq!(rows[paragraph + 2], "  │ Read second");
                    } else {
                        assert_eq!(paragraph, rows.len() - 1);
                    }
                } else if with_next_command {
                    assert_eq!(rows, vec!["• Ran", "  │ Read first", "  │ Read second"]);
                }
                assert!(
                    cell.display_lines(width)
                        .iter()
                        .all(|line| line.width() <= usize::from(width))
                );
            }
        }
    }
}
