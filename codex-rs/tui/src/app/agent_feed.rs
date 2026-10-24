//! Read-only, bounded assistant-message feed for the current root's descendants.
#[path = "agent_feed_cards.rs"]
mod cards;
#[path = "agent_feed_io.rs"]
mod io;
#[path = "agent_feed_semantic.rs"]
pub(crate) mod semantic;
#[cfg(test)]
#[path = "agent_feed_tests.rs"]
mod tests;
#[path = "agent_feed_windows.rs"]
mod windows;

use super::*;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadItem;
use ratatui::buffer::Buffer;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

const MAX_MESSAGES: usize = 200;
const MAX_BYTES: usize = 1024 * 1024;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
struct MessageKey(ThreadId, String, String);

#[derive(Debug)]
struct Message {
    key: MessageKey,
    text: String,
    complete: bool,
    live: bool,
    reasoning: bool,
    // Byte ranges into text; there is no separately retained copy of streamed parts.
    parts: Vec<(i64, std::ops::Range<usize>)>,
    order: (i64, u64),
    truncated: bool,
    rendered: std::cell::RefCell<Option<(u16, PathBuf, Vec<Line<'static>>)>>,
}

#[derive(Default)]
pub(super) struct RootFeed {
    windows: HashMap<ThreadId, semantic::Window>,
    bubbles: Vec<Message>,
    published: HashMap<ThreadId, [u8; 32]>,
    semantic: bool,
    cards: HashMap<ThreadId, cards::CardState>,
    windows_ui: std::cell::RefCell<windows::Windows>,
    manual: Option<bool>,
    agents: HashMap<ThreadId, String>,
    messages: Vec<Message>,
    versions: HashMap<ThreadId, (i64, bool)>,
    notice: Option<String>,
    sequence: u64,
}

impl RootFeed {
    fn enabled(&self) -> bool {
        self.manual.unwrap_or_else(|| {
            let now = chrono::Utc::now().timestamp();
            self.agents.keys().any(|id| self.card_visible(*id, now))
        })
    }

    fn upsert(
        &mut self,
        key: MessageKey,
        text: &str,
        complete: bool,
        live: bool,
        timestamp: i64,
        append: bool,
    ) {
        self.upsert_kind(key, text, complete, live, timestamp, append, false);
    }

    fn upsert_kind(
        &mut self,
        key: MessageKey,
        text: &str,
        complete: bool,
        live: bool,
        timestamp: i64,
        append: bool,
        reasoning: bool,
    ) {
        let observed_key = key.clone();
        if !self.agents.contains_key(&key.0) {
            return;
        }
        if let Some(message) = self.messages.iter_mut().find(|message| message.key == key) {
            // History snapshots may race newer streaming text or a completed notification.
            if (!live && message.live && (message.complete || !complete))
                || (message.complete && !complete)
            {
                return;
            }
            if append {
                if !message.truncated {
                    message.text.push_str(text);
                }
            } else if !(reasoning && text.is_empty() && !message.text.is_empty()) {
                message.text = text.to_owned();
                message.truncated = false;
                message.parts.clear();
            }
            message.reasoning = reasoning;
            message.complete |= complete;
            message.live |= live;
            bound_message(message);
            *message.rendered.get_mut() = None;
        } else {
            self.sequence += 1;
            let mut message = Message {
                key,
                text: text.to_owned(),
                complete,
                live,
                reasoning,
                parts: Vec::new(),
                order: (timestamp, self.sequence),
                truncated: false,
                rendered: Default::default(),
            };
            bound_message(&mut message);
            self.messages.push(message);
            self.messages.sort_by_key(|message| message.order);
        }
        self.capture_summary_input(&observed_key, timestamp);
        let mut bytes: usize = self.messages.iter().map(|message| message.text.len()).sum();
        while self.messages.len() > MAX_MESSAGES || bytes > MAX_BYTES {
            bytes -= self.messages.remove(0).text.len();
        }
    }

