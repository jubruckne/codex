//! Bounded overview previews from existing reads and already-delivered events.
//! Observing activity never attaches to a thread or fetches additional history.
//! Message previews retain Markdown until the details panel's width is known.

use super::App;
use super::ThreadBufferedEvent;
use super::ThreadEventAttachment;
use super::agents_overview_view::AgentsOverviewGroup;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::McpServerElicitationRequest;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;
use codex_utils_absolute_path::AbsolutePathBuf;
use ratatui::style::Stylize;
use ratatui::text::Line;
use std::collections::HashMap;

const PREVIEW_CHARS: usize = 512;

pub(super) fn preview_agent_message(text: &str) -> String {
    preview_markdown(&crate::markdown::normalize_markdown_for_rendering(text))
}

pub(super) fn preview_markdown(text: &str) -> String {
    text.chars()
        .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
        .take(PREVIEW_CHARS)
        .collect()
}

#[derive(Clone, Default)]
pub(super) struct AgentsOverviewDetails {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) usage_lines: Vec<Line<'static>>,
    pub(super) last_message: Option<(String, AbsolutePathBuf)>,
    pub(super) current_activity: Option<String>,
    pub(super) agent_path: Option<String>,
    pub(super) updated_at: Option<i64>,
    pub(super) completed: bool,
}

pub(super) fn preview_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_whitespace() { ' ' } else { ch })
        .filter(|ch| !ch.is_control())
        .take(PREVIEW_CHARS)
        .collect()
}

#[derive(Default)]
pub(super) struct AgentsOverviewActivity {
    reasoning: Option<ReasoningPreview>,
    last_message: Option<String>,
    last_reasoning_header: Option<String>,
    active_items: Vec<(String, String)>,
    updated_at: Option<i64>,
    completed: Option<bool>,
}

struct ReasoningPreview {
    turn_id: String,
    item_id: String,
    summary_index: i64,
    buffer: String,
    header: Option<String>,
}

fn item_activity(item: &ThreadItem) -> Option<String> {
    Some(match item {
        ThreadItem::Reasoning { .. } => "Thinking".into(),
        ThreadItem::AgentMessage { .. } => "Writing response".into(),
        ThreadItem::Plan { .. } => "Planning".into(),
        ThreadItem::WebSearch(_) => "Searching web".into(),
        ThreadItem::CommandExecution { command, .. } => format!("Running {}", preview_text(command)),
        ThreadItem::ContextCompaction { .. } => "Compacting context".into(),
        ThreadItem::FileChange { .. } => "Editing files".into(),
        ThreadItem::McpToolCall { server, tool, .. } => format!("Calling {server}.{tool}"),
        ThreadItem::DynamicToolCall { tool, .. } => format!("Calling {tool}"),
        ThreadItem::CollabAgentToolCall { tool, .. } if *tool == codex_app_server_protocol::CollabAgentTool::Wait => "Waiting for agent".into(),
        ThreadItem::CollabAgentToolCall { .. } => "Managing agents".into(),
        _ => return None,
    })
}

