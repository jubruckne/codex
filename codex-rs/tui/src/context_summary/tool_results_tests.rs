use super::*;
use serde_json::json;

fn item(value: serde_json::Value) -> ResponseItem {
    serde_json::from_value(value).unwrap()
}

fn code(id: &str, source: &str) -> ResponseItem {
    item(
        json!({"type":"custom_tool_call", "name":"exec", "namespace":"functions", "call_id":id, "input":source}),
    )
}

fn result(id: &str, text: &str) -> ResponseItem {
    item(json!({"type":"custom_tool_call_output", "call_id":id, "output":text}))
}

fn analyze(items: Vec<ResponseItem>) -> ToolResultsSummary {
    let history: Vec<_> = items.into_iter().map(ResponseItemEnvelope::from).collect();
    let mut analysis = Analysis::new(&history);
    for entry in &history {
        if matches!(
            entry.item,
            ResponseItem::FunctionCallOutput { .. }
                | ResponseItem::CustomToolCallOutput { .. }
                | ResponseItem::ToolSearchOutput { .. }
        ) {
            analysis.record(&entry.item, item_cost(&entry.item));
        }
    }
    analysis.finish()
}

#[test]
fn context_summary_operations_classify_shell_commands_and_redirections() {
    for (script, expected) in [
        ("cat src/a.rs", Operation::Reads),
        ("sed -n '1,40p' src/a.rs", Operation::Reads),
        ("rg --files src", Operation::Reads),
        ("rg -n TODO src", Operation::Searches),
        ("git -C .build/upstream diff --stat", Operation::Git),
        ("cargo +nightly test --locked", Operation::Tests),
        (
            "RUST_MIN_STACK=8388608 cargo test --lib > .build/tests.log 2>&1",
            Operation::Tests,
        ),
        ("cargo build --locked", Operation::Builds),
        ("sed -i '' 's/a/b/' src/a.rs", Operation::Edits),
        ("git add src/a.rs", Operation::Edits),
        ("npm run build", Operation::Builds),
        ("npm test", Operation::Tests),
        ("curl https://example.invalid", Operation::Other),
    ] {
        assert_eq!(shell_unit(script).operations, vec![expected], "{script}");
    }
}

#[test]
fn context_summary_operations_shell_pipelines_and_batches_keep_all_hints() {
    assert_eq!(
        shell_unit("cd repo && cat a.rs; rg TODO src; cargo test").operations,
        vec![Operation::Reads, Operation::Searches, Operation::Tests]
    );
    assert_eq!(
        shell_unit("cat a.rs | head -10").operations,
        vec![Operation::Reads, Operation::Reads]
    );
}

#[test]
fn context_summary_operations_do_not_classify_quoted_or_heredoc_program_text() {
    assert_eq!(
        shell_unit("echo 'cargo test'; cat a.rs").operations,
        vec![Operation::Other, Operation::Reads]
    );
    assert_eq!(
        shell_unit("python3 - <<'PY'\nprint('cargo test')\nPY\ncat a.rs").operations,
        vec![Operation::Other, Operation::Reads]
    );
    assert_eq!(
        shell_unit("not a valid 'shell").operations,
        vec![Operation::Other]
    );
}

#[test]
fn context_summary_operations_scan_static_code_mode_and_ignore_comments_strings_regexes() {
    let source = r#"
        // tools.exec_command({cmd:"cargo test"})
        const fake = "tools.exec_command({cmd:'cargo build'})";
        const regex = /tools.exec_command({cmd:'cargo test'})/;
        const results = await Promise.allSettled([
            tools.exec_command({cmd:"cat a.rs"}),
            tools.exec_command({'cmd':'rg -n TODO src'}),
        ]);
        results.forEach(text);
        /* tools.apply_patch("ignored") */
    "#;
    let units = js_units(source);
    assert_eq!(units.len(), 2);
    assert_eq!(units[0].operations, vec![Operation::Reads]);
    assert_eq!(units[1].operations, vec![Operation::Searches]);
}

