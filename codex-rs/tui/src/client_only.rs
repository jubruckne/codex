//! Invariants of the custom frontend. No config flag or UI action can opt out.
//! Thread/turn operations remain owned and persisted by the official daemon.

use crate::AppServerTarget;
use crate::Cli;
use crate::app_event::AppEvent;
use crate::legacy_core::config::edit::ConfigEditsBuilder;
use codex_app_server_client::RemoteAppServerEndpoint;
use codex_app_server_protocol::JSONRPCRequest;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::io;
use std::path::Path;

pub(crate) const ENABLED: bool = true;
pub(crate) const DAEMON_GUIDANCE: &str =
    "Codex++ cannot manage the daemon. Use the official Codex CLI.";
pub(crate) const CONFIG_GUIDANCE: &str = "Codex++ cannot change shared configuration, credentials or installation state. Use the official Codex CLI; session-only task settings remain available.";

pub(crate) fn unavailable() -> io::Error {
    io::Error::new(
        io::ErrorKind::NotConnected,
        "Cannot connect to the official Codex daemon.\n\nCodex++ requires an already-running official Codex service.\nStart it using the official Codex CLI, then retry.",
    )
}

pub(crate) async fn discover(home: &Path) -> io::Result<AbsolutePathBuf> {
    crate::maybe_probe_default_daemon_socket(home)
        .await
        .ok_or_else(unavailable)
}

pub(crate) fn validate_launch(
    cli: &Cli,
    remote: Option<&RemoteAppServerEndpoint>,
) -> io::Result<()> {
    if cli.no_daemon || cli.oss || cli.config_profile_v2.is_some() || remote.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Codex++ requires the existing official local daemon; --no-daemon, --oss, profiles and custom server endpoints are unsupported.",
        ));
    }
    Ok(())
}

pub(crate) fn validate_target(target: &AppServerTarget) -> io::Result<()> {
    match target {
        AppServerTarget::LocalDaemon {
            endpoint: RemoteAppServerEndpoint::UnixSocket { .. },
            allow_embedded_fallback: false,
        } => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Codex++ supports attachment to the official local daemon only; embedded execution and fallback are disabled.",
        )),
    }
}

pub(crate) fn deny_shared_write() -> anyhow::Result<()> {
    anyhow::bail!(CONFIG_GUIDANCE)
}

/// Every local legacy config writer uses this instead of the upstream apply method.
pub(crate) trait ProtectedConfigEdits {
    async fn reject_shared_config_write(self) -> anyhow::Result<()>;
}

impl ProtectedConfigEdits for ConfigEditsBuilder {
    async fn reject_shared_config_write(self) -> anyhow::Result<()> {
        deny_shared_write()
    }
}

/// Installed at the transport boundary, including raw RPCs and cloned request handles.
/// Mutations of ordinary tasks (interrupt, archive, thread settings) are intentionally allowed.
pub(crate) fn filter_request(request: &JSONRPCRequest) -> io::Result<()> {
    let method = request.method.as_str();
    let shared_config_write =
        codex_app_server_protocol::ClientRequest::try_from(request.clone()).is_ok_and(|request| {
            matches!(
                request.serialization_scope(),
                Some(codex_app_server_protocol::ClientRequestSerializationScope::Global("config"))
            )
        }) && !matches!(
            method,
            "plugin/skill/read"
                | "plugin/share/list"
                | "windowsSandbox/readiness"
                | "experimentalFeature/list"
        );
    let blocked = shared_config_write
        || (method.starts_with("config/") && method != "config/read")
        || method == "skills/config/write"
        || method.starts_with("externalAgentConfig/import")
            && method != "externalAgentConfig/import/readHistories"
        || matches!(
            method,
            "plugin/install"
                | "plugin/uninstall"
                | "plugin/reconcile"
                | "skills/extraRoots/set"
                | "experimentalFeature/enablement/set"
                | "marketplace/add"
                | "marketplace/remove"
                | "marketplace/upgrade"
                | "account/login/start"
                | "account/login/cancel"
                | "account/logout"
                | "account/gatewayOAuth/login"
                | "mcpServer/oauth/login"
                | "account/bedrock/setup"
                | "windowsSandbox/setupStart"
        )
        || method.starts_with("plugin/share/") && method != "plugin/share/list"
        || method.starts_with("appServer/daemon/");
    if blocked {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{CONFIG_GUIDANCE} Blocked operation: {method}."),
        ));
    }
    Ok(())
}

