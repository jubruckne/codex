//! Live commands keep complete source; routine inspection stays out of native scrollback.
use super::model::ExecCall;
use super::model::ExecCell;
use crate::history_cell::HistoryRenderMode;
use crate::line_truncation::truncate_line_with_ellipsis_if_overflow;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::motion::loading_glyph;
use crate::render::highlight::highlight_code_to_lines;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::adaptive_wrap_hyperlink_lines;
use crate::terminal_hyperlinks::plain_hyperlink_lines;
use crate::wrapping::RtOptions;
use codex_ansi_escape::ansi_escape_line;
use codex_utils_elapsed::format_duration;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::borrow::Cow;

const OUTPUT_ROWS: usize = 8;

// Conservative detection: one simple Python stdin invocation and one literal delimiter.
// Unknown shell here-documents make the whole script fall back to the existing shell grammar.
fn python_delimiter(line: &str) -> Option<String> {
    let (invocation, delimiter) = line.split_once("<<")?;
    if delimiter.starts_with('-') {
        return None;
    }
    let words = shlex::split(invocation)?;
    let interpreter = std::path::Path::new(words.first()?).file_name()?.to_str()?;
    if !matches!(interpreter, "python" | "python3")
        || !words[1..]
            .iter()
            .all(|word| matches!(word.as_str(), "-" | "-u" | "-B"))
    {
        return None;
    }
    let delimiter = shlex::split(delimiter)?;
    let [delimiter] = delimiter.as_slice() else {
        return None;
    };
    if delimiter.is_empty()
        || !delimiter
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
    {
        return None;
    }
    Some(delimiter.clone())
}

fn exact_highlight(text: &str, language: &str) -> Vec<Line<'static>> {
    let original: Vec<_> = text.split('\n').collect();
    let mut highlighted = highlight_code_to_lines(text, language);
    if highlighted.len() + 1 == original.len() && original.last() == Some(&"") {
        highlighted.push(Line::default());
    }
    if highlighted.len() != original.len()
        || highlighted
            .iter()
            .zip(&original)
            .any(|(line, source)| line.to_string() != *source)
    {
        return original
            .into_iter()
            .map(|line| Line::from(line.to_string()))
            .collect();
    }
    highlighted
}

pub(super) fn command_highlights(script: &str) -> Vec<Line<'static>> {
    let source: Vec<_> = script.split('\n').collect();
    let mut lines = exact_highlight(script, "bash");
    let mut index = 0;
    while index < source.len() {
        if !source[index].contains("<<") {
            index += 1;
            continue;
        }
        let Some(delimiter) = python_delimiter(source[index]) else {
            return exact_highlight(script, "bash");
        };
        let Some(end) = (index + 1..source.len()).find(|next| source[*next] == delimiter) else {
            return exact_highlight(script, "bash");
        };
        if end > index + 1 {
            let body = source[index + 1..end].join("\n");
            lines.splice(index + 1..end, exact_highlight(&body, "python"));
        }
        index = end + 1;
    }
    lines
}

fn output_line(raw: &str, width: usize) -> Line<'static> {
    // The existing terminal emulator handles cursor-return/erase progress without touching records.
    let mut line = if raw.contains('\r') {
        let columns = raw.len().saturating_mul(4).clamp(1, usize::from(u16::MAX)) as u16;
        let mut terminal = vt100::Parser::new(1, columns, 0);
        terminal.process(b"\x1b[?7l"); // progress is a logical line, never a wrapping screen
        terminal.process(raw.as_bytes());
        let bytes = terminal
            .screen()
            .rows_formatted(0, columns)
            .next()
            .unwrap_or_default();
        ansi_escape_line(&String::from_utf8_lossy(&bytes))
    } else {
        ansi_escape_line(raw)
    };
    for span in &mut line.spans {
        span.style = span.style.add_modifier(Modifier::DIM);
    }
    truncate_line_with_ellipsis_if_overflow(line, width)
}

