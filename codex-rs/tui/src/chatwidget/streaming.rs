//! Streaming transcript updates for `ChatWidget`.
//!
//! This module owns assistant, plan, and reasoning deltas, including stream-tail
//! cells, commit ticks, and interrupt deferral.

use super::*;
use crate::markdown_render::ListSpacing;

fn streamed_message_matches_completed(streamed: &str, completed: &str) -> bool {
    streamed == completed || streamed.strip_suffix('\n') == Some(completed)
}

impl ChatWidget {
    pub(super) fn on_reasoning_item_started(&mut self, id: String) {
        if self.status_state.reasoning_resume_turn_id.take().is_some()
            && self.status_state.reasoning_item_id.as_ref() != Some(&id)
        {
            self.on_agent_reasoning_final();
        }
        if self.status_state.reasoning_item_id.as_ref() == Some(&id) {
            return;
        }
        self.status_state.reasoning_item_id = Some(id);
        self.status_state.reasoning_recovered_after_refresh = false;
        self.reasoning_buffer.clear();
        self.reasoning_summary_parts.clear();
        self.restore_reasoning_status_header();
    }

    pub(super) fn restore_reasoning_status_header(&mut self) {
        if self.safety_buffering_is_waiting()
            || self.unified_exec_wait_streak.is_some()
            || self.status_state.compaction.is_some()
            || !self.status_state.pending_guardian_review_status.is_empty()
        {
            return;
        }
        if self.bottom_pane.is_task_running()
            || self.status_state.current_status.is_guardian_review()
        {
            self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Working;
            if self.status_state.current_status.header != "Working"
                || self.status_state.current_status.details.is_some()
                || self
                    .bottom_pane
                    .status_widget()
                    .is_none_or(|status| status.header() != "Working")
            {
                self.set_status_header(String::from("Working"));
            }
        }
    }

    /// Preserve received answer and plan source before ordinary turn termination.
    pub(super) fn flush_answer_and_plan_streams(&mut self) {
        self.flush_answer_stream_with_separator();
        if let Some(mut controller) = self.plan_stream_controller.take() {
            self.clear_active_stream_tail();
            let (cell, source) = controller.finalize();
            if let Some(cell) = cell {
                self.add_boxed_history(cell);
            }
            if let Some(source) = source {
                self.note_stream_consolidation_queued();
                self.app_event_tx
                    .send(AppEvent::ConsolidateProposedPlan(source));
            }
            self.request_pending_usage_output_insertion_after_stream_shutdown();
        }
    }

    pub(super) fn flush_answer_stream_with_separator(&mut self) {
        self.flush_answer_stream(/*completed_message*/ None);
    }

