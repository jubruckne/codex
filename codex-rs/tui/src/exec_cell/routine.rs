//! Conservative display classification for completed routine shell inspection.
//! This does not affect execution, approval, or retained transcript records.

use codex_shell_command::bash::parse_shell_script_into_commands;
use codex_shell_command::bash::try_parse_shell;
use std::path::Component;
use std::path::Path;

/// The boolean records whether exit 1 means a search found no matches.
pub(super) fn routine_script(script: &str) -> Option<bool> {
    let commands = inspection_commands(script)?;
    if commands.is_empty() {
        return None;
    }
    let mut searches_only = true;
    for command in commands {
        searches_only &= routine_command(&command)?;
    }
    Some(searches_only)
}

/// Allow literal stdout files under the checkout's scratch directory, then let
/// the existing strict parser validate all remaining shell syntax.
pub(super) fn inspection_commands(script: &str) -> Option<Vec<Vec<String>>> {
    if let Some(commands) = parse_shell_script_into_commands(script) {
        return Some(commands);
    }
    let tree = try_parse_shell(script)?;
    if tree.root_node().has_error() {
        return None;
    }
    let mut stack = vec![tree.root_node()];
    let mut redirects = Vec::new();
    while let Some(node) = stack.pop() {
        if node.kind() == "file_redirect" {
            if node.child_by_field_name("descriptor").is_some()
                || node.child(0)?.kind() != ">"
                || node.named_child_count() != 1
            {
                return None;
            }
            let destination = node.child_by_field_name("destination")?;
            let source = destination.utf8_text(script.as_bytes()).ok()?;
            let literal = parse_shell_script_into_commands(&format!("cat {source}"))?;
            let [words] = literal.as_slice() else {
                return None;
            };
            let [_, path] = words.as_slice() else {
                return None;
            };
            let mut components = Path::new(path).components().peekable();
            if components.peek() == Some(&Component::CurDir) {
                components.next();
            }
            if components.next() != Some(Component::Normal(".build".as_ref())) {
                return None;
            }
            let remaining: Vec<_> = components.collect();
            if remaining.is_empty()
                || !remaining.iter().all(|component| matches!(component, Component::Normal(_)))
            {
                return None;
            }
            redirects.push(node.byte_range());
            continue;
        }
        let mut cursor = node.walk();
        stack.extend(node.named_children(&mut cursor));
    }
    if redirects.is_empty() {
        return None;
    }
    let mut stripped = script.as_bytes().to_vec();
    for range in redirects {
        stripped[range].fill(b' ');
    }
    parse_shell_script_into_commands(&String::from_utf8(stripped).ok()?)
}

fn routine_command(words: &[String]) -> Option<bool> {
    let (program, args) = words.split_first()?;
    // Do not classify arbitrary project scripts by their basename.
    let program = program
        .strip_prefix("/usr/bin/")
        .or_else(|| program.strip_prefix("/bin/"))
        .unwrap_or(program);
    if args.len() == 1
        && matches!(args[0].as_str(), "--version" | "-V")
        && matches!(
            program,
            "git" | "rg" | "grep" | "sed" | "cat" | "ls" | "cargo" | "rustc"
                | "rustfmt" | "rustup" | "python" | "python3" | "pip" | "pip3"
                | "node" | "npm" | "cmake" | "ninja" | "meson"
        )
    {
        return Some(false);
    }
    match program {
        "rg" if !args.iter().any(|arg| arg == "--pre" || arg.starts_with("--pre=")) => Some(true),
        "grep" | "egrep" | "fgrep" => Some(true),
        "cat" | "head" | "tail" | "ls" | "pwd" | "wc" | "cut" | "tr" | "nl" | "stat" | "cmp"
        | "readlink" | "basename" | "dirname" | "which" | "uname" | "whoami" => Some(false),
        "uniq" if args.len() <= 1 => Some(false),
        "sort"
            if !args.iter().any(|arg| {
                arg.starts_with("--output")
                    || arg.starts_with("--compress-program")
                    || (arg.starts_with('-') && !arg.starts_with("--") && arg.contains('o'))
            }) =>
        {
            Some(false)
        }
        "sed" if sed_read(args).is_some() => Some(false),
        "find"
            if !args.iter().any(|arg| {
                matches!(
                    arg.as_str(),
                    "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fprint"
                        | "-fprint0" | "-fprintf" | "-fls"
                )
            }) =>
        {
            Some(false)
        }
        "git" if git_inspection(args) => Some(false),
        _ => None,
    }
}