pub(crate) fn blocked_event(event: &AppEvent) -> Option<&'static str> {
    match event {
        AppEvent::ConfirmDaemonUpdate(_)
        | AppEvent::RunDaemonUpdate(_)
        | AppEvent::ConfirmDaemonRestart
        | AppEvent::RunDaemonRestart => Some(DAEMON_GUIDANCE),
        AppEvent::OpenExternalAgentConfigMigration => Some(CONFIG_GUIDANCE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use codex_app_server_protocol::RequestId;

    fn rpc(method: &str) -> JSONRPCRequest {
        JSONRPCRequest {
            id: RequestId::Integer(1),
            method: method.into(),
            params: None,
            trace: None,
        }
    }

    #[test]
    fn client_only_blocks_shared_rpc_mutations_and_preserves_tasks() {
        for method in [
            "config/batchWrite",
            "config/value/write",
            "skills/config/write",
            "externalAgentConfig/import",
            "externalAgentConfig/import/recordHistory",
            "plugin/install",
            "plugin/uninstall",
            "marketplace/upgrade",
            "account/login/start",
            "account/logout",
            "mcpServer/oauth/login",
            "account/bedrock/setup",
            "windowsSandbox/setupStart",
            "appServer/daemon/restart",
        ] {
            let err = filter_request(&rpc(method)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(err.to_string().contains("Use the official Codex CLI"));
        }
        for method in [
            "account/read",
            "config/read",
            "configRequirements/read",
            "model/list",
            "thread/start",
            "thread/resume",
            "thread/settings/update",
            "thread/archive",
            "thread/unsubscribe",
            "turn/start",
            "turn/interrupt",
            "turn/steer",
        ] {
            filter_request(&rpc(method)).unwrap();
        }
    }

    #[test]
    fn client_only_exclusive_config_reads_remain_allowed() {
        let mut request = rpc("experimentalFeature/list");
        request.params = Some(serde_json::json!({"cursor": null, "limit": 100, "threadId": null}));
        assert!(codex_app_server_protocol::ClientRequest::try_from(request.clone()).is_ok());
        filter_request(&request).unwrap();
        for method in ["windowsSandbox/readiness", "plugin/share/list"] {
            filter_request(&rpc(method)).unwrap();
        }
        for method in [
            "skills/extraRoots/set",
            "experimentalFeature/enablement/set",
        ] {
            assert!(filter_request(&rpc(method)).is_err());
        }
    }

    #[test]
    fn client_only_rejects_embedded_remote_and_fallback_targets() {
        let endpoint = RemoteAppServerEndpoint::UnixSocket {
            socket_path: AbsolutePathBuf::from_absolute_path("/tmp/official.sock").unwrap(),
        };
        validate_target(&AppServerTarget::LocalDaemon {
            endpoint: endpoint.clone(),
            allow_embedded_fallback: false,
        })
        .unwrap();
        for target in [
            AppServerTarget::Embedded,
            AppServerTarget::Remote {
                endpoint: endpoint.clone(),
            },
            AppServerTarget::LocalDaemon {
                endpoint,
                allow_embedded_fallback: true,
            },
        ] {
            assert!(validate_target(&target).is_err());
        }
    }

    #[test]
    fn client_only_rejects_embedded_launch_flags() {
        validate_launch(&Cli::parse_from(["codex++"]), None).unwrap();
        for flag in ["--no-daemon", "--oss"] {
            assert!(validate_launch(&Cli::parse_from(["codex++", flag]), None).is_err());
        }
    }

    #[tokio::test]
    async fn client_only_missing_daemon_leaves_home_untouched() {
        let home = tempfile::tempdir().unwrap();
        let config = "# older official config\nmodel = 'existing-model'\n";
        std::fs::write(home.path().join("config.toml"), config).unwrap();
        let err = discover(home.path()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("Start it using the official Codex CLI")
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("config.toml")).unwrap(),
            config
        );
        assert_eq!(std::fs::read_dir(home.path()).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn client_only_config_builder_cannot_write() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        let original = "# preserve this exactly\nmodel = 'old'\n";
        std::fs::write(&path, original).unwrap();
        let err = ConfigEditsBuilder::new(home.path())
            .set_model(Some("new"), None)
            .reject_shared_config_write()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("shared configuration"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }

    #[test]
    fn client_only_blocks_direct_daemon_events() {
        use crate::update_action::DaemonUpdateSource;
        assert!(blocked_event(&AppEvent::ConfirmDaemonRestart).is_some());
        assert!(blocked_event(&AppEvent::RunDaemonRestart).is_some());
        assert!(blocked_event(&AppEvent::RunDaemonUpdate(DaemonUpdateSource::ThisCli)).is_some());
        assert!(
            blocked_event(&AppEvent::ConfirmDaemonUpdate(
                DaemonUpdateSource::PublicStable
            ))
            .is_some()
        );
        assert!(blocked_event(&AppEvent::OpenDaemonMenu).is_none());
    }

    #[test]
    fn client_only_suppresses_only_cosmetic_version_notice() {
        let settings = codex_config::types::Tui {
            show_server_version_notice: true,
            ..Default::default()
        };
        assert!(
            crate::status::remote_connection::server_version_notice_for_tui(
                &settings,
                "0.0.0",
                Some("0.161.0")
            )
            .is_none()
        );
        assert!(
            crate::status::remote_connection::server_version_notice("0.0.0", Some("0.161.0"))
                .is_some()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn client_only_transport_blocks_writes_on_cloned_handles_and_reattaches()
    -> color_eyre::Result<()> {
        use futures::{SinkExt, StreamExt};
        use serde_json::json;
        use tokio_tungstenite::tungstenite::Message;
        let home = tempfile::tempdir()?;
        let socket = home.path().join("official.sock");
        let listener = tokio::net::UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let mut methods = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().await?;
                let mut ws = tokio_tungstenite::accept_async(stream).await?;
                while let Some(Ok(Message::Text(text))) = ws.next().await {
                    let value: serde_json::Value = serde_json::from_str(&text)?;
                    if value.get("id").is_none() {
                        continue;
                    }
                    let method = value["method"].as_str().unwrap();
                    methods.push(method.to_string());
                    let reply = if method == "initialize" {
                        json!({"id": value["id"], "result": {"userAgent": "mock/0.161.0"}})
                    } else {
                        json!({"id": value["id"], "error": {"code": -32601, "message": "unsupported task feature"}})
                    };
                    ws.send(Message::Text(reply.to_string().into())).await?;
                }
            }
            Ok::<_, color_eyre::eyre::Report>(methods)
        });
        let target = AppServerTarget::LocalDaemon {
            endpoint: RemoteAppServerEndpoint::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path(&socket)?,
            },
            allow_embedded_fallback: false,
        };
        for _ in 0..2 {
            let client = crate::app_server_connection::connect(&target).await?;
            let handle = client.request_handle().clone();
            let err = handle
                .request(codex_app_server_protocol::ClientRequest::ConfigBatchWrite {
                    request_id: RequestId::Integer(3),
                    params: codex_app_server_protocol::ConfigBatchWriteParams {
                        edits: vec![],
                        file_path: None,
                        expected_version: None,
                        reload_user_config: true,
                    },
                })
                .await
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            let response = handle
                .request(codex_app_server_protocol::ClientRequest::TurnInterrupt {
                    request_id: RequestId::Integer(4),
                    params: codex_app_server_protocol::TurnInterruptParams {
                        thread_id: "existing-thread".into(),
                        turn_id: "existing-turn".into(),
                    },
                })
                .await?;
            let error = response.unwrap_err();
            assert_eq!(error.code, -32601);
            assert_eq!(error.message, "unsupported task feature");
            client.shutdown().await?;
        }
        let methods = server.await??;
        assert_eq!(
            methods,
            [
                "initialize",
                "turn/interrupt",
                "initialize",
                "turn/interrupt"
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn client_only_shared_database_is_not_opened_or_migrated() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let config = crate::legacy_core::config::ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await?;
        let database = config.sqlite_config().state_db_path();
        std::fs::write(&database, b"older schema or unavailable database")?;
        let target = AppServerTarget::LocalDaemon {
            endpoint: RemoteAppServerEndpoint::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path(
                    home.path().join("official.sock"),
                )?,
            },
            allow_embedded_fallback: false,
        };
        assert!(
            crate::init_state_db_for_app_server_target(&config, &target)
                .await?
                .is_none()
        );
        assert_eq!(
            std::fs::read(database)?,
            b"older schema or unavailable database"
        );
        Ok(())
    }
}
