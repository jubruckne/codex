//! Literal call evidence only: never open files or execute recorded commands.
use super::*;
use std::collections::BTreeMap;
use std::path::{Component, PathBuf};

#[derive(Clone)]
pub(super) struct Evidence {
    operation: Operation,
    targets: Vec<String>,
    directory: Option<PathBuf>,
}

impl Evidence {
    pub(super) fn native(
        operation: Operation,
        name: &str,
        args: Option<&serde_json::Value>,
    ) -> Self {
        let value = |keys: &[&str]| keys.iter().find_map(|key| args?.get(key)?.as_str());
        let target = match operation {
            Operation::Reads if name == "read_file" => {
                value(&["path", "file_path"]).map(str::to_string)
            }
            Operation::Searches => value(&["query", "pattern"]).map(|query| {
                format!(
                    "{name} {} {}",
                    serde_json::to_string(query).unwrap_or_default(),
                    value(&["path"]).unwrap_or("")
                )
            }),
            _ => None,
        };
        Self {
            operation,
            targets: target.into_iter().collect(),
            directory: Some(PathBuf::new()),
        }
    }

    pub(super) fn shell(operation: Operation, words: &[String], directory: Option<&Path>) -> Self {
        let mut targets = Vec::new();
        if operation == Operation::Searches {
            // Full normalized argv preserves pattern, scope and options when deduplicating.
            if directory.is_some() {
                targets.push(codex_shell_command::parse_command::shlex_join(words));
            }
        } else if operation == Operation::Reads {
            let paths = read_paths(words);
            if let Some(directory) = directory {
                for path in paths {
                    if !path.contains(['$', '`', '*', '?', '[', '\n'])
                        && path != "-"
                        && !path.starts_with('~')
                    {
                        let path = normalize(&directory.join(path))
                            .to_string_lossy()
                            .into_owned();
                        // A repeated operand is still one individual read of that file by this command.
                        if !targets.contains(&path) {
                            targets.push(path);
                        }
                    }
                }
            }
        }
        Self {
            operation,
            targets,
            directory: directory.map(Path::to_path_buf),
        }
    }
}

fn read_paths(words: &[String]) -> Vec<String> {
    let Some(first) = words.first() else {
        return Vec::new();
    };
    let args = &words[1..];
    let mut paths = Vec::new();
    match program(first) {
        "cat" => {
            for arg in args {
                if arg == "--"
                    || (arg.starts_with('-')
                        && arg.len() > 1
                        && arg[1..].chars().all(|c| "AbEenstTuv".contains(c)))
                {
                    continue;
                }
                if arg.starts_with('-') {
                    return Vec::new();
                }
                paths.push(arg.clone());
            }
        }
        "head" | "tail" => {
            let mut iter = args.iter();
            while let Some(arg) = iter.next() {
                if matches!(arg.as_str(), "-n" | "-c" | "--lines" | "--bytes") {
                    iter.next();
                } else if arg == "--"
                    || matches!(arg.as_str(), "-q" | "-v")
                    || arg
                        .strip_prefix('-')
                        .is_some_and(|n| n.parse::<u64>().is_ok())
                    || arg.starts_with("--lines=")
                    || arg.starts_with("--bytes=")
                {
                } else if arg.starts_with('-') {
                    return Vec::new();
                } else {
                    paths.push(arg.clone());
                }
            }
        }
        "sed" => {
            let mut args = args;
            if args.first().is_some_and(|arg| arg == "-n") {
                args = &args[1..];
            } else {
                return Vec::new();
            }
            if args.first().is_some_and(|arg| arg == "-e") {
                args = &args[1..];
            }
            let Some((expression, files)) = args.split_first() else {
                return Vec::new();
            };
            // Only simple print ranges are reads; scripts and redirection are not filenames.
            if !expression.strip_suffix('p').is_some_and(|range| {
                !range.is_empty()
                    && range
                        .chars()
                        .all(|c| c.is_ascii_digit() || c == ',' || c == '$')
            }) {
                return Vec::new();
            }
            if files.iter().any(|file| file.starts_with('-')) {
                return Vec::new();
            }
            paths.extend_from_slice(files);
        }
        _ => {}
    }
    paths
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if result.file_name().is_some_and(|name| name != "..") {
                    result.pop();
                } else if !result.has_root() {
                    result.push("..");
                }
            }
            _ => result.push(component.as_os_str()),
        }
    }
    result
}