pub(super) fn sed_read(args: &[String]) -> Option<(Vec<String>, String)> {
    let (quiet, args) = args.split_first()?;
    if !matches!(quiet.as_str(), "-n" | "--quiet" | "--silent") {
        return None;
    }
    let args = if args
        .first()
        .is_some_and(|arg| matches!(arg.as_str(), "-e" | "--expression"))
    {
        &args[1..]
    } else {
        args
    };
    let (expression, paths) = args.split_first()?;
    let range = expression.strip_suffix('p')?;
    let addresses: Vec<_> = range.split(',').collect();
    let valid = (1..=2).contains(&addresses.len())
        && addresses.iter().all(|address| {
            *address == "$"
                || (!address.is_empty() && address.chars().all(|ch| ch.is_ascii_digit()))
        })
        && paths.iter().all(|path| !path.starts_with('-'));
    valid.then(|| (paths.to_vec(), range.replace(',', "–").replace('$', "end")))
}

fn git_inspection(mut args: &[String]) -> bool {
    while args.first().is_some_and(|arg| arg == "-C") {
        if args.len() < 3 {
            return false;
        }
        args = &args[2..];
    }
    args.first().is_some_and(|arg| {
        matches!(
            arg.as_str(),
            "status" | "diff" | "log" | "show" | "ls-files" | "ls-tree" | "rev-parse"
                | "describe"
        )
    }) && !args.iter().any(|arg| {
        arg.starts_with("--output") || matches!(arg.as_str(), "--ext-diff" | "--textconv")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routine_commands_recognizes_inspection_and_plain_groups() {
        for script in [
            "rg -n -C 6 'mark_running|mark_stopped' a.rs b.rs",
            "rg --files --hidden -g AGENTS.md -g '!.git' .",
            "sed -n '150,260p' app/events.rs",
            "sed -n -e '10,$p' file",
            "cat build.sh | head -25",
            "ls -la; pwd; wc -l file",
            "git -C .build/upstream status --short; git diff --stat",
            "git log -5 --oneline; git show HEAD:file; git rev-parse HEAD",
            "cargo --version; python3 -V; node --version",
            "find . -name AGENTS.md; sort file | uniq",
            "cmp .build/expected.patch .build/current.patch",
            "git -C .build/upstream diff --binary HEAD > .build/routine-applied.patch\ncmp .build/expected.patch .build/routine-applied.patch",
            "cat file > './.build/scratch output.txt'",
        ] {
            assert!(routine_script(script).is_some(), "{script}");
        }
        assert_eq!(routine_script("rg a file | grep b"), Some(true));
        assert_eq!(routine_script("rg a file; cat other"), Some(false));
    }

    #[test]
    fn routine_commands_keeps_changes_execution_and_complex_shell_visible() {
        for script in [
            "cargo test", "cargo build", "npm test", "./build.sh --version",
            "git checkout main", "git commit -m update", "git branch new",
            "git diff --output=changes.patch", "git -c alias.status=custom status",
            "sed -i 's/a/b/' file", "sed -n '1p;w copy' file", "sed -n '1p' -i file",
            "rg --pre ./filter pattern file", "rg --pre=./filter pattern file",
            "find . -delete", "find . -exec touch {} +", "find . -fprint listing",
            "sort -o sorted file", "sort -no sorted file", "sort --output=sorted file",
            "uniq file copy", "file -C", "cat file | tee copy",
            "cat file > copy", "cat $(touch file)", "cat <(run-something)",
            "rg pattern file; cargo test", "cat file &", "", "echo hello",
            "git diff > changes.patch", "git diff > .build/../changes.patch",
            "git diff > .build", "git diff > .build/diff.patch > changes.patch",
            "git diff > .build/diff.patch --stat", "git diff > .build/$(touch file)",
            "cargo test > .build/test.log", "git diff 2> .build/errors.log",
        ] {
            assert_eq!(routine_script(script), None, "{script}");
        }
    }
}