impl App {
    pub(super) fn track_agents_overview_activity(
        &mut self,
        thread_id: ThreadId,
        notification: &ServerNotification,
    ) {
        if !self.agents_overview.threads.contains_key(&thread_id) {
            return;
        }
        let activity = self.agents_overview.activity.entry(thread_id).or_default();
        let previous_timestamp = activity.updated_at;
        let meaningful = match notification {
            ServerNotification::ItemStarted(started) => {
                if let Some(label) = item_activity(&started.item) {
                    activity.active_items.retain(|(id, _)| id != started.item.id());
                    activity.active_items.push((started.item.id().to_string(), label));
                }
                true
            }
            ServerNotification::ItemCompleted(completed) => {
                activity.active_items.retain(|(id, _)| id != completed.item.id());
                if let ThreadItem::Reasoning { summary, .. } = &completed.item {
                    if let Some(text) = summary.iter().rev().find(|text| !text.trim().is_empty()) {
                        activity.last_reasoning_header = Some(crate::chatwidget::extract_first_bold(text)
                            .map(|header| preview_text(&header)).unwrap_or_else(|| preview_text(text)));
                    }
                }
                if activity.reasoning.as_ref().is_some_and(|reasoning| reasoning.item_id == completed.item.id()) {
                    activity.reasoning = None;
                }
                true
            }
            ServerNotification::TurnStarted(_) => {
                activity.active_items.clear();
                activity.completed = Some(false);
                activity.last_reasoning_header = None;
                true
            }
            ServerNotification::TurnCompleted(turn) => {
                activity.active_items.clear();
                activity.completed = Some(turn.turn.status == codex_app_server_protocol::TurnStatus::Completed);
                true
            }
            ServerNotification::AgentMessageDelta(delta) => {
                if !activity.active_items.iter().any(|(id, _)| id == &delta.item_id) {
                    activity.active_items.push((delta.item_id.clone(), "Writing response".into()));
                }
                true
            }
            ServerNotification::ThreadClosed(_) => {
                activity.active_items.clear();
                activity.reasoning = None;
                true
            }
            ServerNotification::ReasoningSummaryTextDelta(_)
            | ServerNotification::CommandExecutionOutputDelta(_)
            | ServerNotification::ThreadStatusChanged(_) => true,
            _ => false,
        };
        if meaningful {
            let timestamp = match notification {
                ServerNotification::ItemStarted(item) => Some(item.started_at_ms / 1000),
                ServerNotification::ItemCompleted(item) => Some(item.completed_at_ms / 1000),
                ServerNotification::TurnStarted(turn) => turn.turn.started_at,
                ServerNotification::TurnCompleted(turn) => turn.turn.completed_at,
                _ => None,
            }.filter(|timestamp| *timestamp > 0).unwrap_or_else(|| chrono::Utc::now().timestamp());
            activity.updated_at = activity.updated_at.into_iter().chain(Some(timestamp)).max();
        }
        let changed = match notification {
            ServerNotification::ReasoningSummaryTextDelta(delta) => {
                let had_header = activity
                    .reasoning
                    .as_ref()
                    .is_some_and(|reasoning| reasoning.header.is_some());
                if activity.reasoning.as_ref().is_none_or(|reasoning| {
                    reasoning.turn_id != delta.turn_id
                        || reasoning.item_id != delta.item_id
                        || reasoning.summary_index != delta.summary_index
                }) {
                    activity.reasoning = Some(ReasoningPreview {
                        turn_id: delta.turn_id.clone(),
                        item_id: delta.item_id.clone(),
                        summary_index: delta.summary_index,
                        buffer: String::new(),
                        header: None,
                    });
                }
                let Some(reasoning) = activity.reasoning.as_mut() else {
                    return;
                };
                if reasoning.header.is_none() {
                    let remaining = PREVIEW_CHARS.saturating_sub(reasoning.buffer.chars().count());
                    reasoning.buffer.extend(delta.delta.chars().take(remaining));
                    reasoning.header = crate::chatwidget::extract_first_bold(&reasoning.buffer)
                        .map(|header| preview_text(&header));
                }
                if let Some(header) = &reasoning.header {
                    activity.last_reasoning_header = Some(header.clone());
                }
                !had_header && reasoning.header.is_some()
            }
            ServerNotification::ItemCompleted(ItemCompletedNotification {
                item: ThreadItem::AgentMessage { text, .. },
                ..
            }) => {
                activity.last_message = Some(preview_agent_message(text));
                activity.reasoning = None;
                true
            }
            ServerNotification::TurnStarted(_)
            | ServerNotification::TurnCompleted(_)
            | ServerNotification::ReasoningSummaryPartAdded(_)
            | ServerNotification::ItemStarted(_) => activity.reasoning.take().is_some(),
            ServerNotification::ThreadStatusChanged(status)
                if !matches!(status.status, ThreadStatus::Active { .. }) =>
            {
                activity.active_items.clear();
                activity.reasoning.take().is_some()
            }
            _ => false,
        };
        let phase_changed = matches!(notification,
            ServerNotification::ItemStarted(_) | ServerNotification::ItemCompleted(_)
                | ServerNotification::TurnStarted(_) | ServerNotification::TurnCompleted(_)
                | ServerNotification::ThreadStatusChanged(_));
        if changed || phase_changed || activity.updated_at != previous_timestamp {
            self.repaint_agents_overview();
        }
    }

    pub(super) fn agents_overview_updated_at(&self, thread: &Thread) -> Option<i64> {
        let metadata = (thread.updated_at > 0).then_some(thread.updated_at);
        let received = ThreadId::from_string(&thread.id).ok()
            .and_then(|id| self.agents_overview.activity.get(&id))
            .and_then(|activity| activity.updated_at);
        metadata.into_iter().chain(received).max()
    }

