use super::*;
use crate::app::agents_overview::tests::overview_thread;
use crate::app::tests::make_test_app_with_channels;
use codex_app_server_protocol::{
    ThreadClosedNotification, ThreadStatusChangedNotification, ThreadTokenUsage,
    ThreadTokenUsageUpdatedNotification, TokenUsageBreakdown,
};

fn feed() -> RootFeed {
    let mut feed = RootFeed::default();
    for (index, name) in [
        (2, "/root/feed_reasoning"),
        (3, "/root/audit_scrollback_only"),
    ] {
        let id = ThreadId::from_u128(index);
        feed.agents.insert(id, name.into());
        feed.cards.insert(
            id,
            CardState {
                last_activity: Some(chrono::Utc::now().timestamp()),
                tokens: Some((1100, 300)),
                ..Default::default()
            },
        );
        feed.upsert(MessageKey(id,"t".into(),"message".into()),"The agent has completed detailed source checks and is investigating the remaining display issue. Next it plans to run focused tests.",true,true,1000,false);
    }
    feed
}
fn render(feed: &RootFeed, width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    let mut agents: Vec<_> = feed
        .agents
        .keys()
        .copied()
        .filter(|id| feed.card_visible(*id, 1000))
        .collect();
    agents.sort_by_key(|id| id.to_string());
    for (slot, agent) in agents.into_iter().enumerate() {
        let top = (slot as u16).saturating_mul(CARD_HEIGHT);
        if top >= height {
            break;
        }
        feed.render_agent_card(
            agent,
            Rect::new(0, top, width, CARD_HEIGHT.min(height - top)),
            false,
            &mut buf,
            Path::new("/tmp"),
        );
    }
    buf
}
fn text(buf: &Buffer) -> String {
    crate::app::owned_transcript::tests::buffer_text(buf)
}
#[test]
fn agent_cards_have_thirteen_rows_stable_colors_and_right_aligned_reported_tokens() {
    let mut feed = feed();
    let buf = render(&feed, 45, 26);
    let output = text(&buf);
    assert!(output.contains("feed_reasoning"));
    assert!(output.contains("audit_scrollback_only"));
    for top in [0, 13] {
        assert_eq!(buf[(0, top)].symbol(), "╭");
        assert_eq!(buf[(44, top)].symbol(), "╮");
        assert_eq!(buf[(0, top + 12)].symbol(), "╰");
        assert_eq!(buf[(44, top + 12)].symbol(), "╯");
        assert!(output.lines().nth(usize::from(top)).unwrap().contains("○ "));
        assert!(!output.contains("Idle"));
        assert!(
            output
                .lines()
                .nth(usize::from(top + 12))
                .unwrap()
                .contains("↑ 1.1k · ↓ 300")
        );
    }
    let id = ThreadId::from_u128(3);
    let before = buf[(0, 13)].fg;
    assert_eq!(before, color(id));
    feed.cards.get_mut(&ThreadId::from_u128(2)).unwrap().closed = true;
    let after = render(&feed, 45, 26);
    assert_eq!(after[(0, 0)].fg, before);
    assert!(!text(&after).contains("feed_reasoning"));
    insta::assert_snapshot!("agent_cards_two_agents", output);
}
#[test]
fn agent_cards_admit_recent_resumed_agents_and_keep_them_as_they_age() {
    let state = CardState {
        last_activity: Some(100),
        ..Default::default()
    };
    assert!(state.visible(1900));
    assert!(
        state.visible(9999),
        "an admitted window ages into gray rather than disappearing"
    );
    assert!(state.inactive(701));
    assert!(!state.inactive(700));
    let mut unknown = CardState::default();
    assert!(unknown.visible(1901));
    unknown.last_activity = Some(100);
    assert!(
        !unknown.visible(1901),
        "unknown dates must not permanently admit stale history"
    );
    let old = CardState {
        last_activity: Some(100),
        ..Default::default()
    };
    assert!(
        !old.visible(1901),
        "a fresh resume omits agents idle over thirty minutes"
    );
    let unloaded = CardState {
        last_activity: Some(100),
        status: ThreadStatus::NotLoaded,
        ..Default::default()
    };
    assert!(
        unloaded.visible(1900),
        "recent unloaded history is still useful on resume"
    );
    let running = CardState {
        last_activity: Some(100),
        status: ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
        },
        ..Default::default()
    };
    assert!(running.visible(9999));
    assert!(!running.inactive(9999));
    let new = CardState {
        last_activity: Some(100),
        started_in_session: true,
        closed: true,
        ..Default::default()
    };
    assert!(new.visible(9999));
    assert!(new.inactive(9999));
    let removed = CardState {
        started_in_session: true,
        removed: true,
        ..Default::default()
    };
    assert!(!removed.visible(100));
}

