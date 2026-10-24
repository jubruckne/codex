//! Multi-agent picker navigation and labeling state for the TUI app.
//!
//! This module exists to keep the pure parts of multi-agent navigation out of [`crate::app::App`].
//! It owns the stable spawn-order cache used by the `/subagents` picker, keyboard next/previous
//! navigation, and the contextual footer label for the thread currently being watched.
//!
//! Responsibilities here are intentionally narrow:
//! - remember picker entries and their first-seen order
//! - remember which V2 child threads are owned by their parent agent
//! - answer traversal questions like "what is the next thread?"
//! - derive user-facing picker/footer text from cached thread metadata
//!
//! Responsibilities that stay in `App`:
//! - discovering threads from the backend
//! - deciding which thread is currently displayed
//! - mutating UI state such as switching threads or updating the footer widget
//!
//! The key invariant is that traversal follows first-seen spawn order rather than thread-id sort
//! order. Once a thread id is observed it keeps its place in the cycle even if the entry is later
//! updated or marked closed.

use crate::multi_agents::AgentPickerThreadEntry;
use crate::multi_agents::SubAgentActivityDisplay;
use crate::multi_agents::format_agent_picker_item_name;
use crate::multi_agents::next_agent_shortcut;
use crate::multi_agents::previous_agent_shortcut;
use codex_app_server_protocol::ThreadActiveFlag;
use codex_app_server_protocol::ThreadStatus;
use codex_protocol::ThreadId;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::collections::HashSet;
use uuid::Uuid;

/// Small state container for multi-agent picker ordering and labeling.
///
/// `App` owns thread lifecycle and UI side effects. This type keeps the pure rules for stable
/// spawn-order traversal, picker copy, and active-agent labels together and separately testable.
///
/// The core invariant is that `order` records first-seen thread ids exactly once, while `threads`
/// stores the latest metadata for those ids. Mutation is intentionally funneled through `upsert`,
/// `mark_closed`, and `clear` so those two collections do not drift semantically even if they are
/// temporarily out of sync during teardown races.
#[derive(Debug, Default)]
pub(crate) struct AgentNavigationState {
    /// Latest picker metadata for each tracked thread id.
    threads: HashMap<ThreadId, AgentPickerThreadEntry>,
    /// Stable first-seen traversal order for picker rows and keyboard cycling.
    order: Vec<ThreadId>,
    /// Threads with observed terminal liveness that must not be revived by delayed activity.
    stopped_threads: HashSet<ThreadId>,
    /// Spawned child threads whose instructions are owned by their parent agent.
    parent_owned_threads: HashSet<ThreadId>,
    picker_excluded_threads: HashSet<ThreadId>,
    /// Stable numeric destinations; slot zero belongs to the primary thread.
    quick_switch_root: Option<ThreadId>,
    quick_switch_slots: [Option<ThreadId>; 9],
    quick_switch_width: usize,
    /// Server timestamps and received activity, in Unix milliseconds. Metadata reads never
    /// invent activity, and stale replies cannot move a thread's update time backwards.
    last_updates: HashMap<ThreadId, i64>,
    /// Coalesces root refreshes while rejecting replies from a previous session.
    pub(super) picker_refresh: Option<AgentPickerRefreshState>,
}

/// Selection stays visually dominant; other destinations reflect their known state.
fn agent_footer_style(
    selected: bool,
    closed: bool,
    running: bool,
    status: Option<&ThreadStatus>,
) -> Style {
    if selected {
        return Style::default().fg(crate::style::accent_color()).bold().not_dim();
    }
    if closed {
        return crate::style::secondary_text_style().dim();
    }
    if running {
        if let Some(ThreadStatus::Active { active_flags }) = status
            && (active_flags.contains(&ThreadActiveFlag::WaitingOnApproval)
                || active_flags.contains(&ThreadActiveFlag::WaitingOnUserInput))
        {
            return crate::style::agent_waiting_style();
        }
        // A newer live event can supersede idle/unloaded/error metadata from a read.
        return crate::style::agent_working_style();
    }
    match status {
        Some(ThreadStatus::SystemError) => {
            crate::style::status_style(crate::style::StatusTone::Failure).not_bold().not_dim()
        }
        Some(ThreadStatus::NotLoaded) => crate::style::secondary_text_style().dim(),
        _ => crate::style::secondary_text_style(),
    }
}

/// Direction of keyboard traversal through the stable picker order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AgentNavigationDirection {
    /// Move toward the entry that was seen earlier in spawn order, wrapping at the front.
    Previous,
    /// Move toward the entry that was seen later in spawn order, wrapping at the end.
    Next,
}

#[derive(Clone, Copy)]
pub(super) enum AgentPickerThreadVisibility {
    Visible,
    Hidden,
}

#[derive(Debug)]
pub(super) struct AgentPickerRefreshState {
    root: ThreadId,
    request_id: Uuid,
    pub(super) visibility_changed_threads: HashSet<ThreadId>,
    pub(super) follow_up_requested: bool,
}

impl AgentNavigationState {
    pub(crate) fn begin_picker_refresh(&mut self, thread_id: ThreadId) -> Option<Uuid> {
        if self
            .picker_refresh
            .as_ref()
            .is_some_and(|refresh| refresh.root == thread_id)
        {
            return None;
        }
        let request_id = Uuid::new_v4();
        self.picker_refresh = Some(AgentPickerRefreshState {
            root: thread_id,
            request_id,
            visibility_changed_threads: HashSet::new(),
            follow_up_requested: false,
        });
        Some(request_id)
    }

    pub(crate) fn queue_picker_refresh(&mut self, thread_id: ThreadId) -> bool {
        let Some(refresh) = self.picker_refresh.as_mut() else {
            return false;
        };
        if refresh.root != thread_id {
            return false;
        }
        refresh.follow_up_requested = true;
        true
    }

    pub(super) fn finish_picker_refresh(
        &mut self,
        thread_id: ThreadId,
        request_id: Uuid,
    ) -> Option<AgentPickerRefreshState> {
        if !self
            .picker_refresh
            .as_ref()
            .is_some_and(|refresh| refresh.root == thread_id && refresh.request_id == request_id)
        {
            return None;
        }
        self.picker_refresh.take()
    }