impl ExecCell {
    fn hidden_routine_call(&self, call: &ExecCall) -> bool {
        if !self.native_committed
            || call.is_user_shell_command()
            || call.is_unified_exec_interaction()
        {
            return false;
        }
        let Some(no_match_exit) = super::routine::routine_script(&self.original_command(call)) else {
            return false;
        };
        let exit_code = self
            .reported_exit_codes
            .get(&call.call_id)
            .copied()
            .flatten()
            .or_else(|| call.output.as_ref().map(|output| output.exit_code));
        let known_no_match = no_match_exit
            && self.reported_exit_codes.get(&call.call_id) != Some(&None);
        exit_code.is_none_or(|code| code == 0 || (code == 1 && known_no_match))
    }

    pub(super) fn original_command<'a>(&'a self, call: &'a ExecCall) -> Cow<'a, str> {
        // Unwrap only the existing recognized shell wrapper, retaining its exact script bytes.
        if let Some((_, script)) =
            codex_shell_command::parse_command::extract_shell_command(&call.command)
        {
            return Cow::Borrowed(script);
        }
        self.original_commands
            .get(&call.call_id)
            .map(|command| Cow::Borrowed(command.as_str()))
            .unwrap_or_else(|| {
                Cow::Owned(crate::exec_command::strip_bash_lc_and_escape(&call.command))
            })
    }