#[tokio::test]
async fn agent_cards_resume_does_not_refresh_activity_and_current_run_agents_survive_close() {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let id = ThreadId::from_u128(2);
    let now = chrono::Utc::now().timestamp();
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    let mut thread = overview_thread(id, Some(root), "worker", ThreadStatus::Idle);
    thread.created_at = now - 7200;
    thread.updated_at = now - 1801;
    app.agents_overview.threads.insert(id, Some(thread.clone()));
    app.sync_agent_feed_root();
    assert!(!app.agent_feed.roots[&root].card_visible(id, now));
    app.observe_agent_feed(&ServerNotification::ThreadStarted(
        codex_app_server_protocol::ThreadStartedNotification {
            thread: thread.clone(),
        },
    ));
    let card = &app.agent_feed.roots[&root].cards[&id];
    assert_eq!(card.last_activity, Some(thread.updated_at));
    assert!(!card.visible(now));
    thread.created_at = now + 1;
    thread.updated_at = now + 1;
    app.observe_agent_feed(&ServerNotification::ThreadStarted(
        codex_app_server_protocol::ThreadStartedNotification { thread },
    ));
    app.observe_agent_feed(&ServerNotification::ThreadClosed(
        ThreadClosedNotification {
            thread_id: id.to_string(),
        },
    ));
    let card = app
        .agent_feed
        .roots
        .get_mut(&root)
        .unwrap()
        .cards
        .get_mut(&id)
        .unwrap();
    card.last_activity = Some(now - 3600);
    assert!(card.visible(now));
    assert!(card.inactive(now));
}

#[test]
fn agent_cards_new_session_identity_uses_creation_time_not_resume_notifications() {
    let mut thread = overview_thread(ThreadId::from_u128(2), None, "worker", ThreadStatus::Idle);
    thread.created_at = 100;
    thread.updated_at = 200;
    let mut resumed = CardState::default();
    resumed.metadata(&thread, true, 1000);
    assert!(!resumed.started_in_session);
    assert!(!resumed.visible(2001));
    thread.created_at = 1001;
    let mut created = CardState::default();
    created.metadata(&thread, true, 1000);
    assert!(created.started_in_session);
    assert!(created.visible(10000));
}

#[test]
fn agent_cards_inactive_name_and_border_share_gray_and_restore_on_activity() {
    let mut feed = feed();
    let id = ThreadId::from_u128(2);
    let state = feed.cards.get_mut(&id).unwrap();
    state.last_activity = Some(chrono::Utc::now().timestamp() - 601);
    let buf = render(&feed, 45, 26);
    assert_eq!(buf[(0, 0)].fg, Color::DarkGray);
    assert_eq!(buf[(4, 0)].fg, buf[(0, 0)].fg);
    feed.cards.get_mut(&id).unwrap().last_activity = Some(chrono::Utc::now().timestamp());
    let buf = render(&feed, 45, 26);
    assert_eq!(buf[(0, 0)].fg, color(id));
    assert_eq!(buf[(4, 0)].fg, color(id));
}