    fn reasoning_summary(
        &mut self,
        key: MessageKey,
        summary: &[String],
        complete: bool,
        live: bool,
        timestamp: i64,
    ) {
        let text = summary
            .iter()
            .filter(|part| !part.is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("\n\n");
        self.upsert_kind(key.clone(), &text, complete, live, timestamp, false, true);
        if let Some(message) = self.messages.iter_mut().find(|message| message.key == key)
            && message.text == text
        {
            let mut offset = 0;
            message.parts = summary
                .iter()
                .enumerate()
                .filter_map(|(index, part)| {
                    if part.is_empty() {
                        return None;
                    }
                    let start = offset;
                    offset += part.len() + 2;
                    Some((index as i64, start..start + part.len()))
                })
                .collect();
        }
    }

    fn reasoning_delta(&mut self, key: MessageKey, index: i64, delta: &str, timestamp: i64) {
        // Empty part-added notifications must not create duplicate or trailing separators.
        if index < 0 || delta.is_empty() || !self.agents.contains_key(&key.0) {
            return;
        }
        if !self.messages.iter().any(|message| message.key == key) {
            self.upsert_kind(key.clone(), "", false, true, timestamp, false, true);
        }
        let Some(message) = self.messages.iter_mut().find(|message| message.key == key) else {
            return;
        };
        if message.complete || message.truncated {
            return;
        }
        let mut parts = message
            .parts
            .iter()
            .map(|(index, range)| (*index, &message.text[range.clone()]))
            .collect::<Vec<_>>();
        let position = parts.partition_point(|(part, _)| *part < index);
        if parts.get(position).is_none_or(|(part, _)| *part != index) {
            parts.insert(position, (index, ""));
        }
        let mut text = String::new();
        let mut ranges = Vec::new();
        for (part, existing) in parts {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            let start = text.len();
            text.push_str(existing);
            if part == index {
                text.push_str(delta);
            }
            ranges.push((part, start..text.len()));
        }
        message.text = text;
        message.parts = ranges;
        message.live = true;
        message.reasoning = true;
        bound_message(message);
        *message.rendered.get_mut() = None;
        // Apply the shared aggregate limit after growing a reasoning item.
        self.capture_summary_input(&key, timestamp);
        let mut bytes: usize = self.messages.iter().map(|message| message.text.len()).sum();
        while self.messages.len() > MAX_MESSAGES || bytes > MAX_BYTES {
            bytes -= self.messages.remove(0).text.len();
        }
    }

    fn capture_summary_input(&mut self, key: &MessageKey, timestamp: i64) {
        if let Some(message) = self.messages.iter().find(|message| &message.key == key) {
            self.windows
                .entry(key.0)
                .or_default()
                .observe_at(message, timestamp);
        }
    }
}

fn bound_message(message: &mut Message) {
    if message.text.len() > MAX_MESSAGE_BYTES {
        let mut end = MAX_MESSAGE_BYTES;
        while !message.text.is_char_boundary(end) {
            end -= 1;
        }
        message.text.truncate(end);
        message.truncated = true;
    }
    message.parts.retain_mut(|(_, range)| {
        range.end = range.end.min(message.text.len());
        range.start < range.end
    });
}

#[derive(Default)]
pub(super) struct AgentFeed {
    pub(super) roots: HashMap<ThreadId, RootFeed>,
    pub(super) root: Option<ThreadId>,
    generation: u64,
    task: Option<tokio::task::AbortHandle>,
    refreshed: Option<Instant>,
    full_refresh: Option<Instant>,
    dirty: bool,
    summarizer: Option<semantic::Client>,
    semantic_available: bool,
    card_versions: HashMap<ThreadId, u64>,
    session_started_at: Option<i64>,
}
impl Drop for AgentFeed {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Debug)]
pub(crate) struct FeedSnapshot {
    latest_turns: HashMap<ThreadId, String>,
    threads: Vec<Thread>,
    messages: Vec<(
        ThreadId,
        String,
        String,
        String,
        bool,
        i64,
        Option<Vec<String>>,
    )>,
    versions: HashMap<ThreadId, (i64, bool)>,
    errors: Vec<String>,
}

impl App {
    pub(super) fn sync_agent_feed_root(&mut self) {
        let session_started_at = *self
            .agent_feed
            .session_started_at
            .get_or_insert_with(|| chrono::Utc::now().timestamp());
        let root = self.agents_overview_root_id();
        if self.agent_feed.root != root {
            self.cancel_agent_window_drag();
            if let Some(task) = self.agent_feed.task.take() {
                task.abort();
            }
            self.agent_feed.root = root;
            self.agent_feed.generation += 1;
            self.agent_feed.refreshed = None;
            self.agent_feed.full_refresh = None;
            self.agent_feed.dirty = true;
        }
        if let Some(root) = root {
            let feed = self.agent_feed.roots.entry(root).or_default();
            feed.semantic = self.agent_feed.semantic_available;
            for (id, thread) in self
                .agents_overview
                .threads
                .iter()
                .filter_map(|(id, thread)| thread.as_ref().map(|thread| (*id, thread)))
            {
                if id != root && belongs_to_root(id, root, &self.agents_overview.threads) {
                    feed.agents.insert(id, agent_label(thread));
                    let card = feed.cards.entry(id).or_default();
                    card.metadata(thread, false, session_started_at);
                    if let Some(tokens) = self
                        .agents_overview
                        .usage
                        .get(&id)
                        .and_then(|usage| usage.tokens.as_ref())
                    {
                        card.tokens = (tokens.input_tokens >= 0 && tokens.output_tokens >= 0)
                            .then_some((tokens.input_tokens, tokens.output_tokens));
                    }
                    if self.agents_overview.removed_threads.contains(&id) {
                        card.removed = true;
                        card.closed = true;
                    }
                }
            }
        }
    }