#[test]
fn context_summary_operations_handle_static_escaped_and_unicode_strings() {
    let source = r#"await tools.exec_command({cmd:'cat \'a b.rs\''});
        await tools.exec_command({cmd:`rg -n 'é' src`});
        await tools.exec_command({cmd:'c\x61t a.rs'});"#;
    let units = js_units(source);
    assert_eq!(units.len(), 3);
    assert_eq!(units[0].operations, vec![Operation::Reads]);
    assert_eq!(units[1].operations, vec![Operation::Searches]);
    assert_eq!(units[2].operations, vec![Operation::Reads]);
}

#[test]
fn context_summary_operations_computed_spread_and_duplicate_commands_are_unknown() {
    for source in [
        "await tools.exec_command({cmd: command});",
        "await tools.exec_command({cmd: 'cat a' + suffix});",
        "await tools.exec_command({cmd: `cat ${path}`});",
        "await tools.exec_command({...options, cmd:'cat a'});",
        "await tools.exec_command({cmd:'cat a', ...options});",
        "await tools.exec_command({cmd:'cat a', cmd:'cargo test'});",
    ] {
        assert_eq!(
            js_units(source)[0].operations,
            vec![Operation::Other],
            "{source}"
        );
    }
    assert!(js_units("await tools.exec_command({cmd:'unterminated").is_empty());
}

#[test]
fn context_summary_operations_code_mode_scanner_is_bounded() {
    let source = format!(
        "{} await tools.exec_command({{cmd:'cat a'}});",
        ";".repeat(40_000)
    );
    assert!(js_units(&source).is_empty());
}

#[test]
fn context_summary_operations_regex_after_return_or_arrow_is_not_a_tool_call() {
    let units = js_units(
        r#"
        function pattern() { return /tools.exec_command({cmd:'cargo test'})/; }
        const other = () => /tools.exec_command({cmd:'cargo build'})/;
        const fraction = 20 / 4;
        await tools.exec_command({cmd:'cat a.rs'});
    "#,
    );
    assert_eq!(units.len(), 1);
    assert_eq!(units[0].operations, vec![Operation::Reads]);
}

#[test]
fn context_summary_operations_match_native_calls_and_preserve_existing_costs() {
    let call = item(
        json!({"type":"function_call", "name":"exec_command", "call_id":"a", "arguments":"{\"cmd\":\"cat a.rs\"}"}),
    );
    let output = item(json!({"type":"function_call_output", "call_id":"a", "output":"content"}));
    let cost = item_cost(&output).tokens();
    let summary = analyze(vec![call, output]);
    assert_eq!(summary.categories[Operation::Reads as usize].tokens, cost);
    assert_eq!(
        summary.categories.iter().map(|row| row.tokens).sum::<u64>(),
        cost
    );
    assert_eq!(summary.largest.len(), 1);
    assert!(summary.largest[0].description.contains("cat a.rs"));
}

#[test]
fn context_summary_operations_split_combined_shell_output_by_command_count() {
    let output = result("a", &"x".repeat(400));
    let total = item_cost(&output).tokens();
    let summary = analyze(vec![
        code(
            "a",
            "text(await tools.exec_command({cmd:'cat a; cat b; rg TODO src'}));",
        ),
        output,
    ]);
    let reads = summary.categories[Operation::Reads as usize].tokens;
    let searches = summary.categories[Operation::Searches as usize].tokens;
    assert_eq!(reads + searches, total);
    assert!(reads >= searches * 2 - 1 && reads <= searches * 2 + 1);
    assert_eq!(summary.categories[Operation::Reads as usize].items, 1);
    assert_eq!(summary.categories[Operation::Searches as usize].items, 1);
}

