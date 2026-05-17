use std::sync::Arc;

use super::{
    Actor, AssistantTaskStartedPayload, BranchRef, CompactSummary, EventEnvelope, EventKind,
    EventPayload, EventQuery, EventQueryError, EventReader, EventStore, EventWriter,
    PartialEnvelope, QueryOrder, SessionId, SessionScope, TaskId, TaskScope,
};

#[test]
fn task_scoped_query_returns_only_matching_task_events() {
    let fixture = ReaderFixture::new();
    fixture.append("task-a", "session-a", "workspace-a", "main", "alpha");
    fixture.append("task-b", "session-a", "workspace-a", "main", "beta");
    fixture.append("task-a", "session-b", "workspace-b", "dev", "gamma");

    let events = fixture
        .reader
        .execute(EventQuery::new().task("task-a"))
        .expect("task query succeeds");

    assert_eq!(event_objectives(&events), vec!["alpha", "gamma"]);
}

#[test]
fn session_scoped_query_returns_only_matching_session_events() {
    let fixture = ReaderFixture::new();
    fixture.append("task-a", "session-a", "workspace-a", "main", "alpha");
    fixture.append("task-b", "session-b", "workspace-a", "main", "beta");
    fixture.append("task-c", "session-a", "workspace-b", "dev", "gamma");

    let events = fixture
        .reader
        .execute(EventQuery::new().session("session-a"))
        .expect("session query succeeds");

    assert_eq!(event_objectives(&events), vec!["alpha", "gamma"]);
}

#[test]
fn workspace_branch_query_returns_only_matching_workspace_branch_events() {
    let fixture = ReaderFixture::new();
    fixture.append("task-a", "session-a", "workspace-a", "main", "alpha");
    fixture.append("task-b", "session-b", "workspace-a", "dev", "beta");
    fixture.append("task-c", "session-c", "workspace-b", "main", "gamma");

    let events = fixture
        .reader
        .execute(EventQuery::new().workspace("workspace-a").branch("main"))
        .expect("workspace branch query succeeds");

    assert_eq!(event_objectives(&events), vec!["alpha"]);
}

#[test]
fn missing_scope_returns_unscoped_error() {
    let fixture = ReaderFixture::new();

    let error = fixture
        .reader
        .execute(EventQuery::new().kind(EventKind::AssistantTaskStarted))
        .expect_err("missing scope should fail");

    assert!(matches!(error, EventQueryError::Unscoped));
}

#[test]
fn limit_ceiling_is_enforced() {
    let fixture = ReaderFixture::new();

    let error = fixture
        .reader
        .execute(EventQuery::new().task("task-a").limit(10_001))
        .expect_err("limit above ceiling should fail");

    assert!(matches!(
        error,
        EventQueryError::LimitTooLarge {
            requested: 10_001,
            ceiling: 10_000
        }
    ));
}

#[test]
fn payload_corruption_returns_typed_error() {
    let fixture = ReaderFixture::new_spilled();
    let event = fixture.append(
        "task-a",
        "session-a",
        "workspace-a",
        "main",
        &"x".repeat(5000),
    );
    let spill_id = match event.payload_location {
        super::PayloadLocation::Spilled { row_id } => row_id,
        other => panic!("expected spilled payload, got {other:?}"),
    };
    fixture.corrupt_payload(spill_id);

    let error = fixture
        .reader
        .execute(EventQuery::new().task("task-a"))
        .expect_err("corrupted payload should fail");

    match error {
        EventQueryError::PayloadCorruption { event_id, .. } => {
            assert_eq!(event_id.ulid, event.event_id.ulid);
        }
        other => panic!("expected payload corruption, got {other:?}"),
    }
}

#[test]
fn stream_yields_same_events_as_execute_in_order() {
    let fixture = ReaderFixture::new();
    for index in 0..6 {
        fixture.append(
            "task-a",
            "session-a",
            "workspace-a",
            "main",
            &format!("event-{index}"),
        );
    }

    let query = EventQuery::new()
        .task("task-a")
        .order(QueryOrder::OldestFirst)
        .limit(10);
    let execute = fixture
        .reader
        .execute(query.clone())
        .expect("execute succeeds");
    let stream = fixture
        .reader
        .stream(query)
        .collect::<Result<Vec<_>, _>>()
        .expect("stream succeeds");

    assert_eq!(execute, stream);
}

