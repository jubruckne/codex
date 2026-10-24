//! Read-only context inspector using the same scrolling, tabbed picker as /config.
use super::*;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::{
    ColumnWidthMode, ListSelectionView, PickerSurface, SearchMode, SelectionDescriptionLayout,
    SelectionItem, SelectionRowDisplay, SelectionTab, SelectionViewParams, footer_hint_items_line,
};
use crate::keymap::{ListAction, ListKeymap};
use crate::render::renderable::Renderable;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Paragraph, Widget},
};

struct Header(Vec<Line<'static>>);
impl Renderable for Header {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(word_wrap_lines(&self.0, area.width as usize)).render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        word_wrap_lines(&self.0, width.max(1) as usize).len() as u16
    }
}

fn item(name: &str, description: &str) -> SelectionItem {
    let clean = |text: &str| {
        text.chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect::<String>()
    };
    let name = clean(name);
    let description = clean(description);
    SelectionItem {
        name: name.to_string(),
        description: Some(format!("{name} · {description}")),
        category_tag: (!description.is_empty()).then(|| description.to_string()),
        selected_description: Some(format!("{name}\n{description}")),
        search_value: Some(format!("{name} {description}")),
        dismiss_on_select: false,
        ..Default::default()
    }
}

fn panel_item(tab: &str, name: &str, description: &str, cwd: &Path) -> SelectionItem {
    let mut row = item(name, description);
    if tab == "Reads" && cwd.is_absolute() && Path::new(name).is_absolute() {
        if let Ok(relative) = Path::new(name).strip_prefix(cwd) {
            if !relative.as_os_str().is_empty() {
                let relative = relative.to_string_lossy();
                let shortened = item(&relative, description);
                row.name = shortened.name;
                // Full paths remain available in the selected details and global filter.
                row.search_value = Some(format!(
                    "{} {}",
                    row.search_value.unwrap_or_default(),
                    row.name
                ));
            }
        }
    }
    row
}