#[test]
fn context_summary_operations_split_combined_code_mode_batches_without_double_counting() {
    let output = result("a", &"x".repeat(404));
    let total = item_cost(&output).tokens();
    let summary = analyze(vec![
        code(
            "a",
            "await tools.exec_command({cmd:'cat a'}); await tools.exec_command({cmd:'cargo test'});",
        ),
        output,
    ]);
    let reads = summary.categories[Operation::Reads as usize].tokens;
    let tests = summary.categories[Operation::Tests as usize].tokens;
    assert_eq!(reads + tests, total);
    assert!(reads.abs_diff(tests) <= 1);
    assert!(
        summary
            .lines(false)
            .iter()
            .any(|line| line.to_string().contains("counts may overlap"))
    );
}

#[test]
fn context_summary_operations_separate_matched_blocks_use_output_size_weights() {
    let body = vec![
        json!({"type":"input_text", "text":"Script completed\nOutput:"}),
        json!({"type":"input_text", "text":json!({"chunk_id":"a", "output":"x".repeat(4000), "exit_code":0}).to_string()}),
        json!({"type":"input_text", "text":"{}"}),
    ];
    let output = item(json!({"type":"custom_tool_call_output", "call_id":"a", "output":body}));
    let total = item_cost(&output).tokens();
    let summary = analyze(vec![
        code(
            "a",
            "text(await tools.exec_command({cmd:'cat a'})); text(await tools.apply_patch('patch body'));",
        ),
        output,
    ]);
    let reads = summary.categories[Operation::Reads as usize].tokens;
    let edits = summary.categories[Operation::Edits as usize].tokens;
    assert_eq!(reads + edits, total);
    assert!(reads > total * 9 / 10);
    assert!(edits < total / 10);
}

#[test]
fn context_summary_operations_all_settled_output_blocks_keep_size_weights() {
    let output = item(
        json!({"type":"custom_tool_call_output", "call_id":"a", "output":[
        {"type":"input_text", "text":json!({"status":"fulfilled", "value":{"chunk_id":"a", "output":"x".repeat(4000)}}).to_string()},
        {"type":"input_text", "text":json!({"status":"fulfilled", "value":{"chunk_id":"b", "output":"ok"}}).to_string()}]}),
    );
    let summary = analyze(vec![
        code(
            "a",
            "await Promise.allSettled([tools.exec_command({cmd:'cat a'}), tools.exec_command({cmd:'cargo test'})]);",
        ),
        output,
    ]);
    assert!(
        summary.categories[Operation::Reads as usize].tokens
            > summary.categories[Operation::Tests as usize].tokens * 5
    );
}

#[test]
fn context_summary_operations_unmatched_extra_output_is_other() {
    let body = vec![
        json!({"type":"input_text", "text":json!({"chunk_id":"a", "output":"read"}).to_string()}),
        json!({"type":"input_text", "text":"{}"}),
        json!({"type":"input_text", "text":"extra generated output".repeat(50)}),
    ];
    let output = item(json!({"type":"custom_tool_call_output", "call_id":"a", "output":body}));
    let total = item_cost(&output).tokens();
    let summary = analyze(vec![
        code(
            "a",
            "await tools.exec_command({cmd:'cat a'}); await tools.apply_patch('patch');",
        ),
        output,
    ]);
    assert_eq!(
        summary.categories.iter().map(|row| row.tokens).sum::<u64>(),
        total
    );
    assert!(summary.categories[Operation::Other as usize].tokens > total / 2);
}

#[test]
fn context_summary_operations_unmatched_blocks_are_shared_by_remaining_commands() {
    let output = item(
        json!({"type":"custom_tool_call_output", "call_id":"a", "output":[
        {"type":"input_text", "text":"{}"},
        {"type":"input_text", "text":"unstructured combined read and search output".repeat(20)}]}),
    );
    let total = item_cost(&output).tokens();
    let summary = analyze(vec![
        code(
            "a",
            "await tools.apply_patch('patch'); await tools.exec_command({cmd:'cat a'}); await tools.exec_command({cmd:'rg TODO src'});",
        ),
        output,
    ]);
    let reads = summary.categories[Operation::Reads as usize].tokens;
    let searches = summary.categories[Operation::Searches as usize].tokens;
    assert!(reads > 0 && reads.abs_diff(searches) <= 1);
    assert_eq!(
        summary.categories.iter().map(|row| row.tokens).sum::<u64>(),
        total
    );
}