    pub(super) fn agent_windows_enabled(&self, tui: &tui::Tui) -> bool {
        self.local_settings.tui.fullscreen_transcript
            && tui.is_owned_screen()
            && !self.chat_widget.has_active_view()
            && !self.chat_widget.is_external_writer_view()
            && self.chat_widget.no_modal_or_popup_active()
            && self.overlay.is_none()
            && self
                .agent_feed
                .root
                .and_then(|root| self.agent_feed.roots.get(&root))
                .is_some_and(RootFeed::enabled)
    }

    fn cancel_agent_window_drag(&self) {
        if let Some(feed) = self
            .agent_feed
            .root
            .and_then(|root| self.agent_feed.roots.get(&root))
        {
            feed.cancel_window_drag();
        }
    }

    /// Windows own pointer gestures before transcript/composer hit testing.
    pub(super) fn handle_agent_feed_event(&mut self, tui: &mut tui::Tui, event: &TuiEvent) -> bool {
        if matches!(
            event,
            TuiEvent::FocusLost | TuiEvent::Resume | TuiEvent::Resize(_)
        ) {
            self.cancel_agent_window_drag();
            return false;
        }
        if !self.agent_windows_enabled(tui) {
            self.cancel_agent_window_drag();
            return false;
        }
        let TuiEvent::Mouse(mouse) = event else {
            return false;
        };
        let handled = self
            .agent_feed
            .root
            .and_then(|root| self.agent_feed.roots.get(&root))
            .is_some_and(|feed| feed.window_mouse(mouse));
        if handled {
            self.transcript_view.end_drag();
            self.chat_widget.end_composer_drag();
            if mouse.kind != crossterm::event::MouseEventKind::Moved {
                tui.frame_requester().schedule_frame();
            }
        }
        handled
    }

    pub(super) fn toggle_agent_feed(&mut self, tui: &mut tui::Tui) {
        if !self.local_settings.tui.fullscreen_transcript || !tui.is_owned_screen() {
            self.chat_widget.add_info_message("The subagent feed requires tui.fullscreen_transcript=true and fullscreen rendering.".into(), None);
            return;
        }
        self.cancel_agent_window_drag();
        self.sync_agent_feed_root();
        if let Some(root) = self.agent_feed.root {
            let feed = self.agent_feed.roots.entry(root).or_default();
            feed.manual = Some(!feed.enabled());
            self.agent_feed.dirty = true;
            tui.frame_requester().schedule_frame();
        }
        self.refresh_agent_summaries();
    }

    pub(super) fn tick_agent_feed(&mut self, tui: &mut tui::Tui, server: &AppServerSession) {
        if !self.local_settings.tui.fullscreen_transcript || !tui.is_owned_screen() {
            return;
        }
        self.sync_agent_feed_root();
        self.refresh_agent_summaries();
        // A scheduled frame also refreshes roots whose children were started by another client.
        tui.frame_requester()
            .schedule_frame_in(Duration::from_secs(5));
        if self.is_offline() {
            return;
        }
        let Some(root) = self.agent_feed.root else {
            return;
        };
        if self.agent_feed.task.is_some()
            || (!self.agent_feed.dirty
                && self
                    .agent_feed
                    .refreshed
                    .is_some_and(|time| time.elapsed() < Duration::from_secs(5)))
        {
            return;
        }
        self.agent_feed.dirty = false;
        let feed = self.agent_feed.roots.entry(root).or_default();
        let force = self
            .agent_feed
            .full_refresh
            .is_none_or(|time| time.elapsed() >= Duration::from_secs(30));
        let versions = if force {
            self.agent_feed.full_refresh = Some(Instant::now());
            HashMap::new()
        } else {
            feed.versions.clone()
        };
        let read_messages = feed.manual != Some(false);
        let known: Vec<_> = feed
            .agents
            .keys()
            .filter_map(|id| self.agents_overview.threads.get(id).and_then(Clone::clone))
            .collect();
        self.agent_feed.card_versions = feed
            .cards
            .iter()
            .map(|(id, card)| (*id, card.event_revision))
            .collect();
        let generation = self.agent_feed.generation;
        let handle = server.request_handle();
        let tx = self.app_event_tx.clone();
        self.agent_feed.task = Some(
            tokio::spawn(async move {
                let snapshot = io::load(&handle, root, known, versions, read_messages).await;
                tx.send(AppEvent::AgentFeedReady {
                    root,
                    generation,
                    snapshot,
                });
            })
            .abort_handle(),
        );
    }

