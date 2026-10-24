//! Approximate operation attribution for complete, model-visible outer tool results.
//!
//! Match calls by ID. Static Code Mode calls and shell commands supply operation hints, never
//! execution evidence. Recognizable output blocks supply byte weights; other mixed output is
//! shared by command count. Integer shares always conserve the existing result token estimate.

use std::collections::HashMap;
use std::path::Path;

use codex_protocol::models::LocalShellAction;
use codex_shell_command::bash::parse_shell_script_into_commands;
use codex_shell_command::bash::try_parse_shell;
use ratatui::style::Stylize;

use super::*;

mod details;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Operation {
    Reads,
    Searches,
    Git,
    Builds,
    Tests,
    Edits,
    Other,
}

impl Operation {
    const ALL: [Self; 7] = [
        Self::Reads,
        Self::Searches,
        Self::Git,
        Self::Builds,
        Self::Tests,
        Self::Edits,
        Self::Other,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Reads => "File reads / listings",
            Self::Searches => "Searches",
            Self::Git => "Git inspection",
            Self::Builds => "Builds",
            Self::Tests => "Tests",
            Self::Edits => "Edits",
            Self::Other => "Other / unclassified",
        }
    }
}

#[derive(Debug)]
struct Largest {
    ordinal: usize,
    tokens: u64,
    unknown: bool,
    description: String,
    shares: [u64; 7],
}

#[derive(Debug, Default)]
pub(super) struct ToolResultsSummary {
    categories: [Row; 7],
    largest: Vec<Largest>,
    details: details::Details,
}

impl ToolResultsSummary {
    pub(super) fn detail_rows(&self, reads: bool) -> Vec<(String, String)> {
        self.details.rows(reads)
    }

    pub(super) fn lines(&self, narrow: bool) -> Vec<Line<'static>> {
        if self.largest.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![
            Line::default(),
            "Tool results by operation (approximate)".bold().into(),
        ];
        if !narrow {
            lines.push(
                "Operation                Results   Estimated tokens"
                    .bold()
                    .into(),
            );
        }
        let mut categories: Vec<_> = Operation::ALL
            .into_iter()
            .filter(|op| self.categories[*op as usize].items > 0)
            .collect();
        categories.sort_by_key(|op| (std::cmp::Reverse(self.categories[*op as usize].tokens), *op));
        for op in categories {
            let row = &self.categories[op as usize];
            let count = count_label(row.items);
            let tokens = token_label(row.tokens, row.unknown);
            lines.push(
                if narrow {
                    format!("{}: {} results, {} tokens", op.label(), count, tokens)
                } else {
                    format!("{:<24} {:>7} {:>18}", op.label(), count, tokens)
                }
                .into(),
            );
        }
        lines.push(Line::default());
        lines.push("Five largest tool results".bold().into());
        for (rank, result) in self.largest.iter().enumerate() {
            lines.push(
                format!(
                    "{}. {} tokens · result #{} · {}",
                    rank + 1,
                    token_label(result.tokens, result.unknown),
                    result.ordinal,
                    result.description
                )
                .into(),
            );
            let shares = Operation::ALL
                .into_iter()
                .filter_map(|op| {
                    let tokens = result.shares[op as usize];
                    (tokens > 0)
                        .then(|| format!("{} {}", op.label(), token_label(tokens, result.unknown)))
                })
                .collect::<Vec<_>>()
                .join("; ");
            if !shares.is_empty() {
                lines.push(format!("   {shares}").into());
            }
        }
        lines.push("Operation labels and mixed-batch shares are approximate. Recognizable output blocks use order and size weights; unresolved batches use command counts. Category token shares sum to Tool results; result counts may overlap.".into());
        lines
    }
}

#[derive(Clone)]
struct Unit {
    operations: Vec<Operation>,
    label: String,
    shape: Shape,
    evidence: Vec<details::Evidence>,
    directory: Option<String>,
}

#[derive(Clone, Copy)]
enum Shape {
    Shell,
    Patch,
    Other,
}