#[test]
fn context_summary_operations_unknown_results_and_duplicate_call_ids_are_other() {
    let output = result("missing", "unmatched output");
    let expected = item_cost(&output).tokens();
    assert_eq!(
        analyze(vec![output]).categories[Operation::Other as usize].tokens,
        expected
    );
    let summary = analyze(vec![
        code("a", "tools.exec_command({cmd:'cat a'})"),
        code("a", "tools.exec_command({cmd:'cargo test'})"),
        result("a", "output"),
    ]);
    assert_eq!(summary.categories[Operation::Other as usize].items, 1);
    assert_eq!(summary.categories[Operation::Reads as usize].items, 0);
}

#[test]
fn context_summary_operations_named_outputs_and_tool_search_survive_missing_calls() {
    let summary = analyze(vec![
        item(json!({"type":"function_call_output", "name":"apply_patch", "output":"ok"})),
        item(
            json!({"type":"tool_search_output", "call_id":null, "status":"completed", "execution":"client", "tools":[]}),
        ),
    ]);
    assert_eq!(summary.categories[Operation::Edits as usize].items, 1);
    assert_eq!(summary.categories[Operation::Searches as usize].items, 1);
}

#[test]
fn context_summary_operations_unknown_media_keeps_unknown_cost_markers() {
    let output = item(
        json!({"type":"custom_tool_call_output", "call_id":"a", "output":[{"type":"input_audio", "audio_url":"opaque audio"}]}),
    );
    let summary = analyze(vec![output]);
    assert!(summary.categories[Operation::Other as usize].unknown);
    assert!(summary.largest[0].unknown);
    assert!(
        summary
            .lines(false)
            .iter()
            .any(|line| line.to_string().contains(">="))
    );
}

#[test]
fn context_summary_operations_largest_five_are_sorted_and_ties_are_stable() {
    let mut items = Vec::new();
    for index in 0..8 {
        let id = index.to_string();
        items.push(code(&id, "tools.exec_command({cmd:'cat a'})"));
        items.push(result(&id, &"x".repeat(index * 40)));
    }
    let summary = analyze(items);
    assert_eq!(summary.largest.len(), 5);
    assert_eq!(
        summary
            .largest
            .iter()
            .map(|result| result.ordinal)
            .collect::<Vec<_>>(),
        vec![8, 7, 6, 5, 4]
    );
    assert!(
        summary
            .largest
            .windows(2)
            .all(|pair| pair[0].tokens >= pair[1].tokens)
    );
    let summary = analyze(vec![
        result("a", "same"),
        result("b", "same"),
        result("c", "same"),
    ]);
    assert_eq!(
        summary
            .largest
            .iter()
            .map(|result| result.ordinal)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
}

#[test]
fn context_summary_operations_category_rows_are_sorted_by_estimated_tokens() {
    let summary = analyze(vec![
        code("a", "tools.exec_command({cmd:'cargo test'})"),
        result("a", &"x".repeat(400)),
        code("b", "tools.exec_command({cmd:'cat a'})"),
        result("b", "small"),
    ]);
    let lines = summary.lines(false);
    let tests = lines
        .iter()
        .position(|line| line.to_string().starts_with("Tests "))
        .unwrap();
    let reads = lines
        .iter()
        .position(|line| line.to_string().starts_with("File reads / listings "))
        .unwrap();
    assert!(tests < reads);
}

#[test]
fn context_summary_operations_previews_never_print_arbitrary_arguments_or_output() {
    let source = "tools.exec_command({cmd:\"curl -H 'Authorization: SECRET_TOKEN' https://example.invalid\"})";
    let summary = analyze(vec![
        code("a", source),
        result("a", "PRIVATE_OUTPUT_CONTENT"),
    ]);
    let text = summary
        .lines(false)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("curl"));
    assert!(!text.contains("SECRET_TOKEN"));
    assert!(!text.contains("PRIVATE_OUTPUT_CONTENT"));
    assert!(!text.contains("example.invalid"));
}

