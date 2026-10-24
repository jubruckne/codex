use super::*;
use crate::app::agents_overview::tests::overview_thread;
use crate::app::tests::make_test_app_with_channels;
use codex_app_server_protocol::{
    AgentMessageDeltaNotification, ItemCompletedNotification, ThreadStatus,
};

fn feed() -> (RootFeed, ThreadId) {
    let child = ThreadId::from_u128(2);
    let mut feed = RootFeed::default();
    feed.agents.insert(child, "/worker".into());
    (feed, child)
}
#[test]
fn agent_feed_streams_replace_authoritatively_and_deduplicate() {
    let (mut feed, id) = feed();
    let key = MessageKey(id, "turn".into(), "item".into());
    feed.upsert(key.clone(), "Hello", false, true, 10, true);
    feed.upsert(key.clone(), " world", false, true, 11, true);
    assert_eq!(feed.messages[0].text, "Hello world");
    feed.upsert(key.clone(), "Final", true, true, 12, false);
    feed.upsert(key.clone(), "Stale snapshot", true, false, 10, false);
    feed.upsert(key, " late", false, true, 13, true);
    assert_eq!(feed.messages.len(), 1);
    assert_eq!(feed.messages[0].text, "Final");
}
#[test]
fn agent_feed_history_never_replaces_newer_stream() {
    let (mut feed, id) = feed();
    let key = MessageKey(id, "t".into(), "i".into());
    feed.upsert(key.clone(), "live delta", false, true, 20, true);
    feed.upsert(key, "stale", false, false, 10, false);
    assert_eq!(feed.messages[0].text, "live delta");
}
#[test]
fn agent_feed_interleaved_messages_keep_first_arrival_order() {
    let (mut feed, a) = feed();
    let b = ThreadId::from_u128(3);
    feed.agents.insert(b, "/other".into());
    let first = MessageKey(a, "t".into(), "a".into());
    feed.upsert(first.clone(), "one", false, true, 1, true);
    feed.upsert(
        MessageKey(b, "t".into(), "b".into()),
        "two",
        false,
        true,
        2,
        true,
    );
    feed.upsert(first.clone(), " more", false, true, 3, true);
    assert_eq!(feed.messages[0].key, first);
    assert_eq!(feed.messages[0].text, "one more");
}
#[test]
fn agent_feed_retention_and_unicode_truncation_are_bounded() {
    let (mut feed, id) = feed();
    for n in 0..220 {
        feed.upsert(
            MessageKey(id, "t".into(), n.to_string()),
            "message",
            true,
            false,
            n,
            false,
        );
    }
    assert_eq!(feed.messages.len(), MAX_MESSAGES);
    assert_eq!(feed.messages[0].key.2, "20");
    let huge = "🌻".repeat(MAX_MESSAGE_BYTES);
    for n in 220..250 {
        feed.upsert(
            MessageKey(id, "t".into(), n.to_string()),
            &huge,
            true,
            false,
            n,
            false,
        );
    }
    assert!(feed.messages.iter().map(|m| m.text.len()).sum::<usize>() <= MAX_BYTES);
    assert!(feed.messages.last().unwrap().truncated);
    assert!(
        feed.messages
            .last()
            .unwrap()
            .text
            .is_char_boundary(feed.messages.last().unwrap().text.len())
    );
}
#[test]
fn agent_feed_latest_response_card_renders_markdown_unicode_and_notice() {
    let (mut feed, id) = feed();
    feed.upsert(
        MessageKey(id, "t".into(), "i".into()),
        "**Done** 🌻\n\nTests passed",
        true,
        false,
        1,
        false,
    );
    feed.notice = Some("Partial history".into());
    let area = Rect::new(0, 0, 40, 12);
    let mut buf = Buffer::empty(area);
    feed.render_agent_card(id, area, false, &mut buf, Path::new("/tmp"));
    let text = crate::app::owned_transcript::tests::buffer_text(&buf);
    assert!(text.contains("╭ · worker"));
    assert!(text.contains("worker"));
    assert!(text.contains("Done"));
    assert!(text.contains("Tests passed"));
    assert!(text.contains("Partial history"));
    insta::assert_snapshot!("agent_feed_latest_card", text);
    let area = Rect::new(0, 0, 1, 1);
    feed.render_agent_card(id, area, false, &mut Buffer::empty(area), Path::new("/tmp"));
}
#[test]
fn agent_feed_empty_and_newest_messages() {
    let (mut feed, id) = feed();
    let area = Rect::new(0, 0, 35, 12);
    let mut buf = Buffer::empty(area);
    feed.render_agent_card(id, area, false, &mut buf, Path::new("/tmp"));
    assert!(!crate::app::owned_transcript::tests::buffer_text(&buf).contains("No response yet."));
    assert!(
        !crate::app::owned_transcript::tests::buffer_text(&buf)
            .contains("Waiting for a local summary")
    );
    for n in 0..20 {
        feed.upsert(
            MessageKey(id, "t".into(), n.to_string()),
            &format!("reply {n}"),
            true,
            false,
            n,
            false,
        );
    }
    feed.render_agent_card(id, area, false, &mut buf, Path::new("/tmp"));
    let text = crate::app::owned_transcript::tests::buffer_text(&buf);
    assert!(text.contains("reply 19"));
    assert!(!text.contains("reply 0"));
}

