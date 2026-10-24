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
        order: (index as i64, index),
        truncated: false,
        rendered: Default::default(),
    }
}
fn input(text: &str) -> Arc<Input> {
    let mut window = Window::default();
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
fn semantic_window_retains_four_responses_and_intervening_summaries_in_order() {
    let agent = ThreadId::from_u128(2);
    let mut window = Window::default();
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
    let text = format_entries(&input.entries, INPUT_BYTES);
    assert!(!text.contains("response 1"));
    assert!(!text.contains("thinking 1"));
    let mut last = 0;
    for index in 2..6 {
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
        4
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
            index,
            false,
        );
    }
    for index in 0..250 {
        feed.upsert(
            MessageKey(busy, "t".into(), index.to_string()),
            "busy response",
            true,
            true,
            index + 10,
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
            index + 300,
        );
    }
    let window = feed.windows.get_mut(&first).unwrap();
    let input = window.input().unwrap();
    assert_eq!(
        input
            .entries
            .iter()
            .filter(|entry| !entry.reasoning)
            .count(),
        4
    );
    assert!(input.entries.len() <= 32);
    for budget in [2048, INPUT_BYTES] {
        let formatted = format_entries(&input.entries, budget);
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
    counts: Arc<Counts>,
    started: tokio::sync::mpsc::UnboundedSender<()>,
}
impl Backend for Fake {
    fn available(&self) -> bool {
        self.available
    }
    async fn summarize(&mut self, input: &Input) -> Result<String, String> {
        self.counts.started.fetch_add(1, Ordering::SeqCst);
        let active = self.counts.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.counts.maximum.fetch_max(active, Ordering::SeqCst);
        let mut guard = Guard {
            counts: self.counts.clone(),
            completed: false,
        };
        let _ = self.started.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
        guard.completed = true;
        Ok(input.entries.last().unwrap().text.clone())
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
            assert_eq!(result.unwrap(), "newest");
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
        0,
        false,
    );
    let window = feed.windows.get_mut(&agent).unwrap();
    let old = window.revision;
    let old_hash = window.input().unwrap().hash;
    feed.reasoning_delta(
        MessageKey(agent, "t".into(), "r".into()),
        0,
        "new reasoning",
        1,
    );
    app.apply_agent_feed_semantic(Event::Finished {
        root,
        agent,
        revision: old,
        hash: old_hash,
        result: Ok("stale".into()),
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
        result: Ok("wrong root".into()),
    });
    for _ in 0..2 {
        app.apply_agent_feed_semantic(Event::Finished {
            root,
            agent,
            revision,
            hash,
            result: Ok("Completed the parser fix. Next it will run focused tests.".into()),
        });
    }
    let feed = &app.agent_feed.roots[&root];
    assert_eq!(feed.bubbles.len(), 1);
    let area = Rect::new(0, 0, 42, 12);
    let mut buf = Buffer::empty(area);
    feed.render_agent_card(agent, area, false, &mut buf, Path::new("/tmp"));
    let text = crate::app::owned_transcript::tests::buffer_text(&buf);
    assert!(text.contains("╭ · ─ Worker"));
    assert!(text.contains("Completed the parser fix."));
    assert!(!text.contains("new reasoning"));
    insta::assert_snapshot!("semantic_agent_bubble", text);
    let feed = app.agent_feed.roots.get_mut(&root).unwrap();
    feed.upsert(
        MessageKey(agent, "t".into(), "next".into()),
        "The focused tests passed.",
        true,
        true,
        2,
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
        result: Ok("The focused tests passed. Next it will review the change.".into()),
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
    let data = input(
        "The agent fixed a parser bug and added regression coverage. Focused tests passed. It is investigating a remaining Windows path failure; it plans to reproduce that failure and fix path normalization next. It has not run the full suite or merged the changes.",
    );
    let result = tokio::time::timeout(Duration::from_secs(60), backend.summarize(&data))
        .await
        .unwrap()
        .unwrap();
    assert!(backend.settle().await);
    assert!(!result.is_empty());
    eprintln!("Local summary: {result}");
}

#[test]
fn semantic_history_order_overrides_arrival_times_without_restarting_unchanged_inputs() {
    let agent = ThreadId::from_u128(2);
    let mut window = Window::default();
    let mut response = message(agent, 2, false, "second response");
    response.order = (1000, 1);
    window.observe(&response);
    let initial = window.input().unwrap();
    let first_revision = window.revision;
    window.begin_history();
    let first = message(agent, 0, false, "first response");
    let mut thinking = message(agent, 1, true, "between responses");
    thinking.order = (0, 1);
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
    let text = format_entries(&input.entries, INPUT_BYTES);
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
