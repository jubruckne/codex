//! Passive history recovery: never resume, attach, or acquire a thread writer lease.
use super::*;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_protocol::*;
use futures::{StreamExt, stream};

pub(super) async fn load(
    handle: &AppServerRequestHandle,
    root: ThreadId,
    known: Vec<Thread>,
    versions: HashMap<ThreadId, (i64, bool)>,
    read_messages: bool,
) -> FeedSnapshot {
    let mut snapshot = FeedSnapshot {
        latest_turns: HashMap::new(),
        threads: known,
        messages: Vec::new(),
        versions: HashMap::new(),
        errors: Vec::new(),
    };
    // Fetch descendants explicitly: the dashboard's recent-project window is not tree membership.
    let mut cursor = None;
    let mut seen_cursors = HashSet::new();
    loop {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            handle.request_typed::<ThreadListResponse>(ClientRequest::ThreadList {
                request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
                params: ThreadListParams {
                    cursor: cursor.clone(),
                    limit: Some(100),
                    excluded_thread_ids: None,
                    sort_key: None,
                    sort_direction: None,
                    model_providers: Some(Vec::new()),
                    source_kinds: Some(vec![ThreadSourceKind::SubAgentThreadSpawn]),
                    originators: None,
                    archived: Some(false),
                    section_id: None,
                    project_id: None,
                    cwd: None,
                    use_state_db_only: true,
                    search_term: None,
                    parent_thread_id: None,
                    ancestor_thread_id: Some(root.to_string()),
                },
            }),
        )
        .await;
        match result {
            Ok(Ok(page)) => {
                for thread in page
                    .data
                    .into_iter()
                    .filter(|thread| !thread.ephemeral && thread.id != root.to_string())
                {
                    if let Some(old) = snapshot.threads.iter_mut().find(|old| old.id == thread.id) {
                        *old = thread;
                    } else {
                        snapshot.threads.push(thread);
                    }
                }
                if page.next_cursor.is_none() || page.next_cursor == cursor {
                    break;
                }
                if !seen_cursors.insert(page.next_cursor.clone()) {
                    snapshot
                        .errors
                        .push("agent discovery cursor repeated".into());
                    break;
                }
                cursor = page.next_cursor;
            }
            _ => {
                snapshot.errors.push("agent discovery unavailable".into());
                break;
            }
        }
    }
    if !read_messages {
        return snapshot;
    }
    let pending = snapshot
        .threads
        .iter()
        .filter_map(|thread| {
            let id = ThreadId::from_string(&thread.id).ok()?;
            let active = matches!(thread.status, ThreadStatus::Active { .. });
            let version = (thread.updated_at, active);
            (active || versions.get(&id) != Some(&version)).then_some((id, version))
        })
        .collect::<Vec<_>>();
    let mut reads = stream::iter(pending.into_iter().map(|(id, version)| async move {
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            read_messages_for_thread(handle, id),
        )
        .await;
        (id, version, result)
    }))
    .buffer_unordered(4);
    while let Some((id, version, result)) = reads.next().await {
        match result {
            Ok(Ok((messages, latest_turn))) => {
                snapshot.latest_turns.insert(id, latest_turn);
                snapshot.messages.extend(messages);
                snapshot.versions.insert(id, version);
            }
            _ => snapshot.errors.push(format!("{} unavailable", id)),
        }
    }
    snapshot.messages.sort_by(|a, b| {
        a.5.cmp(&b.5)
            .then_with(|| a.0.to_string().cmp(&b.0.to_string()))
    });
    snapshot
}

type LoadedMessage = (
    ThreadId,
    String,
    String,
    String,
    bool,
    i64,
    Option<Vec<String>>,
);
async fn read_messages_for_thread(
    handle: &AppServerRequestHandle,
    id: ThreadId,
) -> std::result::Result<(Vec<LoadedMessage>, semantic::TurnHistory), String> {
    let page = handle
        .request_typed::<ThreadTurnsListResponse>(ClientRequest::ThreadTurnsList {
            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
            params: ThreadTurnsListParams {
                thread_id: id.to_string(),
                cursor: None,
                limit: Some(10),
                sort_direction: Some(SortDirection::Desc),
                items_view: Some(TurnItemsView::Full),
            },
        })
        .await
        .map_err(|error| error.to_string())?;
    let context = |turn: &Turn| semantic::TurnContext {
        id: turn.id.clone(),
        status: turn.status.clone(),
    };
    let latest_turn = semantic::TurnHistory {
        latest: page.data.first().map(context),
        completed: page
            .data
            .iter()
            .find(|turn| turn.status != TurnStatus::InProgress)
            .map(context),
    };
    let mut messages = Vec::new();
    for turn in page.data.into_iter().rev() {
        let time = turn.started_at.unwrap_or(0).saturating_mul(1000);
        let complete = turn.status != TurnStatus::InProgress;
        for item in turn.items {
            match item {
                ThreadItem::AgentMessage { id: item, text, .. } => {
                    messages.push((id, turn.id.clone(), item, text, complete, time, None));
                }
                ThreadItem::Reasoning {
                    id: item, summary, ..
                } => {
                    messages.push((
                        id,
                        turn.id.clone(),
                        item,
                        String::new(),
                        complete,
                        time,
                        Some(summary),
                    ));
                }
                _ => {}
            }
        }
    }
    Ok((messages, latest_turn))
}