#[tokio::test]
async fn agent_feed_auto_activation_manual_override_and_resize() -> Result<()> {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let child = ThreadId::from_u128(2);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    app.sync_agent_feed_root();
    assert!(!app.agent_windows_enabled(&tui));
    app.agents_overview.threads.insert(
        child,
        Some(overview_thread(
            child,
            Some(root),
            "Worker",
            ThreadStatus::Idle,
        )),
    );
    app.sync_agent_feed_root();
    assert!(app.agent_windows_enabled(&tui));
    app.toggle_agent_feed(&mut tui);
    assert!(!app.agent_windows_enabled(&tui));
    let nested = ThreadId::from_u128(3);
    app.agents_overview.threads.insert(
        nested,
        Some(overview_thread(
            nested,
            Some(child),
            "Nested",
            ThreadStatus::Idle,
        )),
    );
    app.sync_agent_feed_root();
    assert!(!app.agent_windows_enabled(&tui));
    app.toggle_agent_feed(&mut tui);
    assert!(app.agent_windows_enabled(&tui));
    app.local_settings.tui.fullscreen_transcript = false;
    assert!(!app.agent_windows_enabled(&tui));
    tui.set_owned_screen(false)?;
    Ok(())
}
#[tokio::test]
async fn agent_feed_filters_root_other_trees_and_tools() {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let child = ThreadId::from_u128(2);
    let other = ThreadId::from_u128(9);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    app.agents_overview.threads.insert(
        root,
        Some(overview_thread(root, None, "Root", ThreadStatus::Idle)),
    );
    app.agents_overview.threads.insert(
        child,
        Some(overview_thread(
            child,
            Some(root),
            "Worker",
            ThreadStatus::Idle,
        )),
    );
    for id in [root, child, other] {
        app.observe_agent_feed(&ServerNotification::AgentMessageDelta(
            AgentMessageDeltaNotification {
                thread_id: id.to_string(),
                turn_id: "t".into(),
                item_id: "m".into(),
                delta: "reply".into(),
            },
        ));
    }
    app.observe_agent_feed(&ServerNotification::ItemCompleted(
        ItemCompletedNotification {
            thread_id: child.to_string(),
            turn_id: "t".into(),
            item: ThreadItem::ContextCompaction {
                id: "compact".into(),
            },
            completed_at_ms: 0,
        },
    ));
    let feed = &app.agent_feed.roots[&root];
    assert_eq!(feed.messages.len(), 1);
    assert_eq!(feed.messages[0].key.0, child);
}
#[tokio::test]
async fn agent_feed_stale_snapshot_and_invalidation() {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.sync_agent_feed_root();
    let generation = app.agent_feed.generation;
    let snapshot = FeedSnapshot {
        latest_turns: HashMap::new(),
        threads: vec![],
        messages: vec![],
        versions: HashMap::new(),
        errors: vec!["stale".into()],
    };
    app.invalidate_agent_feed();
    app.apply_agent_feed_snapshot(root, generation, snapshot);
    assert!(app.agent_feed.roots[&root].notice.is_none());
    assert!(app.agent_feed.dirty);
}
#[test]
fn agent_feed_keymap_default_remapping_and_conflicts() {
    use codex_config::types::{KeybindingSpec, KeybindingsSpec, TuiKeymap};
    let mut config = TuiKeymap::default();
    let runtime = RuntimeKeymap::from_config(&config).unwrap();
    assert!(
        runtime
            .app
            .toggle_agent_feed
            .is_pressed(KeyCode::F(6).into())
    );
    config.global.toggle_agent_feed = Some(KeybindingsSpec::One(KeybindingSpec("f12".into())));
    let runtime = RuntimeKeymap::from_config(&config).unwrap();
    assert!(
        runtime
            .app
            .toggle_agent_feed
            .is_pressed(KeyCode::F(12).into())
    );
    config.global.toggle_agent_feed = Some(KeybindingsSpec::One(KeybindingSpec("ctrl-t".into())));
    assert!(RuntimeKeymap::from_config(&config).is_err());
}