    /// Returns the cached picker entry for a specific thread id.
    ///
    /// Callers use this when they already know which thread they care about and need the last
    /// metadata captured for picker or footer rendering. If a caller assumes every tracked thread
    /// must be present here, shutdown races can turn that assumption into a panic elsewhere, so
    /// this stays optional.
    pub(crate) fn get(&self, thread_id: &ThreadId) -> Option<&AgentPickerThreadEntry> {
        self.threads.get(thread_id)
    }

    pub(crate) fn is_parent_owned(&self, thread_id: ThreadId) -> bool {
        self.parent_owned_threads.contains(&thread_id)
    }

    /// Marks a spawned child thread as view-only for direct user instructions.
    pub(crate) fn mark_parent_owned(&mut self, thread_id: ThreadId) {
        self.parent_owned_threads.insert(thread_id);
    }

    /// Returns whether the picker currently has any visible threads.
    ///
    /// This is the cheapest way for `App` to decide whether opening the picker should show "No
    /// agents available yet." rather than constructing picker rows from an empty state.
    pub(crate) fn is_empty(&self) -> bool {
        self.threads
            .keys()
            .all(|thread_id| self.picker_excluded_threads.contains(thread_id))
    }

    /// Inserts or updates a picker entry while preserving first-seen traversal order.
    ///
    /// The key invariant of this module is enforced here: a thread id is appended to `order` only
    /// the first time it is seen. Later updates may change nickname, role, or closed state, but
    /// they must not move the thread in the cycle or keyboard navigation would feel unstable.
    pub(crate) fn upsert(
        &mut self,
        thread_id: ThreadId,
        agent_nickname: Option<String>,
        agent_role: Option<String>,
        is_closed: bool,
    ) {
        if !self.threads.contains_key(&thread_id) {
            self.order.push(thread_id);
        }
        let (previous_agent_path, previous_is_running) = self
            .threads
            .get(&thread_id)
            .map(|entry| (entry.agent_path.clone(), entry.is_running))
            .unwrap_or((None, false));
        self.threads.insert(
            thread_id,
            AgentPickerThreadEntry {
                agent_nickname,
                agent_role,
                agent_path: previous_agent_path,
                is_running: previous_is_running && !is_closed,
                is_closed,
            },
        );
    }

    pub(crate) fn record_sub_agent_activity(&mut self, activity: SubAgentActivityDisplay) {
        if !self.threads.contains_key(&activity.thread_id) {
            self.order.push(activity.thread_id);
        }
        let entry =
            self.threads
                .entry(activity.thread_id)
                .or_insert_with(|| AgentPickerThreadEntry {
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    is_running: false,
                    is_closed: false,
                });
        let timestamp_ms = chrono::Utc::now().timestamp_millis();
        let updated = self
            .last_updates
            .entry(activity.thread_id)
            .or_insert(timestamp_ms);
        *updated = (*updated).max(timestamp_ms);
        entry.agent_path = Some(activity.agent_path);
        if activity.is_running_hint
            && !entry.is_closed
            && !self.stopped_threads.contains(&activity.thread_id)
        {
            entry.is_running = true;
        } else {
            entry.is_running = false;
            self.stopped_threads.insert(activity.thread_id);
        }
    }

    pub(crate) fn mark_running(&mut self, thread_id: ThreadId) {
        if self
            .threads
            .get(&thread_id)
            .is_some_and(|entry| entry.is_closed)
        {
            return;
        }
        self.stopped_threads.remove(&thread_id);
        self.set_running(thread_id, /*is_running*/ true);
    }

    pub(crate) fn mark_stopped(&mut self, thread_id: ThreadId) {
        self.stopped_threads.insert(thread_id);
        self.set_running(thread_id, /*is_running*/ false);
    }

