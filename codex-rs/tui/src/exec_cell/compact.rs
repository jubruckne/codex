//! Owned-transcript command previews with full command/output retained for local disclosure.

use super::model::ExecCall;
use super::model::ExecCell;
use super::native::command_highlights;
use crate::history_cell::ActivityDisclosure;
use crate::history_cell::HistoryRenderMode;
use crate::history_cell::activity_preview::DETAIL_PREVIEW_LINES;
use crate::history_cell::activity_preview::clipped_line;
use crate::motion::MotionMode;
use crate::motion::ReducedMotionIndicator;
use crate::motion::activity_indicator;
use crate::terminal_hyperlinks::HyperlinkLine;
use crate::terminal_hyperlinks::adaptive_wrap_hyperlink_lines;
use crate::terminal_hyperlinks::plain_hyperlink_lines;
use crate::wrapping::RtOptions;
use codex_ansi_escape::ansi_escape_line;
use codex_utils_elapsed::format_duration;
use ratatui::style::Modifier;
use ratatui::style::Stylize;
use ratatui::text::Line;

impl ExecCell {
    pub(super) fn suppress_completed_output(call: &ExecCall) -> bool {
        call.duration.is_some()
            && !call.is_user_shell_command()
            && !call.is_unified_exec_interaction()
    }

    pub(super) fn command_failed(&self, call: &ExecCall) -> bool {
        self.reported_exit_codes
            .get(&call.call_id)
            .copied()
            .flatten()
            .or_else(|| call.output.as_ref().map(|output| output.exit_code))
            .is_some_and(|code| code != 0)
    }

    pub(super) fn append_command_metadata(&self, call: &ExecCall, lines: &mut [Line<'static>]) {
        if call.duration.is_none()
            || call.is_unified_exec_interaction()
            || call.is_user_shell_command()
        {
            return;
        }
        let Some(last) = lines.last_mut() else {
            return;
        };
        if self.command_failed(call) {
            let exit_code = self
                .reported_exit_codes
                .get(&call.call_id)
                .copied()
                .unwrap_or_else(|| call.output.as_ref().map(|output| output.exit_code));
            last.push_span(match exit_code {
                Some(code) => format!(" (exit {code})").red(),
                None => " (failed)".red(),
            });
        }
        if let Some(duration) = call.duration.filter(|duration| duration.as_secs() >= 10) {
            last.push_span(format!(" · {}", format_duration(duration)).dim());
        }
    }

    pub(super) fn command_disclosure(&self, width: u16) -> Option<ActivityDisclosure> {
        let [call] = self.group.calls.as_slice() else {
            return None;
        };
        // Interaction output is intentionally absent from the expanded transcript.
        if !call.is_unified_exec_interaction()
            && let Some(output) = &call.output
        {
            let retained = output.line_counts().1;
            if Self::suppress_completed_output(call) && retained > 0 {
                return Some(ActivityDisclosure::OutputLines(retained));
            }
            let clipped = output
                .lines()
                .rev()
                .take(DETAIL_PREVIEW_LINES)
                .filter(|raw| ansi_escape_line(raw.as_ref()).width() + 4 > usize::from(width))
                .count();
            let hidden = retained.saturating_sub(DETAIL_PREVIEW_LINES) + clipped;
            if hidden > 0 {
                return Some(ActivityDisclosure::OutputLines(hidden));
            }
        }
        if !self
            .group
            .details
            .lines_after(/*after_calls*/ 1, width, HistoryRenderMode::Rich)
            .is_empty()
        {
            return Some(ActivityDisclosure::Generic);
        }
        call.output
            .as_ref()
            .is_some_and(|output| {
                output.line_counts().1 > DETAIL_PREVIEW_LINES
                    || output.transcript_lines().any(|line| {
                        ansi_escape_line(line.as_ref()).width() + 4 > usize::from(width)
                    })
            })
            .then_some(ActivityDisclosure::Generic)
    }

    pub(super) fn compact_command_lines(&self, width: u16) -> Vec<HyperlinkLine> {
        let [call] = self.group.calls.as_slice() else {
            return Vec::new();
        };
        if call.is_unified_exec_interaction()
            && !self.is_active()
            && call.output.as_ref().is_some_and(|output| {
                output.exit_code == 0 && output.lines().all(|line| line.trim().is_empty())
            })
        {
            return Vec::new();
        }
        let marker = if self.command_failed(call) {
            "•".red().bold()
        } else if self.is_active() {
            activity_indicator(
                call.start_time,
                MotionMode::from_animations_enabled(self.animations_enabled()),
                ReducedMotionIndicator::StaticBullet,
            )
            .unwrap_or_else(|| "•".dim())
        } else {
            "•".green().bold()
        };
        let title = if self.is_active() {
            "Running".to_owned()
        } else if call.is_unified_exec_interaction() {
            "Background output".to_owned()
        } else {
            "Ran".to_owned()
        };
        let script = self.original_command(call);
        let mut command_lines = command_highlights(&script);
        self.append_command_metadata(call, &mut command_lines);
        let mut lines = adaptive_wrap_hyperlink_lines(
            &plain_hyperlink_lines(command_lines),
            RtOptions::new(usize::from(width).max(1))
                .initial_indent(Line::from(vec![
                    marker,
                    " ".into(),
                    title.bold(),
                    " ".into(),
                ]))
                .subsequent_indent("  │ ".dim().into()),
        );
        if Self::suppress_completed_output(call) {
            return lines;
        }
        if let Some(output) = &call.output {
            let tail: Vec<_> = output.lines().rev().take(DETAIL_PREVIEW_LINES).collect();
            for (index, raw) in tail.into_iter().rev().enumerate() {
                let mut line = ansi_escape_line(raw.as_ref());
                line.spans.insert(
                    /*index*/ 0,
                    if index == 0 { "  └ " } else { "    " }.dim(),
                );
                line.spans.iter_mut().for_each(|span| {
                    span.style = span.style.add_modifier(Modifier::DIM);
                });
                lines.push(clipped_line(line, width));
            }
            if lines.len() == 1 && !call.is_unified_exec_interaction() {
                lines.push(clipped_line(Line::from("  └ (no output)".dim()), width));
            }
        }
        lines
    }
}