    pub(super) fn native_command_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        let mut lines = Vec::new();
        let previews: Vec<_> = self
            .iter_calls()
            .map(|call| {
                if call.is_user_shell_command()
                    || call.is_unified_exec_interaction()
                    || (call.start_time.is_none()
                        && call.output.as_ref().is_some_and(|output| output.exit_code != 0)
                        && !self.hidden_routine_call(call))
                {
                    None
                } else {
                    super::preview::CommandPreview::parse(&self.original_command(call))
                }
            })
            .collect();
        let short_paths = super::preview::short_paths(&previews);
        let visible: Vec<_> = self
            .iter_calls()
            .map(|call| !self.hidden_routine_call(call))
            .collect();
        let grouped = visible.iter().filter(|visible| **visible).count() > 1;
        if grouped {
            let header = Line::from(
                if self
                    .iter_calls()
                    .zip(&visible)
                    .any(|(call, visible)| {
                        *visible && call.duration.is_none() && call.start_time.is_some()
                    })
                {
                    "• Running"
                } else {
                    "• Ran"
                },
            )
            .bold();
            lines.push(
                if self.iter_calls().zip(&visible).any(|(call, visible)| {
                    *visible
                        && call.start_time.is_none()
                        && call
                            .output
                            .as_ref()
                            .is_some_and(|output| output.exit_code != 0)
                }) {
                    header.red().into()
                } else {
                    header.into()
                },
            );
        }
        let mut pending = Vec::new();
        let flush = |pending: &mut Vec<&super::preview::CommandPreview>, lines: &mut Vec<HyperlinkLine>| {
            let summary = super::preview::summary_lines(pending, &short_paths);
            if !summary.is_empty() {
                lines.push(Line::from(vec!["• ".dim(), "Explored".bold()]).into());
            }
            for (index, line) in summary.into_iter().enumerate() {
                lines.extend(adaptive_wrap_hyperlink_lines(
                    &plain_hyperlink_lines(vec![line]),
                    RtOptions::new(usize::from(width).max(1))
                        .initial_indent(if index == 0 { "  └ ".dim().into() } else { "    ".into() })
                        .subsequent_indent("    ".into()),
                ));
            }
            pending.clear();
        };
        for (index, call) in self.iter_calls().enumerate() {
            if !visible[index] {
                if let Some(preview) = &previews[index] {
                    pending.push(preview);
                }
                let details = self.group.details.lines_after(
                    index + 1,
                    width,
                    HistoryRenderMode::Rich,
                );
                if !details.is_empty() {
                    flush(&mut pending, &mut lines);
                    lines.extend(details);
                    if visible[index + 1..].iter().any(|visible| *visible)
                        && lines
                            .last()
                            .is_some_and(|line| !line.line.to_string().trim().is_empty())
                    {
                        lines.push(HyperlinkLine::from(""));
                    }
                }
                continue;
            }
            flush(&mut pending, &mut lines);
            let running = call.duration.is_none() && call.start_time.is_some();
            let failed = call
                .output
                .as_ref()
                .is_some_and(|output| !running && output.exit_code != 0);
            let marker = if failed {
                "•".red().bold()
            } else if running {
                activity_indicator(
                    call.start_time,
                    MotionMode::from_animations_enabled(self.animations_enabled()),
                    ReducedMotionIndicator::StaticBullet,
                )
                .unwrap_or_else(|| "•".dim())
            } else {
                "•".green().bold()
            };
            let heading = if grouped {
                Line::from("  │ ".dim())
            } else {
                Line::from(vec![
                    marker,
                    " ".into(),
                    if running {
                        "Running ".bold()
                    } else {
                        "Ran ".bold()
                    },
                ])
            };
            let command = if let Some(preview) = &previews[index] {
                preview.lines(usize::from(width).saturating_sub(4), &short_paths)
            } else {
                command_highlights(&self.original_command(call))
            };
            let mut command = adaptive_wrap_hyperlink_lines(
                &plain_hyperlink_lines(command),
                RtOptions::new(usize::from(width).max(1))
                    .initial_indent(heading)
                    .subsequent_indent("  │ ".dim().into()),
            );
            // Execution metadata belongs to the command, never to an output row.
            if !command.is_empty() {
                let mut result = String::new();
                if failed
                    && let Some(exit_code) = self
                        .reported_exit_codes
                        .get(&call.call_id)
                        .copied()
                        .unwrap_or_else(|| call.output.as_ref().map(|output| output.exit_code))
                {
                    result.push_str(&format!(" (exit {exit_code})"));
                } else if failed {
                    result.push_str(" (failed)");
                }
                if let Some(duration) = call.duration.filter(|duration| duration.as_secs() >= 10) {
                    result.push_str(&format!(" · {}", format_duration(duration)));
                } else if let Some(start) = call.start_time.filter(|_| running) {
                    result.push_str(&format!(" · {}", format_duration(start.elapsed())));
                }
                if !result.is_empty() {
                    let last = command
                        .iter()
                        .rposition(|line| line.line.to_string().trim() != "│")
                        .unwrap_or(command.len() - 1);
                    let metadata = Line::from(result).dim();
                    let reserved = metadata.width().min(usize::from(width));
                    let mut final_line = truncate_line_with_ellipsis_if_overflow(
                        command[last].line.clone(),
                        usize::from(width).saturating_sub(reserved),
                    );
                    final_line.spans.extend(metadata.spans);
                    command[last] =
                        truncate_line_with_ellipsis_if_overflow(final_line, usize::from(width))
                            .into();
                    lines.append(&mut command);
                } else {
                    lines.append(&mut command);
                }
            }
            if running && !self.native_committed {
                let tail: Vec<_> = call
                    .output
                    .as_ref()
                    .map(|output| output.lines().rev().take(OUTPUT_ROWS).collect::<Vec<_>>())
                    .unwrap_or_default();
                if tail.is_empty() {
                    let glyph = match (call.start_time, self.native_frame_requester.as_ref()) {
                        (Some(start), Some(requester)) => loading_glyph(
                            start,
                            MotionMode::from_animations_enabled(self.animations_enabled()),
                            requester,
                        ),
                        _ => "◌",
                    };
                    lines.push(
                        truncate_line_with_ellipsis_if_overflow(
                            Line::from(format!("  └ {glyph}")).dim(),
                            usize::from(width),
                        )
                        .into(),
                    );
                } else {
                    for (row, raw) in tail.into_iter().rev().enumerate() {
                        let prefix = if row == 0 { "  └ " } else { "    " };
                        let mut line =
                            output_line(raw.as_ref(), usize::from(width).saturating_sub(4));
                        line.spans.insert(0, prefix.dim());
                        lines.push(
                            truncate_line_with_ellipsis_if_overflow(line, usize::from(width))
                                .into(),
                        );
                    }
                }
            }
            let details = self
                .group
                .details
                .lines_after(index + 1, width, HistoryRenderMode::Rich);
            if !details.is_empty() {
                lines.extend(details);
                // Reasoning inside a group needs separation from the following command too.
                if visible[index + 1..].iter().any(|visible| *visible)
                    && lines.last().is_some_and(|line| {
                        line.line
                            .spans
                            .iter()
                            .any(|span| !span.content.trim().is_empty())
                    })
                {
                    lines.push(HyperlinkLine::from(""));
                }
            }
        }
        flush(&mut pending, &mut lines);
        lines
    }
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;
