//! Deterministic live previews for simple searches and file reads.

use super::routine::inspection_commands;
use super::routine::routine_script;
use super::routine::sed_read;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::style::accent_color;
use codex_shell_command::bash::try_parse_shell;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::collections::BTreeSet;
use std::collections::HashMap;
use unicode_width::UnicodeWidthStr;

#[derive(Debug)]
enum Action {
    Search {
        query: String,
        paths: Vec<String>,
    },
    List(Vec<String>),
    Read(Vec<(String, Option<String>)>),
    Raw(String),
}

#[derive(Debug)]
pub(super) struct CommandPreview {
    actions: Vec<Action>,
}

impl CommandPreview {
    pub(super) fn parse(script: &str) -> Option<Self> {
        routine_script(script)?;
        let tree = try_parse_shell(script)?;
        let mut stack = vec![tree.root_node()];
        let mut units = Vec::new();
        while let Some(node) = stack.pop() {
            match node.kind() {
                "program" | "list" => {
                    let mut cursor = node.walk();
                    let children: Vec<_> = node.named_children(&mut cursor).collect();
                    stack.extend(children.into_iter().rev());
                }
                "command" | "pipeline" | "redirected_statement" => {
                    let mut descendants = vec![node];
                    let mut redirected = false;
                    while let Some(child) = descendants.pop() {
                        redirected |= child.kind() == "file_redirect";
                        let mut cursor = child.walk();
                        descendants.extend(child.named_children(&mut cursor));
                    }
                    units.push((
                        node.utf8_text(script.as_bytes()).ok()?.to_string(),
                        redirected,
                    ));
                }
                _ => return None,
            }
        }
        let mut actions = Vec::new();
        let mut compacted = false;
        for (unit, redirected) in units {
            let words = inspection_commands(&unit)?;
            let action = if redirected {
                None
            } else {
                simple_action(&words)
            }
            .unwrap_or_else(|| Action::Raw(unit));
            compacted |= !matches!(action, Action::Raw(_));
            if let Action::Read(files) = action {
                if let Some(Action::Read(previous)) = actions.last_mut() {
                    previous.extend(files);
                } else {
                    actions.push(Action::Read(files));
                }
            } else {
                actions.push(action);
            }
        }
        compacted.then_some(Self { actions })
    }

    fn paths(&self) -> Vec<&str> {
        let mut paths = Vec::new();
        for action in &self.actions {
            match action {
                Action::Search { paths: targets, .. } | Action::List(targets) => {
                    paths.extend(targets.iter().map(String::as_str));
                }
                Action::Read(files) => paths.extend(files.iter().map(|(path, _)| path.as_str())),
                Action::Raw(_) => {}
            }
        }
        paths
    }