#[tokio::test]
async fn agent_feed_split_composer_cursor_and_resize_rendering() -> Result<()> {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let child = ThreadId::from_u128(2);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    app.local_settings.tui.animations = false;
    app.chat_widget
        .apply_external_edit("Keep this draft while toggling".into());
    app.agents_overview.threads.insert(
        child,
        Some(overview_thread(
            child,
            Some(root),
            "Worker",
            ThreadStatus::Active {
                active_flags: Vec::new(),
            },
        )),
    );
    app.sync_agent_feed_root();
    app.agent_feed.roots.get_mut(&root).unwrap().upsert(
        MessageKey(child, "t".into(), "m".into()),
        "Worker reply",
        true,
        false,
        1,
        false,
    );
    app.transcript_cells = vec![Arc::new(crate::history_cell::AgentMarkdownCell::new(
        "Main transcript".into(),
        &app.config.cwd,
    ))];
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    for width in [180, 119, 120, 180] {
        let size = Size::new(width, 24);
        tui.terminal.resize(size)?;
        tui.screen_size_for_event(&TuiEvent::Resize(size))?;
        app.render_owned_transcript(&mut tui, size)?;
        let buf = crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal);
        let text = crate::app::owned_transcript::tests::buffer_text(buf);
        assert!(text.contains("Worker reply"));
        assert!(text.contains("Keep this draft while toggling"));
        assert!(tui.terminal.last_known_cursor_pos.x < width);
        assert!(text.contains('▲'));
        assert_eq!(app.render_owned_transcript(&mut tui, size)?.width, width);
    }
    app.toggle_agent_feed(&mut tui);
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "Keep this draft while toggling"
    );
    tui.set_owned_screen(false)?;
    Ok(())
}

#[test]
fn agent_feed_default_yields_to_existing_shortcuts() {
    use codex_config::types::{KeybindingSpec, KeybindingsSpec, TuiKeymap};
    let mut config = TuiKeymap::default();
    config.global.open_transcript = Some(KeybindingsSpec::One(KeybindingSpec("f6".into())));
    let runtime = RuntimeKeymap::from_config(&config).unwrap();
    assert!(runtime.app.toggle_agent_feed.is_empty());
    assert!(runtime.app.open_transcript.is_pressed(KeyCode::F(6).into()));
}