#[test]
fn context_summary_operations_allocation_conserves_tokens_and_handles_overflow() {
    for total in [0, 1, 2, 3, 10, 101, u64::MAX] {
        for weights in [[0; 7], [1, 1, 1, 0, 0, 0, 0], [u64::MAX; 7]] {
            let shares = split(total, weights);
            assert_eq!(
                shares.iter().map(|value| u128::from(*value)).sum::<u128>(),
                u128::from(total)
            );
        }
    }
}

#[test]
fn context_summary_operations_top_list_uses_only_surviving_replayed_history() {
    let old = code("old", "tools.exec_command({cmd:'cargo test'})");
    let retained = code("new", "tools.exec_command({cmd:'cat new.rs'})");
    let checkpoint = codex_rollout::decode_rollout_line(
        json!({"timestamp":"2026-10-21T12:00:00Z", "type":"compacted", "payload":{
        "message":"summary", "replacement_history":[retained], "window_number":1}}),
    )
    .unwrap()
    .item;
    let history = replay(vec![
        RolloutItem::ResponseItem(old.into()),
        RolloutItem::ResponseItem(result("old", &"x".repeat(2000)).into()),
        checkpoint,
        RolloutItem::ResponseItem(result("new", "small retained result").into()),
    ])
    .unwrap();
    let report = summarize(&history, Some(""));
    assert_eq!(
        report.tool_results.categories[Operation::Tests as usize].items,
        0
    );
    assert_eq!(
        report.tool_results.categories[Operation::Reads as usize].items,
        1
    );
    assert_eq!(
        report
            .tool_results
            .categories
            .iter()
            .map(|row| row.tokens)
            .sum::<u64>(),
        report.rows[Kind::Results as usize].tokens
    );
}

#[test]
fn context_summary_operations_rendering_wraps_long_labels_and_has_no_empty_sections() {
    let history: Vec<_> = [
        code(
            "a",
            "tools.exec_command({cmd:'cat a_very_long_filename_é.rs; rg TODO src'})",
        ),
        result("a", &"x".repeat(404)),
    ]
    .into_iter()
    .map(ResponseItemEnvelope::from)
    .collect();
    let cell = ContextSummaryCell {
        summary: summarize(&history, Some("")),
        usage: None,
    };
    let text = cell
        .raw_lines()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Tool results by operation (approximate)"));
    assert!(text.contains("Five largest tool results"));
    assert!(text.contains("command counts"));
    for width in [1, 20, 51, 80] {
        assert!(
            cell.display_lines(width)
                .iter()
                .all(|line| line.width() <= usize::from(width))
        );
    }
    assert!(ToolResultsSummary::default().lines(false).is_empty());
}

#[test]
fn context_summary_details_dynamic_workdirs_are_not_assumed_to_be_session_cwd() {
    let summary = analyze(vec![
        code(
            "a",
            "text(await tools.exec_command({cmd:'cat a',workdir:directory}));",
        ),
        result("a", "12345678"),
    ]);
    let rows = summary.detail_rows(true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "Listings / unidentified file reads");
}

#[test]
fn context_summary_details_native_reads_use_explicit_workdir_and_merge_ranges() {
    let summary = analyze(vec![
        item(
            json!({"type":"function_call","name":"read_file","call_id":"a","arguments":json!({"path":"a.rs","workdir":"/repo/src","start_line":1}).to_string()}),
        ),
        result("a", "12345678"),
        item(
            json!({"type":"function_call","name":"read_file","call_id":"b","arguments":json!({"path":"./a.rs","workdir":"/repo/src","start_line":40}).to_string()}),
        ),
        result("b", "12345678"),
    ]);
    let rows = summary.detail_rows(true);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "/repo/src/a.rs");
    assert!(rows[0].1.starts_with("2 reads"));
}
