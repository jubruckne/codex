//! Attachment-only startup preserves the official daemon and rejects fallback.

use super::*;
use crate::legacy_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

#[test]
fn feature_overrides_allow_daemon_without_allowing_arbitrary_config() {
    for (raw, eligible) in [
        ("features.app_server_websocket_compression=true", true),
        ("features.app_server_websocket_compression=false", true),
        ("features.worktrees='true'", false),
        ("features={worktrees=true,shell_tool=false}", true),
        ("suppress_unstable_features_warning=true", true),
        ("suppress_unstable_features_warning='true'", false),
        ("features.shell_tool=false", true),
        ("features.worktrees.enabled=true", false),
        ("tui.fullscreen_transcript=true", true),
        ("tui.fullscreen_transcript=false", true),
        ("tui.fullscreen_transcript=\"true\"", false),
        ("tui={fullscreen_transcript=true}", true),
        ("tui={fullscreen_transcript='true'}", false),
        ("tui={fullscreen_transcript=true,animations=false}", false),
        ("features={}", false),
        ("model='test'", false),
    ] {
        let overrides = codex_utils_cli::CliConfigOverrides {
            raw_overrides: vec![raw.to_string()],
        }
        .parse_overrides()
        .unwrap();
        assert_eq!(
            daemon_startup::config_exclusion(
                &overrides,
                &LoaderOverrides::default(),
                /*strict_config*/ false,
                /*bypass_hook_trust*/ false,
            )
            .is_none(),
            eligible,
            "{raw}"
        );
    }
}

