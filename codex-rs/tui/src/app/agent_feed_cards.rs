//! Latest-state floating agent windows. Polling and local inference are not agent activity.
use super::*;
use crate::app::app_server_event_targets::{
    ServerNotificationThreadTarget, server_notification_thread_target,
};
use codex_app_server_protocol::{ThreadActiveFlag, ThreadStatus};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, BorderType};
use sha2::{Digest, Sha256};

pub(super) const CARD_HEIGHT: u16 = 11;
const IDLE_SECONDS: i64 = 10 * 60;

pub(super) struct CardState {
    pub(super) status: ThreadStatus,
    last_activity: Option<i64>,
    started_in_session: bool,
    admitted: std::cell::Cell<bool>,
    pub(super) removed: bool,
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
            started_in_session: false,
            admitted: std::cell::Cell::new(false),
            removed: false,
            metadata_at: -1,
            event_revision: 0,
            closed: false,
            tokens: None,
        }
    }
}
impl CardState {
    pub(super) fn metadata(&mut self, thread: &Thread, force: bool, session_started_at: i64) {
        self.started_in_session |= thread.created_at > 0 && thread.created_at >= session_started_at;
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
    fn inactive(&self, now: i64) -> bool {
        (self.closed || !matches!(self.status, ThreadStatus::Active { .. }))
            && self
                .last_activity
                .is_some_and(|time| now.saturating_sub(time) > IDLE_SECONDS)
    }

    fn visible(&self, _now: i64) -> bool {
        if self.removed || (self.closed && !self.started_in_session) {
            return false;
        }
        // Historical agents enter the view only while actually generating. Once
        // admitted, they remain available during this TUI run as before.
        if self.started_in_session
            || matches!(&self.status, ThreadStatus::Active { active_flags } if active_flags.is_empty())
        {
            self.admitted.set(true);
        }
        self.admitted.get()
    }
    pub(super) fn label(&self) -> &'static str {
        if self.closed {
            return "Inactive";
        }
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
            ThreadStatus::NotLoaded => "Inactive",
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
            .is_some_and(|state| state.visible(now))
    }
    #[cfg(test)]
    pub(super) fn render_agent_card(
        &self,
        agent: ThreadId,
        card: Rect,
        collapsed: bool,
        buf: &mut Buffer,
        cwd: &Path,
    ) {
        self.render_agent_card_with_activity(agent, card, collapsed, buf, cwd, None);
    }

    /// The borders carry state and usage; every interior row is available to the summary.
    pub(super) fn render_agent_card_with_activity(
        &self,
        agent: ThreadId,
        card: Rect,
        collapsed: bool,
        buf: &mut Buffer,
        cwd: &Path,
        activity: Option<&'static str>,
    ) {
        let mut card = card.intersection(buf.area);
        if collapsed {
            card.height = card.height.min(3);
        }
        if card.is_empty() {
            return;
        }
        ratatui::widgets::Clear.render(card, buf);
        let state = self.cards.get(&agent);
        let accent = if state.is_some_and(|state| state.inactive(chrono::Utc::now().timestamp())) {
            Color::DarkGray
        } else {
            color(agent)
        };
        let label = state.map(CardState::label).unwrap_or("Unknown");
        let working = activity.unwrap_or("◌");
        let glyph = match label {
            "Working" => working,
            "Awaiting approval" => "?",
            "Awaiting input" => "◇",
            "Idle" | "Inactive" => "○",
            "Error" => "!",
            "Closed" => "×",
            _ => "·",
        };
        let usage = (!collapsed)
            .then(|| state.and_then(|state| state.tokens))
            .flatten()
            .map(|(input, output)| {
                Line::from(format!(
                    " ↑ {} · ↓ {} ",
                    crate::token_usage::compact_turn_tokens(input),
                    crate::token_usage::compact_turn_tokens(output)
                ))
                .fg(accent)
            })
            .filter(|line| line.width() <= usize::from(card.width.saturating_sub(4)));
        let usage_width = usage.as_ref().map_or(0, |line| line.width() as u16);
        let mut border = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent));
        if !collapsed && self.notice.is_some() {
            let notice = if card.width.saturating_sub(usage_width + 4) >= 17 {
                " Partial history "
            } else {
                " ! "
            };
            if card.width.saturating_sub(usage_width + 4) >= notice.len() as u16 {
                border = border.title_bottom(Line::from(notice).dim());
            }
        }
        border.render(card, buf);
        if let Some(usage) = usage {
            usage.render(
                Rect::new(
                    card.right() - usage_width - 2,
                    card.bottom() - 1,
                    usage_width,
                    1,
                ),
                buf,
            );
        }
        if card.width >= 5 {
            let agent_name = self
                .agents
                .get(&agent)
                .map(String::as_str)
                .unwrap_or("Unknown");
            let basename = agent_name
                .rsplit('/')
                .find(|part| !part.is_empty())
                .unwrap_or(agent_name);
            let title_width = card.width.saturating_sub(6);
            if title_width > 0 {
                crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                    Line::from(format!(" {glyph} {basename} "))
                        .fg(accent)
                        .bold(),
                    usize::from(title_width),
                )
                .render(Rect::new(card.x + 1, card.y, title_width, 1), buf);
            }
            Line::from(if collapsed { " ▼ " } else { " ▲ " })
                .fg(accent)
                .bold()
                .render(Rect::new(card.right() - 4, card.y, 3, 1), buf);
        }
        if card.height < 3 || card.width < 3 {
            return;
        }
        let inset = if card.width >= 5 { 2 } else { 1 };
        let width = card.width.saturating_sub(inset * 2);
        if collapsed {
            crate::line_truncation::truncate_line_with_ellipsis_if_overflow(
                Line::from(if self.semantic {
                    self.captions.get(&agent).cloned().unwrap_or_default()
                } else {
                    label.to_owned()
                })
                .fg(accent),
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
                        summary.text.lines().map(|line| Line::from(line.to_owned())),
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
        let rows = usize::from(card.height.saturating_sub(2));
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
                card.y + 1,
                width,
                card.height.saturating_sub(2),
            ),
            buf,
        );
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
        let session_started_at = *self.agent_feed.session_started_at.get_or_insert(now);
        let mut changed = false;
        for feed in self
            .agent_feed
            .roots
            .values_mut()
            .filter(|feed| feed.agents.contains_key(&agent))
        {
            let card = feed.cards.entry(agent).or_default();
            match notification {
                ServerNotification::ThreadClosed(_) => {
                    card.closed = true;
                    card.status = ThreadStatus::NotLoaded;
                }
                ServerNotification::ThreadArchived(_) | ServerNotification::ThreadDeleted(_) => {
                    card.removed = true;
                    card.closed = true;
                    card.tokens = None;
                }
                ServerNotification::ThreadStarted(event) => {
                    card.closed = false;
                    card.removed = false;
                    card.metadata(&event.thread, true, session_started_at);
                    card.tokens = None;
                    // Resuming an old thread is not new productive activity.
                    if card.started_in_session {
                        card.last_activity = Some(now);
                    }
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