#[tokio::test]
async fn agent_feed_history_rpc_is_passive_paged_and_recovers_messages() -> Result<()> {
    use codex_app_server_client::{
        AppServerClient, RemoteAppServerClient, RemoteAppServerConnectArgs, RemoteAppServerEndpoint,
    };
    use codex_app_server_protocol::{JSONRPCMessage, Turn, TurnItemsView, TurnStatus};
    use futures::{SinkExt, StreamExt};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    let root = ThreadId::from_u128(1);
    let child = ThreadId::from_u128(2);
    let nested = ThreadId::from_u128(3);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let now = chrono::Utc::now().timestamp();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let mut reads = 0;
        while let Some(Ok(WsMessage::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text).unwrap() else {
                continue;
            };
            let result = match request.method.as_str() {
                "initialize" => json!({"userAgent":"test"}),
                "thread/list" => {
                    let params = request.params.unwrap();
                    assert_eq!(params["ancestorThreadId"], root.to_string());
                    let mut threads = if params["cursor"].is_null() {
                        vec![overview_thread(
                            child,
                            Some(root),
                            "Worker",
                            ThreadStatus::Idle,
                        )]
                    } else {
                        vec![overview_thread(
                            nested,
                            Some(child),
                            "Nested",
                            ThreadStatus::Idle,
                        )]
                    };
                    for thread in &mut threads {
                        thread.updated_at = now;
                    }
                    json!({"data":threads,"nextCursor":if params["cursor"].is_null(){Some("next")}else{None},"backwardsCursor":null})
                }
                "thread/turns/list" => {
                    reads += 1;
                    let params = request.params.unwrap();
                    assert_eq!(params["limit"], 10);
                    assert_eq!(params["itemsView"], "full");
                    let turn = Turn {
                        id: "turn".into(),
                        root_turn_id: None,
                        items_view: TurnItemsView::Full,
                        status: TurnStatus::Completed,
                        error: None,
                        started_at: Some(100),
                        completed_at: Some(200),
                        duration_ms: Some(100_000),
                        items: vec![
                            ThreadItem::Reasoning {
                                id: "reasoning".into(),
                                summary: vec!["Recovered thinking".into(), "More thinking".into()],
                                content: vec!["Private reasoning must not appear".into()],
                            },
                            ThreadItem::AgentMessage {
                                id: "message".into(),
                                text: format!("Reply from {}", params["threadId"]),
                                phase: None,
                                memory_citation: None,
                                delivery: None,
                                questions: None,
                            },
                        ],
                    };
                    let mut newest = turn.clone();
                    newest.id = "empty-latest-turn".into();
                    newest.items.clear();
                    newest.status = TurnStatus::InProgress;
                    newest.started_at = Some(300);
                    newest.completed_at = None;
                    json!({"data":[newest,turn],"nextCursor":null,"backwardsCursor":null})
                }
                method => panic!("feed attempted unexpected RPC: {method}"),
            };
            socket
                .send(WsMessage::Text(
                    json!({"id":request.id,"result":result}).to_string().into(),
                ))
                .await
                .unwrap();
        }
        reads
    });
    let client = AppServerClient::Remote(
        RemoteAppServerClient::connect(RemoteAppServerConnectArgs {
            endpoint: RemoteAppServerEndpoint::WebSocket {
                websocket_url: format!("ws://{address}"),
                auth_token: None,
            },
            client_name: "feed-test".into(),
            client_version: "1".into(),
            experimental_api: true,
            mcp_server_openai_form_elicitation: false,
            opt_out_notification_methods: vec![],
            channel_capacity: 32,
        })
        .await?,
    );
    let snapshot = io::load(&client.request_handle(), root, vec![], HashMap::new(), true).await;
    assert!(snapshot.errors.is_empty());
    assert_eq!(snapshot.messages.len(), 4);
    assert_eq!(snapshot.latest_turns.len(), 2);
    assert!(
        snapshot
            .latest_turns
            .values()
            .all(
                |turn| turn.latest.as_ref().unwrap().id == "empty-latest-turn"
                    && turn.completed.as_ref().unwrap().id == "turn"
            )
    );
    assert_eq!(snapshot.threads.len(), 2);
    let (mut app, _, _) = make_test_app_with_channels().await;
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.sync_agent_feed_root();
    let versions = snapshot.versions.clone();
    let threads = snapshot.threads.clone();
    app.apply_agent_feed_snapshot(root, app.agent_feed.generation, snapshot);
    let feed = &app.agent_feed.roots[&root];
    let thoughts = feed
        .messages
        .iter()
        .filter(|message| message.reasoning)
        .collect::<Vec<_>>();
    assert_eq!(thoughts.len(), 2);
    assert!(
        thoughts
            .iter()
            .all(|message| message.text == "Recovered thinking\n\nMore thinking")
    );
    let area = Rect::new(0, 0, 80, 30);
    let mut buf = Buffer::empty(area);
    feed.render_windows(area, &mut buf, Path::new("/tmp"), None);
    let rendered = crate::app::owned_transcript::tests::buffer_text(&buf);
    assert!(rendered.contains("Reply from"));
    assert!(!rendered.contains("Recovered thinking"));
    assert!(!rendered.contains("Private reasoning"));
    let cached = io::load(&client.request_handle(), root, threads, versions, true).await;
    assert!(cached.messages.is_empty());
    client.shutdown().await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), server).await??,
        2
    );
    Ok(())
}