    fn flush_answer_stream(&mut self, completed_message: Option<&str>) {
        let had_stream_controller = self.stream_controller.is_some();
        if let Some(mut controller) = self.stream_controller.take() {
            let needs_scrollback_reflow = controller.needs_scrollback_reflow();
            self.clear_active_stream_tail();
            let (cell, streamed_source) = controller.finalize();
            let completed_display = completed_message.map(|source| {
                parse_assistant_markdown(source, self.config.cwd.as_path()).visible_markdown
            });
            let completed_message_differs = completed_display.as_deref().is_some_and(|completed| {
                let Some(streamed) = streamed_source.as_deref() else {
                    return true;
                };
                // Stream finalization supplies one trailing newline when the last delta omitted it.
                !streamed_message_matches_completed(streamed, completed)
            });
            let scrollback_reflow = if needs_scrollback_reflow || completed_message_differs {
                crate::app_event::ConsolidationScrollbackReflow::Required
            } else {
                crate::app_event::ConsolidationScrollbackReflow::IfResizeReflowRan
            };
            let deferred_history_cell =
                if scrollback_reflow == crate::app_event::ConsolidationScrollbackReflow::Required {
                    cell
                } else {
                    if let Some(cell) = cell {
                        self.add_boxed_history(cell);
                    }
                    None
                };
            // Consolidate the run of streaming AgentMessageCells into a single AgentMarkdownCell
            // that can re-render from source on resize.
            if completed_message.is_none() {
                self.transcript.last_flushed_agent_message = self
                    .transcript
                    .active_streamed_agent_message
                    .take()
                    .zip(streamed_source.clone());
            }
            let copy_source = completed_message.map(str::to_owned).or(streamed_source);
            let source = copy_source.as_deref().map(|source| {
                parse_assistant_markdown(source, self.config.cwd.as_path()).visible_markdown
            });
            if let Some(source) = source {
                let inline_visualization_context = self.thread_id.and_then(|thread_id| {
                    crate::inline_visualization::InlineVisualizationContext::from_config(
                        &self.config,
                        thread_id,
                    )
                });
                self.note_stream_consolidation_queued();
                self.app_event_tx.send(AppEvent::ConsolidateAgentMessage {
                    source,
                    copy_source,
                    cwd: self.config.cwd.to_path_buf(),
                    inline_visualization_context,
                    scrollback_reflow,
                    deferred_history_cell,
                });
            }
        }
        self.adaptive_chunking.reset();
        if had_stream_controller && self.stream_controllers_idle() {
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }
        if had_stream_controller {
            self.request_pending_usage_output_insertion_after_stream_shutdown();
        }
        if completed_message.is_some() {
            self.transcript.active_streamed_agent_message = None;
            self.transcript.last_flushed_agent_message = None;
        }
    }

    pub(super) fn stream_controllers_idle(&self) -> bool {
        self.stream_controller
            .as_ref()
            .map(|controller| controller.queued_lines() == 0)
            .unwrap_or(true)
            && self
                .plan_stream_controller
                .as_ref()
                .map(|controller| controller.queued_lines() == 0)
                .unwrap_or(true)
    }

    /// Restore the status indicator only after commentary completion is pending,
    /// the turn is still running, and all stream queues have drained.
    ///
    /// This gate prevents flicker while normal output is still actively
    /// streaming, but still restores a visible "working" affordance when a
    /// commentary block ends before the turn itself has completed.
    pub(super) fn maybe_restore_status_indicator_after_stream_idle(&mut self) {
        if !self.status_state.pending_status_indicator_restore
            || !self.bottom_pane.is_task_running()
            || !self.stream_controllers_idle()
        {
            return;
        }

        self.bottom_pane.ensure_status_indicator();
        self.set_status(
            self.status_state.current_status.header.clone(),
            self.status_state.current_status.details.clone(),
            StatusDetailsCapitalization::Preserve,
            self.status_state.current_status.details_max_lines,
        );
        self.status_state.pending_status_indicator_restore = false;
    }

    fn completed_agent_message_was_flushed(&self, message: &str) -> bool {
        self.transcript
            .last_completed_agent_message
            .as_ref()
            .zip(self.transcript.last_flushed_agent_message.as_ref())
            .is_some_and(|(completed, (flushed, streamed))| {
                completed == flushed && streamed_message_matches_completed(streamed, message)
            })
    }

    pub(super) fn finalize_completed_assistant_message(&mut self, message: Option<&str>) {
        if self.stream_controller.is_none()
            && let Some(message) = message
            && !message.is_empty()
            && !self.completed_agent_message_was_flushed(message)
        {
            let displayed =
                parse_assistant_markdown(message, self.config.cwd.as_path()).visible_markdown;
            self.handle_streaming_delta(displayed);
        }
        // Item completion is authoritative. Use it for consolidation so any
        // deltas dropped by a saturated transport cannot truncate the transcript.
        self.flush_answer_stream(message);
        self.handle_stream_finished();
        self.request_redraw();
    }

    pub(super) fn on_agent_message_delta(&mut self, delta: String) {
        self.handle_streaming_delta(delta);
    }