    pub(super) fn record_agents_overview_turn_metadata(&mut self, thread_id: ThreadId, thread: &Thread) {
        if let Some(turn) = thread.turns.last() {
            let activity = self.agents_overview.activity.entry(thread_id).or_default();
            if activity.updated_at.is_none() {
                activity.completed = Some(turn.status == codex_app_server_protocol::TurnStatus::Completed);
                // Recover explicitly running tools from existing reads when we missed ItemStarted.
                if turn.status == codex_app_server_protocol::TurnStatus::InProgress {
                    activity.last_reasoning_header = turn.items.iter().rev().find_map(|item| match item {
                        ThreadItem::Reasoning { summary, .. } => summary.iter().rev()
                            .find(|text| !text.trim().is_empty()).map(|text| crate::chatwidget::extract_first_bold(text)
                                .map(|header| preview_text(&header)).unwrap_or_else(|| preview_text(text))),
                        _ => None,
                    });
                    activity.active_items = turn.items.iter().filter(|item| match item {
                        ThreadItem::CommandExecution { status, .. } => *status == codex_app_server_protocol::CommandExecutionStatus::InProgress,
                        ThreadItem::McpToolCall { status, .. } => *status == codex_app_server_protocol::McpToolCallStatus::InProgress,
                        ThreadItem::DynamicToolCall { status, .. } => *status == codex_app_server_protocol::DynamicToolCallStatus::InProgress,
                        ThreadItem::FileChange { status, .. } => *status == codex_app_server_protocol::PatchApplyStatus::InProgress,
                        ThreadItem::CollabAgentToolCall { status, .. } => *status == codex_app_server_protocol::CollabAgentToolCallStatus::InProgress,
                        _ => false,
                    }).filter_map(|item| item_activity(item).map(|label| (item.id().to_string(), label))).collect();
                }
            }
        }
    }