    pub(super) fn lines(&self, width: usize, names: &HashMap<String, String>) -> Vec<Line<'static>> {
        let name = |path: &str| {
            names.get(path).map(String::as_str).unwrap_or(path).to_string()
        };
        let mut lines = Vec::new();
        for action in &self.actions {
            match action {
                Action::Search { query, paths } => {
                    let targets = paths
                        .iter()
                        .map(|path| name(path))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let target_width = if targets.is_empty() {
                        0
                    } else {
                        4 + targets.width()
                    };
                    let budget = width
                        .saturating_sub(9 + target_width)
                        .max(8)
                        .min(width.saturating_sub(9).max(1));
                    let query = query.replace('\n', "\\n");
                    let query = truncate_line_with_ellipsis_if_overflow(Line::from(query), budget);
                    let mut line = Line::from(vec!["Search ".fg(accent_color()), "'".into()]);
                    line.spans.extend(query.spans);
                    line.push_span("'");
                    if !paths.is_empty() {
                        line.push_span(" in ");
                        line.push_span(targets);
                    }
                    lines.push(line);
                }
                Action::List(paths) => lines.push(Line::from(vec![
                    "List files in ".fg(accent_color()),
                    paths
                        .iter()
                        .map(|path| name(path))
                        .collect::<Vec<_>>()
                        .join(", ")
                        .into(),
                ])),
                Action::Read(files) => {
                    let files = files
                        .iter()
                        .map(|(path, range)| {
                            let path = name(path);
                            range
                                .as_ref()
                                .map(|range| format!("{path}:{range}"))
                                .unwrap_or(path)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    lines.push(Line::from(vec!["Read ".fg(accent_color()), files.into()]));
                }
                Action::Raw(source) => {
                    lines.extend(source.lines().map(|line| Line::from(line.to_string())));
                }
            }
        }
        lines
    }
}

// Summaries merge only across adjacent routine calls, never across reasoning or changes.
pub(super) fn summary_lines(
    previews: &[&CommandPreview],
    names: &HashMap<String, String>,
) -> Vec<Line<'static>> {
    enum Summary {
        Read(Vec<String>),
        Search(Vec<String>, Vec<String>),
        List(Vec<String>),
    }
    fn extend_unique(target: &mut Vec<String>, values: impl IntoIterator<Item = String>) {
        for value in values {
            if !target.contains(&value) {
                target.push(value);
            }
        }
    }
    let mut summaries = Vec::new();
    for preview in previews {
        for action in &preview.actions {
            match action {
                Action::Read(files) => {
                    let index = summaries.last().filter(|item| matches!(item, Summary::Read(_))).map(|_| summaries.len() - 1)
                        .unwrap_or_else(|| {
                            summaries.push(Summary::Read(Vec::new()));
                            summaries.len() - 1
                        });
                    if let Summary::Read(paths) = &mut summaries[index] {
                        extend_unique(paths, files.iter().map(|(path, _)| path.clone()));
                    }
                }
                Action::Search { query, paths } => {
                    let index = summaries.last().filter(|item| {
                        matches!(item, Summary::Search(_, targets) if targets == paths)
                    }).map(|_| summaries.len() - 1).unwrap_or_else(|| {
                        summaries.push(Summary::Search(Vec::new(), paths.clone()));
                        summaries.len() - 1
                    });
                    if let Summary::Search(queries, _) = &mut summaries[index] {
                        extend_unique(queries, [query.clone()]);
                    }
                }
                Action::List(paths) => {
                    let index = summaries.last().filter(|item| matches!(item, Summary::List(_))).map(|_| summaries.len() - 1)
                        .unwrap_or_else(|| {
                            summaries.push(Summary::List(Vec::new()));
                            summaries.len() - 1
                        });
                    if let Summary::List(targets) = &mut summaries[index] {
                        extend_unique(targets, paths.clone());
                    }
                }
                Action::Raw(_) => {}
            }
        }
    }
    let paths_text = |paths: &[String]| paths.iter()
        .map(|path| names.get(path).unwrap_or(path).as_str())
        .collect::<Vec<_>>().join(", ");
    summaries.into_iter().map(|summary| {
        let (verb, details) = match summary {
            Summary::Read(paths) => ("Read ", paths_text(&paths)),
            Summary::List(paths) => ("Listed files in ", paths_text(&paths)),
            Summary::Search(queries, paths) => {
                let queries = queries.iter().map(|query| format!("'{}'", query.replace('\n', "\\n")))
                    .collect::<Vec<_>>().join(", ");
                let targets = if paths.is_empty() { String::new() }
                    else { format!(" in {}", paths_text(&paths)) };
                ("Searched ", format!("{queries}{targets}"))
            }
        };
        Line::from(vec![verb.fg(accent_color()), details.into()])
    }).collect()
}

pub(super) fn short_paths(previews: &[Option<CommandPreview>]) -> HashMap<String, String> {
    let paths: BTreeSet<_> = previews
        .iter()
        .flatten()
        .flat_map(CommandPreview::paths)
        .collect();
    let components: Vec<_> = paths
        .iter()
        .map(|path| path.split('/').filter(|part| !part.is_empty()).collect::<Vec<_>>())
        .collect();
    paths
        .iter()
        .zip(&components)
        .map(|(path, parts)| {
            let short = (1..=parts.len())
                .find_map(|count| {
                    let suffix = &parts[parts.len() - count..];
                    let unique = components.iter().zip(&paths).all(|(other, other_path)| {
                        other_path == path
                            || other.len() < suffix.len()
                            || !other[other.len() - suffix.len()..]
                                .iter()
                                .zip(suffix)
                                .all(|(left, right)| left.eq_ignore_ascii_case(right))
                    });
                    unique.then(|| suffix.join("/"))
                })
                .unwrap_or_else(|| path.to_string());
            (path.to_string(), short)
        })
        .collect()
}

fn simple_action(commands: &[Vec<String>]) -> Option<Action> {
    if let [cat, head] = commands {
        let [program, path] = cat.as_slice() else {
            return None;
        };
        if program != "cat" || path.starts_with('-') || head.first()? != "head" {
            return None;
        }
        let count = match &head[1..] {
            [count] => count.strip_prefix('-')?.parse::<usize>().ok()?,
            [flag, count] if flag == "-n" => count.parse::<usize>().ok()?,
            _ => return None,
        };
        return (count > 0)
            .then(|| Action::Read(vec![(path.clone(), Some(format!("1–{count}")))]));
    }
    let [words] = commands else {
        return None;
    };
    let (program, args) = words.split_first()?;
    match program.as_str() {
        "rg" => rg_action(args),
        "sed" => {
            let (paths, range) = sed_read(args)?;
            (!paths.is_empty()).then(|| {
                Action::Read(paths.into_iter().map(|path| (path, Some(range.clone()))).collect())
            })
        }
        "cat" if !args.is_empty() && args.iter().all(|arg| !arg.starts_with('-')) => {
            Some(Action::Read(
                args.iter().cloned().map(|path| (path, None)).collect(),
            ))
        }
        _ => None,
    }
}