impl ContextSummaryCell {
    pub(crate) fn into_panel(
        self,
        tx: AppEventSender,
        keymap: ListKeymap,
        cwd: &Path,
    ) -> ListSelectionView {
        let mut rows = self
            .summary
            .rows
            .iter()
            .zip(Kind::ALL)
            .filter(|(row, _)| row.items > 0)
            .map(|(row, kind)| {
                (
                    kind.label().to_string(),
                    format!(
                        "{} items · {} tokens",
                        count_label(row.items),
                        token_label(row.tokens, row.unknown)
                    ),
                )
            })
            .collect::<Vec<_>>();
        let item_lines = self.item_lines(true);
        let total_start = item_lines
            .iter()
            .position(|line| line.to_string().starts_with("Estimated total:"))
            .unwrap_or(item_lines.len());
        rows.extend(
            item_lines[total_start..]
                .iter()
                .filter(|line| !line.to_string().is_empty())
                .map(|line| (line.to_string(), String::new())),
        );
        let sections = [
            ("Item type", "Persisted context estimate", rows),
            (
                "Tool results",
                "Operation totals and five largest results; attribution is approximate.",
                self.summary
                    .tool_results
                    .lines(true)
                    .into_iter()
                    .filter(|line| !line.to_string().is_empty())
                    .map(|line| (line.to_string(), String::new()))
                    .collect(),
            ),
            (
                "Searches",
                "All de-duplicated literal searches, sorted by accumulated estimated tokens.",
                self.summary.tool_results.detail_rows(false),
            ),
            (
                "Reads",
                "All identified files, sorted by accumulated estimated tokens. Counts are individual read commands.",
                self.summary.tool_results.detail_rows(true),
            ),
        ];
        let search = crate::key_hint::plain(KeyCode::Char('/'));
        let (code, modifiers) = search.parts();
        let event = KeyEvent::new(code, modifiers);
        let search_enabled = keymap.action_for(event).is_none() && !keymap.has_chord_prefix(event);
        let mut hints = [
            (ListAction::MoveLeft, "previous tab"),
            (ListAction::MoveRight, "next tab"),
            (ListAction::MoveUp, "up"),
            (ListAction::MoveDown, "down"),
            (ListAction::Cancel, "clear search/close"),
        ]
        .into_iter()
        .filter_map(|(action, label)| {
            keymap
                .primary_hint(action)
                .map(|key| (key.display_label(), label.into()))
        })
        .collect::<Vec<_>>();
        if search_enabled {
            hints.push((search.display_label(), "search".into()));
        }
        let search_items = sections
            .iter()
            .flat_map(|(title, _, rows)| {
                rows.iter().map(move |(name, description)| {
                    let mut row = panel_item(title, name, description, cwd);
                    row.category_tag = Some(format!("{title} {}", description));
                    row
                })
            })
            .collect();
        let tabs = sections.into_iter().map(|(title, subtitle, rows)| SelectionTab {
            id: title.to_string(),
            label: title.to_string(),
            header: Box::new(Header(vec!["Persisted context estimate".bold().into(), subtitle.to_string().into(), "Estimates use roughly 4 bytes/token. Mixed output shares use command counts; counts describe recorded calls, not confirmed execution.".into()])),
            items: if rows.is_empty() { vec![item("No entries in the saved context", "")] } else { rows.iter().map(|(name, description)| panel_item(title, name, description, cwd)).collect() },
        }).collect();
        ListSelectionView::new(
            SelectionViewParams {
                view_id: Some("context-summary"),
                picker_surface: PickerSurface::Panel,
                max_visible_rows: 18,
                reserve_result_rows: false,
                allow_input_when_disconnected: true,
                row_display: SelectionRowDisplay::SingleLine,
                col_width_mode: ColumnWidthMode::AutoAllRows,
                description_layout: SelectionDescriptionLayout::SelectedBelowWhenNarrow {
                    min_description_width: u16::MAX,
                },
                footer_hint: Some(footer_hint_items_line(&hints)),
                is_searchable: search_enabled,
                search_mode: SearchMode::OnDemand(search),
                search_placeholder: Some("Search context details".to_string()),
                search_items,
                tabs,
                ..Default::default()
            },
            tx,
            keymap,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bottom_pane::BottomPaneView;
    use crate::keymap::RuntimeKeymap;
    use crossterm::event::KeyModifiers;
    use serde_json::json;

    fn panel() -> ListSelectionView {
        let mut history = Vec::new();
        for i in 0..25 {
            let call: ResponseItem = serde_json::from_value(json!({"type":"function_call","name":"exec_command","call_id":format!("{i}"),"arguments":json!({"cmd":format!("cat file{i:02}; rg TODO{i:02} src"), "workdir":"/repo"}).to_string()})).unwrap();
            let output: ResponseItem = serde_json::from_value(json!({"type":"function_call_output","call_id":format!("{i}"),"output":"x".repeat((i + 1) * 8)})).unwrap();
            history.extend([
                ResponseItemEnvelope::from(call),
                ResponseItemEnvelope::from(output),
            ]);
        }
        let summary = summarize(&history, None);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ContextSummaryCell {
            summary,
            usage: None,
        }
        .into_panel(
            AppEventSender::new(tx),
            RuntimeKeymap::defaults().list,
            Path::new("/repo"),
        )
    }

    fn key(pane: &mut ListSelectionView, code: KeyCode) {
        pane.handle_key_event(KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn render(pane: &ListSelectionView, width: u16) -> String {
        let area = Rect::new(0, 0, width, pane.desired_height(width));
        let mut buffer = Buffer::empty(area);
        pane.render(area, &mut buffer);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn context_summary_panel_tabs_scroll_all_entries_and_show_counts() {
        let mut pane = panel();
        assert_eq!(pane.active_tab_id().as_deref(), Some("Item type"));
        for title in ["Tool results", "Searches", "Reads"] {
            key(&mut pane, KeyCode::Right);
            assert_eq!(pane.active_tab_id().as_deref(), Some(title));
        }
        for _ in 0..24 {
            key(&mut pane, KeyCode::Down);
        }
        assert_eq!(pane.selected_index(), Some(24));
        let wide = render(&pane, 120);
        assert!(wide.contains("/repo/file00"), "{wide}");
        assert!(wide.contains("1 reads"), "{wide}");
        let narrow = render(&pane, 40);
        assert!(narrow.contains("/repo/file00"), "{narrow}");
        assert!(narrow.contains("tokens"), "{narrow}");
        key(&mut pane, KeyCode::Enter);
        assert!(!pane.is_complete());
        key(&mut pane, KeyCode::Esc);
        assert!(pane.is_complete());
    }

    #[test]
    fn context_summary_panel_global_search_finds_entries_beyond_visible_rows() {
        let mut pane = panel();
        key(&mut pane, KeyCode::Char('/'));
        for ch in "TODO00".chars() {
            key(&mut pane, KeyCode::Char(ch));
        }
        let rendered = render(&pane, 100);
        assert!(rendered.contains("TODO00"), "{rendered}");
        assert!(rendered.contains("1 searches"), "{rendered}");
        assert!(!rendered.contains("TODO24"), "{rendered}");
    }
}