#[test]
fn agent_cards_collapse_to_status_only_and_clear_only_the_window_rectangle() {
    let feed = feed();
    let id = ThreadId::from_u128(2);
    let area = Rect::new(0, 0, 45, 12);
    let mut expanded = Buffer::empty(area);
    feed.render_agent_card(id, area, false, &mut expanded, Path::new("/tmp"));
    let output = text(&expanded);
    assert!(output.contains("completed detailed source"));
    assert!(output.contains("↑ 1.1k · ↓ 300"));
    assert_eq!(expanded[(42, 0)].symbol(), "▲");
    let mut collapsed = Buffer::empty(area);
    for y in 0..area.height {
        for x in 0..area.width {
            collapsed[(x, y)].set_symbol("z");
        }
    }
    feed.render_agent_card(id, area, true, &mut collapsed, Path::new("/tmp"));
    let output = text(&collapsed);
    assert_eq!(collapsed[(42, 0)].symbol(), "▼");
    assert_eq!(collapsed[(0, 2)].symbol(), "╰");
    assert!(output.lines().nth(1).unwrap().contains("Idle"));
    assert!(!output.contains("completed detailed source"));
    assert!(!output.contains('↑') && !output.contains('↓'));
    assert!(
        output
            .lines()
            .skip(3)
            .all(|line| line.chars().all(|ch| ch == 'z'))
    );
}
#[test]
fn agent_cards_resize_summary_body_and_keep_unicode_titles_away_from_the_control() {
    let mut feed = feed();
    let id = ThreadId::from_u128(2);
    feed.agents
        .insert(id, "/root/日本語の長いエージェント名".into());
    for height in [6, 8, 12, 18] {
        let area = Rect::new(0, 0, 24, height);
        let mut buf = Buffer::empty(area);
        feed.render_agent_card(id, area, false, &mut buf, Path::new("/tmp"));
        assert_eq!(buf[(21, 0)].symbol(), "▲");
        assert_eq!(buf[(23, height - 1)].symbol(), "╯");
        assert!(text(&buf).lines().next().unwrap().contains("○ "));
        assert!(
            text(&buf)
                .lines()
                .nth(usize::from(height - 1))
                .unwrap()
                .contains('↑')
        );
    }
}
#[test]
fn agent_cards_bound_summaries_without_moving_footer_or_breaking_unicode() {
    let mut feed = feed();
    let id = ThreadId::from_u128(2);
    feed.upsert(
        MessageKey(id, "t".into(), "new".into()),
        &"🌻 very long summary with a path /long/path ".repeat(100),
        true,
        true,
        1001,
        false,
    );
    let buf = render(&feed, 27, 26);
    let output = text(&buf);
    assert!(output.lines().nth(11).unwrap().contains('…'));
    assert!(output.lines().nth(12).unwrap().contains("↑ 1.1k · ↓ 300"));
    assert_eq!(buf[(26, 12)].symbol(), "╯");
    for width in 0..9 {
        for height in [0, 1, 11, 12, 24] {
            let _ = render(&feed, width, height);
        }
    }
}
#[tokio::test]
async fn agent_cards_status_usage_and_lifecycle_events_are_not_model_activity() {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let id = ThreadId::from_u128(2);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    let mut thread = overview_thread(id, Some(root), "Worker", ThreadStatus::Idle);
    thread.updated_at = chrono::Utc::now().timestamp();
    app.agents_overview.threads.insert(id, Some(thread));
    app.sync_agent_feed_root();
    let usage = TokenUsageBreakdown {
        input_tokens: 1100,
        output_tokens: 300,
        total_tokens: 1400,
        cached_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 0,
    };
    app.observe_agent_feed(&ServerNotification::ThreadTokenUsageUpdated(
        ThreadTokenUsageUpdatedNotification {
            thread_id: id.to_string(),
            turn_id: "t".into(),
            token_usage: ThreadTokenUsage {
                total: usage.clone(),
                last: usage,
                model_context_window: None,
            },
        },
    ));
    let before = app.agent_feed.roots[&root].cards[&id].last_activity;
    assert_eq!(
        app.agent_feed.roots[&root].cards[&id].tokens,
        Some((1100, 300))
    );
    assert!(app.agent_feed.roots[&root].windows.is_empty());
    app.observe_agent_feed(&ServerNotification::ThreadStatusChanged(
        ThreadStatusChangedNotification {
            thread_id: id.to_string(),
            status: ThreadStatus::Active {
                active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
            },
        },
    ));
    assert_eq!(
        app.agent_feed.roots[&root].cards[&id].label(),
        "Awaiting approval"
    );
    app.observe_agent_feed(&ServerNotification::ThreadClosed(
        ThreadClosedNotification {
            thread_id: id.to_string(),
        },
    ));
    app.sync_agent_feed_root();
    assert!(!app.agent_feed.roots[&root].card_visible(id, chrono::Utc::now().timestamp()));
    assert_eq!(app.agent_feed.roots[&root].cards[&id].last_activity, before);
}
#[tokio::test]
async fn agent_cards_delayed_metadata_cannot_resurrect_a_closed_agent() {
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let id = ThreadId::from_u128(2);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    let mut thread = overview_thread(
        id,
        Some(root),
        "Worker",
        ThreadStatus::Active {
            active_flags: Vec::new(),
        },
    );
    thread.updated_at = 1000;
    app.agents_overview.threads.insert(id, Some(thread.clone()));
    app.sync_agent_feed_root();
    app.observe_agent_feed(&ServerNotification::ThreadClosed(
        ThreadClosedNotification {
            thread_id: id.to_string(),
        },
    ));
    let before = app.agent_feed.roots[&root].cards[&id].last_activity;
    app.apply_agent_feed_snapshot(
        root,
        app.agent_feed.generation,
        FeedSnapshot {
            latest_turns: HashMap::new(),
            threads: vec![thread],
            messages: Vec::new(),
            versions: HashMap::new(),
            errors: Vec::new(),
        },
    );
    assert!(!app.agent_feed.roots[&root].card_visible(id, 1000));
    assert_eq!(app.agent_feed.roots[&root].cards[&id].last_activity, before);
    app.observe_agent_feed(&ServerNotification::ThreadStatusChanged(
        ThreadStatusChangedNotification {
            thread_id: id.to_string(),
            status: ThreadStatus::Active {
                active_flags: Vec::new(),
            },
        },
    ));
    assert!(app.agent_feed.roots[&root].card_visible(id, chrono::Utc::now().timestamp()));
}

