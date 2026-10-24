//! Grouped live tasks with shared identity colors and stable title, status and age columns.
//! Columns keep fixed proportions; metadata drops at narrow widths.
//! The final cell stays blank inside the full-width selection highlight.

use super::render::line;
use super::render::row;
use super::*;
use crate::bottom_pane::selection_style;

fn columns(area: Rect) -> (Rect, Rect, Rect) {
    let (title_width, activity_width, updated_width, gap) = if area.width >= 64 {
        // Split usable cells 45% / 45% / 10%, independent of row content.
        let usable = area.width - 4;
        let updated = usable / 10;
        let title = (usable - updated) / 2;
        (title, usable - updated - title, updated, 2)
    } else {
        let updated = if area.width >= 18 { 7 } else { 0 };
        (area.width - updated, 0, updated, 0)
    };
    let title = Rect::new(area.x, area.y, title_width, area.height);
    let activity = Rect::new(title.right() + gap, area.y, activity_width, area.height);
    let updated = Rect::new(area.right() - updated_width, area.y, updated_width, area.height);
    (title, activity, updated)
}

fn compact_age(reference: i64, timestamp: Option<i64>) -> String {
    let Some(timestamp) = timestamp else { return "—".into(); };
    let seconds = reference.saturating_sub(timestamp).max(0);
    match seconds {
        0..=4 => "now".into(),
        5..=59 => format!("{seconds}s"),
        60..=3599 => format!("{}m", seconds / 60),
        3600..=86399 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

impl AgentsOverviewView {
    pub(super) fn render_tree_rows(&self, area: Rect, buf: &mut Buffer) {
        let row_width = area.width;
        let area = Rect {
            width: row_width.saturating_sub(/*rhs*/ 1),
            ..area
        };
        if area.is_empty() {
            return;
        }
        let indices = self.selectable_indices();
        let mut state = self.state();
        if indices.is_empty() {
            line(
                if state.connection_notice.is_some() {
                    "Reconnecting…"
                } else if state.loading && self.rows.is_empty() {
                    "Loading tasks…"
                } else if state.refresh_failed {
                    "Could not load tasks"
                } else if self.rows.is_empty() {
                    "No tasks yet"
                } else {
                    "No matching tasks"
                }
                .dim(),
                area,
                buf,
            );
            return;
        }
        let entries = indices;
        let selected = entries.iter().position(|index| *index == self.selected).unwrap_or_default();
        let padding = u16::from(area.height >= 3);
        let viewport = row(area, padding, area.height - padding * 2);
        if padding > 0 {
            let (title, status, updated) = columns(row(area, /*offset*/ 0, /*height*/ 1));
            line("Task".dim(), title, buf);
            line("Current activity".dim(), status, buf);
            Line::from("Updated".dim())
                .right_aligned()
                .render(updated, buf);
        }
        let reference = chrono::Utc::now().timestamp();
        if let Some(requester) = &self.frame_requester {
            requester.schedule_frame_in(std::time::Duration::from_secs(1));
        }
        let height = usize::from(viewport.height);
        state.page_height = height;
        let mut start = state.scroll.min(entries.len().saturating_sub(height));
        if selected < start {
            start = selected;
        }
        if selected >= start + height {
            start = selected.saturating_add(/*rhs*/ 1).saturating_sub(height);
        }
        state.scroll = start;
        for (offset, entry) in entries.iter().skip(start).take(height).enumerate() {
            let index = *entry;
            let task = &self.rows[index];
            let rect = row(viewport, offset as u16, 1);
            let selected = index == self.selected;
            let inactive = task.details.completed || task.group == AgentsOverviewGroup::Finished;
            let style = if selected { selection_style() } else if inactive { Style::default().dim() } else { Style::default() };
            buf.set_style(Rect { width: row_width, ..rect }, style);
            let (_, mut indicator) = Self::tree_status(task);
            if task.group == AgentsOverviewGroup::Working {
                if let Some(requester) = &self.frame_requester {
                    indicator = crate::motion::loading_glyph(self.started_at,
                        crate::motion::MotionMode::from_animations_enabled(self.animations_enabled), requester).into();
                }
            }
            let (title, activity, updated) = columns(rect);
            let depth = self.hierarchy_depth(index);
            let indent = depth.saturating_mul(2).min(usize::from(title.width.saturating_sub(10)));
            let connector = if depth > 0 {
                format!("{}{} ", " ".repeat(indent.saturating_sub(2)),
                    if self.rows[index + 1..].iter().any(|row| row.thread.parent_thread_id == task.thread.parent_thread_id) { "├─" } else { "└─" })
            } else { String::new() };
            let name = if depth > 0 {
                task.details.agent_path.as_deref()
                    .unwrap_or_else(|| display_title(&task.thread))
            } else { display_title(&task.thread) };
            line(Line::from(vec![
                if selected { "› ".into() } else { "  ".into() },
                connector.into(), indicator, " ".into(), format!("{name}{}", if task.has_voice { "  voice" } else { "" }).into(),
            ]).style(style), title, buf);
            if self.current_tree(index) {
                line(Line::from(task.details.current_activity.clone().unwrap_or_default()).style(style), activity, buf);
            }
            if !updated.is_empty() {
                Line::from(compact_age(reference, task.details.updated_at)).style(style).right_aligned().render(updated, buf);
            }
        }
        if padding > 0 && start > 0 {
            line("↑".dim(), row(area, /*offset*/ 0, /*height*/ 1), buf);
        }
        if padding > 0 && start + height < entries.len() {
            line(
                "↓".dim(),
                row(
                    area,
                    area.height.saturating_sub(/*rhs*/ 1),
                    /*height*/ 1,
                ),
                buf,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_overview_age_and_column_priority() {
        for (age, expected) in [(0, "now"), (12, "12s"), (120, "2m"), (3600, "1h"), (259200, "3d")] {
            assert_eq!(compact_age(300000, Some(300000 - age)), expected);
        }
        assert_eq!(compact_age(300000, None), "—");
        let (task, activity, updated) = columns(Rect::new(0, 0, 30, 1));
        assert!(task.width >= 20);
        assert_eq!(activity.width, 0);
        assert_eq!(updated.width, 7);
        let (task, activity, updated) = columns(Rect::new(0, 0, 184, 1));
        assert_eq!((task.width, activity.width, updated.width), (81, 81, 18));
        for width in [0, 12, 30, 63, 64, 80, 175, 180, 300, u16::MAX] {
            let area = Rect::new(0, 0, width, 1);
            let (task, activity, updated) = columns(area);
            assert!(task.right() <= activity.x && activity.right() <= updated.x);
            assert_eq!(updated.right(), area.right());
            if width >= 64 {
                assert!(activity.width.abs_diff(task.width) <= 1);
                assert_eq!(task.width + activity.width + updated.width + 4, width);
            }
        }
    }
}
