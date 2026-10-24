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
                admitted: std::cell::Cell::new(true),
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
fn agent_cards_have_ten_rows_stable_colors_and_right_aligned_reported_tokens() {
    let mut feed = feed();
    let buf = render(&feed, 45, 20);
    let output = text(&buf);
    assert!(output.contains("feed_reasoning"));
    assert!(output.contains("audit_scrollback_only"));
    for top in [0, 10] {
        assert_eq!(buf[(0, top)].symbol(), "╭");
        assert_eq!(buf[(44, top)].symbol(), "╮");
        assert_eq!(buf[(0, top + 9)].symbol(), "╰");
        assert_eq!(buf[(44, top + 9)].symbol(), "╯");
        assert!(output.lines().nth(usize::from(top)).unwrap().contains("○ "));
        assert!(!output.contains("Idle"));
        assert!(
            output
                .lines()
                .nth(usize::from(top + 9))
                .unwrap()
                .contains("↑ 1.1k · ↓ 300")
        );
    }
    let id = ThreadId::from_u128(3);
    let before = buf[(0, 10)].fg;
    assert_eq!(before, color(id));
    feed.cards.get_mut(&ThreadId::from_u128(2)).unwrap().closed = true;
    let after = render(&feed, 45, 20);
    assert_eq!(after[(0, 0)].fg, before);
    assert!(!text(&after).contains("feed_reasoning"));
    insta::assert_snapshot!("agent_cards_two_agents", output);
}
#[test]
fn agent_cards_resume_only_generating_agents_and_admit_later_activity() {
    for status in [
        ThreadStatus::Idle,
        ThreadStatus::NotLoaded,
        ThreadStatus::SystemError,
        ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
        },
        ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
        },
    ] {
        for last_activity in [None, Some(100), Some(9999)] {
            let mut state = CardState {
                status: status.clone(),
                last_activity,
                ..Default::default()
            };
            assert!(
                !state.visible(9999),
                "resumed {status:?} must stay hidden regardless of recency"
            );
            state.status = ThreadStatus::Active {
                active_flags: vec![],
            };
            assert!(
                state.visible(9999),
                "a hidden agent appears when it starts generating"
            );
            state.status = ThreadStatus::Idle;
            assert!(
                state.visible(9999),
                "admitted agents remain available in this run"
            );
        }
    }
    let mut running = CardState {
        last_activity: Some(100),
        status: ThreadStatus::Active {
            active_flags: vec![],
        },
        ..Default::default()
    };
    assert!(running.visible(9999));
    assert!(!running.inactive(9999));
    running.status = ThreadStatus::Idle;
    assert!(running.visible(9999));
    assert!(running.inactive(701));
    assert!(!running.inactive(700));
    let new = CardState {
        last_activity: Some(100),
        started_in_session: true,
        closed: true,
        ..Default::default()
    };
    assert!(new.visible(9999));
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
    let buf = render(&feed, 45, 20);
    assert_eq!(buf[(0, 0)].fg, Color::DarkGray);
    assert_eq!(buf[(4, 0)].fg, buf[(0, 0)].fg);
    feed.cards.get_mut(&id).unwrap().last_activity = Some(chrono::Utc::now().timestamp());
    let buf = render(&feed, 45, 20);
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
    let buf = render(&feed, 27, 20);
    let output = text(&buf);
    assert!(output.lines().nth(8).unwrap().contains('…'));
    assert!(output.lines().nth(9).unwrap().contains("↑ 1.1k · ↓ 300"));
    assert_eq!(buf[(26, 9)].symbol(), "╯");
    for width in 0..9 {
        for height in [0, 1, 9, 10, 11, 20] {
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
    // A resumed tree can retain idle descendants, including ones with recent activity.
    for card in feed.cards.values_mut() {
        card.admitted.set(false);
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
    assert!(!feed.enabled());
    feed.cards.get_mut(&id).unwrap().status = ThreadStatus::Idle;
    assert!(!feed.enabled());
    feed.cards.get_mut(&id).unwrap().status = ThreadStatus::Active {
        active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
    };
    assert!(!feed.enabled());
    feed.cards.get_mut(&id).unwrap().status = ThreadStatus::Active {
        active_flags: Vec::new(),
    };
    assert!(
        feed.enabled(),
        "a resumed agent opens the view when generation starts"
    );
    feed.cards.get_mut(&id).unwrap().status = ThreadStatus::Idle;
    feed.cards.get_mut(&id).unwrap().last_activity = Some(1000);
    assert!(feed.enabled(), "admitted windows stay in the current run");
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
    let buf = render(&feed, 45, 20);
    let output = text(&buf);
    let footer = output.lines().nth(9).unwrap();
    assert!(footer.starts_with('╰') && footer.ends_with('╯'));
    assert!(!footer.contains('↑') && !footer.contains('↓'), "{footer}");
    feed.cards.get_mut(&id).unwrap().tokens = Some((0, 0));
    let buf = render(&feed, 45, 20);
    assert!(text(&buf).lines().nth(9).unwrap().contains("↑ 0 · ↓ 0"));
    feed.cards.get_mut(&id).unwrap().tokens = Some((1100, 300));
    let buf = render(&feed, 45, 20);
    assert!(
        text(&buf)
            .lines()
            .nth(9)
            .unwrap()
            .contains("↑ 1.1k · ↓ 300")
    );
}

#[test]
fn agent_cards_use_all_eight_interior_rows_and_show_state_in_the_title() {
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
    let buf = render(&feed, 45, 20);
    let output = text(&buf);
    assert!(output.lines().nth(1).unwrap().contains("line01"));
    assert!(output.lines().nth(8).unwrap().contains("line08"));
    assert!(output.lines().nth(8).unwrap().contains('…'));
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
        let buf = render(&feed, 45, 20);
        let output = text(&buf);
        assert!(
            output
                .lines()
                .next()
                .unwrap()
                .contains(&format!("{glyph} ")),
            "{output}"
        );
        assert!(!output.lines().nth(8).unwrap().contains("Working"));
    }
}

#[test]
fn agent_cards_collapsed_semantic_window_shows_caption_instead_of_status() {
    let mut feed = feed();
    feed.semantic = true;
    let id = ThreadId::from_u128(2);
    feed.captions.insert(id, "I finished the research.".into());
    let area = Rect::new(0, 0, 45, 3);
    let mut buf = Buffer::empty(area);
    feed.render_agent_card(id, area, true, &mut buf, Path::new("/tmp"));
    let output = text(&buf);
    assert!(
        output
            .lines()
            .nth(1)
            .unwrap()
            .contains("I finished the research.")
    );
    assert!(!output.contains("Idle"));
    feed.captions.clear();
    let mut empty = Buffer::empty(area);
    feed.render_agent_card(id, area, true, &mut empty, Path::new("/tmp"));
    assert!(!text(&empty).contains("Idle"));
    assert!(
        text(&empty)
            .lines()
            .nth(1)
            .unwrap()
            .trim_matches(['│', ' '])
            .is_empty()
    );
}