#[test]
fn tail_returns_newest_events_for_task_scope() {
    let fixture = ReaderFixture::new();
    fixture.append("task-a", "session-a", "workspace-a", "main", "first");
    fixture.append("task-a", "session-a", "workspace-a", "main", "second");
    fixture.append("task-a", "session-a", "workspace-a", "main", "third");

    let events = fixture
        .reader
        .tail(
            TaskScope {
                task_id: TaskId {
                    value: "task-a".to_string(),
                },
            },
            2,
        )
        .expect("tail succeeds");

    assert_eq!(event_objectives(&events), vec!["third", "second"]);
}

#[test]
fn tail_returns_newest_events_for_session_scope() {
    let fixture = ReaderFixture::new();
    fixture.append("task-a", "session-a", "workspace-a", "main", "first");
    fixture.append("task-b", "session-a", "workspace-a", "main", "second");
    fixture.append("task-c", "session-b", "workspace-a", "main", "third");

    let events = fixture
        .reader
        .tail(
            SessionScope {
                session_id: SessionId {
                    value: "session-a".to_string(),
                },
            },
            2,
        )
        .expect("tail succeeds");

    assert_eq!(event_objectives(&events), vec!["second", "first"]);
}

struct ReaderFixture {
    reader: EventReader,
    store: Arc<EventStore>,
    inline_ceiling: usize,
}

impl ReaderFixture {
    fn new() -> Self {
        Self::with_inline_ceiling(4096)
    }

    fn new_spilled() -> Self {
        Self::with_inline_ceiling(1)
    }

    fn with_inline_ceiling(inline_ceiling: usize) -> Self {
        let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
        let reader = EventReader::new(store.clone());
        Self {
            reader,
            store,
            inline_ceiling,
        }
    }

    fn append(
        &self,
        task_id: &str,
        session_id: &str,
        workspace_id: &str,
        branch: &str,
        objective: &str,
    ) -> EventEnvelope {
        let writer = EventWriter::new(
            self.store.clone(),
            workspace_id.to_string(),
            self.inline_ceiling,
        );
        let payload = EventPayload::AssistantTaskStarted(AssistantTaskStartedPayload {
            context_handle_id: None,
            seed_event_ids: Vec::new(),
            initial_memory_ids: Vec::new(),
            objective: objective.to_string(),
        });
        let envelope = PartialEnvelope {
            workspace_id: Some(workspace_id.to_string()),
            branch: BranchRef {
                name: branch.to_string(),
            },
            session_id: SessionId {
                value: session_id.to_string(),
            },
            task_id: Some(TaskId {
                value: task_id.to_string(),
            }),
            actor: Actor::Assistant {
                model: "gpt-5.5".to_string(),
            },
            kind: payload.kind(),
            references: Vec::new(),
            summary: CompactSummary::new(format!("summary for {task_id}/{session_id}")).unwrap(),
            payload,
        };
        let event_id = writer.append(envelope).expect("event appends");
        self.reader
            .execute(
                EventQuery::new()
                    .task(task_id)
                    .session(session_id)
                    .workspace(workspace_id)
                    .branch(branch)
                    .limit(10)
                    .order(QueryOrder::NewestFirst),
            )
            .expect("event reloads")
            .into_iter()
            .find(|event| event.event_id.ulid == event_id.ulid)
            .expect("inserted event is queryable")
    }

    fn corrupt_payload(&self, row_id: i64) {
        self.store.with_connection(|conn| {
            conn.execute(
                "UPDATE event_payloads SET bytes = ?1, bytes_len = ?2 WHERE row_id = ?3",
                rusqlite::params![b"corrupted".to_vec(), 9_i64, row_id],
            )
            .expect("payload row corrupts");
        });
    }
}

fn event_objectives(events: &[EventEnvelope]) -> Vec<&str> {
    events
        .iter()
        .map(|event| match &event.payload {
            EventPayload::AssistantTaskStarted(payload) => payload.objective.as_str(),
            other => panic!("unexpected payload {other:?}"),
        })
        .collect()
}