#[tokio::test]
async fn agent_feed_root_changes_preserve_manual_choice_and_reject_other_root_results() {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let first = ThreadId::from_u128(1);
    let second = ThreadId::from_u128(9);
    let child = ThreadId::from_u128(2);
    app.primary_thread_id = Some(first);
    app.active_thread_id = Some(first);
    app.agents_overview.threads.insert(
        child,
        Some(overview_thread(
            child,
            Some(first),
            "Worker",
            ThreadStatus::Idle,
        )),
    );
    app.sync_agent_feed_root();
    app.agent_feed.roots.get_mut(&first).unwrap().manual = Some(false);
    let generation = app.agent_feed.generation;
    app.primary_thread_id = Some(second);
    app.active_thread_id = Some(second);
    app.sync_agent_feed_root();
    app.apply_agent_feed_snapshot(
        first,
        generation,
        FeedSnapshot {
            latest_turns: HashMap::new(),
            threads: vec![],
            messages: vec![],
            versions: HashMap::new(),
            errors: vec!["old root".into()],
        },
    );
    assert_eq!(app.agent_feed.root, Some(second));
    assert!(app.agent_feed.roots[&second].notice.is_none());
    app.primary_thread_id = Some(first);
    app.active_thread_id = Some(child);
    app.sync_agent_feed_root();
    assert_eq!(app.agent_feed.root, Some(first));
    assert!(!app.agent_feed.roots[&first].enabled());
}

#[tokio::test]
async fn agent_feed_floating_window_collapse_expand_and_pointer_routing() -> Result<()> {
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let child = ThreadId::from_u128(2);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    app.agents_overview.threads.insert(
        child,
        Some(overview_thread(
            child,
            Some(root),
            "Worker",
            ThreadStatus::Active {
                active_flags: Vec::new(),
            },
        )),
    );
    app.sync_agent_feed_root();
    app.agent_feed.roots.get_mut(&root).unwrap().upsert(
        MessageKey(child, "t".into(), "m".into()),
        "Window progress summary",
        true,
        true,
        1,
        false,
    );
    app.chat_widget.apply_external_edit("retained draft".into());
    let mut tui = crate::tui::test_support::make_test_tui()?;
    tui.set_owned_screen(true)?;
    let size = Size::new(180, 24);
    tui.terminal.resize(size)?;
    tui.screen_size_for_event(&TuiEvent::Resize(size))?;
    let bottom = app.render_owned_transcript(&mut tui, size)?;
    assert_eq!(bottom.width, 180);
    let mouse = |kind, column, row| {
        TuiEvent::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    let buffer = crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal);
    let index = buffer
        .content()
        .iter()
        .position(|c| c.symbol() == "▲")
        .expect("expanded control");
    let (x, y) = buffer.pos_of(index);
    assert!(y < bottom.y);
    assert!(app.handle_agent_feed_event(
        &mut tui,
        &mouse(MouseEventKind::Down(MouseButton::Left), x, y)
    ));
    assert!(app.handle_agent_feed_event(
        &mut tui,
        &mouse(MouseEventKind::Up(MouseButton::Left), x, y)
    ));
    tui.screen_size_for_event(&TuiEvent::Resize(size))?;
    app.render_owned_transcript(&mut tui, size)?;
    let output = crate::app::owned_transcript::tests::buffer_text(
        crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal),
    );
    assert!(output.contains('▼'));
    assert!(!output.contains("Window progress summary"));
    assert!(output.contains("Working"));
    assert!(app.handle_agent_feed_event(
        &mut tui,
        &mouse(MouseEventKind::Down(MouseButton::Left), x, y)
    ));
    app.handle_agent_feed_event(
        &mut tui,
        &mouse(MouseEventKind::Up(MouseButton::Left), x, y),
    );
    tui.screen_size_for_event(&TuiEvent::Resize(size))?;
    app.render_owned_transcript(&mut tui, size)?;
    let output = crate::app::owned_transcript::tests::buffer_text(
        crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal),
    );
    assert!(output.contains('▲'));
    assert!(output.contains("Window progress summary"));
    assert!(!app.handle_agent_feed_event(
        &mut tui,
        &mouse(MouseEventKind::Down(MouseButton::Left), 0, bottom.y)
    ));
    app.handle_agent_feed_event(&mut tui, &TuiEvent::FocusLost);
    app.toggle_agent_feed(&mut tui);
    tui.screen_size_for_event(&TuiEvent::Resize(size))?;
    app.render_owned_transcript(&mut tui, size)?;
    let output = crate::app::owned_transcript::tests::buffer_text(
        crate::custom_terminal::test_support::last_rendered_buffer(&tui.terminal),
    );
    assert!(!output.contains('▲'));
    app.toggle_agent_feed(&mut tui);
    tui.screen_size_for_event(&TuiEvent::Resize(size))?;
    app.render_owned_transcript(&mut tui, size)?;
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "retained draft"
    );
    tui.set_owned_screen(false)?;
    Ok(())
}