    pub(super) fn apply_agent_feed_snapshot(
        &mut self,
        root: ThreadId,
        generation: u64,
        snapshot: FeedSnapshot,
    ) {
        if self.agent_feed.root != Some(root) || self.agent_feed.generation != generation {
            return;
        }
        let session_started_at = *self
            .agent_feed
            .session_started_at
            .get_or_insert_with(|| chrono::Utc::now().timestamp());
        self.agent_feed.task = None;
        self.agent_feed.refreshed = Some(Instant::now());
        let feed = self.agent_feed.roots.entry(root).or_default();
        for thread in snapshot.threads {
            if let Ok(id) = ThreadId::from_string(&thread.id) {
                feed.agents.insert(id, agent_label(&thread));
                let card = feed.cards.entry(id).or_default();
                if card.event_revision
                    == self.agent_feed.card_versions.get(&id).copied().unwrap_or(0)
                {
                    card.metadata(&thread, true, session_started_at);
                }
                if self.agents_overview.removed_threads.contains(&id) {
                    card.removed = true;
                    card.closed = true;
                }
                self.agents_overview
                    .threads
                    .entry(id)
                    .or_insert(Some(thread));
            }
        }
        for (agent, turn) in snapshot.latest_turns {
            // A history request can finish after a newer live turn has started.
            feed.windows.entry(agent).or_default().history_turn(turn);
        }
        let mut chronology: HashMap<ThreadId, Vec<MessageKey>> = HashMap::new();
        for (id, turn, item, ..) in &snapshot.messages {
            chronology
                .entry(*id)
                .or_default()
                .push(MessageKey(*id, turn.clone(), item.clone()));
        }
        for agent in chronology.keys() {
            feed.windows.entry(*agent).or_default().begin_history();
        }
        for (id, turn, item, text, complete, time, reasoning) in snapshot.messages {
            if let Some(summary) = reasoning {
                feed.reasoning_summary(MessageKey(id, turn, item), &summary, complete, false, time);
            } else {
                feed.upsert(
                    MessageKey(id, turn, item),
                    &text,
                    complete,
                    false,
                    time,
                    false,
                );
            }
        }
        for (agent, keys) in chronology {
            feed.windows.entry(agent).or_default().finish_history(&keys);
        }
        feed.versions.extend(snapshot.versions);
        feed.notice = (!snapshot.errors.is_empty())
            .then(|| format!("Partial history: {}", snapshot.errors.join("; ")));
        self.refresh_agent_summaries();
    }

    pub(super) fn observe_agent_feed(&mut self, notification: &ServerNotification) {
        self.observe_agent_feed_inner(notification);
        self.observe_agent_cards(notification);
        self.refresh_agent_summaries();
    }

