//! Latest-state floating agent windows. Polling and local inference are not agent activity.
use super::*;
use crate::app::app_server_event_targets::{
    ServerNotificationThreadTarget, server_notification_thread_target,
};
use codex_app_server_protocol::{ThreadActiveFlag, ThreadStatus};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, BorderType};
use sha2::{Digest, Sha256};

pub(super) const CARD_HEIGHT: u16 = 12;
const IDLE_SECONDS: i64 = 10 * 60;

pub(super) struct CardState {
    pub(super) status: ThreadStatus,
    last_activity: Option<i64>,
    metadata_at: i64,
    pub(super) event_revision: u64,
    pub(super) closed: bool,
    pub(super) tokens: Option<(i64, i64)>,
}
impl Default for CardState {
    fn default() -> Self {
        Self {
            status: ThreadStatus::Idle,
            last_activity: None,
            metadata_at: -1,
            event_revision: 0,
            closed: false,
            tokens: None,
        }
    }
}
impl CardState {
    pub(super) fn metadata(&mut self, thread: &Thread, force: bool) {
        if force || thread.updated_at > self.metadata_at {
            self.status = thread.status.clone();
            self.metadata_at = self.metadata_at.max(thread.updated_at);
            if thread.updated_at > 0 {
                self.last_activity = self
                    .last_activity
                    .into_iter()
                    .chain(Some(thread.updated_at))
                    .max();
            }
        }
    }
    fn visible(&self, now: i64) -> bool {
        !self.closed
            && !matches!(self.status, ThreadStatus::NotLoaded)
            && (matches!(self.status, ThreadStatus::Active { .. })
                || self
                    .last_activity
                    .is_none_or(|time| now.saturating_sub(time) <= IDLE_SECONDS))
    }
    fn label(&self) -> &'static str {
        match &self.status {
            ThreadStatus::Active { active_flags }
                if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval) =>
            {
                "Awaiting approval"
            }
            ThreadStatus::Active { active_flags }
                if active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput) =>
            {
                "Awaiting input"
            }
            ThreadStatus::Active { .. } => "Working",
            ThreadStatus::Idle => "Idle",
            ThreadStatus::SystemError => "Error",
            ThreadStatus::NotLoaded => "Closed",
        }
    }
}
fn color(agent: ThreadId) -> Color {
    const PALETTE: [(u8, u8, u8); 8] = [
        (137, 180, 250),
        (166, 227, 161),
        (203, 166, 247),
        (249, 226, 175),
        (250, 179, 135),
        (148, 226, 213),
        (245, 194, 231),
        (116, 199, 236),
    ];
    crate::terminal_palette::best_color(
        PALETTE[usize::from(Sha256::digest(agent.to_string())[0]) % PALETTE.len()],
    )
}
impl RootFeed {
    pub(super) fn card_visible(&self, agent: ThreadId, now: i64) -> bool {
        self.cards
            .get(&agent)
            .is_none_or(|state| state.visible(now))
    }
    /// Paint one floating window, clearing only the rectangle it covers.
    /// Window placement, stacking and pointer gestures belong to the window controller.
    pub(super) fn render_agent_card(
        &self,
        agent: ThreadId,
        card: Rect,
        collapsed: bool,
        buf: &mut Buffer,
        cwd: &Path,
    ) {
        let mut card = card.intersection(buf.area);
        if collapsed {
            card.height = card.height.min(3);
        }
        if card.is_empty() {
            return;
        }
        ratatui::widgets::Clear.render(card, buf);
        let accent = color(agent);
        let mut border = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent));
        if !collapsed && self.notice.is_some() {
            border = border.title_bottom(Line::from(" Partial history ").dim());
        }
        border.render(card, buf);
        if card.width >= 5 {
            let label = self
                .agents
                .get(&agent)
                .map(String::as_str)
                .unwrap_or("Unknown");
            let basename = label
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(label);
            let title_width = card.width.saturating_sub(7);
            if title_width > 0 {
                crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                    Line::from(format!(" {basename} ")).fg(accent).bold(),
                    usize::from(title_width),
                )
                .render(Rect::new(card.x + 1, card.y, title_width, 1), buf);
            }
            Line::from(if collapsed { "▼" } else { "▲" })
                .fg(accent)
                .bold()
                .render(Rect::new(card.right() - 3, card.y, 1, 1), buf);
        }
        if card.height < 3 || card.width < 3 {
            return;
        }
        let inset = if card.width >= 5 { 2 } else { 1 };
        let width = card.width.saturating_sub(inset * 2);
        let state = self.cards.get(&agent);
        let label = state.map(CardState::label).unwrap_or("Unknown");
        if collapsed {
            crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                Line::from(label).fg(accent),
                usize::from(width),
            )
            .render(Rect::new(card.x + inset, card.y + 1, width, 1), buf);
            return;
        }
        let summary = self
            .semantic
            .then(|| {
                self.bubbles
                    .iter()
                    .rev()
                    .find(|message| message.key.0 == agent)
            })
            .flatten();
        let mut lines = if let Some(summary) = summary {
            let mut cached = summary.rendered.borrow_mut();
            if cached
                .as_ref()
                .is_none_or(|(old, path, _)| *old != width || path != cwd)
            {
                *cached = Some((
                    width,
                    cwd.to_path_buf(),
                    crate::wrapping::word_wrap_lines(
                        [Line::from(summary.text.clone())],
                        usize::from(width),
                    ),
                ));
            }
            cached.as_ref().unwrap().2.clone()
        } else if !self.semantic {
            let mut lines = Vec::new();
            if let Some(text) = self
                .windows
                .get(&agent)
                .and_then(semantic::Window::fallback_text)
            {
                crate::markdown::append_markdown(
                    text,
                    Some(usize::from(width)),
                    Some(cwd),
                    &mut lines,
                );
            }
            lines
        } else {
            Vec::new()
        };
        if lines.is_empty() {
            lines.push(
                Line::from(if self.semantic {
                    "Waiting for a local summary…"
                } else {
                    "No response yet."
                })
                .dim(),
            );
        }
        let rows = usize::from(card.height.saturating_sub(5));
        if rows > 0 && lines.len() > rows {
            lines.truncate(rows);
            let last = lines.last_mut().unwrap();
            last.push_span(" …".dim());
            *last = crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                last.clone(),
                usize::from(width),
            );
        }
        Paragraph::new(lines).render(
            Rect::new(
                card.x + inset,
                card.y + 2,
                width,
                card.height.saturating_sub(5),
            ),
            buf,
        );
        let usage = state
            .and_then(|state| state.tokens)
            .map(|(input, output)| {
                format!(
                    "↑ {} ↓ {}",
                    crate::token_usage::compact_turn_tokens(input),
                    crate::token_usage::compact_turn_tokens(output)
                )
            })
            .unwrap_or_default();
        let available = usize::from(width);
        let usage_width = Line::from(usage.clone()).width();
        let mut footer = if usage_width > 0 && usage_width + 1 < available {
            let mut label = crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                Line::from(label),
                available - usage_width - 1,
            );
            let gap = available.saturating_sub(label.width() + usage_width);
            label.push_span(" ".repeat(gap));
            label.push_span(usage);
            label
        } else {
            crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                Line::from(label),
                available,
            )
        };
        footer = footer.fg(accent);
        footer.render(Rect::new(card.x + inset, card.bottom() - 2, width, 1), buf);
    }
}
impl App {
    /// Existing metadata/events only; no attachment or usage RPC is needed for cards.
    pub(in crate::app) fn observe_agent_cards(&mut self, notification: &ServerNotification) {
        let ServerNotificationThreadTarget::Thread(agent) =
            server_notification_thread_target(notification)
        else {
            return;
        };
        let now = chrono::Utc::now().timestamp();
        let mut changed = false;
        for feed in self
            .agent_feed
            .roots
            .values_mut()
            .filter(|feed| feed.agents.contains_key(&agent))
        {
            let card = feed.cards.entry(agent).or_default();
            match notification {
                ServerNotification::ThreadClosed(_)
                | ServerNotification::ThreadArchived(_)
                | ServerNotification::ThreadDeleted(_) => {
                    card.closed = true;
                    card.tokens = None;
                }
                ServerNotification::ThreadStarted(event) => {
                    card.closed = false;
                    card.metadata(&event.thread, true);
                    card.tokens = None;
                    card.last_activity = Some(now);
                }
                ServerNotification::ThreadStatusChanged(event) => {
                    if matches!(card.status, ThreadStatus::Active { .. })
                        && !matches!(event.status, ThreadStatus::Active { .. })
                    {
                        card.last_activity = Some(now);
                    }
                    if card.closed && !matches!(event.status, ThreadStatus::NotLoaded) {
                        card.last_activity = Some(now);
                    }
                    card.status = event.status.clone();
                    if !matches!(card.status, ThreadStatus::NotLoaded) {
                        card.closed = false;
                    }
                }
                ServerNotification::ThreadTokenUsageUpdated(event) => {
                    let total = &event.token_usage.total;
                    card.tokens = (total.input_tokens >= 0 && total.output_tokens >= 0)
                        .then_some((total.input_tokens, total.output_tokens));
                }
                ServerNotification::TurnStarted(_) => {
                    card.closed = false;
                    card.status = ThreadStatus::Active {
                        active_flags: Vec::new(),
                    };
                }
                ServerNotification::TurnCompleted(event) => {
                    card.status =
                        if event.turn.status == codex_app_server_protocol::TurnStatus::Failed {
                            ThreadStatus::SystemError
                        } else {
                            ThreadStatus::Idle
                        };
                }
                ServerNotification::ThreadReverted(_) => {
                    card.tokens = None;
                }
                ServerNotification::ItemStarted(_)
                | ServerNotification::ItemCompleted(_)
                | ServerNotification::AgentMessageDelta(_)
                | ServerNotification::ReasoningSummaryTextDelta(_)
                | ServerNotification::CommandExecutionOutputDelta(_)
                | ServerNotification::FileChangeOutputDelta(_) => {}
                _ => continue,
            }
            card.event_revision += 1;
            // State/usage bookkeeping is not itself evidence of productive activity.
            if matches!(
                notification,
                ServerNotification::TurnStarted(_)
                    | ServerNotification::TurnCompleted(_)
                    | ServerNotification::ItemStarted(_)
                    | ServerNotification::ItemCompleted(_)
                    | ServerNotification::AgentMessageDelta(_)
                    | ServerNotification::ReasoningSummaryTextDelta(_)
                    | ServerNotification::CommandExecutionOutputDelta(_)
                    | ServerNotification::FileChangeOutputDelta(_)
            ) {
                card.last_activity = Some(now);
            }
            changed = true;
        }
        if changed {
            self.chat_widget.frame_requester.schedule_frame();
        }
    }
}

#[cfg(test)]
#[path = "agent_feed_cards_tests.rs"]
mod tests;