impl Unit {
    pub(super) fn in_directory(mut self, directory: Option<&str>) -> Self {
        self.directory = directory.map(str::to_string);
        self
    }
}

#[derive(Debug, Default)]
pub(super) struct Details {
    reads: BTreeMap<String, Row>,
    searches: BTreeMap<String, Row>,
    unattributed: [Row; 2],
}

impl Details {
    pub(super) fn record(
        &mut self,
        call: &Call,
        unit_weights: &[[u64; 7]],
        shares: [u64; 7],
        unknown: bool,
        directory: &Path,
        count: bool,
    ) {
        for (slot, operation) in [Operation::Reads, Operation::Searches]
            .into_iter()
            .enumerate()
        {
            let mut groups = Vec::new();
            for (unit_index, unit) in call.units.iter().enumerate() {
                let cwd = normalize(&directory.join(unit.directory.as_deref().unwrap_or("")));
                let mut evidence = unit.evidence.iter().filter(|e| e.operation == operation);
                for _ in unit.operations.iter().filter(|op| **op == operation) {
                    let targets = evidence
                        .next()
                        .map(|e| {
                            e.targets
                                .iter()
                                .map(|target| {
                                    if operation == Operation::Reads {
                                        normalize(&cwd.join(target)).to_string_lossy().into_owned()
                                    } else {
                                        format!(
                                            "{target} · in {}",
                                            normalize(&cwd.join(
                                                e.directory.as_deref().unwrap_or(Path::new(""))
                                            ))
                                            .display()
                                        )
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default();
                    groups.push((targets, unit_index));
                }
            }
            if groups.is_empty() {
                self.unattributed[slot].tokens = self.unattributed[slot]
                    .tokens
                    .saturating_add(shares[operation as usize]);
                continue;
            }
            // Match the parent operation budget and its output-block weights before dividing
            // each unresolved shell batch by individual command counts.
            let weights = unit_weights
                .iter()
                .map(|weights| weights[operation as usize])
                .collect::<Vec<_>>();
            let budgets = allocate(shares[operation as usize], &weights);
            let mut offsets = vec![0u64; call.units.len()];
            let counts = call
                .units
                .iter()
                .map(|unit| {
                    unit.operations
                        .iter()
                        .filter(|op| **op == operation)
                        .count() as u64
                })
                .collect::<Vec<_>>();
            for (index, tokens) in budgets.iter().enumerate() {
                if index >= counts.len() || counts[index] == 0 {
                    self.unattributed[slot].tokens =
                        self.unattributed[slot].tokens.saturating_add(*tokens);
                }
            }
            for (targets, index) in groups {
                let total = budgets[index];
                let n = counts[index];
                let tokens = total / n + u64::from(offsets[index] < total % n);
                offsets[index] += 1;
                if targets.is_empty() {
                    let row = &mut self.unattributed[slot];
                    row.items += u64::from(count);
                    row.tokens = row.tokens.saturating_add(tokens);
                    row.unknown |= unknown;
                    continue;
                }
                let n = targets.len() as u64;
                let map = if operation == Operation::Reads {
                    &mut self.reads
                } else {
                    &mut self.searches
                };
                for (index, target) in targets.into_iter().enumerate() {
                    let row = map.entry(target).or_default();
                    row.items += u64::from(count);
                    row.tokens = row
                        .tokens
                        .saturating_add(tokens / n + u64::from((index as u64) < tokens % n));
                    row.unknown |= unknown;
                }
            }
        }
    }

    pub(super) fn rows(&self, reads: bool) -> Vec<(String, String)> {
        let map = if reads { &self.reads } else { &self.searches };
        let mut entries = map.iter().collect::<Vec<_>>();
        entries.sort_by_key(|(key, row)| (std::cmp::Reverse(row.tokens), *key));
        let label = if reads { "reads" } else { "searches" };
        let mut rows = entries
            .into_iter()
            .map(|(key, row)| {
                (
                    key.clone(),
                    format!(
                        "{} {label} · {} tokens",
                        count_label(row.items),
                        token_label(row.tokens, row.unknown)
                    ),
                )
            })
            .collect::<Vec<_>>();
        let unknown = &self.unattributed[usize::from(!reads)];
        if unknown.items > 0 || unknown.tokens > 0 {
            rows.push((
                if reads {
                    "Listings / unidentified file reads"
                } else {
                    "Unidentified searches"
                }
                .to_string(),
                format!(
                    "{} operations · {} tokens",
                    count_label(unknown.items),
                    token_label(unknown.tokens, unknown.unknown)
                ),
            ));
        }
        rows
    }
}

/// Largest remainder allocation for a variable number of nested tool calls.
fn allocate(total: u64, weights: &[u64]) -> Vec<u64> {
    let sum = weights.iter().map(|w| *w as u128).sum::<u128>();
    if sum == 0 {
        return vec![0; weights.len()];
    }
    let mut shares = weights
        .iter()
        .map(|w| (u128::from(total) * u128::from(*w) / sum) as u64)
        .collect::<Vec<_>>();
    let mut order = weights
        .iter()
        .enumerate()
        .filter(|(_, w)| **w > 0)
        .map(|(i, w)| (i, u128::from(total) * u128::from(*w) % sum))
        .collect::<Vec<_>>();
    order.sort_by_key(|(i, r)| (std::cmp::Reverse(*r), *i));
    let remainder = total - shares.iter().sum::<u64>();
    for (i, _) in order.into_iter().take(remainder as usize) {
        shares[i] += 1;
    }
    shares
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn analyze(commands: &[(&str, &str, &str)], outputs: &[(&str, &str)]) -> ToolResultsSummary {
        let mut history = Vec::new();
        for (id, cmd, workdir) in commands {
            history.push(ResponseItemEnvelope::from(serde_json::from_value::<ResponseItem>(json!({"type":"function_call","call_id":id,"name":"exec_command","arguments":json!({"cmd":cmd,"workdir":workdir}).to_string()})).unwrap()));
        }
        for (id, output) in outputs {
            history.push(ResponseItemEnvelope::from(
                serde_json::from_value::<ResponseItem>(
                    json!({"type":"function_call_output","call_id":id,"output":output}),
                )
                .unwrap(),
            ));
        }
        let mut analysis = Analysis::new(&history).in_directory(Path::new("/repo"));
        for entry in &history {
            if matches!(entry.item, ResponseItem::FunctionCallOutput { .. }) {
                analysis.record(&entry.item, item_cost(&entry.item));
            }
        }
        analysis.finish()
    }

    #[test]
    fn context_summary_details_deduplicate_file_ranges_and_workdirs() {
        let summary = analyze(
            &[
                (
                    "a",
                    "cat ./src/a.rs; sed -n '1,30p' src/a.rs; head -n 10 src/a.rs",
                    "/repo",
                ),
                ("b", "cat a.rs", "/repo/src"),
                ("c", "cd src && tail -n 10 a.rs", "/repo"),
            ],
            &[("a", "abcdefgh"), ("b", "abcdefgh"), ("c", "abcdefgh")],
        );
        let row = &summary.details.reads["/repo/src/a.rs"];
        assert_eq!(row.items, 5);
        assert_eq!(summary.details.reads.len(), 1);
        assert_eq!(
            row.tokens,
            summary.categories[Operation::Reads as usize].tokens
        );
    }

    #[test]
    fn context_summary_details_search_quoting_deduplicates_but_options_and_cwd_do_not() {
        let summary = analyze(
            &[
                (
                    "a",
                    "rg -n 'two words' src; rg  -n \"two words\" src; rg -l 'two words' src",
                    "/repo",
                ),
                ("b", "rg -n 'two words' src", "/elsewhere"),
            ],
            &[("a", "abcdefgh"), ("b", "abcdefgh")],
        );
        assert_eq!(summary.details.searches.len(), 3);
        assert_eq!(
            summary
                .details
                .searches
                .values()
                .map(|row| row.items)
                .sum::<u64>(),
            4
        );
        assert!(summary.details.searches.values().any(|row| row.items == 2));
    }

    #[test]
    fn context_summary_details_multifile_listings_and_pipes_conserve_budgets() {
        let summary = analyze(
            &[("a", "cat a b; ls src; cat a | head -10; rg TODO .", "/repo")],
            &[("a", "a".repeat(401).as_str())],
        );
        assert_eq!(summary.details.reads["/repo/a"].items, 2);
        assert_eq!(summary.details.reads["/repo/b"].items, 1);
        for (slot, (operation, map)) in [
            (Operation::Reads, &summary.details.reads),
            (Operation::Searches, &summary.details.searches),
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                map.values().map(|row| row.tokens).sum::<u64>()
                    + summary.details.unattributed[slot].tokens,
                summary.categories[operation as usize].tokens
            );
        }
        assert!(summary.details.unattributed[0].tokens > 0);
    }

    #[test]
    fn context_summary_details_repeated_output_accumulates_tokens_without_counting_call_twice() {
        let summary = analyze(
            &[("a", "cat a", "/repo")],
            &[("a", "12345678"), ("a", "12345678")],
        );
        let row = &summary.details.reads["/repo/a"];
        assert_eq!(row.items, 1);
        assert_eq!(
            row.tokens,
            summary.categories[Operation::Reads as usize].tokens
        );
    }

    #[test]
    fn context_summary_details_unresolved_directories_and_dynamic_paths_stay_unidentified() {
        let summary = analyze(
            &[("a", "cd \"$dir\"; cat a; cat $file; ls", "/repo")],
            &[("a", "abcdefgh")],
        );
        assert!(summary.details.reads.is_empty());
        assert!(summary.details.unattributed[0].tokens > 0);
    }

    #[test]
    fn context_summary_details_unlimited_sorted_entries_and_lexical_parent_paths() {
        let commands = (0..25)
            .map(|i| {
                (
                    i.to_string(),
                    format!("cat ./src/../file{i:02}"),
                    "/repo".to_string(),
                )
            })
            .collect::<Vec<_>>();
        let outputs = (0..25)
            .map(|i| (i.to_string(), "x".repeat((i + 1) * 4)))
            .collect::<Vec<_>>();
        let summary = analyze(
            &commands
                .iter()
                .map(|(id, cmd, dir)| (id.as_str(), cmd.as_str(), dir.as_str()))
                .collect::<Vec<_>>(),
            &outputs
                .iter()
                .map(|(id, text)| (id.as_str(), text.as_str()))
                .collect::<Vec<_>>(),
        );
        let rows = summary.detail_rows(true);
        assert_eq!(rows.len(), 25);
        assert_eq!(rows[0].0, "/repo/file24");
        assert_eq!(rows[24].0, "/repo/file00");
    }

    #[test]
    fn context_summary_details_code_batches_follow_output_block_sizes_and_literal_workdirs() {
        let call: ResponseItem = serde_json::from_value(json!({"type":"custom_tool_call","name":"exec","namespace":"functions","call_id":"a","input":"text(await tools.exec_command({cmd:'cat a', workdir:'/repo/src'})); text(await tools.exec_command({cmd:'cat b', workdir:'/repo'}));"})).unwrap();
        let output: ResponseItem = serde_json::from_value(json!({"type":"custom_tool_call_output","call_id":"a","output":[{"type":"input_text","text":json!({"chunk_id":"a","output":"x".repeat(4000)}).to_string()},{"type":"input_text","text":json!({"chunk_id":"b","output":"y"}).to_string()}]})).unwrap();
        let history = vec![
            ResponseItemEnvelope::from(call),
            ResponseItemEnvelope::from(output.clone()),
        ];
        let mut analysis = Analysis::new(&history).in_directory(Path::new("/repo"));
        analysis.record(&output, item_cost(&output));
        let summary = analysis.finish();
        assert!(
            summary.details.reads["/repo/src/a"].tokens
                > summary.details.reads["/repo/b"].tokens * 20
        );
        assert_eq!(
            summary
                .details
                .reads
                .values()
                .map(|row| row.tokens)
                .sum::<u64>(),
            summary.categories[Operation::Reads as usize].tokens
        );
    }
}