#[tokio::test]
async fn agent_feed_reasoning_parts_complete_and_filter_notifications() {
    use codex_app_server_protocol::{
        ItemStartedNotification, ReasoningSummaryPartAddedNotification,
        ReasoningSummaryTextDeltaNotification,
    };
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let child = ThreadId::from_u128(2);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    app.agents_overview.threads.insert(
        child,
        Some(overview_thread(
            child,
            Some(root),
            "Worker",
            ThreadStatus::Idle,
        )),
    );
    let delta = |thread: ThreadId, index, text: &str| {
        ServerNotification::ReasoningSummaryTextDelta(ReasoningSummaryTextDeltaNotification {
            thread_id: thread.to_string(),
            turn_id: "t".into(),
            item_id: "reason".into(),
            delta: text.into(),
            summary_index: index,
        })
    };
    for id in [root, ThreadId::from_u128(9)] {
        app.observe_agent_feed(&delta(id, 0, "Filtered"));
    }
    app.observe_agent_feed(&ServerNotification::ItemStarted(ItemStartedNotification {
        thread_id: child.to_string(),
        turn_id: "t".into(),
        item: ThreadItem::Reasoning {
            id: "reason".into(),
            summary: vec![],
            content: vec!["Private content".into()],
        },
        started_at_ms: 0,
    }));
    // Parts may arrive out of order; repeated markers never insert extra separators.
    app.observe_agent_feed(&delta(child, 1, "Second"));
    for _ in 0..2 {
        app.observe_agent_feed(&ServerNotification::ReasoningSummaryPartAdded(
            ReasoningSummaryPartAddedNotification {
                thread_id: child.to_string(),
                turn_id: "t".into(),
                item_id: "reason".into(),
                summary_index: 1,
            },
        ));
    }
    app.observe_agent_feed(&delta(child, 0, "First"));
    app.observe_agent_feed(&delta(child, 0, " part"));
    let feed = &app.agent_feed.roots[&root];
    assert_eq!(feed.messages.len(), 1);
    assert_eq!(feed.messages[0].text, "First part\n\nSecond");
    let completion = |summary| {
        ServerNotification::ItemCompleted(ItemCompletedNotification {
            thread_id: child.to_string(),
            turn_id: "t".into(),
            item: ThreadItem::Reasoning {
                id: "reason".into(),
                summary,
                content: vec!["Private content".into()],
            },
            completed_at_ms: 0,
        })
    };
    app.observe_agent_feed(&completion(vec!["Final summary".into(), "Next".into()]));
    app.observe_agent_feed(&delta(child, 1, "late"));
    let feed = &app.agent_feed.roots[&root];
    assert_eq!(feed.messages[0].text, "Final summary\n\nNext");
    assert!(feed.messages[0].complete);
}

#[test]
fn agent_feed_reasoning_empty_completion_preserves_stream_and_retention() {
    let (mut feed, child) = feed();
    let key = MessageKey(child, "t".into(), "r".into());
    feed.reasoning_delta(key.clone(), 0, "Available", 1);
    feed.reasoning_summary(key.clone(), &[String::new()], true, true, 2);
    assert_eq!(feed.messages[0].text, "Available");
    assert!(feed.messages[0].complete);
    feed.reasoning_summary(key, &["Stale history".into()], true, false, 0);
    assert_eq!(feed.messages[0].text, "Available");
    let key = MessageKey(child, "t".into(), "bounded".into());
    feed.reasoning_delta(key.clone(), 0, "First", 3);
    feed.reasoning_delta(key.clone(), 1, &"🌻".repeat(MAX_MESSAGE_BYTES), 3);
    feed.reasoning_delta(key, 2, "Ignored after truncation", 3);
    let message = feed.messages.last().unwrap();
    assert!(message.truncated);
    assert!(message.text.len() <= MAX_MESSAGE_BYTES);
    assert!(
        message
            .parts
            .iter()
            .all(|(_, range)| range.end <= message.text.len())
    );
    assert!(message.parts.len() <= MAX_MESSAGE_BYTES);
    assert!(
        feed.messages
            .iter()
            .map(|message| message.text.len())
            .sum::<usize>()
            <= MAX_BYTES
    );
}