#[test]
fn daemon_launch_telemetry_records_once_on_connection_or_early_return() {
    for connected in [false, true] {
        let observations = std::cell::RefCell::new(Vec::new());
        let launch = daemon_telemetry::Launch(Some(|target: &AppServerTarget, actual: bool| {
            observations.borrow_mut().push((target.clone(), actual));
            Vec::new()
        }));
        if connected {
            launch.record(&AppServerTarget::Embedded, connected);
        } else {
            drop(launch);
        }
        assert_eq!(
            observations.into_inner(),
            vec![(AppServerTarget::Embedded, connected)]
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn client_only_daemon_feature_compatibility_respects_invocation_overrides()
-> color_eyre::Result<()> {
    use codex_app_server_protocol::JSONRPCMessage;
    use futures::SinkExt;
    use futures::StreamExt;
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;

    for scenario in [
        "matching",
        "stale overrides",
        "fullscreen frontend policy",
        "frontend policy and server override",
        "conflicting client",
        "explicit enable",
        "managed",
        "legacy managed",
        "old requirements server",
        "unsupported RPC",
    ] {
        let home = TempDir::new()?;
        let mut config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await?;
        config.features.enable(Feature::ApiKeyModelDiscovery)?;
        let cli_overrides = match scenario {
            "matching" | "stale overrides" => Vec::new(),
            "fullscreen frontend policy" | "frontend policy and server override" => {
                // Match the user's -c flag plus the entry point's injected local policy.
                let mut raw_overrides = vec!["tui.fullscreen_transcript=true".into(), "features.daemon_auto_start=false".into()];
                if scenario == "frontend policy and server override" {
                    raw_overrides.push("features.api_key_model_discovery=false".into());
                }
                codex_utils_cli::CliConfigOverrides { raw_overrides }.parse_overrides().map_err(color_eyre::eyre::Report::msg)?
            },
            _ => vec![(
                "features.api_key_model_discovery".to_string(),
                toml::Value::Boolean(scenario == "explicit enable"),
            )],
        };
        let features = [
            Feature::DaemonAutoStart,
            Feature::ApiKeyModelDiscovery,
            Feature::CodeModeHost,
            Feature::AuthElicitation,
            Feature::McpOAuthRefreshCoordination,
        ].map(|feature| {
            let enabled = if feature == Feature::DaemonAutoStart {
                true // The service is allowed to have a different auto-start preference.
            } else if feature == Feature::ApiKeyModelDiscovery && matches!(scenario, "stale overrides" | "explicit enable") {
                false
            } else {
                config.features.enabled(feature)
            };
            json!({"name": feature.key(), "stage": "beta", "enabled": enabled, "defaultEnabled": false})
        });
        let socket_path =
            AbsolutePathBuf::from_absolute_path_checked(home.path().join("official.sock"))?;
        let listener = tokio::net::UnixListener::bind(socket_path.as_path())?;
        let target = AppServerTarget::LocalDaemon {
            endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
            allow_embedded_fallback: false,
        };
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let JSONRPCMessage::Request(request) = serde_json::from_str(&text).unwrap()
                else {
                    continue;
                };
                if scenario == "fullscreen frontend policy" {
                    assert_eq!(request.method, "initialize", "frontend policy must not query or mutate server feature settings");
                }
                let response = match request.method.as_str() {
                    "initialize" => {
                        json!({"id": request.id, "result": {"userAgent": "daemon-test"}})
                    }
                    "config/read" => {
                        json!({"id": request.id, "result": {"config": {}, "layers": [{
                            "name": {"type": "legacyManagedConfigTomlFromFile"}, "config":
                            if scenario == "legacy managed" {
                                json!({"features": {"api_key_model_discovery": true}})
                            } else { json!({}) }
                        }]}})
                    }
                    "configRequirements/read" if scenario == "old requirements server" => {
                        json!({"id": request.id, "error": {"code": -32600, "message": "Invalid request: unknown variant `configRequirements/read`"}})
                    }
                    "configRequirements/read" => json!({"id": request.id, "result": {
                        "requirements": if scenario == "managed" {
                            json!({"featureRequirements": {"api_key_model_discovery": true}})
                        } else { json!(null) }
                    }}),
                    "experimentalFeature/list" if scenario == "unsupported RPC" => {
                        json!({"id": request.id, "error": {"code": -32601, "message": "method not found"}})
                    }
                    "experimentalFeature/list" => {
                        assert_eq!(request.params.unwrap()["threadId"], json!(null));
                        json!({"id": request.id, "result": {"data": features, "nextCursor": null}})
                    }
                    _ => panic!("unexpected request: {}", request.method),
                };
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
        });
        let result =
            daemon_startup::compatibility_warning(&target, &config, &cli_overrides).await;
        tokio::time::timeout(Duration::from_secs(5), server).await??;
        if matches!(
            scenario,
            "matching" | "stale overrides" | "managed" | "legacy managed" | "fullscreen frontend policy"
        ) {
            assert_eq!(result?, None);
            continue;
        }
        let reason = match scenario {
            "explicit enable" => "This session requires api_key_model_discovery to be enabled",
            "conflicting client" | "old requirements server" | "frontend policy and server override" => {
                "This session requires api_key_model_discovery to be disabled"
            }
            "unsupported RPC" => "Experimental feature request failed",
            _ => unreachable!(),
        };
        let error = result.unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "Cannot use the shared background server: {reason}.\n{}",
                daemon_startup::FAILURE_HINT
            )
        );
        if scenario == "explicit enable" {
            insta::assert_snapshot!("daemon_feature_mismatch_error", error);
        }
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn daemon_connection_rejects_unprotected_socket_before_handshake() -> color_eyre::Result<()> {
    let home = TempDir::new()?;
    let parent = home.path().join("control");
    std::fs::create_dir(&parent)?;
    let socket_path = AbsolutePathBuf::from_absolute_path_checked(parent.join("server.sock"))?;
    let mut listener = codex_uds::UnixListener::bind(socket_path.as_path()).await?;
    let target = AppServerTarget::LocalDaemon {
        allow_embedded_fallback: true,
        endpoint: RemoteAppServerEndpoint::UnixSocket { socket_path },
    };
    tokio::select! {
        result = app_server_connection::connect(&target) => assert!(result.is_err()),
        _ = listener.accept() => panic!("unprotected listener must not receive a connection"),
    }
    Ok(())
}

#[tokio::test]
async fn client_only_daemon_startup_preserves_target_without_fallback() -> color_eyre::Result<()> {
    for scenario in [
        "missing socket",
        "failed handshake",
        "explicit endpoint",
        "required daemon",
    ] {
        let home = TempDir::new()?;
        let config = ConfigBuilder::default()
            .codex_home(home.path().to_path_buf())
            .build()
            .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = if matches!(scenario, "missing socket" | "required daemon") {
            RemoteAppServerEndpoint::UnixSocket {
                socket_path: AbsolutePathBuf::from_absolute_path_checked(
                    home.path().join("gone.sock"),
                )?,
            }
        } else {
            RemoteAppServerEndpoint::WebSocket {
                websocket_url: format!("ws://{}", listener.local_addr()?),
                auth_token: None,
            }
        };
        let reject_handshake = tokio::spawn(async move {
            let _ = listener.accept().await;
        });
        let mut target = if scenario == "explicit endpoint" {
            AppServerTarget::Remote { endpoint }
        } else {
            AppServerTarget::LocalDaemon {
                endpoint,
                allow_embedded_fallback: scenario != "required daemon",
            }
        };
        let original_target = target.clone();
        let mut state_db = None;
        let result = start_app_server(
            &mut target,
            Arg0DispatchPaths::default(),
            config,
            Vec::new(),
            LoaderOverrides::default(),
            /*strict_config*/ false,
            CloudConfigBundleLoader::default(),
            codex_feedback::CodexFeedback::new(),
            /*log_db*/ None,
            &mut state_db,
            Arc::new(EnvironmentManager::default_for_tests()),
            Default::default(),
        )
        .await;
        reject_handshake.abort();
        let error = result
            .err()
            .expect("attachment failure must never embed")
            .to_string();
        if scenario == "required daemon" {
            assert!(error.contains("failed to connect to remote app server"));
        } else {
            assert!(error.contains("embedded execution and fallback are disabled"));
        }
        assert_eq!(target, original_target);
        assert!(state_db.is_none());
    }
    Ok(())
}

#[test]
fn daemon_eligibility_preserves_launch_options_and_explains_exclusions() {
    use clap::Parser;
    for (args, expected) in [
        ("--no-daemon", Some("--no-daemon")),
        ("--worktree", None),
        ("--worktree --no-daemon", Some("--no-daemon")),
        ("--oss", Some("--oss")),
        ("--profile test", Some("--profile")),
        ("--strict-config", Some("--strict-config")),
        (
            "--dangerously-bypass-hook-trust",
            Some("--dangerously-bypass-hook-trust"),
        ),
        (
            "-m test --cd /tmp -i image.png -a never -s workspace-write --add-dir /tmp --no-alt-screen hello",
            None,
        ),
    ] {
        let cli = Cli::parse_from(std::iter::once("codex").chain(args.split_whitespace()));
        assert_eq!(
            daemon_startup::exclusion(
                &cli,
                &[],
                &LoaderOverrides::default(),
                /*workload_identity_selected*/ false,
                /*exec_server_url*/ None
            ),
            expected
        );
    }
    let mut cli = Cli::parse_from(["codex"]);
    let overrides = vec![("web_search".into(), toml::Value::String("live".into()))];
    let loader = LoaderOverrides {
        ignore_user_config: true,
        ..Default::default()
    };
    for (kv, loader, workload, executor, expected) in [
        (
            &overrides[..],
            LoaderOverrides::default(),
            false,
            None,
            "command-line configuration overrides (-c, --enable, --disable, or --search)",
        ),
        (&[][..], loader, false, None, "custom configuration loader"),
        (
            &[][..],
            LoaderOverrides::default(),
            true,
            None,
            "workload identity",
        ),
        (
            &[][..],
            LoaderOverrides::default(),
            false,
            Some(std::ffi::OsStr::new("executor")),
            "executor selection (CODEX_EXEC_SERVER_URL)",
        ),
    ] {
        assert_eq!(
            daemon_startup::exclusion(&cli, kv, &loader, workload, executor),
            Some(expected)
        );
    }
    cli.agents_overview = true;
    cli.strict_config = true;
    assert_eq!(
        daemon_startup::exclusion(
            &cli,
            &overrides,
            &LoaderOverrides::default(),
            /*workload_identity_selected*/ false,
            /*exec_server_url*/ None
        ),
        None
    );
}

#[test]
fn daemon_exclusion_warning_snapshot() {
    use crate::history_cell::HistoryCell;
    let cell = crate::history_cell::StartupWarningsCell::new(vec![
        "Running without the shared background server: --strict-config requires embedded mode."
            .into(),
        format!(
            "Running without the shared background server: {} requires embedded mode.",
            daemon_startup::WSL_DRVFS_EXCLUSION
        ),
    ]);
    let text = cell
        .transcript_lines(/*width*/ 80)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("daemon_exclusion_warning", text);
}