    pub(super) fn on_plan_delta(&mut self, delta: String) {
        if self.active_mode_kind() != ModeKind::Plan {
            return;
        }
        if !self.transcript.plan_item_active {
            self.transcript.plan_item_active = true;
            self.transcript.plan_delta_buffer.clear();
        }
        self.transcript.plan_delta_buffer.push_str(&delta);
        if self.plan_stream_controller.is_none() {
            // Before starting a plan stream, flush any active exec cell group.
            self.flush_unified_exec_wait_streak();
            self.flush_active_cell();
            self.plan_stream_controller = Some(
                PlanStreamController::new(
                    self.current_stream_width(/*reserved_cols*/ 4),
                    &self.config.cwd,
                    self.history_render_mode(),
                )
                .with_list_spacing(
                    if self.local_settings.transcript_mode.is_owned() {
                        ListSpacing::Compact
                    } else {
                        ListSpacing::AfterMultiline
                    },
                ),
            );
        }
        let changed = self
            .plan_stream_controller
            .as_mut()
            .is_some_and(|controller| controller.push(&delta));
        if (changed || delta.contains('\n')) && self.sync_active_stream_tail() {
            self.request_redraw();
        }
        if changed {
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
            self.run_catch_up_commit_tick();
        }
    }

    pub(super) fn on_plan_item_completed(&mut self, text: String) {
        let (plan_text, source) = if text.trim().is_empty() {
            (
                self.transcript.plan_delta_buffer.trim().to_string(),
                self.transcript.plan_delta_buffer.clone(),
            )
        } else {
            (text.clone(), text)
        };
        if !plan_text.trim().is_empty() {
            self.transcript
                .record_agent_markdown(plan_text.clone(), source);
            self.transcript.latest_proposed_plan_markdown = Some(plan_text.clone());
        }
        // Preserve upstream stream-idle restoration bookkeeping for plan output so
        // completion can restore it once stream queues are idle.
        let should_restore_after_stream = self.plan_stream_controller.is_some();
        self.transcript.plan_delta_buffer.clear();
        self.transcript.plan_item_active = false;
        self.transcript.saw_plan_item_this_turn = true;
        let (finalized_streamed_cell, consolidated_plan_source) =
            if let Some(mut controller) = self.plan_stream_controller.take() {
                let source_only = controller.has_live_tail() && controller.tail_starts_stream();
                self.clear_active_stream_tail();
                let (cell, source) = controller.finalize();
                if source_only {
                    (None, source)
                } else {
                    (cell, source)
                }
            } else {
                (None, None)
            };
        if let Some(cell) = finalized_streamed_cell {
            self.add_boxed_history(cell);
            // TODO: Replace streamed output with the final plan item text if plan streaming is
            // removed or if we need to reconcile mismatches between streamed and final content.
            if let Some(source) = consolidated_plan_source {
                self.note_stream_consolidation_queued();
                self.app_event_tx
                    .send(AppEvent::ConsolidateProposedPlan(source));
            }
        } else if !plan_text.is_empty() {
            self.add_to_history(history_cell::new_proposed_plan(plan_text, &self.config.cwd));
        } else if let Some(source) = consolidated_plan_source {
            self.note_stream_consolidation_queued();
            self.app_event_tx
                .send(AppEvent::ConsolidateProposedPlan(source));
        }
        if should_restore_after_stream {
            self.status_state.pending_status_indicator_restore = true;
            self.maybe_restore_status_indicator_after_stream_idle();
            self.request_pending_usage_output_insertion_after_stream_shutdown();
        }
    }

    pub(super) fn on_agent_reasoning_delta(&mut self, delta: String) {
        if delta.is_empty() {
            return;
        }
        self.reasoning_buffer.push_str(&delta);
        self.sync_live_reasoning();
        self.restore_reasoning_status_header();
    }

    fn sync_live_reasoning(&mut self) {
        let mut parts = self.reasoning_summary_parts.clone();
        parts.push(self.reasoning_buffer.clone());
        let mut cell = history_cell::new_reasoning_summary_block(parts, &self.config.cwd);
        if let Some(id) = &self.status_state.reasoning_item_id {
            cell.set_source_item_id(id.clone());
        }
        // FrameRequester coalesces deltas; the existing transcript revision invalidates the
        // overlay tail only when source changes. No scrollback insertion/reflow until completion.
        self.status_state.live_reasoning = Some(cell);
        self.bump_active_cell_revision();
        self.request_redraw();
    }

