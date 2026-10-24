//! One contextual row above the draft. Input ownership is independent of instruction text.
use super::*;
use crate::bottom_pane::TranscriptFooter;
use crate::footer_hint::first_fitting_line;
use crate::motion::MotionMode;

impl TranscriptView {
    pub(crate) fn gap_footer(
        &self,
        width: u16,
        motion: MotionMode,
        latest: &str,
    ) -> Option<TranscriptFooter> {
        if width == 0 {
            return None;
        }
        if self.is_search_editing() {
            // Keep room for the Find prefix, a useful query viewport and its caret;
            // the status separator also consumes three columns.
            let max_status = width.saturating_sub(19).min(width / 2).min(48);
            let status = self.search.gap_status(max_status, self.history);
            let reserved = if status.width() > 0 {
                status.width() as u16 + 3
            } else {
                0
            };
            let (mut query, cursor) = self.search_footer(width.saturating_sub(reserved).max(1))?;
            if reserved > 0 {
                query.push_span(" · ".dim());
                query.spans.extend(status.spans);
            }
            return Some(TranscriptFooter {
                text: query.into(),
                cursor_column: self.selection.is_none().then_some(cursor),
                is_interactive: true,
            });
        }
        if let Some(mode) = &self.copy_mode {
            let line = first_fitting_line(
                [
                    format!(
                        "Copy {} · ↑/↓ select · enter copy · esc close",
                        mode.label.to_lowercase()
                    ),
                    format!("Copy {} · enter copy · esc", mode.label.to_lowercase()),
                    "Copy · enter · esc".into(),
                ]
                .map(|line| crate::transcript_view::footer::navigation_line(&line)),
                width,
            );
            return Some(TranscriptFooter {
                text: line.into(),
                cursor_column: None,
                is_interactive: true,
            });
        }
        let pending = self.is_loading_history() || self.history == TranscriptHistoryState::Failed;
        if let Some(selection) = self.selection.as_ref() {
            let selected = (selection.start.key, selection.start.offset)
                != (selection.end.key, selection.end.offset);
            if !selected && !pending {
                return None;
            }
            // A mouse-down anchor alone leaves the draft cursor unchanged.
            return Some(TranscriptFooter {
                text: if pending {
                    self.status_line_with_navigation(latest, motion).into()
                } else {
                    Default::default()
                },
                cursor_column: None,
                is_interactive: selected,
            });
        }
        if self.search.is_reading() && !pending {
            return Some(TranscriptFooter {
                text: self.search.status_line(width, self.history).into(),
                cursor_column: None,
                is_interactive: false,
            });
        }
        if self.is_activity_focused() && !pending {
            return self.disclosure_footer(width);
        }
        if pending {
            let line = first_fitting_line(
                [
                    self.status_line_with_navigation(latest, motion),
                    if self.history == TranscriptHistoryState::Failed {
                        crate::transcript_view::footer::navigation_line(&format!(
                            "{} retry",
                            crate::key_hint::ctrl(crossterm::event::KeyCode::Home).display_label()
                        ))
                    } else {
                        self.status_line_with_navigation("", motion)
                    },
                    if self.history == TranscriptHistoryState::Failed {
                        crate::transcript_view::footer::navigation_line(
                            &crate::key_hint::ctrl(crossterm::event::KeyCode::Home).display_label(),
                        )
                    } else {
                        Line::from("Loading…")
                    },
                    crate::transcript_view::footer::navigation_line(
                        if self.history == TranscriptHistoryState::Failed {
                            "Retry history"
                        } else {
                            "Loading…"
                        },
                    ),
                ],
                width,
            );
            return Some(TranscriptFooter {
                text: line.into(),
                cursor_column: None,
                is_interactive: false,
            });
        }
        // The existing clickable FollowControl already owns new-activity/latest navigation.
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript_view::tests::{cell, render};
    #[test]
    fn shared_gap_search_is_one_row_with_a_visible_unicode_cursor() {
        let cells = vec![cell("source text")];
        let mut view = TranscriptView::default();
        render(&mut view, &cells, 80, 3);
        view.begin_search();
        view.paste_search(&"日本語 query ".repeat(30));
        for _ in 0..10 {
            view.advance_search(&cells);
        }
        for width in [1, 6, 12, 24, 80] {
            let packet = view
                .gap_footer(width, MotionMode::Reduced, "esc latest")
                .unwrap();
            assert_eq!(packet.text.lines.len(), 1);
            assert!(packet.cursor_column.unwrap() < width);
            assert!(packet.text.width() <= usize::from(width));
        }
        let packet = view
            .gap_footer(80, MotionMode::Reduced, "esc latest")
            .unwrap();
        assert!(packet.text.to_string().contains("No matches"));
    }
    #[test]
    fn shared_gap_mouse_selection_has_no_instruction_text_and_loading_still_reports_state() {
        let cells = vec![cell("selected content")];
        let mut view = TranscriptView::default();
        render(&mut view, &cells, 80, 3);
        view.begin_selection(&cells, 0, 0, 3);
        let packet = view
            .gap_footer(80, MotionMode::Reduced, "enter/esc latest")
            .unwrap();
        assert!(packet.is_interactive);
        assert_eq!(packet.text.width(), 0);
        view.history = TranscriptHistoryState::LoadingOlder;
        let packet = view
            .gap_footer(80, MotionMode::Reduced, "enter/esc latest")
            .unwrap();
        assert!(packet.text.to_string().contains("Loading earlier messages"));
        assert!(!packet.text.to_string().contains("copy"));
    }
    #[test]
    fn shared_gap_context_releases_old_follow_targets_and_places_query_caret_above_draft() {
        let cells = vec![cell("one\ntwo\nthree\nfour\nfive")];
        let mut view = TranscriptView::default();
        render(&mut view, &cells, 80, 2);
        view.scroll(&cells, -2);
        let all = Rect::new(0, 0, 80, 12);
        let gap = Rect::new(2, 9, 78, 1);
        let mut buf = Buffer::empty(all);
        view.render_composer_gap(Some(gap), None, &mut buf, std::time::Instant::now());
        view.begin_search();
        view.paste_search("four");
        let packet = view
            .gap_footer(gap.width, MotionMode::Reduced, "esc latest")
            .unwrap();
        let (_, cursor) = view.render_context_gap(
            Some(gap),
            None,
            Some(&packet),
            &mut buf,
            std::time::Instant::now(),
        );
        assert_eq!(cursor.unwrap().1, 9);
        assert!(cursor.unwrap().0 < 80);
        assert!(
            view.handle_follow_control_mouse(crossterm::event::MouseEvent {
                kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                column: 40,
                row: 9,
                modifiers: crossterm::event::KeyModifiers::NONE
            })
            .is_none()
        );
    }
}
