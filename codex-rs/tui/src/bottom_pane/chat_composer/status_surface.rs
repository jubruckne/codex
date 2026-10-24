//! Keep persistent model/context status below input independently of contextual presentation.
//! Fullscreen context uses the existing composer gap above input and reserves no final hint row.
//! Measurement and rendering share the persistent status rectangle; shortcut help grows above
//! input and uses the gap for its close action without adding a contextual row below status.

use super::*;

impl ChatComposer {
    /// Budget for numbered agent destinations after reserving configured status and the
    /// same compact right-side indicator used by the footer's width fallback.
    pub(crate) fn agent_navigation_width(&self, width: u16) -> usize {
        let area = Rect::new(0, 0, width, 1);
        let right = if let Some(label) = self.footer.side_conversation_context_label.as_ref() {
            Some(side_conversation_context_line(label))
        } else if let Some(line) = self.shell_mode_footer_line() {
            Some(line)
        } else if self.footer.status_line_enabled {
            self.mode_indicator_line(/*show_cycle_hint*/ false)
        } else {
            Some(self.right_footer_line_with_context())
        };
        let right_width = right.as_ref().map_or(0, |line| line.width() as u16);
        let left_width = if right_width == 0 {
            inset_footer_hint_area(area).width
        } else {
            max_left_width_for_right(area, right_width)
                .unwrap_or_else(|| inset_footer_hint_area(area).width)
        } as usize;
        let status_width = if self.footer.status_line_enabled {
            self.footer.status_line_value.as_ref().map_or(0, |line| {
                if line.width() == 0 {
                    0
                } else {
                    line.width() + 3
                }
            })
        } else {
            0
        };
        // Reserve root even when configured status consumes the row. Rendering shortens status
        // ahead of the trailing strip; only extremely narrow terminals clip root itself.
        left_width
            .saturating_sub(status_width)
            .max(8)
            .min(left_width)
    }

    pub(super) fn status_surface_height(&self, options: ComposerRenderOptions<'_>) -> u16 {
        u16::from(
            options.separate_status_line
                && (self.footer.status_line_enabled || options.composer_gap.is_some()),
        )
    }

    pub(super) fn hint_footer_props(&self, options: ComposerRenderOptions<'_>) -> FooterProps {
        let mut props = self.footer_props();
        if self.status_surface_height(options) > 0 {
            props.status_line_enabled = false;
            props.status_line_value = None;
            props.active_agent_label = None;
        }
        props
    }

    pub(super) fn render_status_surface(
        &self,
        area: Rect,
        buf: &mut Buffer,
        options: ComposerRenderOptions<'_>,
    ) {
        if area.is_empty() {
            return;
        }
        let mut props = self.footer_props();
        let show_cycle_hint = !props.is_task_running
            && !self.blocks_direct_input
            && self.draft.input_enabled
            && !self.popup_active()
            && matches!(
                props.mode,
                FooterMode::ComposerEmpty | FooterMode::ComposerHasDraft
            )
            && options.footer.is_none_or(|footer| !footer.is_interactive);
        // Without a configured status command, retain the existing context/mode indicators
        // on the persistent surface rather than removing them with the contextual row.
        if !self.footer.status_line_enabled {
            let right = if let Some(label) = self.footer.side_conversation_context_label.as_ref() {
                side_conversation_context_line(label)
            } else if let Some(line) = self.shell_mode_footer_line() {
                line
            } else {
                self.right_footer_line_with_context()
            };
            if let Some(left) = super::super::footer::mode_indicator_line(
                self.footer.collaboration_mode_indicator,
                show_cycle_hint,
            ) && can_show_left_with_context(area, left.width() as u16, right.width() as u16)
            {
                render_footer_line(area, buf, left);
            }
            render_context_right(area, buf, &right);
            return;
        }
        // Persistent context is independent of help, queue, search, and quit modes.
        props.mode = FooterMode::ComposerHasDraft;
        props.is_task_running = false;
        let line = passive_footer_status_line(&props);
        let full = self.mode_indicator_line(show_cycle_hint);
        let left_width = line
            .as_ref()
            .map_or(/*default*/ 0, |line| line.width() as u16);
        let full_width = full
            .as_ref()
            .map_or(/*default*/ 0, |line| line.width() as u16);
        let right = if full_width <= inset_footer_hint_area(area).width
            && can_show_left_with_context(area, left_width, full_width)
        {
            full
        } else {
            self.mode_indicator_line(/*show_cycle_hint*/ false)
        };
        let right_width = right
            .as_ref()
            .map_or(/*default*/ 0, |line| line.width() as u16);
        let width = max_left_width_for_right(area, right_width)
            .unwrap_or_else(|| inset_footer_hint_area(area).width);
        let transition = self
            .effort_status_line_transition
            .as_ref()
            .filter(|transition| !transition.is_finished());
        let line = if let Some(transition) = transition {
            transition.render_line(line.as_ref(), width)
        } else {
            line
        };
        if let Some(line) = line {
            render_footer_line(
                area,
                buf,
                truncate_status_line_preserving_agents(
                    line,
                    usize::from(width),
                    props.active_agent_label.as_ref(),
                ),
            );
        }
        if let Some(right) = right {
            render_context_right(area, buf, &right);
        }
        if let Some(url) = self.footer.status_line_hyperlink_url.as_deref() {
            mark_underlined_hyperlink(buf, area, url);
        }
        if transition.is_some()
            && let Some(frame_requester) = &self.frame_requester
        {
            frame_requester.schedule_frame_in(EFFORT_STATUS_LINE_FRAME_TICK);
        }
    }
}

#[cfg(test)]
#[path = "status_surface_tests.rs"]
mod tests;