impl Shape {
    fn matches(self, value: &serde_json::Value) -> bool {
        let value = if value.get("status").and_then(serde_json::Value::as_str) == Some("fulfilled")
        {
            value.get("value").unwrap_or(value)
        } else {
            value
        };
        match self {
            Self::Shell => {
                value.get("output").is_some()
                    && (value.get("chunk_id").is_some() || value.get("wall_time_seconds").is_some())
            }
            Self::Patch => value.as_object().is_some_and(|object| object.is_empty()),
            Self::Other => false,
        }
    }
}

#[derive(Clone)]
struct Call {
    tool: String,
    units: Vec<Unit>,
    code_mode: bool,
}

impl Call {
    fn direct(name: &str, args: Option<&serde_json::Value>) -> Self {
        Self {
            tool: safe_label(name),
            units: vec![unit(name, args)],
            code_mode: false,
        }
    }

    fn code(name: &str, source: &str) -> Self {
        let units = js_units(source);
        Self {
            tool: safe_label(name),
            units: if units.is_empty() {
                vec![unit("unclassified Code Mode", None)]
            } else {
                units
            },
            code_mode: true,
        }
    }

    fn description(&self) -> String {
        let mut labels = Vec::new();
        for unit in &self.units {
            if !labels.contains(&unit.label) {
                labels.push(unit.label.clone());
            }
        }
        let more = labels.len() > 3;
        labels.truncate(3);
        safe_label(&format!(
            "{}: {}{}",
            self.tool,
            labels.join("; "),
            if more { "; …" } else { "" }
        ))
    }

    fn unit_weights(&self, item: &ResponseItem) -> Vec<[u64; 7]> {
        let fallback = || {
            self.units
                .iter()
                .map(|unit| operation_weights(&unit.operations))
                .collect::<Vec<_>>()
        };
        if !self.code_mode || self.units.len() == 1 {
            return fallback();
        }
        let body = match item {
            ResponseItem::FunctionCallOutput { output, .. }
            | ResponseItem::CustomToolCallOutput { output, .. } => &output.body,
            _ => return fallback(),
        };
        let mut used = vec![false; self.units.len()];
        let mut weights = vec![[0u64; 7]; self.units.len()];
        let mut pending = 0u64;
        let mut matched = false;
        let mut part = |text: Option<&str>, cost: Cost| {
            // Code Mode's short transport header is shared proportionally with the whole result.
            if text.is_some_and(|text| text.len() < 256 && text.starts_with("Script completed")) {
                return;
            }
            let value = text.and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok());
            let index = value.as_ref().and_then(|value| {
                self.units
                    .iter()
                    .enumerate()
                    .position(|(index, unit)| !used[index] && unit.shape.matches(value))
            });
            if let Some(index) = index {
                used[index] = true;
                matched = true;
                add_weights(
                    &mut weights[index],
                    split(
                        cost.bytes.max(1),
                        operation_weights(&self.units[index].operations),
                    ),
                );
            } else {
                pending = pending.saturating_add(cost.bytes.max(1));
            }
        };
        match body {
            FunctionCallOutputBody::Text(text) => part(Some(text), Cost::text(text)),
            FunctionCallOutputBody::ContentItems(parts) => {
                for content in parts {
                    match content {
                        FunctionCallOutputContentItem::InputText { text } => {
                            part(Some(text), Cost::text(text))
                        }
                        _ => part(
                            None,
                            output_cost(&FunctionCallOutputBody::ContentItems(vec![
                                content.clone(),
                            ])),
                        ),
                    }
                }
            }
        }
        if !matched {
            return fallback();
        }
        if pending > 0 {
            let remaining: Vec<_> = self
                .units
                .iter()
                .enumerate()
                .filter(|(index, _)| !used[*index])
                .collect();
            let n = remaining
                .iter()
                .map(|(_, unit)| unit.operations.len() as u64)
                .sum::<u64>();
            if n == 0 {
                let mut other = [0; 7];
                other[Operation::Other as usize] = pending;
                weights.push(other);
            } else {
                let mut offset = 0;
                for (index, unit) in remaining {
                    let count = unit.operations.len() as u64;
                    let bytes =
                        pending / n * count + (pending % n).saturating_sub(offset).min(count);
                    add_weights(
                        &mut weights[index],
                        split(bytes, operation_weights(&unit.operations)),
                    );
                    offset += count;
                }
            }
        }
        weights
    }
}

