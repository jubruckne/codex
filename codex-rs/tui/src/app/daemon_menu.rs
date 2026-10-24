//! Local daemon maintenance confirms restarts and updates before acting.
//! Restarts use normal TUI reconnection; updates run after restoring the terminal.

use super::*;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::update_action::DaemonUpdateSource;
use crate::wrapping::word_wrap_lines;
use ratatui::buffer::Buffer;
use ratatui::widgets::Paragraph;

struct DaemonMenuHeader(Vec<Line<'static>>);

impl Renderable for DaemonMenuHeader {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Renderable::render(
            &Paragraph::new(word_wrap_lines(&self.0, usize::from(area.width))),
            area,
            buf,
        );
    }

    fn desired_height(&self, width: u16) -> u16 {
        word_wrap_lines(&self.0, usize::from(width)).len() as u16
    }
}

impl App {
    pub(super) async fn verify_managed_daemon(&mut self) {
        // A discovered socket alone does not prove it belongs to a managed daemon.
        // Probe only when the user opens a view that offers restart, never during startup.
        if !self.reconnect.offline
            && let Some(connection) = &mut self.chat_widget.remote_connection
            && connection.is_local_daemon
            && !connection.is_managed_daemon
        {
            connection.is_managed_daemon =
                codex_app_server_daemon::run(codex_app_server_daemon::LifecycleCommand::Version)
                    .await
                    .is_ok_and(|output| output.backend.is_some());
        }
    }

    pub(super) fn open_daemon_menu(&mut self) {
        let status = self
            .chat_widget
            .remote_connection
            .as_ref()
            .filter(|_| matches!(self.app_server_target, AppServerTarget::LocalDaemon { .. }))
            .map(|connection| format!("Running daemon: {}", connection.version))
            .unwrap_or_else(|| "Not connected to the local background server.".to_string());
        let mut header = vec![Line::from("Daemon".bold()), Line::from(status.dim())];
        header.push(Line::from(crate::client_only::DAEMON_GUIDANCE.dim()));
        let items = vec![SelectionItem {
            name: "Close".to_string(),
            dismiss_on_select: true,
            ..Default::default()
        }];
        self.chat_widget.show_selection_view(SelectionViewParams {
            header: Box::new(DaemonMenuHeader(header)),
            items,
            ..SelectionViewParams::picker()
        });
    }

    pub(super) fn confirm_daemon_restart(&mut self) {
        self.chat_widget
            .add_error_message(crate::client_only::DAEMON_GUIDANCE.to_string());
    }

    pub(super) fn confirm_daemon_update(&mut self, source: DaemonUpdateSource) {
        let _ = source;
        self.chat_widget
            .add_error_message(crate::client_only::DAEMON_GUIDANCE.to_string());
    }
}

#[cfg(test)]
#[path = "daemon_menu_tests.rs"]
mod tests;