    pub(super) fn on_agent_reasoning_final(&mut self) {
        // Replace the render-only live tail with one authoritative, identified history cell.
        if !self.reasoning_buffer.is_empty() {
            self.reasoning_summary_parts
                .push(std::mem::take(&mut self.reasoning_buffer));
        }
        self.status_state.live_reasoning = None;
        self.bump_active_cell_revision();
        if !self.reasoning_summary_parts.is_empty() {
            let reasoning_parts = std::mem::take(&mut self.reasoning_summary_parts);
            let mut cell =
                history_cell::new_reasoning_summary_block(reasoning_parts, &self.config.cwd);
            if let Some(id) = &self.status_state.reasoning_item_id {
                cell.set_source_item_id(id.clone());
            }
            let cell: Box<dyn HistoryCell> = cell;
            let result = match self.transcript.active_cell.as_mut() {
                Some(active) => active.append_reasoning(cell),
                None => Err(cell),
            };
            match result {
                Ok(()) => self.bump_active_cell_revision(),
                Err(cell) => self.add_boxed_history(cell),
            }
        }
        self.reasoning_buffer.clear();
        // Keep the last useful summary through tools and later empty items.
        self.status_state.reasoning_item_id = None;
        self.status_state.reasoning_resume_turn_id = None;
        self.status_state.reasoning_recovered_after_refresh = false;
        self.reasoning_summary_parts.clear();
        self.request_redraw();
    }

    pub(super) fn on_reasoning_section_break(&mut self) {
        // Preserve summary part boundaries in the render-only tail and completed transcript.
        if !self.reasoning_buffer.is_empty() {
            self.reasoning_summary_parts
                .push(std::mem::take(&mut self.reasoning_buffer));
        }
    }

    pub(super) fn on_stream_error(&mut self, message: String, additional_details: Option<String>) {
        self.status_state.remember_retry_status_header();
        self.bottom_pane.ensure_status_indicator();
        self.status_state.terminal_title_status_kind = TerminalTitleStatusKind::Thinking;
        self.set_status(
            message,
            additional_details,
            StatusDetailsCapitalization::CapitalizeFirst,
            STATUS_DETAILS_DEFAULT_MAX_LINES,
        );
    }

    /// Handle completion of an `AgentMessage` turn item.
    ///
    /// Commentary completion sets a deferred restore flag so the status row
    /// returns once stream queues are idle. Final-answer completion (or absent
    /// phase for legacy models) clears the flag to preserve historical behavior.
    pub(super) fn on_agent_message_item_completed(
        &mut self,
        item: AgentMessageItem,
        turn_id: &str,
        from_replay: bool,
    ) {
        if !from_replay && let Some(questions) = &item.questions {
            self.add_async_questions(&item.id, questions);
        }
        self.transcript.last_completed_agent_message = Some((turn_id.to_string(), item.id.clone()));
        let mut message = String::new();
        for content in &item.content {
            match content {
                AgentMessageContent::Text { text } => message.push_str(text),
            }
        }
        let parsed = parse_assistant_markdown(&message, self.config.cwd.as_path());
        if from_replay
            && self.stream_controller.is_none()
            && !parsed.visible_markdown.is_empty()
            && !self.completed_agent_message_was_flushed(&message)
        {
            self.prepare_assistant_message();
            self.mark_safety_buffering_agent_message_started();
            let context = self.thread_id.and_then(|thread_id| {
                crate::inline_visualization::InlineVisualizationContext::from_config(
                    &self.config,
                    thread_id,
                )
            });
            self.add_to_history(
                history_cell::AgentMarkdownCell::new_with_inline_visualizations(
                    parsed.visible_markdown.clone(),
                    self.config.cwd.as_path(),
                    context,
                )
                .with_copy_source(Some(message.clone())),
            );
            self.handle_stream_finished();
            self.request_redraw();
        } else {
            self.finalize_completed_assistant_message(Some(&message));
        }
        if !parsed.visible_markdown.is_empty() {
            self.transcript
                .record_agent_markdown(parsed.visible_markdown.clone(), message);
        }
        if !from_replay
            && let Some(cwd) = parsed.last_created_branch_cwd()
            && let Some(thread_id) = self.thread_id
            && let Some(runner) = self.workspace_command_runner.clone()
        {
            let branch_cwd = PathBuf::from(cwd);
            let cwd = self.config.cwd.to_path_buf();
            let tx = self.app_event_tx.clone();
            tokio::spawn(async move {
                if let Some(branch) =
                    crate::branch_summary::current_branch_name(runner.as_ref(), &branch_cwd).await
                {
                    tx.send(AppEvent::SyncThreadGitBranch {
                        thread_id,
                        branch,
                        cwd,
                    });
                }
            });
        }
        self.status_state.pending_status_indicator_restore = item.questions.is_some()
            || match item.phase {
                // Models that don't support preambles only output AgentMessageItems on turn completion.
                Some(MessagePhase::FinalAnswer) | None => {
                    !self.input_queue.pending_steers.is_empty()
                }
                Some(MessagePhase::Commentary | MessagePhase::PartialAnswer) => true,
            };
        self.maybe_restore_status_indicator_after_stream_idle();
    }