pub(super) struct Analysis {
    calls: HashMap<String, Call>,
    summary: ToolResultsSummary,
    ordinal: usize,
    directory: std::path::PathBuf,
    counted: std::collections::HashSet<String>,
}

impl Analysis {
    pub(super) fn new(history: &[ResponseItemEnvelope]) -> Self {
        let mut calls = HashMap::new();
        for envelope in history {
            let item = &envelope.item;
            let pair = match item {
                ResponseItem::FunctionCall {
                    call_id,
                    name,
                    namespace,
                    arguments,
                    ..
                } => {
                    let full_name = qualified(namespace.as_deref(), name);
                    let args = serde_json::from_str::<serde_json::Value>(arguments).ok();
                    let call = if is_code_mode(name, namespace.as_deref()) {
                        args.as_ref()
                            .and_then(|value| value.get("code").and_then(serde_json::Value::as_str))
                            .map(|source| Call::code(&full_name, source))
                            .unwrap_or_else(|| Call::direct(&full_name, args.as_ref()))
                    } else {
                        Call::direct(&full_name, args.as_ref())
                    };
                    Some((call_id.clone(), call))
                }
                ResponseItem::CustomToolCall {
                    call_id,
                    name,
                    namespace,
                    input,
                    ..
                } => {
                    let full_name = qualified(namespace.as_deref(), name);
                    let call = if is_code_mode(name, namespace.as_deref()) {
                        Call::code(&full_name, input)
                    } else {
                        Call::direct(
                            &full_name,
                            serde_json::from_str::<serde_json::Value>(input)
                                .ok()
                                .as_ref(),
                        )
                    };
                    Some((call_id.clone(), call))
                }
                ResponseItem::LocalShellCall {
                    call_id: Some(id),
                    action: LocalShellAction::Exec(action),
                    ..
                } => {
                    let script =
                        codex_shell_command::parse_command::extract_shell_command(&action.command)
                            .map(|(_, script)| script.to_string())
                            .unwrap_or_else(|| {
                                codex_shell_command::parse_command::shlex_join(&action.command)
                            });
                    Some((
                        id.clone(),
                        Call {
                            tool: "local_shell".to_string(),
                            units: vec![
                                shell_unit(&script)
                                    .in_directory(action.working_directory.as_deref()),
                            ],
                            code_mode: false,
                        },
                    ))
                }
                ResponseItem::ToolSearchCall {
                    call_id: Some(id), ..
                } => Some((id.clone(), Call::direct("tool_search", None))),
                _ => None,
            };
            if let Some((id, call)) = pair {
                // Duplicate IDs cannot prove which invocation produced the result.
                calls
                    .entry(id)
                    .and_modify(|old| *old = Call::direct("ambiguous call ID", None))
                    .or_insert(call);
            }
        }
        Self {
            calls,
            summary: ToolResultsSummary::default(),
            ordinal: 0,
            directory: std::path::PathBuf::new(),
            counted: Default::default(),
        }
    }

    pub(super) fn in_directory(mut self, directory: &Path) -> Self {
        self.directory = directory.to_path_buf();
        self
    }