fn rg_action(args: &[String]) -> Option<Action> {
    let mut query = None;
    let mut paths = Vec::new();
    let mut list = false;
    let mut positional = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if !positional && arg == "--" {
            positional = true;
        } else if !positional && arg == "--files" {
            list = true;
        } else if !positional && matches!(arg.as_str(), "-e" | "--regexp") {
            if query.is_some() {
                return None;
            }
            index += 1;
            query = Some(args.get(index)?.clone());
        } else if !positional && matches!(arg.as_str(), "-A" | "-B" | "-C" | "-g" | "-t" | "-T" | "-m"
            | "--after-context" | "--before-context" | "--context" | "--glob" | "--iglob"
            | "--type" | "--type-not" | "--max-count" | "--max-columns")
        {
            index += 1;
            args.get(index)?;
        } else if !positional && matches!(arg.as_str(), "-n" | "-i" | "-s" | "-S" | "-F" | "-w" | "-x"
            | "-l" | "-c" | "-v" | "--line-number" | "--ignore-case" | "--case-sensitive"
            | "--smart-case" | "--fixed-strings" | "--word-regexp" | "--line-regexp"
            | "--hidden" | "--no-ignore" | "--no-ignore-vcs" | "--follow" | "--count"
            | "--files-with-matches" | "--invert-match" | "--multiline" | "--pcre2")
        {
            // Formatting and search flags remain available in the full command.
        } else if !positional && arg.starts_with('-') {
            return None;
        } else if !list && query.is_none() {
            query = Some(arg.clone());
        } else {
            paths.push(arg.clone());
        }
        index += 1;
    }
    if list {
        if query.is_some() {
            return None;
        }
        if paths.is_empty() {
            paths.push(".".into());
        }
        Some(Action::List(paths))
    } else {
        Some(Action::Search { query: query?, paths })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(script: &str) -> Vec<String> {
        let previews = vec![Some(CommandPreview::parse(script).unwrap())];
        let paths = short_paths(&previews);
        previews[0].as_ref().unwrap().lines(160, &paths).iter().map(ToString::to_string).collect()
    }

    #[test]
    fn routine_commands_live_previews_preserve_queries_files_and_read_ranges() {
        assert_eq!(preview("rg -n 'sync_agent_navigation_strip' .build/upstream/tui/src/app/thread_routing.rs"), ["Search 'sync_agent_navigation_strip' in thread_routing.rs"]);
        assert_eq!(preview("rg -n -C 6 'mark_running|mark_stopped' file1.rs file2.rs"), ["Search 'mark_running|mark_stopped' in file1.rs, file2.rs"]);
        assert_eq!(preview("rg --files .build/upstream/tui/src/app"), ["List files in app"]);
        assert_eq!(preview("sed -n '150,260p' app/app_server_events.rs; cat build.sh"), ["Read app_server_events.rs:150–260, build.sh"]);
        assert_eq!(preview("cat build.sh | head -25"), ["Read build.sh:1–25"]);
        assert_eq!(preview("git status --short; cat build.sh; cmp a b"), ["git status --short", "Read build.sh", "cmp a b"]);
    }

    #[test]
    fn routine_commands_live_previews_disambiguate_paths_and_bound_long_queries() {
        let previews = vec![CommandPreview::parse("cat a/style.rs"), CommandPreview::parse("cat b/style.rs")];
        let paths = short_paths(&previews);
        assert_eq!(paths["a/style.rs"], "a/style.rs");
        assert_eq!(paths["b/style.rs"], "b/style.rs");
        let script = format!("rg '{}' app/file.rs", "界".repeat(100));
        let preview = CommandPreview::parse(&script).unwrap();
        let line = preview.lines(40, &HashMap::new())[0].to_string();
        assert!(line.contains('…') && line.ends_with("in app/file.rs"));
        assert!(!line.contains(&"界".repeat(100)));
        assert!(line.width() <= 40);
        let fits = CommandPreview::parse("rg 'abcdefghijklmnopqrstuvwxyz012345678' app/file.rs").unwrap();
        assert!(!fits.lines(60, &HashMap::new())[0].to_string().contains('…'));
    }

    #[test]
    fn routine_commands_live_previews_keep_unrecognized_execution_verbatim() {
        for script in ["sed -i 's/a/b/' file", "cat file | tee copy", "rg a file; cargo test", "cat file > copy", "rg --version", "cat file > .build/out | head -25", "cat -n | head -25", "rg app --files"] {
            assert!(CommandPreview::parse(script).is_none(), "{script}");
        }
    }
}
