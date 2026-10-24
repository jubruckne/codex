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
                last_activity: Some(1000),
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
fn agent_cards_have_twelve_rows_stable_colors_and_right_aligned_reported_tokens() {
    let mut feed = feed();
    let buf = render(&feed, 45, 24);
    let output = text(&buf);
    assert!(output.contains("feed_reasoning"));
    assert!(output.contains("audit_scrollback_only"));
    for top in [0, 12] {
        assert_eq!(buf[(0, top)].symbol(), "╭");
        assert_eq!(buf[(44, top)].symbol(), "╮");
        assert_eq!(buf[(0, top + 11)].symbol(), "╰");
        assert_eq!(buf[(44, top + 11)].symbol(), "╯");
        assert!(
            output
                .lines()
                .nth(usize::from(top + 10))
                .unwrap()
                .contains("Idle")
        );
        assert!(
            output
                .lines()
                .nth(usize::from(top + 10))
                .unwrap()
                .contains("↑ 1.1k ↓ 300")
        );
    }
    let id = ThreadId::from_u128(3);
    let before = buf[(0, 12)].fg;
    assert_eq!(before, color(id));
    feed.cards.get_mut(&ThreadId::from_u128(2)).unwrap().closed = true;
    let after = render(&feed, 45, 24);
    assert_eq!(after[(0, 0)].fg, before);
    assert!(!text(&after).contains("feed_reasoning"));
    insta::assert_snapshot!("agent_cards_two_agents", output);
}
#[test]
fn agent_cards_filter_closed_and_idle_after_ten_minutes_but_keep_running_and_waiting() {
    let mut state = CardState {
        last_activity: Some(100),
        ..Default::default()
    };
    assert!(state.visible(700));
    assert!(!state.visible(701));
    state.status = ThreadStatus::Active {
        active_flags: Vec::new(),
    };
    assert!(state.visible(9999));
    state.status = ThreadStatus::Active {
        active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
    };
    assert!(state.visible(9999));
    assert_eq!(state.label(), "Awaiting input");
    state.closed = true;
    assert!(!state.visible(100));
    state.closed = false;
    state.status = ThreadStatus::NotLoaded;
    assert!(!state.visible(100));
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
    assert!(output.contains("↑ 1.1k ↓ 300"));
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
        assert!(
            text(&buf)
                .lines()
                .nth(usize::from(height - 2))
                .unwrap()
                .contains("Idle")
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
    let buf = render(&feed, 27, 24);
    let output = text(&buf);
    assert!(output.lines().nth(8).unwrap().contains('…'));
    assert!(output.lines().nth(10).unwrap().contains("↑ 1.1k ↓ 300"));
    assert_eq!(buf[(26, 11)].symbol(), "╯");
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
    // A resumed tree may retain descendants while every card is old, closed, or unloaded.
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
    assert!(feed.enabled());
    feed.cards.get_mut(&id).unwrap().last_activity = Some(1000);
    assert!(!feed.enabled());
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
    let buf = render(&feed, 45, 24);
    let output = text(&buf);
    let footer = output.lines().nth(10).unwrap();
    assert!(footer.contains("Idle"));
    assert!(!footer.contains('↑') && !footer.contains('↓'), "{footer}");
    feed.cards.get_mut(&id).unwrap().tokens = Some((0, 0));
    let buf = render(&feed, 45, 24);
    assert!(text(&buf).lines().nth(10).unwrap().contains("↑ 0 ↓ 0"));
    feed.cards.get_mut(&id).unwrap().tokens = Some((1100, 300));
    let buf = render(&feed, 45, 24);
    assert!(text(&buf).lines().nth(10).unwrap().contains("↑ 1.1k ↓ 300"));
}
