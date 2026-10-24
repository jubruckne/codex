//! Composer-owned presentation for the existing row above fullscreen input.
//! Query state stays with its editor; the app paints this packet and its cursor in one rectangle.

use super::*;
use unicode_width::UnicodeWidthStr;

impl ChatComposer {
    pub(crate) fn composer_gap_footer(
        &self,
        width: u16,
        _warning_count: usize,
    ) -> Option<TranscriptFooter> {
        if width == 0 {
            return None;
        }
        let area = Rect::new(0, 0, width, 1);
        if let Some(query) = self.draft.textarea.vim_query() {
            let mut buffer = Buffer::empty(area);
            query.render(area, &mut buffer);
            let cursor_column = query.cursor_pos(area).map(|(column, _)| column);
            let mut spans = Vec::new();
            let mut column = 0;
            while column < width {
                let cell = &buffer[(column, 0)];
                spans.push(Span::styled(cell.symbol().to_owned(), cell.style()));
                column += cell.symbol().width().max(1) as u16;
            }
            return Some(TranscriptFooter {
                text: Line::from(spans).into(),
                cursor_column,
                is_interactive: true,
            });
        }
        if let Some(query) = self.history_search_gap_footer(width) {
            return Some(query);
        }
        if self.shortcut_overlay_visible() {
            return Some(TranscriptFooter {
                text: super::super::shortcut_overlay::close_hint(&self.footer_props(), width)
                    .into(),
                cursor_column: None,
                is_interactive: false,
            });
        }
        let line = if self.footer.flash_visible() {
            self.footer.flash.as_ref()?.line.clone()
        } else if let Some(items) = &self.footer.hint_override {
            if items.is_empty() {
                return None;
            }
            let mut buffer = Buffer::empty(Rect::new(0, 0, width.saturating_add(2), 1));
            let area = buffer.area;
            render_footer_hint_items(area, &mut buffer, items);
            let text: String = (2..area.width).map(|x| buffer[(x, 0)].symbol()).collect();
            Line::from(text)
        } else {
            return None;
        };
        Some(TranscriptFooter {
            text: truncate_line_with_ellipsis_if_overflow(line, usize::from(width)).into(),
            cursor_column: None,
            is_interactive: false,
        })
    }

    pub(crate) fn composer_gap_warning(
        &self,
        width: u16,
        warning_count: usize,
    ) -> Option<Line<'static>> {
        (width > 0 && warning_count > 0).then(|| self.warning_notice(warning_count, width))
    }

    /// The app supplies only the rectangle of an actually painted warning badge.
    pub(crate) fn set_composer_gap_warning_area(&self, area: Option<Rect>, warning_count: usize) {
        self.footer
            .warning_notice_area
            .set(area.filter(|_| warning_count > 0));
    }
}
