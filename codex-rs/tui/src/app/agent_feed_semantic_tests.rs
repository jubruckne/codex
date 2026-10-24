use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

fn message(agent: ThreadId, index: u64, reasoning: bool, text: &str) -> Message {
    Message {
        key: MessageKey(agent, "turn".into(), index.to_string()),
        text: text.into(),
        complete: true,
        live: true,
        reasoning,
        parts: Vec::new(),
        order: (chrono::Utc::now().timestamp_millis() + index as i64, index),
        truncated: false,
        rendered: Default::default(),
    }
}
fn summary(text: &str) -> Summary {
    Summary {
        text: text.into(),
        caption: "I finished the research.".into(),
    }
}
fn source_window() -> Window {
    let mut window = Window::default();
    window.start_turn("turn");
    window
}
fn input(text: &str) -> Arc<Input> {
    let mut window = source_window();
    window.observe(&message(ThreadId::from_u128(2), 1, false, text));
    window.input().unwrap()
}
fn job(revision: u64, text: &str) -> Job {
    Job {
        root: ThreadId::from_u128(1),
        agent: ThreadId::from_u128(2),
        revision,
        ready: tokio::time::Instant::now(),
        input: input(text),
    }
}
#[test]
fn semantic_window_retains_selected_turn_responses_and_summaries_in_order() {
    let agent = ThreadId::from_u128(2);
    let mut window = source_window();
    for index in 0..6 {
        window.observe(&message(
            agent,
            index * 2,
            false,
            &format!("response {index}"),
        ));
        window.observe(&message(
            agent,
            index * 2 + 1,
            true,
            &format!("thinking {index}"),
        ));
    }
    let input = window.input().unwrap();
    let text = format_entries(&input.entries, INPUT_BYTES, &input.latest_turn);
    assert!(text.contains("response 2"));
    assert!(text.contains("thinking 2"));
    let mut last = 0;
    for index in 0..6 {
        for part in [format!("response {index}"), format!("thinking {index}")] {
            let offset = text.find(&part).unwrap();
            assert!(offset >= last);
            last = offset;
        }
    }
    assert_eq!(
        input
            .entries
            .iter()
            .filter(|entry| !entry.reasoning)
            .count(),
        6
    );
    let previous = input.hash;
    let revision = window.revision;
    window.observe(&message(agent, 11, true, "thinking 5"));
    assert_eq!(window.revision, revision);
    assert_eq!(window.input().unwrap().hash, previous);
    window.observe(&message(agent, 11, true, "new thinking"));
    assert_ne!(window.input().unwrap().hash, previous);
}
#[test]
fn semantic_windows_survive_global_feed_retention_and_bound_unicode() {
    let mut feed = RootFeed::default();
    let first = ThreadId::from_u128(2);
    let busy = ThreadId::from_u128(3);
    feed.agents.insert(first, "First".into());
    feed.agents.insert(busy, "Busy".into());
    for index in 0..4 {
        feed.upsert(
            MessageKey(first, "t".into(), index.to_string()),
            "first response",
            true,
            true,
            chrono::Utc::now().timestamp_millis() + index,
            false,
        );
    }
    for index in 0..250 {
        feed.upsert(
            MessageKey(busy, "t".into(), index.to_string()),
            "busy response",
            true,
            true,
            chrono::Utc::now().timestamp_millis() + index + 10,
            false,
        );
    }
    assert!(!feed.messages.iter().any(|message| message.key.0 == first));
    assert_eq!(feed.windows[&first].entries.len(), 4);
    for index in 0..100 {
        feed.reasoning_delta(
            MessageKey(first, "r".into(), index.to_string()),
            0,
            &"🌻".repeat(6000),
            chrono::Utc::now().timestamp_millis() + index + 300,
        );
    }
    let window = feed.windows.get_mut(&first).unwrap();
    window.finish_turn("r", TurnStatus::Completed);
    let input = window.input().unwrap();
    assert!(input.entries.iter().all(|entry| entry.reasoning));
    assert!(input.entries.len() <= 32);
    for budget in [2048, INPUT_BYTES] {
        let formatted = format_entries(&input.entries, budget, &input.latest_turn);
        assert!(formatted.len() <= budget, "{} > {budget}", formatted.len());
        assert!(formatted.contains("[excerpt truncated]"));
    }
}
#[derive(Default)]
struct Counts {
    active: AtomicUsize,
    maximum: AtomicUsize,
    started: AtomicUsize,
    cancelled: AtomicUsize,
}
struct Guard {
    counts: Arc<Counts>,
    completed: bool,
}
impl Drop for Guard {
    fn drop(&mut self) {
        self.counts.active.fetch_sub(1, Ordering::SeqCst);
        if !self.completed {
            self.counts.cancelled.fetch_add(1, Ordering::SeqCst);
        }
    }
}
struct Fake {
    available: bool,
    delay: Duration,
    counts: Arc<Counts>,
    started: tokio::sync::mpsc::UnboundedSender<()>,
}
impl Backend for Fake {
    fn available(&self) -> bool {
        self.available
    }
    async fn summarize(&mut self, input: &Input) -> Result<Summary, String> {
        self.counts.started.fetch_add(1, Ordering::SeqCst);
        let active = self.counts.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.counts.maximum.fetch_max(active, Ordering::SeqCst);
        let mut guard = Guard {
            counts: self.counts.clone(),
            completed: false,
        };
        let _ = self.started.send(());
        tokio::time::sleep(self.delay).await;
        guard.completed = true;
        Ok(summary(&input.entries.last().unwrap().text))
    }
    async fn settle(&mut self) -> bool {
        true
    }
}
fn fake(available: bool) -> (Fake, tokio::sync::mpsc::UnboundedReceiver<()>, Arc<Counts>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let counts = Arc::new(Counts::default());
    (
        Fake {
            available,
            delay: Duration::from_millis(100),
            counts: counts.clone(),
            started: tx,
        },
        rx,
        counts,
    )
}
#[tokio::test(start_paused = true)]
async fn semantic_worker_cancels_superseded_requests_serializes_and_caches() {
    let (backend, mut started, counts) = fake(true);
    let (tx, rx) = watch::channel(vec![job(1, "obsolete")]);
    let (events, mut results) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(worker(backend, rx, move |event| {
        let _ = events.send(event);
    }));
    assert!(matches!(
        results.recv().await,
        Some(Event::Availability(true))
    ));
    started.recv().await.unwrap();
    tx.send_replace(vec![job(2, "newest")]);
    started.recv().await.unwrap();
    match results.recv().await.unwrap() {
        Event::Finished {
            revision, result, ..
        } => {
            assert_eq!(revision, 2);
            assert_eq!(result.unwrap().text, "newest");
        }
        _ => panic!("expected completion"),
    }
    assert_eq!(counts.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(counts.maximum.load(Ordering::SeqCst), 1);
    tx.send_replace(vec![job(3, "newest")]);
    assert!(matches!(
        results.recv().await,
        Some(Event::Finished { revision: 3, .. })
    ));
    assert_eq!(counts.started.load(Ordering::SeqCst), 2);
    // A different agent is queued behind the first one, never run in parallel.
    let first = job(4, "first agent");
    let mut other = job(4, "other agent");
    other.agent = ThreadId::from_u128(3);
    tx.send_replace(vec![first, other]);
    for _ in 0..2 {
        assert!(matches!(results.recv().await, Some(Event::Finished { .. })));
    }
    assert_eq!(counts.maximum.load(Ordering::SeqCst), 1);
    drop(tx);
    task.await.unwrap();
}
#[tokio::test(start_paused = true)]
async fn semantic_worker_coalesces_pending_updates_and_cancels_on_root_change() {
    let (backend, mut started, counts) = fake(true);
    let mut pending = job(1, "old");
    pending.ready += DEBOUNCE;
    let (tx, rx) = watch::channel(vec![pending]);
    let (events, mut results) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(worker(backend, rx, move |event| {
        let _ = events.send(event);
    }));
    results.recv().await.unwrap();
    let mut newest = job(3, "latest pending");
    newest.ready += DEBOUNCE;
    tx.send_replace(vec![newest]);
    started.recv().await.unwrap();
    tx.send_replace(Vec::new());
    tokio::task::yield_now().await;
    assert_eq!(counts.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(counts.started.load(Ordering::SeqCst), 1);
    assert!(results.try_recv().is_err());
    drop(tx);
    task.await.unwrap();
}
#[tokio::test]
async fn semantic_unavailable_backend_stops_without_inference() {
    let (backend, _, counts) = fake(false);
    let (_tx, rx) = watch::channel(vec![job(1, "data")]);
    let events = std::cell::RefCell::new(Vec::new());
    worker(backend, rx, |event| events.borrow_mut().push(event)).await;
    assert!(matches!(
        events.borrow().as_slice(),
        [Event::Availability(false)]
    ));
    assert_eq!(counts.started.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn semantic_results_reject_stale_generations_and_roots_and_render_bubbles() {
    let (mut app, _, _) = crate::app::tests::make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let agent = ThreadId::from_u128(2);
    app.agent_feed.root = Some(root);
    app.agent_feed.semantic_available = true;
    let feed = app.agent_feed.roots.entry(root).or_default();
    feed.semantic = true;
    feed.agents.insert(agent, "Worker".into());
    feed.upsert(
        MessageKey(agent, "t".into(), "m".into()),
        "original",
        true,
        true,
        chrono::Utc::now().timestamp_millis(),
        false,
    );
    let window = feed.windows.get_mut(&agent).unwrap();
    window.finish_turn("t", TurnStatus::Completed);
    let old = window.revision;
    let old_hash = window.input().unwrap().hash;
    feed.reasoning_delta(
        MessageKey(agent, "t".into(), "r".into()),
        0,
        "new reasoning",
        chrono::Utc::now().timestamp_millis() + 1,
    );
    app.apply_agent_feed_semantic(Event::Finished {
        root,
        agent,
        revision: old,
        hash: old_hash,
        result: Ok(summary("stale")),
    });
    assert!(app.agent_feed.roots[&root].bubbles.is_empty());
    let window = app
        .agent_feed
        .roots
        .get_mut(&root)
        .unwrap()
        .windows
        .get_mut(&agent)
        .unwrap();
    let hash = window.input().unwrap().hash;
    let revision = window.revision;
    app.apply_agent_feed_semantic(Event::Finished {
        root: ThreadId::from_u128(9),
        agent,
        revision,
        hash,
        result: Ok(summary("wrong root")),
    });
    for _ in 0..2 {
        app.apply_agent_feed_semantic(Event::Finished {
            root,
            agent,
            revision,
            hash,
            result: Ok(summary(
                "Completed the parser fix. Next it will run focused tests.",
            )),
        });
    }
    let feed = &app.agent_feed.roots[&root];
    assert_eq!(feed.bubbles.len(), 1);
    let area = Rect::new(0, 0, 42, 12);
    let mut buf = Buffer::empty(area);
    feed.render_agent_card(agent, area, false, &mut buf, Path::new("/tmp"));
    let text = crate::app::owned_transcript::tests::buffer_text(&buf);
    assert!(text.contains("╭ · Worker"));
    assert!(text.contains("Completed the parser fix."));
    assert!(!text.contains("new reasoning"));
    insta::assert_snapshot!("semantic_agent_bubble", text);
    let feed = app.agent_feed.roots.get_mut(&root).unwrap();
    feed.upsert(
        MessageKey(agent, "t".into(), "next".into()),
        "The focused tests passed.",
        true,
        true,
        chrono::Utc::now().timestamp_millis() + 2,
        false,
    );
    let window = feed.windows.get_mut(&agent).unwrap();
    let hash = window.input().unwrap().hash;
    let revision = window.revision;
    app.apply_agent_feed_semantic(Event::Finished {
        root,
        agent,
        revision,
        hash,
        result: Ok(summary(
            "The focused tests passed. Next it will review the change.",
        )),
    });
    let feed = &app.agent_feed.roots[&root];
    assert_eq!(feed.bubbles.len(), 1);
    assert!(
        feed.bubbles[0]
            .text
            .starts_with("The focused tests passed.")
    );
    assert!(!feed.bubbles[0].text.contains("Completed the parser fix"));
    app.apply_agent_feed_semantic(Event::Availability(false));
    assert!(!app.agent_feed.roots[&root].semantic);
    assert!(!app.agent_feed.roots[&root].messages.is_empty());
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[tokio::test]
#[ignore = "Runs the actual on-device Apple model; invoke explicitly for local smoke validation"]
async fn semantic_apple_model_smoke() {
    let mut backend = Apple::default();
    eprintln!("Apple Foundation Models available: {}", backend.available());
    if !backend.available() {
        return;
    }
    for (label, text, unsupported) in [
        (
            "read-only recommendation",
            "I finished read-only source research and identified existing tool-result fields. I recommend compact evidence records and factual evaluation fixtures. I did not implement changes, edit files, or run tests.",
            vec!["i implemented", "i ran tests", "i plan to"],
        ),
        (
            "proposal and future plan",
            "I inspected the cache implementation and proposed a design. I have not changed files. I plan to implement the design next.",
            vec!["i implemented", "i'm implementing", "i am implementing"],
        ),
        (
            "tests added but not run",
            "I added regression tests for the parser. I have not run them. Next I will run the focused test suite.",
            vec!["tests passed", "i ran", "i verified"],
        ),
    ] {
        let mut window = source_window();
        window.observe(&message(ThreadId::from_u128(2), 0, false, text));
        window.finish_turn("turn", TurnStatus::Completed);
        let data = window.input_for(false).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(60), backend.summarize(&data))
            .await
            .unwrap()
            .unwrap();
        assert!(backend.settle().await);
        assert!(result.text.split_whitespace().count() <= 40);
        assert!(result.caption.split_whitespace().count() <= 10);
        let output = format!("{} {}", result.text, result.caption).to_lowercase();
        for claim in unsupported {
            assert!(
                !output.contains(claim),
                "{label}: unsupported claim {claim:?} in {output}"
            );
        }
        eprintln!("{label}: {}\nCaption: {}", result.text, result.caption);
    }
}

#[test]
fn semantic_history_order_overrides_arrival_times_without_restarting_unchanged_inputs() {
    let agent = ThreadId::from_u128(2);
    let mut window = source_window();
    let mut response = message(agent, 2, false, "second response");
    response.order = (chrono::Utc::now().timestamp_millis() + 1000, 1);
    window.observe(&response);
    let initial = window.input().unwrap();
    let first_revision = window.revision;
    window.begin_history();
    let first = message(agent, 0, false, "first response");
    let mut thinking = message(agent, 1, true, "between responses");
    thinking.order = (chrono::Utc::now().timestamp_millis(), 1);
    window.observe(&first);
    window.observe(&thinking);
    window.observe(&response);
    let keys = [
        first.key.clone(),
        thinking.key.clone(),
        response.key.clone(),
    ];
    window.finish_history(&keys);
    let input = window.input().unwrap();
    let text = format_entries(&input.entries, INPUT_BYTES, &input.latest_turn);
    assert_ne!(input.hash, initial.hash);
    assert!(window.revision > first_revision);
    assert!(text.find("first response").unwrap() < text.find("between responses").unwrap());
    assert!(text.find("between responses").unwrap() < text.find("second response").unwrap());
    let revision = window.revision;
    let changed = window.changed;
    window.begin_history();
    window.observe(&first);
    window.observe(&thinking);
    window.observe(&response);
    window.finish_history(&keys);
    assert_eq!(window.input().unwrap().hash, input.hash);
    assert_eq!(window.revision, revision);
    assert_eq!(window.changed, changed);
}

#[test]
fn semantic_source_age_uses_milliseconds_and_expires_without_new_events() {
    let agent = ThreadId::from_u128(2);
    let now = 3_000_000;
    let mut window = source_window();
    window.observe_at(
        &message(agent, 1, false, "boundary response"),
        now - SOURCE_AGE_MS,
    );
    window.observe_at(
        &message(agent, 2, true, "later reasoning"),
        now - SOURCE_AGE_MS + 100,
    );
    window.observe_at(&message(agent, 3, false, "latest response"), now);
    let boundary = window.input_at(now).unwrap();
    assert_eq!(
        boundary.entries.len(),
        3,
        "exactly fifteen minutes is retained"
    );
    let old_revision = window.revision;
    let after = window.input_at(now + 1).unwrap();
    assert_eq!(after.entries.len(), 2);
    assert_ne!(after.hash, boundary.hash);
    assert!(window.revision > old_revision);
    assert!(
        !format_entries(&after.entries, INPUT_BYTES, &after.latest_turn)
            .contains("boundary response")
    );
    let after_reasoning = window.input_at(now + 101).unwrap();
    assert_eq!(after_reasoning.entries.len(), 1);
    assert_ne!(after_reasoning.hash, after.hash);
    let revision = window.revision;
    assert!(window.input_at(now + SOURCE_AGE_MS + 1).is_none());
    assert!(
        window.revision > revision,
        "empty input invalidates an outstanding generation"
    );
    let revision = window.revision;
    assert!(window.input_at(now + SOURCE_AGE_MS + 2).is_none());
    assert_eq!(window.revision, revision);
}

#[test]
fn semantic_unknown_history_dates_are_excluded_and_hydration_does_not_make_old_text_recent() {
    let agent = ThreadId::from_u128(2);
    let mut window = source_window();
    let unknown = message(agent, 1, false, "unknown date");
    window.observe_at(&unknown, 0);
    assert!(window.input_at(3_000_000).is_none());
    let old = message(agent, 2, false, "old persisted response");
    for _ in 0..2 {
        window.begin_history();
        window.observe_at(&old, 1_000_000);
        window.finish_history(&[unknown.key.clone(), old.key.clone()]);
        assert!(window.input_at(3_000_000).is_none());
    }
}

#[test]
fn semantic_streaming_updates_use_event_dates_without_reordering_or_repeating_unchanged_input() {
    let agent = ThreadId::from_u128(2);
    let now = 3_000_000;
    let mut window = source_window();
    let mut streamed = message(agent, 1, false, "initial source");
    let original_order = streamed.order;
    window.observe_at(&streamed, now - SOURCE_AGE_MS - 1);
    assert!(window.input_at(now).is_none());
    streamed.text = "new source update".into();
    window.observe_at(&streamed, now - 1);
    let input = window.input_at(now).unwrap();
    assert_eq!(input.entries[0].order, original_order);
    assert_eq!(input.entries[0].source_at_ms, now - 1);
    let revision = window.revision;
    window.observe_at(&streamed, now);
    let refreshed = window.input_at(now).unwrap();
    assert_eq!(refreshed.hash, input.hash);
    assert_eq!(window.revision, revision);
    assert!(refreshed.expires_at_ms > input.expires_at_ms);
}

#[tokio::test(start_paused = true)]
async fn semantic_worker_rejects_expired_jobs_and_cancels_at_source_expiry() {
    let (mut backend, mut started, counts) = fake(true);
    backend.delay = Duration::from_secs(60);
    let mut expired = job(1, "expired before inference");
    Arc::get_mut(&mut expired.input).unwrap().expires_at_ms =
        chrono::Utc::now().timestamp_millis() - 1;
    let (tx, rx) = watch::channel(vec![expired]);
    let (events, mut results) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(worker(backend, rx, move |event| {
        let _ = events.send(event);
    }));
    assert!(matches!(
        results.recv().await,
        Some(Event::Availability(true))
    ));
    tokio::task::yield_now().await;
    assert_eq!(counts.started.load(Ordering::SeqCst), 0);
    let mut brief = job(2, "expires during inference");
    Arc::get_mut(&mut brief.input).unwrap().expires_at_ms =
        chrono::Utc::now().timestamp_millis() + 10_000;
    tx.send_replace(vec![brief]);
    started.recv().await.unwrap();
    tokio::time::advance(Duration::from_secs(11)).await;
    tokio::task::yield_now().await;
    assert_eq!(counts.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(counts.active.load(Ordering::SeqCst), 0);
    assert!(results.try_recv().is_err());
    drop(tx);
    task.await.unwrap();
}

#[test]
fn semantic_current_turn_excludes_every_previous_turn_entry() {
    let agent = ThreadId::from_u128(2);
    let mut window = source_window();
    for (index, turn, reasoning, text) in [
        (0, "old", false, "I finished the audit."),
        (1, "old", true, "I might examine the old test next."),
        (2, "new", true, "I'm checking window resizing."),
        (3, "new", false, "The resizing fix passes."),
    ] {
        let mut entry = message(agent, index, reasoning, text);
        entry.key.1 = turn.into();
        window.observe(&entry);
    }
    window.start_turn("new");
    let input = window.input_for(true).unwrap();
    assert_eq!(input.entries.len(), 2);
    let text = input.formatted(2048);
    assert!(text.contains("Current turn: in progress"));
    assert!(!text.contains("audit"));
    assert!(!text.contains("old test"));
    assert!(text.contains("checking window resizing"));
    assert!(text.contains("resizing fix passes"));
    assert!(text.len() <= 2048);
}

#[test]
fn semantic_empty_current_turn_has_no_input_and_invalidates_previous_result() {
    let agent = ThreadId::from_u128(2);
    let mut window = source_window();
    window.observe(&message(agent, 0, false, "I plan to inspect an old test."));
    let old = window.input().unwrap();
    let revision = window.revision;
    window.start_turn("new");
    assert!(window.input_for(true).is_none());
    assert!(window.revision > revision);
    window.history_turns(
        TurnHistory {
            latest: Some(TurnContext {
                id: "turn".into(),
                status: TurnStatus::Completed,
            }),
            completed: Some(TurnContext {
                id: "turn".into(),
                status: TurnStatus::Completed,
            }),
        },
        false,
    );
    assert!(
        window.input_for(true).is_none(),
        "stale history cannot restore old focus"
    );
    let mut reasoning = message(agent, 1, true, "I'm checking a new task.");
    reasoning.key.1 = "new".into();
    window.observe(&reasoning);
    let next = window.input_for(true).unwrap();
    assert_ne!(next.hash, old.hash);
    assert_eq!(next.entries.len(), 1);
}

#[test]
fn semantic_idle_uses_last_finished_turn_while_active_empty_turn_stays_empty() {
    let agent = ThreadId::from_u128(2);
    let mut window = Window::default();
    window.observe(&message(agent, 0, false, "I finished read-only research."));
    window.history_turns(
        TurnHistory {
            latest: Some(TurnContext {
                id: "empty-latest-turn".into(),
                status: TurnStatus::InProgress,
            }),
            completed: Some(TurnContext {
                id: "turn".into(),
                status: TurnStatus::Completed,
            }),
        },
        true,
    );
    assert!(window.input_for(true).is_none());
    let idle = window.input_for(false).unwrap();
    assert_eq!(idle.latest_turn, "turn");
    assert!(
        idle.formatted(INPUT_BYTES)
            .contains("Last finished turn: completed")
    );
    window.start_turn("next");
    let mut response = message(agent, 1, false, "I added tests but did not run them.");
    response.key.1 = "next".into();
    window.observe(&response);
    let running = window.input_for(true).unwrap();
    window.finish_turn("next", TurnStatus::Failed);
    let failed = window.input_for(false).unwrap();
    assert_eq!(failed.latest_turn, "next");
    assert_ne!(
        failed.hash, running.hash,
        "lifecycle changes invalidate a running summary"
    );
    assert!(
        failed
            .formatted(INPUT_BYTES)
            .contains("Last finished turn: failed")
    );
    assert_eq!(failed.entries.len(), 1);
    window.finish_turn("next", TurnStatus::Interrupted);
    assert!(
        window
            .input_for(false)
            .unwrap()
            .formatted(INPUT_BYTES)
            .contains("interrupted")
    );
}

#[tokio::test]
async fn semantic_turn_started_notification_changes_focus_before_any_model_output() {
    use crate::app::agents_overview::tests::overview_thread;
    use crate::app::tests::make_test_app_with_channels;
    use codex_app_server_protocol::{Turn, TurnItemsView, TurnStartedNotification, TurnStatus};
    let (mut app, _, _) = make_test_app_with_channels().await;
    let root = ThreadId::from_u128(1);
    let agent = ThreadId::from_u128(2);
    app.primary_thread_id = Some(root);
    app.active_thread_id = Some(root);
    app.local_settings.tui.fullscreen_transcript = true;
    app.agents_overview.threads.insert(
        agent,
        Some(overview_thread(
            agent,
            Some(root),
            "Worker",
            codex_app_server_protocol::ThreadStatus::Idle,
        )),
    );
    app.sync_agent_feed_root();
    let window = app
        .agent_feed
        .roots
        .get_mut(&root)
        .unwrap()
        .windows
        .entry(agent)
        .or_default();
    window.finish_turn("turn", TurnStatus::Completed);
    window.observe(&message(agent, 0, false, "Earlier work."));
    let previous = window.input().unwrap().hash;
    let previous_revision = window.revision;
    app.agent_feed.semantic_available = true;
    let feed = app.agent_feed.roots.get_mut(&root).unwrap();
    feed.bubbles
        .push(message(agent, 99, false, "Previous turn summary."));
    feed.published.insert(agent, previous);
    feed.captions.insert(agent, "Previous caption.".into());
    app.observe_agent_feed(&ServerNotification::TurnStarted(TurnStartedNotification {
        thread_id: agent.to_string(),
        turn: Turn {
            id: "new-turn".into(),
            root_turn_id: None,
            items_view: TurnItemsView::Full,
            status: TurnStatus::InProgress,
            error: None,
            started_at: Some(chrono::Utc::now().timestamp()),
            completed_at: None,
            duration_ms: None,
            items: vec![],
        },
    }));
    let window = app
        .agent_feed
        .roots
        .get_mut(&root)
        .unwrap()
        .windows
        .get_mut(&agent)
        .unwrap();
    assert!(window.input_for(true).is_none());
    assert_ne!(window.last_hash, Some(previous));
    assert_eq!(
        app.agent_feed.roots[&root].bubbles[0].text,
        "Previous turn summary."
    );
    assert_eq!(
        app.agent_feed.roots[&root].published.get(&agent),
        Some(&previous)
    );
    app.apply_agent_feed_semantic(Event::Finished {
        root,
        agent,
        revision: previous_revision,
        hash: previous,
        result: Ok(summary("Obsolete previous-turn generation.")),
    });
    assert_eq!(
        app.agent_feed.roots[&root].bubbles[0].text,
        "Previous turn summary."
    );
    let feed = app.agent_feed.roots.get_mut(&root).unwrap();
    feed.upsert(
        MessageKey(agent, "new-turn".into(), "new-message".into()),
        "I finished the new research.",
        true,
        true,
        chrono::Utc::now().timestamp_millis(),
        false,
    );
    let window = feed.windows.get_mut(&agent).unwrap();
    let current = window.input_for(true).unwrap();
    assert_eq!(current.entries.len(), 1);
    assert!(!current.formatted(INPUT_BYTES).contains("Earlier work"));
    let revision = window.revision;
    app.apply_agent_feed_semantic(Event::Finished {
        root,
        agent,
        revision,
        hash: current.hash,
        result: Err("Invalid local summary or caption".into()),
    });
    assert_eq!(
        app.agent_feed.roots[&root].bubbles[0].text,
        "Previous turn summary."
    );
    assert_eq!(
        app.agent_feed.roots[&root].captions[&agent],
        "Previous caption."
    );
    app.apply_agent_feed_semantic(Event::Finished {
        root,
        agent,
        revision,
        hash: current.hash,
        result: Ok(summary("New turn summary.")),
    });
    let feed = &app.agent_feed.roots[&root];
    assert_eq!(feed.bubbles.len(), 1);
    assert_eq!(feed.bubbles[0].text, "New turn summary.");
    assert_eq!(feed.captions[&agent], "I finished the research.");
    assert_eq!(feed.published.get(&agent), Some(&current.hash));
}

#[test]
fn semantic_missing_lifecycle_does_not_guess_a_current_or_completed_turn() {
    let mut window = Window::default();
    window.observe(&message(
        ThreadId::from_u128(2),
        0,
        false,
        "Old unclassified output.",
    ));
    assert!(window.input_for(true).is_none());
    assert!(window.input_for(false).is_none());
}

#[test]
fn semantic_summary_preserves_two_paragraphs_and_normalizes_wrapped_lines() {
    assert_eq!(
        normalize_summary(
            "\nI finished  read-only\nresearch.\r\n  \r\nI have not\nchanged files.\n\nTests were not run.\n"
        ),
        "I finished read-only research.\n\nI have not changed files. Tests were not run."
    );
    assert_eq!(normalize_summary(" \n\t"), "");
}

#[test]
fn semantic_validates_summary_and_caption_lengths_without_cutting_sentences() {
    let valid = Summary {
        text: "I finished read-only research.\n\nI have not changed files.".into(),
        caption: "I finished  read-only\nresearch.".into(),
    };
    let result = validate_summary(valid).unwrap();
    assert_eq!(
        result.text,
        "I finished read-only research.\n\nI have not changed files."
    );
    assert_eq!(result.caption, "I finished read-only research.");
    assert!(
        validate_summary(Summary {
            text: "word ".repeat(40),
            caption: "word ".repeat(10)
        })
        .is_ok()
    );
    for bad in [
        Summary {
            text: "word ".repeat(41),
            caption: "Done.".into(),
        },
        Summary {
            text: "Done.".into(),
            caption: "word ".repeat(11),
        },
        Summary {
            text: "Done.".into(),
            caption: " ".into(),
        },
        Summary {
            text: " ".into(),
            caption: "Done.".into(),
        },
    ] {
        assert!(validate_summary(bad).is_err());
    }
}

#[test]
fn semantic_idle_final_report_supersedes_earlier_reasoning_and_plans() {
    let agent = ThreadId::from_u128(2);
    let mut window = source_window();
    window.observe(&message(
        agent,
        1,
        true,
        "I might implement this and run tests.",
    ));
    window.observe(&message(agent, 2, false, "I'm investigating the design."));
    window.observe(&message(
        agent,
        3,
        false,
        "I finished read-only research. No files changed or tests run.",
    ));
    assert_eq!(window.input_for(true).unwrap().entries.len(), 3);
    window.finish_turn("turn", TurnStatus::Completed);
    let final_report = window.input_for(false).unwrap();
    assert_eq!(final_report.entries.len(), 1);
    assert!(
        !final_report
            .formatted(INPUT_BYTES)
            .contains("might implement")
    );
    assert!(
        final_report
            .formatted(INPUT_BYTES)
            .contains("No files changed")
    );
    let hash = final_report.hash;
    window.observe(&message(
        agent,
        1,
        true,
        "Late delivery of an old speculative idea.",
    ));
    assert_eq!(window.input_for(false).unwrap().hash, hash);
}