    pub(crate) fn set_running(&mut self, thread_id: ThreadId, is_running: bool) {
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.is_running = is_running;
        }
    }

    pub(crate) fn set_agent_path(&mut self, thread_id: ThreadId, agent_path: Option<String>) {
        if let Some(agent_path) = agent_path
            && let Some(entry) = self.threads.get_mut(&thread_id)
        {
            entry.agent_path = Some(agent_path);
        }
    }

    /// Marks a thread as closed without removing it from the traversal cache.
    ///
    /// Closed threads stay in the picker and in spawn order so users can still review them and so
    /// next/previous navigation does not reshuffle around disappearing entries. If a caller "cleans
    /// this up" by deleting the entry instead, wraparound navigation will silently change shape
    /// mid-session.
    pub(crate) fn mark_closed(&mut self, thread_id: ThreadId) {
        if let Some(entry) = self.threads.get_mut(&thread_id) {
            entry.is_closed = true;
            entry.is_running = false;
        } else {
            self.upsert(
                thread_id, /*agent_nickname*/ None, /*agent_role*/ None,
                /*is_closed*/ true,
            );
        }
    }

    /// Drops all cached picker state.
    ///
    /// This is used when `App` tears down thread event state and needs the picker cache to return
    /// to a pristine single-session state.
    pub(crate) fn clear(&mut self) {
        self.threads.clear();
        self.order.clear();
        self.stopped_threads.clear();
        self.parent_owned_threads.clear();
        self.picker_excluded_threads.clear();
        self.picker_refresh = None;
        self.quick_switch_root = None;
        self.quick_switch_slots = [None; 9];
        self.last_updates.clear();
    }

    /// Removes a tracked thread entirely from picker metadata and traversal order.
    ///
    /// This is reserved for entries that were only discovered opportunistically and never became
    /// replayable local threads. Keeping those around after the backend confirms they are gone
    /// would leave ghost rows in `/subagents`.
    pub(crate) fn remove(&mut self, thread_id: ThreadId) {
        self.threads.remove(&thread_id);
        self.order.retain(|candidate| *candidate != thread_id);
        self.stopped_threads.remove(&thread_id);
        self.parent_owned_threads.remove(&thread_id);
        self.picker_excluded_threads.remove(&thread_id);
        self.last_updates.remove(&thread_id);
        for slot in &mut self.quick_switch_slots {
            if *slot == Some(thread_id) {
                *slot = None;
            }
        }
    }

    /// Returns whether there is at least one tracked thread other than the primary one.
    ///
    /// `App` uses this to decide whether the picker should be available even when the collaboration
    /// feature flag is currently disabled, because already-existing sub-agent threads should remain
    /// inspectable.
    pub(crate) fn has_non_primary_thread(&self, primary_thread_id: Option<ThreadId>) -> bool {
        self.threads.keys().any(|thread_id| {
            Some(*thread_id) != primary_thread_id
                && !self.picker_excluded_threads.contains(thread_id)
        })
    }

    /// Returns live picker rows in the same order users cycle through them.
    ///
    /// The `order` vector is intentionally historical and may briefly contain thread ids that no
    /// longer have cached metadata, so this filters through the map instead of assuming both
    /// collections are perfectly synchronized.
    pub(crate) fn ordered_threads(&self) -> Vec<(ThreadId, &AgentPickerThreadEntry)> {
        self.order
            .iter()
            .filter_map(|thread_id| self.threads.get(thread_id).map(|entry| (*thread_id, entry)))
            .collect()
    }

    pub(crate) fn visible_threads(&self) -> Vec<(ThreadId, &AgentPickerThreadEntry)> {
        self.ordered_threads()
            .into_iter()
            .filter(|(thread_id, _)| !self.picker_excluded_threads.contains(thread_id))
            .collect()
    }

    pub(super) fn set_picker_thread_visibility(
        &mut self,
        thread_id: ThreadId,
        visibility: AgentPickerThreadVisibility,
    ) {
        let track_hidden = self.threads.contains_key(&thread_id) || self.picker_refresh.is_some();
        match visibility {
            AgentPickerThreadVisibility::Visible => self.picker_excluded_threads.remove(&thread_id),
            AgentPickerThreadVisibility::Hidden => {
                track_hidden && self.picker_excluded_threads.insert(thread_id)
            }
        };
        if let Some(refresh) = self.picker_refresh.as_mut() {
            refresh.visibility_changed_threads.insert(thread_id);
        }
    }

    pub(crate) fn prune_untracked_picker_exclusions(&mut self) {
        self.picker_excluded_threads
            .retain(|thread_id| self.threads.contains_key(thread_id));
    }

    pub(crate) fn ordered_path_backed_subagent_threads(
        &self,
        primary_thread_id: Option<ThreadId>,
    ) -> Vec<(ThreadId, &AgentPickerThreadEntry)> {
        self.visible_threads()
            .into_iter()
            .filter(|(thread_id, entry)| {
                Some(*thread_id) != primary_thread_id
                    && entry
                        .agent_path
                        .as_deref()
                        .is_some_and(|agent_path| !agent_path.trim().is_empty())
            })
            .collect()
    }

    /// Returns tracked thread ids in the same stable order used by the picker.
    pub(crate) fn tracked_thread_ids(&self) -> Vec<ThreadId> {
        self.ordered_threads()
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .collect()
    }

    /// Returns the adjacent thread id for keyboard navigation in stable spawn order.
    ///
    /// The caller must pass the thread whose transcript is actually being shown to the user, not
    /// just whichever thread bookkeeping most recently marked active. If the wrong current thread
    /// is supplied, next/previous navigation will jump in a way that feels nondeterministic even
    /// though the cache itself is correct.
    pub(crate) fn adjacent_thread_id(
        &self,
        current_displayed_thread_id: Option<ThreadId>,
        direction: AgentNavigationDirection,
    ) -> Option<ThreadId> {
        let current_thread_id = current_displayed_thread_id?;
        let current_idx = self
            .order
            .iter()
            .position(|thread_id| *thread_id == current_thread_id)?;
        (1..self.order.len()).find_map(|offset| {
            let idx = match direction {
                AgentNavigationDirection::Next => (current_idx + offset) % self.order.len(),
                AgentNavigationDirection::Previous => {
                    (current_idx + self.order.len() - offset) % self.order.len()
                }
            };
            let thread_id = self.order[idx];
            (self.threads.contains_key(&thread_id)
                && !self.picker_excluded_threads.contains(&thread_id))
            .then_some(thread_id)
        })
    }

    pub(crate) fn note_updated_at(&mut self, thread_id: ThreadId, timestamp_ms: i64) {
        let updated = self.last_updates.entry(thread_id).or_insert(timestamp_ms);
        *updated = (*updated).max(timestamp_ms);
    }

    fn quick_switch_is_active(&self, thread_id: ThreadId) -> bool {
        self.threads
            .get(&thread_id)
            .is_some_and(|entry| entry.is_running && !entry.is_closed)
    }

    pub(crate) fn set_quick_switch_width(&mut self, width: usize) {
        self.quick_switch_width = width;
    }

    fn quick_switch_label(&self, thread_id: ThreadId, root: ThreadId) -> String {
        if thread_id == root {
            return "root".to_string();
        }
        let Some(entry) = self.threads.get(&thread_id) else {
            return "agent".to_string();
        };
        match entry
            .agent_path
            .as_deref()
            .map(str::trim)
            .filter(|path| !path.is_empty())
        {
            Some(path) => path
                .strip_prefix("/root/")
                .map_or_else(|| path.to_string(), |relative| format!("/{relative}")),
            None => format_agent_picker_item_name(
                entry.agent_nickname.as_deref(),
                entry.agent_role.as_deref(),
                /*is_primary*/ false,
            ),
        }
    }

    /// Only shorten at word/path boundaries, never in the middle of a Unicode character.
    /// Prefixes are preferred when unique; retaining a suffix distinguishes shared prefixes.
    fn quick_switch_label_variants(label: &str) -> Vec<String> {
        if label.trim_start_matches('/').chars().count() <= 10 {
            return Vec::new();
        }
        let boundaries: Vec<_> = label
            .char_indices()
            .filter(|(index, ch)| {
                *index > 0 && (matches!(*ch, '_' | '-' | '/' | '.') || ch.is_whitespace())
            })
            .collect();
        let width = crate::width::display_width(label);
        let mut variants = Vec::new();
        for (start, separator) in &boundaries {
            let start = *start + separator.len_utf8();
            if start < label.len() {
                let path_prefix = if label.starts_with('/') { "/" } else { "" };
                variants.push(format!("{path_prefix}…{}", &label[start..]));
            }
        }
        for (end, _) in &boundaries {
            let prefix = &label[..*end];
            variants.push(format!("{prefix}…"));
            for (start, separator) in &boundaries {
                let start = *start + separator.len_utf8();
                if start > *end && start < label.len() {
                    variants.push(format!("{prefix}…{}", &label[start..]));
                }
            }
        }
        variants.retain(|variant| crate::width::display_width(variant) < width);
        variants.sort();
        variants.dedup();
        variants
    }

    fn quick_switch_labels(
        &self,
        slots: &[Option<ThreadId>; 9],
        root: ThreadId,
        shorten: bool,
    ) -> [Option<String>; 9] {
        let mut labels = slots.map(|slot| slot.map(|id| self.quick_switch_label(id, root)));
        if !shorten || Self::quick_switch_labels_width(&labels) <= self.quick_switch_width {
            return labels;
        }
        let full_widths = labels.each_ref().map(|label| {
            label.as_deref().map_or(0, crate::width::display_width)
        });
        let variants = labels.each_ref().map(|label| {
            label
                .as_deref()
                .map(Self::quick_switch_label_variants)
                .unwrap_or_default()
        });
        // Reject ambiguous abbreviations against both full labels and all other variants.
        // This makes shared prefixes retain distinguishing suffixes deterministically.
        let unique: [Vec<String>; 9] = std::array::from_fn(|index| {
            variants[index]
                .iter()
                .filter(|variant| {
                    *variant != "root"
                        && (0..labels.len()).all(|other| {
                            other == index
                                || (labels[other].as_ref() != Some(*variant)
                                    && !variants[other].contains(*variant))
                        })
                })
                .cloned()
                .collect()
        });
        loop {
            let width = Self::quick_switch_labels_width(&labels);
            if width <= self.quick_switch_width {
                break;
            }
            let mut best = None;
            for (index, options) in unique.iter().enumerate() {
                let Some(label) = &labels[index] else {
                    continue;
                };
                let current_width = crate::width::display_width(label);
                for variant in options {
                    let variant_width = crate::width::display_width(variant);
                    if variant_width >= current_width {
                        continue;
                    }
                    let saved = current_width - variant_width;
                    // Spread shortening across names in small boundary steps, stopping as
                    // soon as the row fits. Ties follow slot order and prefer unique prefixes.
                    let score = (full_widths[index] - variant_width, saved, index);
                    if best.as_ref().is_none_or(|(old_score, _, _)| score < *old_score) {
                        best = Some((score, index, variant.clone()));
                    }
                }
            }
            let Some((_, index, variant)) = best else {
                break;
            };
            labels[index] = Some(variant);
        }
        labels
    }

    fn quick_switch_labels_width(labels: &[Option<String>; 9]) -> usize {
        // `[0 root]`, then one space plus `[N path]` for each occupied single-digit slot.
        8 + labels
            .iter()
            .flatten()
            .map(|label| crate::width::display_width(label) + 5)
            .sum::<usize>()
    }

    fn quick_switch_slots_width(
        &self,
        slots: &[Option<ThreadId>; 9],
        root: ThreadId,
        shorten: bool,
    ) -> usize {
        Self::quick_switch_labels_width(&self.quick_switch_labels(slots, root, shorten))
    }

    fn refresh_quick_switch_slots(&mut self, root: ThreadId) {
        if self.quick_switch_root != Some(root) {
            self.quick_switch_root = Some(root);
            self.quick_switch_slots = [None; 9];
        }
        let previous_slots = self.quick_switch_slots;
        for index in 0..self.quick_switch_slots.len() {
            if self.quick_switch_slots[index].is_some_and(|thread_id| {
                thread_id == root
                    || !self.threads.contains_key(&thread_id)
                    || self.picker_excluded_threads.contains(&thread_id)
            }) {
                self.quick_switch_slots[index] = None;
            }
        }
        // First shorten to protect active destinations, then retire inactive slots first.
        // Surviving agents retain their numbers, including when this leaves a gap.
        while self.quick_switch_slots_width(
            &self.quick_switch_slots,
            root,
            self.quick_switch_slots
                .iter()
                .flatten()
                .any(|id| self.quick_switch_is_active(*id)),
        ) > self.quick_switch_width
        {
            let remove = self.quick_switch_eviction_candidates().first().copied();
            let Some(index) = remove else {
                break;
            };
            self.quick_switch_slots[index] = None;
        }
        let mut candidates: Vec<_> = self
            .visible_threads()
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .filter(|thread_id| *thread_id != root)
            .collect();
        // Stable sorting preserves first-seen order when timestamps are equal or unavailable.
        candidates.sort_by_key(|thread_id| {
            (
                Reverse(self.quick_switch_is_active(*thread_id)),
                Reverse(self.last_updates.get(thread_id).copied().unwrap_or(0)),
            )
        });
        for candidate in candidates {
            if self.quick_switch_slots.contains(&Some(candidate)) {
                continue;
            }
            let previous_index = previous_slots
                .iter()
                .position(|slot| *slot == Some(candidate));
            // A resize can remove a long label and then admit a shorter one in the same frame.
            // If that shorter agent was already shown, restore its original number.
            let vacant_index = if let Some(index) = previous_index {
                if self.quick_switch_slots[index].is_some() {
                    continue;
                }
                Some(index)
            } else {
                self.quick_switch_slots.iter().position(Option::is_none)
            };
            if let Some(index) = vacant_index {
                let mut trial = self.quick_switch_slots;
                trial[index] = Some(candidate);
                if self.quick_switch_slots_width(&trial, root, self.quick_switch_is_active(candidate))
                    <= self.quick_switch_width
                {
                    self.quick_switch_slots = trial;
                    continue;
                }
            }
            if !self.quick_switch_is_active(candidate) {
                continue;
            }
            let candidate_updated = self.last_updates.get(&candidate).copied().unwrap_or(0);
            let mut trial = self.quick_switch_slots;
            let mut replacement = previous_index;
            // Active arrivals displace inactive agents immediately. Among active agents, each
            // evicted destination must be more than ten seconds older than the new candidate.
            for index in self.quick_switch_eviction_candidates() {
                let thread_id = trial[index].expect("occupied eviction candidate");
                let updated = self.last_updates.get(&thread_id).copied().unwrap_or(0);
                if self.quick_switch_is_active(thread_id)
                    && candidate_updated.saturating_sub(updated) <= 10_000
                {
                    break;
                }
                trial[index] = None;
                let target = *replacement.get_or_insert(index);
                let mut fitted = trial;
                fitted[target] = Some(candidate);
                if self.quick_switch_slots_width(&fitted, root, true) <= self.quick_switch_width {
                    self.quick_switch_slots = fitted;
                    break;
                }
            }
        }
    }

    fn quick_switch_eviction_candidates(&self) -> Vec<usize> {
        let mut candidates: Vec<_> = (0..self.quick_switch_slots.len())
            .filter(|index| self.quick_switch_slots[*index].is_some())
            .collect();
        candidates.sort_by_key(|index| {
            let thread_id = self.quick_switch_slots[*index].expect("occupied slot");
            (
                self.quick_switch_is_active(thread_id),
                self.last_updates.get(&thread_id).copied().unwrap_or(0),
                Reverse(*index),
            )
        });
        candidates
    }

    /// Numeric shortcuts refer to the same retained slots used to draw the footer.
    pub(crate) fn quick_switch_thread(&self, index: usize) -> Option<ThreadId> {
        if index == 0 {
            return self.quick_switch_root;
        }
        let thread_id = self.quick_switch_slots.get(index - 1).copied().flatten()?;
        (self.threads.contains_key(&thread_id)
            && !self.picker_excluded_threads.contains(&thread_id))
        .then_some(thread_id)
    }

    pub(crate) fn quick_switch_index(event: KeyEvent) -> Option<usize> {
        if event.kind != KeyEventKind::Press || event.modifiers != KeyModifiers::CONTROL {
            return None;
        }
        match event.code {
            KeyCode::Char(digit @ '0'..='9') => Some((digit as u8 - b'0') as usize),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn quick_switch_line(
        &mut self,
        current_displayed_thread_id: Option<ThreadId>,
        primary_thread_id: Option<ThreadId>,
    ) -> Option<Line<'static>> {
        self.quick_switch_line_with_status(current_displayed_thread_id, primary_thread_id, |_| None)
    }

    pub(crate) fn quick_switch_line_with_status<'status>(
        &mut self,
        current_displayed_thread_id: Option<ThreadId>,
        primary_thread_id: Option<ThreadId>,
        mut status_for: impl FnMut(ThreadId) -> Option<&'status ThreadStatus>,
    ) -> Option<Line<'static>> {
        let Some(root) = primary_thread_id else {
            self.quick_switch_root = None;
            self.quick_switch_slots = [None; 9];
            return None;
        };
        self.refresh_quick_switch_slots(root);
        let labels = self.quick_switch_labels(
            &self.quick_switch_slots,
            root,
            self.quick_switch_slots
                .iter()
                .flatten()
                .any(|id| self.quick_switch_is_active(*id)),
        );
        let mut spans = Vec::new();
        for index in 0..=self.quick_switch_slots.len() {
            let Some(thread_id) = self.quick_switch_thread(index) else {
                continue;
            };
            let label = if index == 0 {
                "root"
            } else {
                labels[index - 1].as_deref().expect("occupied slot has a label")
            };
            if !spans.is_empty() {
                spans.push(Span::raw(" "));
            }
            let span = Span::raw(format!("[{index} {label}]"));
            let entry = self.threads.get(&thread_id);
            spans.push(span.style(agent_footer_style(
                current_displayed_thread_id == Some(thread_id),
                entry.is_some_and(|entry| entry.is_closed),
                self.quick_switch_is_active(thread_id),
                status_for(thread_id),
            )));
        }
        Some(Line::from(spans))
    }

    /// Derives the contextual footer label for the currently displayed thread.
    ///
    /// This intentionally returns `None` until there is more than one tracked thread so
    /// single-thread sessions do not waste footer space restating the obvious. When metadata for
    /// the displayed thread is missing, the label falls back to the same generic naming rules used
    /// by the picker.
    pub(crate) fn active_agent_label(
        &self,
        current_displayed_thread_id: Option<ThreadId>,
        primary_thread_id: Option<ThreadId>,
    ) -> Option<String> {
        if self.threads.len() <= 1 {
            return None;
        }

        let thread_id = current_displayed_thread_id?;
        let is_primary = primary_thread_id == Some(thread_id);
        Some(
            self.threads
                .get(&thread_id)
                .map(|entry| {
                    if !is_primary
                        && let Some(agent_path) = entry
                            .agent_path
                            .as_deref()
                            .filter(|agent_path| !agent_path.trim().is_empty())
                    {
                        return format!("`{agent_path}`");
                    }
                    format_agent_picker_item_name(
                        entry.agent_nickname.as_deref(),
                        entry.agent_role.as_deref(),
                        is_primary,
                    )
                })
                .unwrap_or_else(|| {
                    format_agent_picker_item_name(
                        /*agent_nickname*/ None, /*agent_role*/ None, is_primary,
                    )
                }),
        )
    }

    /// Builds the `/subagents` picker subtitle from the same canonical bindings used by key handling.
    ///
    /// Keeping this text derived from the actual shortcut helpers prevents the picker copy from
    /// drifting if the bindings ever change on one platform.
    pub(crate) fn picker_subtitle() -> String {
        let previous: Span<'static> = previous_agent_shortcut().into();
        let next: Span<'static> = next_agent_shortcut().into();
        format!(
            "Select an agent to watch. {} previous, {} next.",
            previous.content, next.content
        )
    }

    #[cfg(test)]
    /// Returns only the ordered thread ids for focused tests of traversal invariants.
    ///
    /// This helper exists so tests can assert on ordering without embedding the full picker entry
    /// payload in every expectation.
    pub(crate) fn ordered_thread_ids(&self) -> Vec<ThreadId> {
        self.ordered_threads()
            .into_iter()
            .map(|(thread_id, _)| thread_id)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn populated_state() -> (AgentNavigationState, ThreadId, ThreadId, ThreadId) {
        let mut state = AgentNavigationState::default();
        let main_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000101").expect("valid thread");
        let first_agent_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000102").expect("valid thread");
        let second_agent_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000103").expect("valid thread");

        state.upsert(
            main_thread_id,
            /*agent_nickname*/ None,
            /*agent_role*/ None,
            /*is_closed*/ false,
        );
        state.upsert(
            first_agent_id,
            Some("Robie".to_string()),
            Some("explorer".to_string()),
            /*is_closed*/ false,
        );
        state.upsert(
            second_agent_id,
            Some("Bob".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ false,
        );

        (state, main_thread_id, first_agent_id, second_agent_id)
    }

    #[test]
    fn upsert_preserves_first_seen_order() {
        let (mut state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

        state.upsert(
            first_agent_id,
            Some("Robie".to_string()),
            Some("worker".to_string()),
            /*is_closed*/ true,
        );

        assert_eq!(
            state.ordered_thread_ids(),
            vec![main_thread_id, first_agent_id, second_agent_id]
        );
    }

    #[test]
    fn parent_owned_state_is_removed_with_thread_metadata() {
        let (mut state, _main_thread_id, first_agent_id, second_agent_id) = populated_state();

        state.mark_parent_owned(first_agent_id);
        assert!(state.is_parent_owned(first_agent_id));
        state.remove(first_agent_id);
        assert!(!state.is_parent_owned(first_agent_id));

        state.mark_parent_owned(second_agent_id);
        state.clear();
        assert!(!state.is_parent_owned(second_agent_id));
    }

    #[test]
    fn picker_refresh_rejects_responses_from_before_clear() {
        let mut state = AgentNavigationState::default();
        let thread_id = ThreadId::new();
        let stale_request = state
            .begin_picker_refresh(thread_id)
            .expect("first picker refresh");

        assert_eq!(state.begin_picker_refresh(thread_id), None);
        state.clear();
        let current_request = state
            .begin_picker_refresh(thread_id)
            .expect("refresh after session reset");

        assert!(
            state
                .finish_picker_refresh(thread_id, stale_request)
                .is_none()
        );
        assert!(
            state
                .finish_picker_refresh(thread_id, current_request)
                .is_some()
        );
        state.set_picker_thread_visibility(ThreadId::new(), AgentPickerThreadVisibility::Hidden);
        assert!(state.picker_excluded_threads.is_empty());
    }

    #[test]
    fn adjacent_thread_id_wraps_in_spawn_order() {
        let (state, main_thread_id, first_agent_id, second_agent_id) = populated_state();

        assert_eq!(
            state.adjacent_thread_id(Some(second_agent_id), AgentNavigationDirection::Next),
            Some(main_thread_id)
        );
        assert_eq!(
            state.adjacent_thread_id(Some(second_agent_id), AgentNavigationDirection::Previous),
            Some(first_agent_id)
        );
        assert_eq!(
            state.adjacent_thread_id(Some(main_thread_id), AgentNavigationDirection::Previous),
            Some(second_agent_id)
        );

        let mut state = state;
        state.set_agent_path(first_agent_id, Some("/root/first".to_string()));
        state.set_picker_thread_visibility(first_agent_id, AgentPickerThreadVisibility::Hidden);
        assert!(
            state
                .ordered_path_backed_subagent_threads(Some(main_thread_id))
                .is_empty()
        );
    }

    #[test]
    fn picker_subtitle_mentions_shortcuts() {
        let previous: Span<'static> = previous_agent_shortcut().into();
        let next: Span<'static> = next_agent_shortcut().into();
        let subtitle = AgentNavigationState::picker_subtitle();

        assert!(subtitle.contains(previous.content.as_ref()));
        assert!(subtitle.contains(next.content.as_ref()));
    }

    #[test]
    fn active_agent_label_tracks_current_thread() {
        let (state, main_thread_id, first_agent_id, _) = populated_state();

        assert_eq!(
            state.active_agent_label(Some(first_agent_id), Some(main_thread_id)),
            Some("Robie [explorer]".to_string())
        );
        assert_eq!(
            state.active_agent_label(Some(main_thread_id), Some(main_thread_id)),
            Some("root".to_string())
        );
    }

    fn quick_switch_state(count: usize) -> (AgentNavigationState, ThreadId, Vec<ThreadId>) {
        let mut state = AgentNavigationState::default();
        let root = ThreadId::new();
        state.upsert(root, None, None, false);
        let agents: Vec<_> = (1..=count).map(|_| ThreadId::new()).collect();
        for (index, id) in agents.iter().copied().enumerate() {
            state.upsert(id, None, None, false);
            state.set_agent_path(id, Some(format!("/root/a{}", index + 1)));
            state.note_updated_at(id, 100_000);
        }
        state.set_quick_switch_width(512);
        (state, root, agents)
    }

    #[test]
    fn agent_footer_colors_preserve_selection_and_slot_identity_across_states() {
        let (mut state, root, agents) = quick_switch_state(1);
        let child = agents[0];
        let selected_style = Style::default().fg(crate::style::accent_color()).bold().not_dim();
        let statuses = [
            (ThreadStatus::Active { active_flags: vec![] }, crate::style::agent_working_style()),
            (ThreadStatus::Active { active_flags: vec![ThreadActiveFlag::WaitingOnApproval] }, crate::style::agent_waiting_style()),
            (ThreadStatus::Active { active_flags: vec![ThreadActiveFlag::WaitingOnUserInput] }, crate::style::agent_waiting_style()),
            (ThreadStatus::SystemError, crate::style::status_style(crate::style::StatusTone::Failure).not_bold().not_dim()),
            (ThreadStatus::Idle, crate::style::secondary_text_style()),
            (ThreadStatus::NotLoaded, crate::style::secondary_text_style().dim()),
        ];
        for (status, expected) in statuses {
            state.set_running(child, matches!(status, ThreadStatus::Active { .. }));
            let line = state.quick_switch_line_with_status(Some(root), Some(root), |id| (id == child).then_some(&status)).unwrap();
            assert_eq!(line.to_string(), "[0 root] [1 /a1]");
            assert_eq!(line.spans[0].style, selected_style);
            assert_eq!(line.spans[2].style, expected);
            assert!(!line.spans[2].style.add_modifier.contains(ratatui::style::Modifier::BOLD));
            let selected = state.quick_switch_line_with_status(Some(child), Some(root), |_| Some(&status)).unwrap();
            assert_eq!(selected.spans[2].style, selected_style);
            assert_eq!(state.quick_switch_thread(1), Some(child));
        }
        state.mark_closed(child);
        let closed = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(closed.spans[2].style, crate::style::secondary_text_style().dim());
        let selected_closed = state.quick_switch_line(Some(child), Some(root)).unwrap();
        assert_eq!(selected_closed.spans[2].style, selected_style);
    }

    #[test]
    fn agent_footer_colors_use_live_activity_when_metadata_is_missing_or_stale() {
        let (mut state, root, agents) = quick_switch_state(1);
        let child = agents[0];
        assert_eq!(state.quick_switch_line(Some(root), Some(root)).unwrap().spans[2].style,
            crate::style::secondary_text_style());
        state.mark_running(child);
        for status in [None, Some(ThreadStatus::Idle), Some(ThreadStatus::NotLoaded), Some(ThreadStatus::SystemError)] {
            let line = state.quick_switch_line_with_status(Some(root), Some(root), |_| status.as_ref()).unwrap();
            assert_eq!(line.spans[2].style, crate::style::agent_working_style());
        }
        state.mark_stopped(child);
        let stale = ThreadStatus::Active { active_flags: vec![ThreadActiveFlag::WaitingOnApproval] };
        let stopped = state.quick_switch_line_with_status(Some(root), Some(root), |_| Some(&stale)).unwrap();
        assert_eq!(stopped.spans[2].style, crate::style::secondary_text_style());
        state.mark_closed(child);
        let line = state.quick_switch_line_with_status(Some(root), Some(root), |_| Some(&stale)).unwrap();
        assert_eq!(line.spans[2].style, crate::style::secondary_text_style().dim());
    }

    #[test]
    fn quick_switch_root_is_zero_even_without_subagents() {
        let (mut state, root, _) = quick_switch_state(0);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root]");
        assert_eq!(state.quick_switch_thread(0), Some(root));
        assert_eq!(state.quick_switch_thread(1), None);
        assert!(
            line.spans[0]
                .style
                .add_modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn quick_switch_uses_remaining_width_instead_of_a_three_agent_limit() {
        let (mut state, root, agents) = quick_switch_state(12);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert!(line.to_string().contains("[9 /a9]"));
        assert_eq!(state.quick_switch_thread(9), Some(agents[8]));
        assert_eq!(state.quick_switch_thread(10), None);

        state.set_quick_switch_width(32); // root plus three complete /aN labels
        let narrow = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(narrow.width(), 32);
        assert_eq!(narrow.to_string(), "[0 root] [1 /a1] [2 /a2] [3 /a3]");
        state.set_quick_switch_width(24);
        let narrower = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(narrower.to_string(), "[0 root] [1 /a1] [2 /a2]");
        assert_eq!(state.quick_switch_thread(3), None);
    }

    #[test]
    fn quick_switch_initially_prioritizes_active_agents_and_recent_updates() {
        let (mut state, root, agents) = quick_switch_state(5);
        for index in [1, 3, 4] {
            state.set_running(agents[index], true);
        }
        state.note_updated_at(agents[3], 130_000);
        state.note_updated_at(agents[4], 120_000);
        state.set_quick_switch_width(32);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root] [1 /a4] [2 /a5] [3 /a2]");
    }

    #[test]
    fn quick_switch_active_arrival_replaces_inactive_without_moving_other_slots() {
        let (mut state, root, agents) = quick_switch_state(4);
        state.set_quick_switch_width(32);
        state.quick_switch_line(Some(root), Some(root));
        state.set_running(agents[3], true);
        state.quick_switch_line(Some(root), Some(root));
        assert_eq!(state.quick_switch_thread(1), Some(agents[0]));
        assert_eq!(state.quick_switch_thread(2), Some(agents[1]));
        assert_eq!(state.quick_switch_thread(3), Some(agents[3]));
    }

    #[test]
    fn quick_switch_recent_active_replacement_requires_more_than_ten_seconds() {
        let (mut state, root, agents) = quick_switch_state(4);
        for id in &agents {
            state.set_running(*id, true);
        }
        state.set_quick_switch_width(32);
        state.quick_switch_line(Some(root), Some(root));
        state.note_updated_at(agents[3], 110_000);
        state.quick_switch_line(Some(root), Some(root));
        assert_eq!(state.quick_switch_thread(3), Some(agents[2]));
        state.note_updated_at(agents[3], 110_001);
        state.quick_switch_line(Some(root), Some(root));
        assert_eq!(state.quick_switch_thread(3), Some(agents[3]));
        assert_eq!(state.quick_switch_thread(1), Some(agents[0]));
        assert_eq!(state.quick_switch_thread(2), Some(agents[1]));
        state.note_updated_at(agents[0], 500_000);
        state.quick_switch_line(Some(root), Some(root));
        assert_eq!(state.quick_switch_thread(1), Some(agents[0]));
    }

    #[test]
    fn quick_switch_stale_metadata_and_repeated_redraws_do_not_reshuffle() {
        let (mut state, root, agents) = quick_switch_state(4);
        state.set_quick_switch_width(32);
        state.quick_switch_line(Some(root), Some(root));
        let slots = state.quick_switch_slots;
        state.note_updated_at(agents[0], 90_000);
        for _ in 0..10 {
            state.quick_switch_line(Some(agents[1]), Some(root));
            assert_eq!(state.quick_switch_slots, slots);
        }
        assert_eq!(state.last_updates[&agents[0]], 100_000);
    }

    #[test]
    fn quick_switch_hidden_and_removed_agents_lose_their_shortcuts() {
        let (mut state, root, agents) = quick_switch_state(3);
        state.quick_switch_line(Some(root), Some(root));
        state.set_picker_thread_visibility(agents[1], AgentPickerThreadVisibility::Hidden);
        assert_eq!(state.quick_switch_thread(2), None);
        state.quick_switch_line(Some(root), Some(root));
        assert_eq!(state.quick_switch_thread(3), Some(agents[2]));
        state.remove(agents[2]);
        assert_eq!(state.quick_switch_thread(3), None);
        state.clear();
        assert_eq!(state.quick_switch_thread(0), None);
    }

    #[test]
    fn quick_switch_resize_keeps_the_number_of_a_shorter_agent_that_still_fits() {
        let (mut state, root, agents) = quick_switch_state(2);
        state.set_agent_path(agents[0], Some("/root/schema_api_review".into()));
        state.set_agent_path(agents[1], Some("/root/review".into()));
        state.quick_switch_line(Some(root), Some(root));
        assert_eq!(state.quick_switch_thread(2), Some(agents[1]));
        state.set_quick_switch_width(20);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root] [2 /review]");
        assert_eq!(state.quick_switch_thread(1), None);
        assert_eq!(state.quick_switch_thread(2), Some(agents[1]));
        assert!(state.quick_switch_line(None, None).is_none());
        assert_eq!(state.quick_switch_thread(0), None);
        assert_eq!(state.quick_switch_thread(2), None);
    }

    #[test]
    fn quick_switch_fits_unicode_paths_and_keeps_nested_names() {
        let (mut state, root, agents) = quick_switch_state(2);
        state.set_agent_path(agents[0], Some("/root/审查".into()));
        state.set_agent_path(agents[1], Some("/root/review/nested".into()));
        state.set_quick_switch_width(18); // root plus the five-column path /审查
        let line = state.quick_switch_line(Some(agents[0]), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root] [1 /审查]");
        assert_eq!(line.width(), 18);
        state.set_quick_switch_width(80);
        let line = state.quick_switch_line(Some(agents[0]), Some(root)).unwrap();
        assert!(line.to_string().contains("[2 /review/nested]"));
    }

    #[test]
    fn quick_switch_shortens_to_admit_active_agents_and_restores_full_names_when_wider() {
        let (mut state, root, agents) = quick_switch_state(2);
        state.set_agent_path(agents[0], Some("/root/schema_api_review".into()));
        state.set_agent_path(agents[1], Some("/root/review".into()));
        for id in &agents {
            state.set_running(*id, true);
        }
        let full = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(full.to_string(), "[0 root] [1 /schema_api_review] [2 /review]");
        state.set_quick_switch_width(33);
        let narrow = state.quick_switch_line(Some(agents[1]), Some(root)).unwrap();
        assert_eq!(narrow.to_string(), "[0 root] [1 /schema…] [2 /review]");
        assert_eq!(narrow.width(), 33);
        assert_eq!(state.quick_switch_thread(1), Some(agents[0]));
        assert_eq!(state.quick_switch_thread(2), Some(agents[1]));
        assert!(
            narrow.spans.last().unwrap().style.add_modifier.contains(ratatui::style::Modifier::BOLD)
        );
        for _ in 0..3 {
            state.note_updated_at(agents[1], 500_000);
            assert_eq!(
                state.quick_switch_line(Some(root), Some(root)).unwrap().to_string(),
                narrow.to_string()
            );
        }
        state.set_quick_switch_width(80);
        assert_eq!(
            state.quick_switch_line(Some(root), Some(root)).unwrap().to_string(),
            full.to_string()
        );
    }

    #[test]
    fn quick_switch_shortening_protects_active_arrivals_without_evicting_existing_slots() {
        let (mut state, root, agents) = quick_switch_state(2);
        state.set_agent_path(agents[0], Some("/root/schema_api_review".into()));
        state.set_agent_path(agents[1], Some("/root/review".into()));
        state.set_quick_switch_width(33);
        let initial = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(initial.to_string(), "[0 root] [1 /schema_api_review]");
        state.set_running(agents[1], true);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root] [1 /schema…] [2 /review]");
        assert_eq!(state.quick_switch_thread(1), Some(agents[0]));
        assert_eq!(state.quick_switch_thread(2), Some(agents[1]));
    }

    #[test]
    fn quick_switch_shared_prefixes_keep_distinguishing_suffixes() {
        let (mut state, root, agents) = quick_switch_state(2);
        for (id, path) in agents
            .iter()
            .zip(["/root/schema_api_review", "/root/schema_api_rewrite"])
        {
            state.set_agent_path(*id, Some(path.into()));
            state.set_running(*id, true);
        }
        state.set_quick_switch_width(47);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root] [1 /schema…review] [2 /schema…rewrite]");
        assert_eq!(line.width(), 47);
        let labels = state.quick_switch_labels(&state.quick_switch_slots, root, true);
        assert_ne!(labels[0], labels[1]);
        state.set_quick_switch_width(35);
        let narrower = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(narrower.to_string(), "[0 root] [1 /…review] [2 /…rewrite]");
        assert_eq!(narrower.width(), 35);
        assert_eq!(state.quick_switch_thread(1), Some(agents[0]));
        assert_eq!(state.quick_switch_thread(2), Some(agents[1]));
    }

    #[test]
    fn quick_switch_abbreviations_cannot_collide_with_a_complete_name() {
        let (mut state, root, agents) = quick_switch_state(2);
        state.set_agent_path(agents[0], Some("/root/schema_api_review".into()));
        state.set_agent_path(agents[1], Some("/root/schema…".into()));
        for id in &agents {
            state.set_running(*id, true);
        }
        state.set_quick_switch_width(40);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root] [1 /schema…review] [2 /schema…]");
        assert_eq!(line.width(), 40);
    }

    #[test]
    fn quick_switch_shortening_threshold_and_boundaries_are_unicode_safe() {
        assert!(AgentNavigationState::quick_switch_label_variants("/api_review").is_empty());
        assert!(!AgentNavigationState::quick_switch_label_variants("/api_reviews").is_empty());
        assert!(AgentNavigationState::quick_switch_label_variants("/abcdefghijklmnop").is_empty());
        for separator in ["_", "-", "/", ".", " "] {
            let variants = AgentNavigationState::quick_switch_label_variants(
                &format!("/schema{separator}review"),
            );
            assert!(variants.contains(&"/schema…".to_string()));
        }
        let combining = AgentNavigationState::quick_switch_label_variants("/e\u{301}valuation_schema_review");
        assert!(combining.iter().all(|label| {
            label.starts_with("/e\u{301}valuation") || label.starts_with("/…")
        }));
        let (mut state, root, agents) = quick_switch_state(2);
        state.set_agent_path(agents[0], Some("/root/审查流程_接口_review".into()));
        state.set_agent_path(agents[1], Some("/root/review".into()));
        for id in &agents {
            state.set_running(*id, true);
        }
        state.set_quick_switch_width(35);
        let line = state.quick_switch_line(Some(root), Some(root)).unwrap();
        assert_eq!(line.to_string(), "[0 root] [1 /审查流程…] [2 /review]");
        assert_eq!(line.width(), 35);
    }

    #[test]
    fn quick_switch_accepts_ctrl_digits_without_stealing_plain_digits_or_escape() {
        for digit in '0'..='9' {
            let event = KeyEvent::new(KeyCode::Char(digit), KeyModifiers::CONTROL);
            assert_eq!(
                AgentNavigationState::quick_switch_index(event),
                Some((digit as u8 - b'0') as usize)
            );
            assert_eq!(
                AgentNavigationState::quick_switch_index(KeyEvent::new(
                    KeyCode::Char(digit), KeyModifiers::NONE,
                )),
                None
            );
        }
        for event in [
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Null, KeyModifiers::CONTROL),
        ] {
            assert_eq!(AgentNavigationState::quick_switch_index(event), None);
        }
        let mut release = KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL);
        release.kind = KeyEventKind::Release;
        assert_eq!(AgentNavigationState::quick_switch_index(release), None);
    }
}