#[test]
fn agent_feed_auto_visibility_matches_renderable_cards_and_preserves_f6_override() {
    let mut feed = feed();
    // A resumed tree may retain descendants while every card is old or explicitly closed.
    for card in feed.cards.values_mut() {
        card.last_activity = Some(1000);
    }
    assert!(!feed.enabled());
    let id = ThreadId::from_u128(2);
    let card = feed.cards.get_mut(&id).unwrap();
    card.last_activity = Some(chrono::Utc::now().timestamp());
    card.closed = true;
    assert!(!feed.enabled());
    let card = feed.cards.get_mut(&id).unwrap();
    card.closed = false;
    card.status = ThreadStatus::NotLoaded;
    assert!(feed.enabled());
    feed.cards.get_mut(&id).unwrap().status = ThreadStatus::Idle;
    assert!(feed.enabled());
    feed.cards.get_mut(&id).unwrap().last_activity = Some(1000);
    assert!(feed.enabled(), "admitted windows stay in the current run");
    feed.cards.get_mut(&id).unwrap().status = ThreadStatus::Active {
        active_flags: Vec::new(),
    };
    assert!(feed.enabled());
    feed.manual = Some(false);
    assert!(!feed.enabled());
    feed.cards.get_mut(&id).unwrap().closed = true;
    feed.manual = Some(true);
    assert!(feed.enabled(), "F6 can still explicitly open an empty pane");
}

#[test]
fn agent_cards_hide_token_counters_until_reported_usage_is_available() {
    let mut feed = feed();
    let id = ThreadId::from_u128(2);
    feed.cards.get_mut(&id).unwrap().tokens = None;
    let buf = render(&feed, 45, 26);
    let output = text(&buf);
    let footer = output.lines().nth(12).unwrap();
    assert!(footer.starts_with('╰') && footer.ends_with('╯'));
    assert!(!footer.contains('↑') && !footer.contains('↓'), "{footer}");
    feed.cards.get_mut(&id).unwrap().tokens = Some((0, 0));
    let buf = render(&feed, 45, 26);
    assert!(text(&buf).lines().nth(12).unwrap().contains("↑ 0 · ↓ 0"));
    feed.cards.get_mut(&id).unwrap().tokens = Some((1100, 300));
    let buf = render(&feed, 45, 26);
    assert!(
        text(&buf)
            .lines()
            .nth(12)
            .unwrap()
            .contains("↑ 1.1k · ↓ 300")
    );
}

#[test]
fn agent_cards_use_all_eleven_interior_rows_and_show_state_in_the_title() {
    let mut feed = feed();
    let id = ThreadId::from_u128(2);
    // Use semantic plain text so markdown wrapping cannot introduce paragraph spacing.
    feed.semantic = true;
    feed.bubbles.push(Message {
        key: MessageKey(id, "s".into(), "s".into()),
        text: (1..=12)
            .map(|i| format!("line{i:02}{}", "x".repeat(35)))
            .collect::<Vec<_>>()
            .join(" "),
        complete: true,
        live: true,
        reasoning: false,
        parts: Vec::new(),
        order: (1, 1),
        truncated: false,
        rendered: Default::default(),
    });
    let buf = render(&feed, 45, 26);
    let output = text(&buf);
    assert!(output.lines().nth(1).unwrap().contains("line01"));
    assert!(output.lines().nth(11).unwrap().contains("line11"));
    assert!(output.lines().nth(11).unwrap().contains('…'));
    for (status, glyph) in [
        (ThreadStatus::Idle, "○"),
        (ThreadStatus::SystemError, "!"),
        (
            ThreadStatus::Active {
                active_flags: Vec::new(),
            },
            "◌",
        ),
        (
            ThreadStatus::Active {
                active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
            },
            "?",
        ),
        (
            ThreadStatus::Active {
                active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
            },
            "◇",
        ),
    ] {
        feed.cards.get_mut(&id).unwrap().status = status;
        let buf = render(&feed, 45, 26);
        let output = text(&buf);
        assert!(
            output
                .lines()
                .next()
                .unwrap()
                .contains(&format!("{glyph} ")),
            "{output}"
        );
        assert!(!output.lines().nth(11).unwrap().contains("Working"));
    }
}
