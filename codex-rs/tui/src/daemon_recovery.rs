//! Client-only compatibility checks report errors; recovery never restarts or falls back.

use crate::AppServerTarget;
#[cfg(test)]
use crate::app_event::AppEvent;
#[cfg(test)]
use crate::app_event_sender::AppEventSender;
#[cfg(test)]
use crate::bottom_pane::BottomPaneView;
#[cfg(test)]
use crate::bottom_pane::ListSelectionView;
#[cfg(test)]
use crate::bottom_pane::SelectionItem;
#[cfg(test)]
use crate::bottom_pane::SelectionViewParams;
use crate::daemon_startup;
#[cfg(test)]
use crate::daemon_startup::CompatibilityError;
#[cfg(test)]
use crate::keymap::RuntimeKeymap;
use crate::legacy_core::config::Config;
#[cfg(test)]
use crate::render::renderable::ColumnRenderable;
#[cfg(test)]
use crate::render::renderable::Renderable;
use crate::startup_draft::StartupDraft;
#[cfg(test)]
use ratatui::style::Stylize;
#[cfg(test)]
use ratatui::text::Line;
#[cfg(test)]
use ratatui::widgets::Paragraph;
#[cfg(test)]
use ratatui::widgets::Wrap;
use std::io;
#[cfg(test)]
use tokio::sync::mpsc::unbounded_channel;

pub(super) async fn check(
    startup: &mut StartupDraft,
    target: &AppServerTarget,
    config: &Config,
    cli_kv_overrides: &[(String, toml::Value)],
    mut managed_daemon: bool,
) -> io::Result<Option<String>> {
    let _ = managed_daemon;
    startup
        .run_until(daemon_startup::compatibility_warning(
            target,
            config,
            cli_kv_overrides,
        ))
        .await?
        .map_err(io::Error::other)
}

#[cfg(test)]
fn recovery_view(
    issue: &CompatibilityError,
    managed_daemon: bool,
    keymap: &RuntimeKeymap,
) -> ListSelectionView {
    let can_restart = managed_daemon && issue.restart_features.is_some();
    let mut header = ColumnRenderable::new();
    header.push(Line::from(
        if issue.restart_features.is_some() {
            "Background server has incompatible feature settings"
        } else {
            "Cannot use the background server"
        }
        .bold(),
    ));
    header.push(Paragraph::new(issue.reason.clone()).wrap(Wrap { trim: false }));
    if let Some(features) = &issue.restart_features {
        header.push(Paragraph::new(
            "These settings persist and can disable functionality for other clients. Restart may interrupt active or queued work."
        ).wrap(Wrap { trim: false }));
        header.push(Line::from(
            "Restart will use these shared feature settings:",
        ));
        let mut remaining = features.iter();
        for (name, enabled) in remaining.by_ref().take(3) {
            header.push(Line::from(format!("  {name} = {enabled}").dim()));
        }
        let count = remaining.len();
        if count > 0 {
            header.push(Line::from(format!("  and {count} more").dim()));
        }
    }
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    ListSelectionView::new(
        SelectionViewParams {
            header: Box::new(header),
            initial_selected_idx: Some(2),
            items: vec![
                SelectionItem {
                    name: "Run without daemon this time".to_string(),
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: if can_restart {
                        "Restart with these settings (recommended)"
                    } else {
                        "Restart with these settings"
                    }
                    .to_string(),
                    dismiss_on_select: true,
                    require_explicit_confirmation: true,
                    is_disabled: !can_restart,
                    disabled_reason: if !managed_daemon {
                        Some("This server is not managed by Codex.".to_string())
                    } else if issue.restart_features.is_none() {
                        Some("Restart cannot resolve this compatibility check.".to_string())
                    } else {
                        None
                    },
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..SelectionViewParams::picker()
        },
        AppEventSender::new(tx),
        keymap.list.clone(),
    )
}

#[cfg(test)]
#[path = "daemon_recovery_tests.rs"]
mod tests;