    pub(super) fn record(&mut self, item: &ResponseItem, cost: Cost) {
        self.ordinal += 1;
        let (id, name, namespace) = match item {
            ResponseItem::FunctionCallOutput {
                call_id,
                name,
                namespace,
                ..
            } => (call_id.as_deref(), name.as_deref(), namespace.as_deref()),
            ResponseItem::CustomToolCallOutput { call_id, name, .. } => {
                (Some(call_id.as_str()), name.as_deref(), None)
            }
            ResponseItem::ToolSearchOutput { call_id, .. } => {
                (call_id.as_deref(), Some("tool_search"), None)
            }
            _ => return,
        };
        let fallback = Call::direct(
            &qualified(namespace, name.unwrap_or("unmatched result")),
            None,
        );
        let call = id.and_then(|id| self.calls.get(id)).unwrap_or(&fallback);
        let unit_weights = call.unit_weights(item);
        let mut weights = [0; 7];
        for weight in &unit_weights {
            add_weights(&mut weights, *weight);
        }
        let shares = split(cost.tokens(), weights);
        for op in Operation::ALL {
            if weights[op as usize] == 0 {
                continue;
            }
            let row = &mut self.summary.categories[op as usize];
            row.items = row.items.saturating_add(1);
            row.tokens = row.tokens.saturating_add(shares[op as usize]);
            row.unknown |= cost.unknown;
        }
        let count = id.is_some_and(|id| self.counted.insert(id.to_string()));
        self.summary.details.record(
            call,
            &unit_weights,
            shares,
            cost.unknown,
            &self.directory,
            count,
        );
        self.summary.largest.push(Largest {
            ordinal: self.ordinal,
            tokens: cost.tokens(),
            unknown: cost.unknown,
            description: call.description(),
            shares,
        });
        self.summary
            .largest
            .sort_by_key(|result| (std::cmp::Reverse(result.tokens), result.ordinal));
        self.summary.largest.truncate(5);
    }

    pub(super) fn finish(self) -> ToolResultsSummary {
        self.summary
    }
}

fn qualified(namespace: Option<&str>, name: &str) -> String {
    match namespace.filter(|namespace| !namespace.is_empty()) {
        Some(namespace) => format!("{namespace}.{name}"),
        None => name.to_string(),
    }
}

fn is_code_mode(name: &str, namespace: Option<&str>) -> bool {
    matches!(name, "functions.exec" | "code_exec" | "code_mode")
        || (name == "exec" && namespace.is_none_or(|namespace| namespace == "functions"))
}

fn operation_weights(operations: &[Operation]) -> [u64; 7] {
    let mut weights = [0u64; 7];
    if operations.is_empty() {
        weights[Operation::Other as usize] = 1;
    }
    for operation in operations {
        weights[*operation as usize] += 1;
    }
    weights
}

fn add_weights(total: &mut [u64; 7], weights: [u64; 7]) {
    for (total, weight) in total.iter_mut().zip(weights) {
        *total = total.saturating_add(weight);
    }
}

/// Largest remainders keep all rounding local to each outer result, including tiny results.
fn split(total: u64, mut weights: [u64; 7]) -> [u64; 7] {
    let mut sum: u128 = weights.iter().map(|weight| u128::from(*weight)).sum();
    if sum == 0 {
        weights[Operation::Other as usize] = 1;
        sum = 1;
    }
    let mut shares = [0u64; 7];
    let mut remainders = Vec::new();
    for (index, weight) in weights.into_iter().enumerate() {
        let product = u128::from(total) * u128::from(weight);
        shares[index] = (product / sum) as u64;
        if weight > 0 {
            remainders.push((index, product % sum));
        }
    }
    let allocated: u64 = shares.iter().sum();
    remainders.sort_by_key(|(index, remainder)| (std::cmp::Reverse(*remainder), *index));
    for (index, _) in remainders.into_iter().take((total - allocated) as usize) {
        shares[index] += 1;
    }
    shares
}

