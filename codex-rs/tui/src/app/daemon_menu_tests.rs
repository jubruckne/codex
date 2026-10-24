use super::*;
use crate::app::test_support::make_test_app;
use crate::chatwidget::tests::helpers::render_bottom_popup;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use crate::status::remote_connection::RemoteConnectionStatus;
use crossterm::event::KeyCode;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn client_only_daemon_version_notice_preserves_actual_versions() {
    let mut app = make_test_app().await;
    app.app_server_target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: false,
        endpoint: crate::RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::relative_to_current_dir("codex.sock").unwrap(),
        },
    };
    for (client, server) in [("0.155.0", "0.154.0"), ("0.0.0", "0.161.0")] {
        app.local_settings.tui.show_server_version_notice = true;
        assert_eq!(
            app.initialize_server_version_notice(client, Some(server)),
            None
        );
        assert_eq!(
            app.agents_overview
                .view_state
                .lock()
                .unwrap()
                .server_version_notice,
            None
        );
        assert_eq!(app.pending_update_action, None);
    }
}

#[tokio::test]
async fn client_only_daemon_menu_is_read_only_even_with_cli_identity() {
    let mut app = make_test_app().await;
    let (chat, _, mut rx, _) = make_chatwidget_manual_with_sender().await;
    app.chat_widget = chat;
    // Even a valid executable identity cannot enable lifecycle actions.
    app.daemon_cli_executable =
        Some(AbsolutePathBuf::from_absolute_path(std::env::current_exe().unwrap()).unwrap());
    app.chat_widget.remote_connection = Some(RemoteConnectionStatus {
        address: "local".into(),
        version: "v0.161.0".into(),
        is_local_daemon: true,
        is_managed_daemon: true,
    });
    app.open_daemon_menu();
    let popup = render_bottom_popup(&app.chat_widget, 100);
    assert!(popup.contains("Use the official Codex CLI"));
    assert!(!popup.contains("Install latest"));
    assert!(!popup.contains("Use this CLI build"));
    assert!(!popup.contains("Restart background server"));
    app.chat_widget.handle_key_event(KeyCode::Enter.into());
    assert!(rx.try_recv().is_err());
    for source in [
        DaemonUpdateSource::ThisCli,
        DaemonUpdateSource::PublicStable,
    ] {
        app.confirm_daemon_update(source);
        while let Ok(event) = rx.try_recv() {
            assert!(!matches!(
                event,
                AppEvent::ConfirmDaemonUpdate(_) | AppEvent::RunDaemonUpdate(_)
            ));
        }
        assert_eq!(app.pending_update_action, None);
    }
}

#[tokio::test]
async fn client_only_unavailable_daemon_menu_offers_guidance_without_update_actions() {
    let mut app = make_test_app().await;
    app.chat_widget.remote_connection = None;
    app.open_daemon_menu();
    let popup = render_bottom_popup(&app.chat_widget, 100);
    assert!(popup.contains("Not connected"));
    assert!(popup.contains("cannot manage the daemon"));
    assert_eq!(app.pending_update_action, None);
}

#[tokio::test]
async fn client_only_direct_daemon_events_cannot_exit_or_update() -> Result<()> {
    let mut app = make_test_app().await;
    let client = crate::tests::start_test_embedded_app_server(app.config.clone()).await?;
    let mut session = AppServerSession::new(
        crate::backend_id::BackendId::new(),
        codex_app_server_client::AppServerClient::InProcess(client),
        crate::app_server_session::ThreadParamsMode::Embedded,
    );
    let mut tui = crate::tui::test_support::make_test_tui()?;
    for event in [
        AppEvent::ConfirmDaemonUpdate(DaemonUpdateSource::ThisCli),
        AppEvent::RunDaemonUpdate(DaemonUpdateSource::PublicStable),
        AppEvent::ConfirmDaemonRestart,
        AppEvent::RunDaemonRestart,
    ] {
        assert!(matches!(
            app.handle_event(&mut tui, &mut session, event).await?,
            AppRunControl::Continue
        ));
        assert_eq!(app.pending_update_action, None);
        assert!(app.daemon_restart.is_none());
    }
    session.shutdown().await?;
    Ok(())
}
