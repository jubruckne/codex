//! Local context reports never submit a model operation or acquire a thread writer.

use super::*;
use crate::context_summary::ContextSummaryCell;

impl ChatWidget {
    pub(super) fn start_context_summary(&mut self) {
        // Invalidate any older request even when this invocation cannot read a rollout.
        self.context_summary_request = None;
        let Some(thread_id) = self.thread_id else {
            self.add_info_message("Context breakdown unavailable: no active thread. Start or resume a conversation first.".to_string(), None);
            return;
        };
        if !self.local_worktree_operations {
            self.add_info_message("Context breakdown unavailable: this frontend cannot read the remote daemon's rollout.".to_string(), None);
            return;
        }
        let Some(path) = self.rollout_path() else {
            self.add_info_message("Context breakdown unavailable: no persisted rollout yet. Retry after the thread has saved history.".to_string(), None);
            return;
        };
        let request_id = Uuid::new_v4();
        self.context_summary_request = Some(request_id);
        let tx = self.app_event_tx.clone();
        let usage = self.token_info.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                crate::context_summary::load(&path, thread_id)
                    .map(|summary| ContextSummaryCell { summary, usage })
            })
            .await
            .unwrap_or_else(|_| {
                Err("The context reader could not complete. Retry /context.".to_string())
            });
            tx.send(AppEvent::ContextSummaryLoaded {
                thread_id,
                request_id,
                result,
            });
        });
    }

    pub(crate) fn finish_context_summary(
        &mut self,
        thread_id: ThreadId,
        request_id: Uuid,
        result: Result<ContextSummaryCell, String>,
    ) -> Option<Box<dyn HistoryCell>> {
        if self.thread_id != Some(thread_id) || self.context_summary_request != Some(request_id) {
            return None;
        }
        self.context_summary_request = None;
        match result {
            Ok(cell) => {
                self.bottom_pane.show_view(Box::new(
                    cell.into_panel(
                        self.app_event_tx.clone(),
                        self.bottom_pane.list_keymap(),
                        self.current_cwd
                            .as_deref()
                            .unwrap_or(self.config.cwd.as_path()),
                    ),
                ));
                None
            }
            Err(error) => Some(Box::new(history_cell::new_info_event(
                format!("Context breakdown unavailable: {error}"),
                None,
            ))),
        }
    }
}