fn unit(name: &str, args: Option<&serde_json::Value>) -> Unit {
    let short = name.rsplit(['.', ':']).next().unwrap_or(name);
    if matches!(short, "exec_command" | "shell_command" | "shell") {
        if let Some(command) = args.and_then(|args| args.get("cmd").or_else(|| args.get("command")))
        {
            if let Some(script) = command.as_str() {
                return shell_unit(script).in_directory(
                    args.and_then(|args| args.get("workdir").or_else(|| args.get("cwd")))
                        .and_then(serde_json::Value::as_str),
                );
            }
            if let Some(words) = command.as_array().and_then(|words| {
                words
                    .iter()
                    .map(serde_json::Value::as_str)
                    .collect::<Option<Vec<_>>>()
            }) {
                return shell_unit(&codex_shell_command::parse_command::shlex_join(
                    &words
                        .iter()
                        .map(|word| word.to_string())
                        .collect::<Vec<_>>(),
                ))
                .in_directory(
                    args.and_then(|args| args.get("workdir").or_else(|| args.get("cwd")))
                        .and_then(serde_json::Value::as_str),
                );
            }
        }
    }
    let (operation, shape) = match short {
        "apply_patch" | "write_file" | "edit_file" => (Operation::Edits, Shape::Patch),
        "read_file" | "list_directory" | "list_files" => (Operation::Reads, Shape::Other),
        "search" | "search_files" | "tool_search" => (Operation::Searches, Shape::Other),
        "write_stdin" => (Operation::Other, Shape::Shell),
        _ => (Operation::Other, Shape::Other),
    };
    Unit {
        operations: vec![operation],
        label: safe_label(short),
        shape,
        evidence: vec![details::Evidence::native(operation, short, args)],
        directory: args
            .and_then(|args| args.get("workdir").or_else(|| args.get("cwd")))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    }
}

fn shell_unit(script: &str) -> Unit {
    let mut operations = Vec::new();
    let mut labels = Vec::new();
    let mut evidence = Vec::new();
    let mut directory = Some(std::path::PathBuf::new());
    if let Some(tree) = try_parse_shell(script).filter(|tree| !tree.root_node().has_error()) {
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "command" {
                let mut cursor = node.walk();
                let name = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "command_name");
                let text = name.and_then(|name| script.get(name.start_byte()..node.end_byte()));
                let words = text
                    .and_then(parse_shell_script_into_commands)
                    .and_then(|mut commands| (commands.len() == 1).then(|| commands.remove(0)));
                if let Some(words) = words {
                    // Only straight-line literal directory changes establish a new scope.
                    if words.first().is_some_and(|word| program(word) == "cd") {
                        let mut parent = node.parent();
                        let mut nested = false;
                        while let Some(ancestor) = parent {
                            nested |= matches!(
                                ancestor.kind(),
                                "subshell"
                                    | "function_definition"
                                    | "if_statement"
                                    | "for_statement"
                                    | "while_statement"
                            );
                            parent = ancestor.parent();
                        }
                        directory = if !nested && words.len() == 2 && !words[1].starts_with('-') {
                            directory.map(|cwd| cwd.join(&words[1]))
                        } else {
                            None
                        };
                    }
                    if let Some(operation) = classify_words(&words) {
                        operations.push(operation);
                        evidence.push(details::Evidence::shell(
                            operation,
                            &words,
                            directory.as_deref(),
                        ));
                        let label = command_label(&words, operation);
                        if !labels.contains(&label) {
                            labels.push(label);
                        }
                    }
                } else {
                    operations.push(Operation::Other);
                    directory = None;
                    labels.push("dynamic command".to_string());
                }
            }
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            stack.extend(children.into_iter().rev());
        }
    }
    if operations.is_empty() {
        operations.push(Operation::Other);
    }
    if labels.is_empty() {
        labels.push("shell command".to_string());
    }
    let more = labels.len() > 3;
    labels.truncate(3);
    Unit {
        operations,
        label: safe_label(&format!(
            "{}{}",
            labels.join(", "),
            if more { ", …" } else { "" }
        )),
        shape: Shape::Shell,
        evidence,
        directory: None,
    }
}