    /// Periodic tick for stream commits. In smooth mode this preserves one-line pacing, while
    /// catch-up mode drains larger batches to reduce queue lag.
    pub(crate) fn on_commit_tick(&mut self) {
        self.run_commit_tick();
    }

    /// Runs a regular periodic commit tick.
    pub(super) fn run_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::AnyMode);
    }

    /// Runs an opportunistic commit tick only if catch-up mode is active.
    pub(super) fn run_catch_up_commit_tick(&mut self) {
        self.run_commit_tick_with_scope(CommitTickScope::CatchUpOnly);
    }

    /// Runs a commit tick for the current stream queue snapshot.
    ///
    /// `scope` controls whether this call may commit in smooth mode or only when catch-up
    /// is currently active. The compact running status stays visible until the turn ends.
    pub(super) fn run_commit_tick_with_scope(&mut self, scope: CommitTickScope) {
        let now = Instant::now();
        let outcome = run_commit_tick(
            &mut self.adaptive_chunking,
            self.stream_controller.as_mut(),
            self.plan_stream_controller.as_mut(),
            scope,
            now,
        );
        for cell in outcome.cells {
            self.add_boxed_history(cell);
        }
        if scope == CommitTickScope::AnyMode || outcome.has_controller {
            self.sync_active_stream_tail();
        }

        if outcome.has_controller && outcome.all_idle {
            self.maybe_restore_status_indicator_after_stream_idle();
            self.app_event_tx.send(AppEvent::StopCommitAnimation);
        }

        if self.turn_lifecycle.agent_turn_running {
            self.refresh_runtime_metrics();
        }
    }

    pub(super) fn flush_interrupt_queue(&mut self) {
        let mut mgr = std::mem::take(&mut self.interrupts);
        mgr.flush_all(self);
        self.interrupts = mgr;
    }

    pub(super) fn flush_interrupt_activity(&mut self) {
        let mut mgr = std::mem::take(&mut self.interrupts);
        mgr.flush_activity(self);
        self.interrupts = mgr;
    }

    /// Move a lifecycle payload into the interrupt queue or its immediate handler.
    #[inline]
    pub(super) fn defer_or_handle<T>(
        &mut self,
        payload: T,
        push: impl FnOnce(&mut InterruptManager, T),
        handle: impl FnOnce(&mut Self, T),
    ) {
        // Preserve deterministic FIFO across queued interrupts: once anything
        // is queued due to an active write cycle, continue queueing until the
        // queue is flushed to avoid reordering (e.g., ExecEnd before ExecBegin).
        if self.stream_controller.is_some() || !self.interrupts.is_empty() {
            push(&mut self.interrupts, payload);
        } else {
            handle(self, payload);
        }
    }

    pub(super) fn handle_stream_finished(&mut self) {
        if self.task_complete_pending {
            self.bottom_pane.hide_status_indicator();
            self.task_complete_pending = false;
        }
        // A completed stream indicates non-exec content was just inserted.
        self.flush_interrupt_queue();
    }

    #[inline]
    pub(super) fn handle_streaming_delta(&mut self, delta: String) {
        if !delta.is_empty() {
            self.mark_safety_buffering_agent_message_started();
        }
        if self.stream_controller.is_none() {
            self.prepare_assistant_message();
            let inline_visualization_context = self.thread_id.and_then(|thread_id| {
                crate::inline_visualization::InlineVisualizationContext::from_config(
                    &self.config,
                    thread_id,
                )
            });
            self.stream_controller = Some(
                StreamController::new_with_inline_visualizations(
                    self.current_stream_width(/*reserved_cols*/ 2),
                    &self.config.cwd,
                    self.history_render_mode(),
                    inline_visualization_context,
                )
                .with_list_spacing(
                    if self.local_settings.transcript_mode.is_owned() {
                        ListSpacing::Compact
                    } else {
                        ListSpacing::AfterMultiline
                    },
                ),
            );
        }
        let changed = self
            .stream_controller
            .as_mut()
            .is_some_and(|controller| controller.push(&delta));
        if (changed || delta.contains('\n')) && self.sync_active_stream_tail() {
            self.request_redraw();
        }
        if changed {
            self.app_event_tx.send(AppEvent::StartCommitAnimation);
            self.run_catch_up_commit_tick();
        }
    }

    pub(super) fn active_cell_is_stream_tail(&self) -> bool {
        self.transcript.active_cell.as_ref().is_some_and(|cell| {
            cell.as_any().is::<history_cell::StreamingAgentTailCell>()
                || cell.as_any().is::<history_cell::StreamingPlanTailCell>()
        })
    }

    pub(super) fn has_active_stream_tail(&self) -> bool {
        (self.stream_controller.is_some() || self.plan_stream_controller.is_some())
            && self.active_cell_is_stream_tail()
    }

    pub(super) fn sync_active_stream_tail(&mut self) -> bool {
        if let Some(controller) = self.stream_controller.as_ref() {
            let tail_lines = controller.current_tail_lines();
            if tail_lines.is_empty() {
                return self.clear_active_stream_tail();
            }

            let cell = history_cell::StreamingAgentTailCell::new(
                tail_lines,
                controller.tail_starts_stream(),
            );
            if self
                .transcript
                .active_cell
                .as_ref()
                .and_then(|active| {
                    active
                        .as_any()
                        .downcast_ref::<history_cell::StreamingAgentTailCell>()
                })
                .is_some_and(|active| active == &cell)
            {
                return false;
            }
            self.transcript.active_cell = Some(Box::new(cell));
            self.bump_active_cell_revision();
            return true;
        }

        if let Some(controller) = self.plan_stream_controller.as_ref() {
            let tail_lines = controller.current_tail_display_lines();
            if tail_lines.is_empty() {
                return self.clear_active_stream_tail();
            }

            let cell = history_cell::StreamingPlanTailCell::new(
                tail_lines,
                !controller.tail_starts_stream(),
            );
            if self
                .transcript
                .active_cell
                .as_ref()
                .and_then(|active| {
                    active
                        .as_any()
                        .downcast_ref::<history_cell::StreamingPlanTailCell>()
                })
                .is_some_and(|active| active == &cell)
            {
                return false;
            }
            self.transcript.active_cell = Some(Box::new(cell));
            self.bump_active_cell_revision();
            return true;
        }

        self.clear_active_stream_tail()
    }

    pub(super) fn clear_active_stream_tail(&mut self) -> bool {
        if self.active_cell_is_stream_tail() {
            self.transcript.active_cell = None;
            self.bump_active_cell_revision();
            return true;
        }
        false
    }
}