    fn observe_agent_feed_inner(&mut self, notification: &ServerNotification) {
        if !self.local_settings.tui.fullscreen_transcript {
            return;
        }
        self.sync_agent_feed_root();
        let Some(root) = self.agent_feed.root else {
            return;
        };
        if matches!(
            notification,
            ServerNotification::AgentMessageDelta(_)
                | ServerNotification::ReasoningSummaryTextDelta(_)
                | ServerNotification::ReasoningSummaryPartAdded(_)
                | ServerNotification::ItemStarted(_)
                | ServerNotification::ItemCompleted(_)
                | ServerNotification::ThreadStarted(_)
                | ServerNotification::TurnCompleted(_)
                | ServerNotification::ThreadStatusChanged(_)
        ) {
            self.chat_widget.frame_requester.schedule_frame();
        }
        let feed = self.agent_feed.roots.entry(root).or_default();
        match notification {
            ServerNotification::TurnStarted(event) => {
                if let Ok(agent) = ThreadId::from_string(&event.thread_id)
                    && feed.agents.contains_key(&agent)
                {
                    feed.windows
                        .entry(agent)
                        .or_default()
                        .start_turn(&event.turn.id);
                }
                self.agent_feed.dirty = true;
                return;
            }
            ServerNotification::ReasoningSummaryTextDelta(delta) => {
                if let Ok(id) = ThreadId::from_string(&delta.thread_id) {
                    feed.reasoning_delta(
                        MessageKey(id, delta.turn_id.clone(), delta.item_id.clone()),
                        delta.summary_index,
                        &delta.delta,
                        chrono::Utc::now().timestamp_millis(),
                    );
                }
                return;
            }
            ServerNotification::ReasoningSummaryPartAdded(_) => return,
            ServerNotification::ItemStarted(item) => {
                if let ThreadItem::Reasoning { id, summary, .. } = &item.item {
                    if let Ok(thread) = ThreadId::from_string(&item.thread_id) {
                        feed.reasoning_summary(
                            MessageKey(thread, item.turn_id.clone(), id.clone()),
                            summary,
                            false,
                            true,
                            chrono::Utc::now().timestamp_millis(),
                        );
                    }
                    return;
                }
            }
            ServerNotification::ItemCompleted(item) => {
                if let ThreadItem::Reasoning { id, summary, .. } = &item.item {
                    if let Ok(thread) = ThreadId::from_string(&item.thread_id) {
                        feed.reasoning_summary(
                            MessageKey(thread, item.turn_id.clone(), id.clone()),
                            summary,
                            true,
                            true,
                            chrono::Utc::now().timestamp_millis(),
                        );
                    }
                    return;
                }
            }
            _ => {}
        }
        let (thread, turn, item, text, complete, append) = match notification {
            ServerNotification::AgentMessageDelta(delta) => (
                &delta.thread_id,
                &delta.turn_id,
                &delta.item_id,
                delta.delta.as_str(),
                false,
                true,
            ),
            ServerNotification::ItemStarted(item) => match &item.item {
                ThreadItem::AgentMessage { id, text, .. } => (
                    &item.thread_id,
                    &item.turn_id,
                    id,
                    text.as_str(),
                    false,
                    false,
                ),
                _ => return,
            },
            ServerNotification::ItemCompleted(item) => match &item.item {
                ThreadItem::AgentMessage { id, text, .. } => (
                    &item.thread_id,
                    &item.turn_id,
                    id,
                    text.as_str(),
                    true,
                    false,
                ),
                _ => return,
            },
            ServerNotification::ThreadStarted(_)
            | ServerNotification::TurnCompleted(_)
            | ServerNotification::ThreadStatusChanged(_) => {
                self.agent_feed.dirty = true;
                return;
            }
            _ => return,
        };
        if let Ok(id) = ThreadId::from_string(thread) {
            feed.upsert(
                MessageKey(id, turn.clone(), item.clone()),
                text,
                complete,
                true,
                chrono::Utc::now().timestamp_millis(),
                append,
            );
        }
    }

    pub(super) fn invalidate_agent_feed(&mut self) {
        self.agent_feed.generation += 1;
        if let Some(task) = self.agent_feed.task.take() {
            task.abort();
        }
        for feed in self.agent_feed.roots.values_mut() {
            feed.versions.clear();
        }
        self.agent_feed.dirty = true;
    }
}

fn parent(thread: &Thread) -> Option<ThreadId> {
    thread
        .parent_thread_id
        .as_deref()
        .and_then(|id| ThreadId::from_string(id).ok())
        .or_else(|| match &thread.source {
            codex_app_server_protocol::SessionSource::SubAgent(
                codex_protocol::protocol::SubAgentSource::ThreadSpawn {
                    parent_thread_id, ..
                },
            ) => Some(*parent_thread_id),
            _ => None,
        })
}
fn belongs_to_root(
    mut id: ThreadId,
    root: ThreadId,
    threads: &HashMap<ThreadId, Option<Thread>>,
) -> bool {
    let mut seen = HashSet::new();
    while seen.insert(id) {
        let Some(next) = threads.get(&id).and_then(Option::as_ref).and_then(parent) else {
            return false;
        };
        if next == root {
            return true;
        }
        id = next;
    }
    false
}
fn agent_label(thread: &Thread) -> String {
    crate::app_server_session::source_agent_path(&thread.source)
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| {
            format!(
                "/{}",
                thread.agent_nickname.as_deref().unwrap_or(&thread.id)
            )
        })
}