    pub(super) fn agents_overview_details(
        &self,
        root: &Thread,
        children: &HashMap<String, Vec<&Thread>>,
    ) -> AgentsOverviewDetails {
        let mut lines = Vec::new();
        // Urgency comes first; on ties, prefer available current detail over history.
        let priority = |thread: &Thread| {
            let group = AgentsOverviewGroup::for_status(&thread.status);
            let has_detail = ThreadId::from_string(&thread.id).is_ok_and(|id| match group {
                AgentsOverviewGroup::NeedsYou => self.agents_overview_request_preview(id).is_some(),
                AgentsOverviewGroup::Working => {
                    self.thread_event_channels
                        .get(&id)
                        .is_none_or(|channel| channel.attachment() == ThreadEventAttachment::Live)
                        && self
                            .agents_overview
                            .activity
                            .get(&id)
                            .and_then(|activity| activity.reasoning.as_ref())
                            .is_some_and(|reasoning| reasoning.header.is_some())
                }
                AgentsOverviewGroup::Ready | AgentsOverviewGroup::Finished => false,
            });
            (group, !has_detail)
        };
        let mut source = root;
        let mut pending = vec![root];
        while let Some(thread) = pending.pop() {
            if priority(thread) < priority(source) {
                source = thread;
            }
            pending.extend(children.get(&thread.id).into_iter().flatten().copied());
            pending.sort_by(|left, right| right.id.cmp(&left.id));
        }
        let Ok(thread_id) = ThreadId::from_string(&source.id) else {
            return AgentsOverviewDetails::default();
        };
        let channel = self.thread_event_channels.get(&thread_id);
        let is_child = source.id != root.id;
        if is_child {
            let path = self.agents_overview_agent_path(source);
            let name = path.as_deref().or(source.name.as_deref()).unwrap_or(&source.id);
            lines.push(Line::default());
            lines.push(vec!["Agent: ".dim(), preview_text(name).into()].into());
        }
        if AgentsOverviewGroup::for_status(&source.status) == AgentsOverviewGroup::NeedsYou {
            if !is_child {
                lines.extend([Line::default(), "Needs attention".red().into()]);
            }
            if let Some(request) = self.agents_overview_request_preview(thread_id) {
                lines.push(request.into());
            }
            lines.push("Open task to review.".dim().into());
            match &source.status {
                ThreadStatus::Active { active_flags } => {
                    if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval) {
                        lines.push("Waiting for approval.".into());
                    }
                    if active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput) {
                        lines.push("Waiting for your response.".into());
                    }
                }
                ThreadStatus::SystemError => lines.push("Task encountered an error.".into()),
                ThreadStatus::NotLoaded | ThreadStatus::Idle => {}
            }
        }
        let activity = self.agents_overview.activity.get(&thread_id).filter(|_| {
            channel.is_none_or(|channel| channel.attachment() == ThreadEventAttachment::Live)
        });
        if matches!(source.status, ThreadStatus::Active { .. })
            && let Some(header) = activity
                .and_then(|activity| activity.reasoning.as_ref())
                .and_then(|reasoning| reasoning.header.as_ref())
        {
            if !is_child {
                lines.push(Line::default());
            }
            lines.extend(["Latest activity".dim().into(), header.clone().into()]);
        }
        let last_message = activity
            .and_then(|activity| activity.last_message.as_ref())
            .or_else(|| self.agents_overview.last_messages.get(&thread_id))
            .filter(|message| !message.trim().is_empty())
            .map(|message| (message.clone(), source.cwd.clone()));
        let current_activity = self.agents_overview.activity.get(&ThreadId::from_string(&root.id).unwrap_or(thread_id));
        AgentsOverviewDetails {
            agent_path: self.agents_overview_agent_path(root),
            current_activity: match &root.status {
                ThreadStatus::Active { active_flags } => {
                    if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval) {
                        Some("Waiting for approval".into())
                    } else if active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput) {
                        Some("Waiting for your response".into())
                    } else {
                        current_activity.and_then(|activity| {
                            activity.active_items.last().map(|(_, label)| {
                                if label == "Thinking" {
                                    activity.reasoning.as_ref().and_then(|reasoning| reasoning.header.clone()).unwrap_or_else(|| label.clone())
                                } else { label.clone() }
                            }).or_else(|| activity.reasoning.as_ref().map(|reasoning| reasoning.header.clone().unwrap_or_else(|| "Thinking".into())))
                        }).or_else(|| current_activity.and_then(|activity| activity.last_reasoning_header.clone()))
                            .or_else(|| Some("Working".into()))
                    }
                }
                ThreadStatus::NotLoaded | ThreadStatus::Idle | ThreadStatus::SystemError => None,
            },
            updated_at: self.agents_overview_updated_at(root),
            completed: current_activity.is_some_and(|activity| activity.completed == Some(true)),
            lines,
            usage_lines: ThreadId::from_string(&root.id)
                .ok()
                .and_then(|id| self.agents_overview.usage.get(&id))
                .map(super::agents_overview_usage::usage_lines)
                .unwrap_or_default(),
            last_message,
        }
    }

    fn agents_overview_agent_path(&self, thread: &Thread) -> Option<String> {
        crate::app_server_session::source_agent_path(&thread.source)
            .filter(|path| !path.trim().is_empty())
            .or_else(|| ThreadId::from_string(&thread.id).ok()
                .and_then(|id| self.agent_navigation.get(&id))
                .and_then(|entry| entry.agent_path.clone())
                .filter(|path| !path.trim().is_empty()))
    }

    fn agents_overview_request_preview(&self, thread_id: ThreadId) -> Option<String> {
        self.agents_overview
            .dispatched_requests
            .get(&thread_id)
            .and_then(|requests| {
                requests
                    .iter()
                    .find_map(|request| request_preview(&request.request))
            })
            .or_else(|| {
                let channel = self
                    .thread_event_channels
                    .get(&thread_id)
                    .filter(|channel| channel.attachment() == ThreadEventAttachment::Live)?;
                let store = channel.store.try_lock().ok()?;
                store.buffer.iter().find_map(|event| {
                    if let ThreadBufferedEvent::Request(request) = event
                        && store
                            .pending_interactive_replay
                            .should_replay_snapshot_request(request)
                    {
                        request_preview(request)
                    } else {
                        None
                    }
                })
            })
    }
}

fn request_preview(request: &ServerRequest) -> Option<String> {
    let text = match request {
        ServerRequest::CommandExecutionRequestApproval { params, .. } => {
            params.reason.as_deref().or(params.command.as_deref())
        }
        ServerRequest::FileChangeRequestApproval { params, .. } => params.reason.as_deref(),
        ServerRequest::PermissionsRequestApproval { params, .. } => params.reason.as_deref(),
        ServerRequest::McpServerElicitationRequest { params, .. } => match &params.request {
            McpServerElicitationRequest::Form { message, .. }
            | McpServerElicitationRequest::OpenAiForm { message, .. }
            | McpServerElicitationRequest::OpenAiElicitationForm { message, .. }
            | McpServerElicitationRequest::Url { message, .. } => Some(message.as_str()),
            McpServerElicitationRequest::UserVerification { .. } => None,
        },
        ServerRequest::ToolRequestUserInput { params, .. } => params
            .questions
            .first()
            .map(|question| question.question.as_str()),
        _ => None,
    }?;
    Some(preview_text(text))
}