fn program(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn git_command(words: &[String]) -> Option<&str> {
    let mut index = 1;
    while let Some(word) = words.get(index) {
        match word.as_str() {
            "-C" | "-c" | "--git-dir" | "--work-tree" => index += 2,
            "--no-pager" | "--no-optional-locks" | "--bare" => index += 1,
            _ if !word.starts_with('-') => return Some(word),
            _ => return None,
        }
    }
    None
}

fn classify_words(words: &[String]) -> Option<Operation> {
    let first = program(words.first()?.as_str());
    let args = &words[1..];
    if first == "cd" || (matches!(first, "export" | "set") && !args.is_empty()) {
        return None;
    }
    Some(match first {
        "cat" | "head" | "tail" | "ls" | "pwd" | "stat" | "wc" | "du" | "file" => Operation::Reads,
        "sed"
            if args.iter().any(|arg| {
                arg == "-i" || arg.starts_with("-i") || arg.starts_with("--in-place")
            }) =>
        {
            Operation::Edits
        }
        "sed" if args.iter().any(|arg| arg == "-n") => Operation::Reads,
        "rg" if args.iter().any(|arg| arg == "--files") => Operation::Reads,
        "rg" | "grep" | "egrep" | "fgrep" => Operation::Searches,
        "find"
            if !args.iter().any(|arg| {
                matches!(arg.as_str(), "-exec" | "-execdir" | "-delete" | "-fprint")
            }) =>
        {
            Operation::Reads
        }
        "git" => match git_command(words) {
            Some("grep") => Operation::Searches,
            Some(
                "status" | "diff" | "log" | "show" | "ls-files" | "ls-tree" | "rev-parse"
                | "describe",
            ) => Operation::Git,
            Some(
                "add" | "apply" | "commit" | "reset" | "checkout" | "switch" | "restore" | "clean"
                | "merge" | "rebase" | "cherry-pick",
            ) => Operation::Edits,
            _ => Operation::Other,
        },
        "cargo" => {
            let action = args
                .iter()
                .find(|arg| !arg.starts_with(['+', '-']))
                .map(String::as_str);
            match action {
                Some("test" | "nextest") => Operation::Tests,
                Some("build" | "check" | "clippy" | "rustc") => Operation::Builds,
                Some("fmt") if !args.iter().any(|arg| arg == "--check") => Operation::Edits,
                _ => Operation::Other,
            }
        }
        "pytest" | "pytest-3" | "ctest" | "nextest" => Operation::Tests,
        "make" | "gmake" | "ninja"
            if args
                .iter()
                .any(|arg| matches!(arg.as_str(), "test" | "check")) =>
        {
            Operation::Tests
        }
        "make" | "gmake" | "ninja" | "rustc" | "clang" | "clang++" | "gcc" | "g++" => {
            Operation::Builds
        }
        "cmake" if args.iter().any(|arg| arg == "--build") => Operation::Builds,
        "meson" if args.first().is_some_and(|arg| arg == "test") => Operation::Tests,
        "meson" if args.first().is_some_and(|arg| arg == "compile") => Operation::Builds,
        "npm" | "pnpm" | "yarn" | "bun" if args.iter().take(2).any(|arg| arg == "test") => {
            Operation::Tests
        }
        "npm" | "pnpm" | "yarn" | "bun" if args.iter().take(2).any(|arg| arg == "build") => {
            Operation::Builds
        }
        "go" if args.first().is_some_and(|arg| arg == "test") => Operation::Tests,
        "go" if args.first().is_some_and(|arg| arg == "build") => Operation::Builds,
        "build.sh" => Operation::Builds,
        "apply_patch" | "rustfmt" | "mkdir" | "cp" | "mv" | "rm" | "touch" | "chmod" => {
            Operation::Edits
        }
        _ => Operation::Other,
    })
}

/// Print executable/subcommand names and read-file basenames, never arbitrary arguments, queries,
/// here-documents, patch bodies, output text, or source code.
fn command_label(words: &[String], operation: Operation) -> String {
    let executable = words.first().map(|word| program(word)).unwrap_or("command");
    let mut label = executable.to_string();
    if executable == "git" {
        if let Some(subcommand) = git_command(words) {
            label.push_str(&format!(" {subcommand}"));
        }
    } else if matches!(
        executable,
        "cargo" | "npm" | "pnpm" | "yarn" | "bun" | "go" | "meson"
    ) {
        if let Some(action) = words
            .iter()
            .skip(1)
            .find(|arg| !arg.starts_with(['+', '-']))
        {
            // Only known operations have a useful, non-arbitrary subcommand label.
            if operation != Operation::Other {
                label.push_str(&format!(" {action}"));
            }
        }
    } else if operation == Operation::Reads && matches!(executable, "cat" | "head" | "tail") {
        if let Some(path) = words
            .get(1..)
            .and_then(|args| args.last())
            .filter(|word| !word.starts_with('-'))
            .and_then(|word| Path::new(word).file_name())
            .and_then(|name| name.to_str())
        {
            label.push_str(&format!(" {path}"));
        }
    }
    safe_label(&label)
}

fn safe_label(text: &str) -> String {
    let text: String = text
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = text.chars().take(96).collect();
    if text.chars().count() > 96 {
        format!("{short}…")
    } else {
        short
    }
}

#[derive(Debug)]
enum Token {
    Word(String),
    String(Option<String>),
    Mark(char),
}

/// A bounded static scanner, not a JavaScript evaluator. Comments, quoted strings and regexes
/// cannot introduce tool calls. Interpolated/computed command arguments remain unclassified.
fn js_units(source: &str) -> Vec<Unit> {
    let Some(tokens) = js_tokens(source) else {
        return Vec::new();
    };
    let mut units = Vec::new();
    for index in 0..tokens.len().saturating_sub(4) {
        let [
            Token::Word(tools),
            Token::Mark('.'),
            Token::Word(method),
            Token::Mark('('),
            ..,
        ] = &tokens[index..]
        else {
            continue;
        };
        if tools != "tools" {
            continue;
        }
        let command = literal_cmd(&tokens[index + 4..]);
        let workdir = literal_property(&tokens[index + 4..], &["workdir", "cwd"]);
        let path = literal_property(&tokens[index + 4..], &["path", "file_path"]);
        let query = literal_property(&tokens[index + 4..], &["query", "pattern"]);
        let args =
            Some(serde_json::json!({"cmd":command,"workdir":workdir,"path":path,"query":query}));
        let mut parsed = unit(method, args.as_ref());
        if workdir.is_none() && has_directory_property(&tokens[index + 4..]) {
            // A runtime expression supplies a directory, so the session cwd is not evidence.
            parsed.evidence.clear();
        }
        units.push(parsed);
    }
    units
}

fn has_directory_property(tokens: &[Token]) -> bool {
    let mut depth = 0i32;
    for token in tokens {
        match token {
            Token::Mark('{' | '[' | '(') => depth += 1,
            Token::Mark('}' | ']' | ')') => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            Token::Word(key) | Token::String(Some(key))
                if depth == 1 && matches!(key.as_str(), "workdir" | "cwd") =>
            {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn literal_cmd(tokens: &[Token]) -> Option<String> {
    literal_property(tokens, &["cmd", "command"])
}

fn literal_property(tokens: &[Token], keys: &[&str]) -> Option<String> {
    if !matches!(tokens.first(), Some(Token::Mark('{'))) {
        return None;
    }
    let mut depth = 0i32;
    let mut cmd = None;
    for (index, token) in tokens.iter().enumerate() {
        match token {
            Token::Mark('{') | Token::Mark('[') | Token::Mark('(') => depth += 1,
            Token::Mark('}') | Token::Mark(']') | Token::Mark(')') => {
                depth -= 1;
                if depth == 0 {
                    return cmd;
                }
            }
            Token::Mark('.') if depth == 1 => return None, // Object spreads may override cmd.
            Token::Word(key) | Token::String(Some(key))
                if depth == 1 && keys.contains(&key.as_str()) =>
            {
                let [
                    Token::Mark(':'),
                    Token::String(Some(value)),
                    Token::Mark(',' | '}'),
                    ..,
                ] = tokens.get(index + 1..)?
                else {
                    return None;
                };
                if cmd.is_some() {
                    return None;
                }
                cmd = Some(value.clone());
            }
            _ => {}
        }
    }
    None
}

fn js_tokens(source: &str) -> Option<Vec<Token>> {
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut tokens = Vec::new();
    while index < bytes.len() {
        if tokens.len() >= 32_768 {
            return None;
        }
        match bytes[index] {
            byte if byte.is_ascii_whitespace() => index += 1,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                let end = source.get(index + 2..)?.find("*/")?;
                index += end + 4;
            }
            b'\'' | b'"' | b'`' => {
                let quote = bytes[index];
                let start = index;
                index += 1;
                let mut escaped = false;
                let mut dynamic = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    if !escaped && byte == quote {
                        break;
                    }
                    dynamic |= quote == b'`' && byte == b'$' && bytes.get(index + 1) == Some(&b'{');
                    if escaped {
                        escaped = false;
                    } else {
                        escaped = byte == b'\\';
                    }
                    index += 1;
                }
                if index == bytes.len() {
                    return None;
                }
                index += 1;
                let literal = &source[start..index];
                let value = if dynamic {
                    None
                } else if quote == b'"' {
                    serde_json::from_str::<String>(literal).ok()
                } else {
                    decode_js_string(&literal[1..literal.len() - 1])
                };
                tokens.push(Token::String(value));
            }
            b'/' => {
                // Regex literals can contain text that looks exactly like a tools invocation.
                // Division is kept as punctuation when a word/string/closing bracket precedes it.
                let regex = tokens.last().is_none_or(|token| match token {
                    Token::Word(word) => matches!(
                        word.as_str(),
                        "return" | "throw" | "yield" | "case" | "typeof" | "void" | "delete"
                    ),
                    Token::String(_) | Token::Mark(')' | ']') => false,
                    Token::Mark(_) => true,
                });
                if regex {
                    index += 1;
                    let mut escaped = false;
                    let mut bracket = false;
                    while index < bytes.len() {
                        let byte = bytes[index];
                        if !escaped && byte == b'/' && !bracket {
                            break;
                        }
                        if !escaped {
                            if byte == b'[' {
                                bracket = true;
                            }
                            if byte == b']' {
                                bracket = false;
                            }
                        }
                        if escaped {
                            escaped = false;
                        } else {
                            escaped = byte == b'\\';
                        }
                        index += 1;
                    }
                    if index == bytes.len() {
                        return None;
                    }
                    index += 1;
                    while bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
                        index += 1;
                    }
                    tokens.push(Token::String(None));
                } else {
                    tokens.push(Token::Mark('/'));
                    index += 1;
                }
            }
            byte if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') => {
                let start = index;
                while bytes
                    .get(index)
                    .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$'))
                {
                    index += 1;
                }
                tokens.push(Token::Word(source[start..index].to_string()));
            }
            byte if byte.is_ascii() => {
                tokens.push(Token::Mark(char::from(byte)));
                index += 1;
            }
            _ => {
                // Skip complete non-ASCII characters without creating fake identifiers.
                index += source[index..].chars().next()?.len_utf8();
                tokens.push(Token::Mark('?'));
            }
        }
    }
    Some(tokens)
}

fn decode_js_string(text: &str) -> Option<String> {
    let mut result = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            result.push(ch);
            continue;
        }
        match chars.next()? {
            'n' => result.push('\n'),
            'r' => result.push('\r'),
            't' => result.push('\t'),
            'b' => result.push('\u{8}'),
            'f' => result.push('\u{c}'),
            '\n' => {}
            '\r' => {}
            quote @ ('\'' | '"' | '`' | '\\' | '/') => result.push(quote),
            'x' => {
                let digits: String = chars.by_ref().take(2).collect();
                if digits.len() != 2 {
                    return None;
                }
                result.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            'u' => {
                let digits: String = chars.by_ref().take(4).collect();
                if digits.len() != 4 {
                    return None;
                }
                result.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(result)
}

#[cfg(test)]
#[path = "tool_results_tests.rs"]
mod tests;
