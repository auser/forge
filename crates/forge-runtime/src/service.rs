use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use forge_config::Config;
use forge_core::{
    CompletionRequest, DecisionRouter, Event, EventKind, ExecutionProvider, ForgeError, Message,
    ModelProvider, RoutingRequest, SessionStore, Skill, SkillMeta, SkillRegistry,
};
use forge_session::{JsonlSessionStore, new_run_id, new_session_id};
use serde::Serialize;
use tokio::sync::broadcast;

/// Skill registry placeholder until forge-skills lands in Phase C.
pub struct NullSkillRegistry;

impl SkillRegistry for NullSkillRegistry {
    fn list(&self) -> Vec<SkillMeta> {
        Vec::new()
    }

    fn activate(&self, name: &str) -> Result<Skill, ForgeError> {
        Err(ForgeError::skill(format!(
            "skill {name:?} unavailable: skill support arrives in Phase C"
        )))
    }
}

/// Result of a completed run.
#[derive(Debug, Clone, Serialize)]
pub struct RunOutcome {
    pub run_id: String,
    pub session_id: String,
    pub text: String,
    pub events: Vec<Event>,
}

/// Transport-neutral agent runtime: routing → model → events. CLI and the
/// future server share this type.
pub struct AgentService {
    model: Arc<dyn ModelProvider>,
    router: Arc<dyn DecisionRouter>,
    execution: Arc<dyn ExecutionProvider>,
    skills: Arc<dyn SkillRegistry>,
    sessions: Arc<JsonlSessionStore>,
    config: Config,
    broadcasters: Mutex<HashMap<String, broadcast::Sender<Event>>>,
}

impl AgentService {
    pub fn new(
        model: Arc<dyn ModelProvider>,
        router: Arc<dyn DecisionRouter>,
        execution: Arc<dyn ExecutionProvider>,
        skills: Arc<dyn SkillRegistry>,
        sessions: Arc<JsonlSessionStore>,
        config: Config,
    ) -> Self {
        Self {
            model,
            router,
            execution,
            skills,
            sessions,
            config,
            broadcasters: Mutex::new(HashMap::new()),
        }
    }

    pub fn model(&self) -> &Arc<dyn ModelProvider> {
        &self.model
    }

    pub fn execution(&self) -> &Arc<dyn ExecutionProvider> {
        &self.execution
    }

    pub fn skills(&self) -> &Arc<dyn SkillRegistry> {
        &self.skills
    }

    pub fn sessions(&self) -> &Arc<JsonlSessionStore> {
        &self.sessions
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    fn broadcaster(&self, run_id: &str) -> broadcast::Sender<Event> {
        self.broadcasters
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(run_id.to_string())
            .or_insert_with(|| broadcast::channel(64).0)
            .clone()
    }

    /// Subscribe to the live event stream of a run. This is the
    /// transport-neutral seam the Phase D SSE endpoint will consume.
    pub fn subscribe(&self, run_id: &str) -> broadcast::Receiver<Event> {
        self.broadcaster(run_id).subscribe()
    }

    /// Append an event to the store, broadcast it to subscribers, and
    /// collect it for the outcome.
    fn emit(
        &self,
        sender: &broadcast::Sender<Event>,
        collected: &mut Vec<Event>,
        event: Event,
    ) -> Result<(), ForgeError> {
        self.sessions.append(&event)?;
        // No subscribers yet is normal for the CLI; not an error.
        let _ = sender.send(event.clone());
        collected.push(event);
        Ok(())
    }

    /// Route the prompt, call the model, and emit the full event trail.
    /// Generates fresh run/session ids.
    pub async fn run(&self, prompt: &str) -> Result<RunOutcome, ForgeError> {
        let session_id = new_session_id();
        let run_id = new_run_id();
        self.run_with_ids(prompt, &run_id, &session_id).await
    }

    /// Start a run on a tokio task without blocking the caller. Returns
    /// the pre-generated `(run_id, session_id)` and the task handle so
    /// transports (the REST server) can return ids immediately and abort
    /// the task on cancel. Events are persisted and broadcast as usual.
    pub fn start_run(
        self: &Arc<Self>,
        prompt: impl Into<String>,
        session_id: Option<String>,
    ) -> (
        String,
        String,
        tokio::task::JoinHandle<Result<RunOutcome, ForgeError>>,
    ) {
        let run_id = new_run_id();
        let session_id = session_id.unwrap_or_else(new_session_id);
        // Create the broadcast channel now so subscribers connecting right
        // after the 202 response miss nothing.
        self.broadcaster(&run_id);
        let service = Arc::clone(self);
        let prompt = prompt.into();
        let (rid, sid) = (run_id.clone(), session_id.clone());
        let handle = tokio::spawn(async move { service.run_with_ids(&prompt, &rid, &sid).await });
        (run_id, session_id, handle)
    }

    /// Record run-scoped client input as a `Note` event (the semantics of
    /// `POST /v1/runs/:id/input` for now — the note is persisted and
    /// broadcast, not fed back into the model).
    pub fn record_input(&self, run_id: &str, input: &str) -> Result<(), ForgeError> {
        let session_id = self
            .sessions
            .find_run(run_id)?
            .ok_or_else(|| ForgeError::session(format!("unknown run: {run_id}")))?;
        let event = Event::new(
            run_id,
            &session_id,
            EventKind::Note {
                message: input.to_string(),
            },
        );
        self.sessions.append(&event)?;
        let _ = self.broadcaster(run_id).send(event);
        Ok(())
    }

    async fn run_with_ids(
        &self,
        prompt: &str,
        run_id: &str,
        session_id: &str,
    ) -> Result<RunOutcome, ForgeError> {
        let run_id = run_id.to_string();
        let session_id = session_id.to_string();
        let sender = self.broadcaster(&run_id);
        let mut collected = Vec::new();

        let fail = |collected: &mut Vec<Event>, error: ForgeError| -> ForgeError {
            let event = Event::new(
                &run_id,
                &session_id,
                EventKind::Error {
                    message: error.to_string(),
                },
            );
            let _ = self.emit(&sender, collected, event);
            error
        };

        self.emit(
            &sender,
            &mut collected,
            Event::new(
                &run_id,
                &session_id,
                EventKind::RunStarted {
                    provider: self.model.name().to_string(),
                    model: self.config.model.clone(),
                },
            ),
        )?;

        let routing_request = RoutingRequest {
            task: prompt.to_string(),
            required_capabilities: Vec::new(),
            candidates: vec![self.config.model.clone()],
        };
        let decision = match self.router.route(&routing_request).await {
            Ok(decision) => decision,
            Err(e) => return Err(fail(&mut collected, e)),
        };
        tracing::info!(
            run_id,
            model = %decision.selected_model,
            confidence = decision.confidence,
            fallback = decision.fallback_used,
            "routing decision"
        );
        self.emit(
            &sender,
            &mut collected,
            Event::new(
                &run_id,
                &session_id,
                EventKind::RoutingDecisionMade {
                    router: decision.router_name.clone(),
                    selected_model: decision.selected_model.clone(),
                    confidence: decision.confidence,
                    fallback_used: decision.fallback_used,
                },
            ),
        )?;

        let mut messages = Vec::new();
        for meta in self.skills.match_task(prompt) {
            match self.skills.activate(&meta.name) {
                Ok(skill) => {
                    self.emit(
                        &sender,
                        &mut collected,
                        Event::new(
                            &run_id,
                            &session_id,
                            EventKind::SkillActivated {
                                name: skill.meta.name.clone(),
                                path: skill.meta.path.clone(),
                            },
                        ),
                    )?;
                    messages.push(Message::system(format!(
                        "Active skill `{}` instructions:\n{}",
                        skill.meta.name, skill.instructions
                    )));
                }
                Err(e) => {
                    tracing::warn!(skill = %meta.name, error = %e, "skill activation failed")
                }
            }
        }
        messages.push(Message::user(prompt));

        let request = CompletionRequest::new(decision.selected_model.clone(), messages);
        let response = match self.model.complete(request).await {
            Ok(response) => response,
            Err(e) => return Err(fail(&mut collected, e)),
        };

        let summary: String = response.content.chars().take(80).collect();
        self.emit(
            &sender,
            &mut collected,
            Event::new(&run_id, &session_id, EventKind::Completed { summary }),
        )?;

        Ok(RunOutcome {
            run_id,
            session_id,
            text: response.content,
            events: collected,
        })
    }

    /// Record cancellation for a run (or session); unknown ids are a
    /// typed error. Aborting an in-flight task is the caller's job (the
    /// server holds the task handle).
    pub fn cancel(&self, run_or_session_id: &str) -> Result<(), ForgeError> {
        let session_id = match self.sessions.find_run(run_or_session_id)? {
            Some(session) => Some(session),
            None => self
                .sessions
                .list_sessions()?
                .iter()
                .any(|s| s.session_id == run_or_session_id)
                .then(|| run_or_session_id.to_string()),
        };
        let Some(session_id) = session_id else {
            return Err(ForgeError::session(format!(
                "unknown run or session: {run_or_session_id}"
            )));
        };
        let event = Event::new(
            run_or_session_id,
            &session_id,
            EventKind::Cancelled {
                reason: "cancelled by user".to_string(),
            },
        );
        self.sessions.append(&event)?;
        let _ = self.broadcaster(run_or_session_id).send(event);
        Ok(())
    }

    /// Load the event history of a session (or the session owning a run).
    /// Real re-execution of the run arrives in a later phase.
    pub fn resume(&self, session_or_run_id: &str) -> Result<Vec<Event>, ForgeError> {
        let session_id = match self.sessions.find_run(session_or_run_id)? {
            Some(session) => session,
            None if self.session_exists(session_or_run_id)? => session_or_run_id.to_string(),
            None => {
                return Err(ForgeError::session(format!(
                    "unknown run or session: {session_or_run_id}"
                )));
            }
        };
        self.sessions.events_for(&session_id)
    }

    /// All events belonging to one run (for the Phase D events endpoint).
    pub fn events(&self, run_id: &str) -> Result<Vec<Event>, ForgeError> {
        match self.sessions.find_run(run_id)? {
            Some(session) => Ok(self
                .sessions
                .events_for(&session)?
                .into_iter()
                .filter(|e| e.run_id == run_id)
                .collect()),
            None => Err(ForgeError::session(format!("unknown run: {run_id}"))),
        }
    }

    fn session_exists(&self, session_id: &str) -> Result<bool, ForgeError> {
        Ok(self
            .sessions
            .list_sessions()?
            .iter()
            .any(|s| s.session_id == session_id))
    }
}

#[cfg(test)]
mod tests {
    use forge_core::EventKind;
    use forge_execution::MockExecution;
    use forge_providers::{MockModel, MockRouter};

    use super::*;

    fn test_service(root: &std::path::Path) -> AgentService {
        AgentService::new(
            Arc::new(MockModel::new()),
            Arc::new(MockRouter::selecting("mock-local")),
            Arc::new(MockExecution::new()),
            Arc::new(NullSkillRegistry),
            Arc::new(JsonlSessionStore::new(root.join("sessions"))),
            Config::default(),
        )
    }

    #[tokio::test]
    async fn full_run_emits_ordered_events() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let service = test_service(tmp.path());

        let outcome = service.run("hello there").await.expect("run succeeds");

        assert_eq!(outcome.text, "mock response to: hello there");
        let kinds: Vec<&str> = outcome
            .events
            .iter()
            .map(|e| match &e.kind {
                EventKind::RunStarted { .. } => "run_started",
                EventKind::RoutingDecisionMade { .. } => "routing_decision_made",
                EventKind::Completed { .. } => "completed",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["run_started", "routing_decision_made", "completed"]);

        // Everything was persisted too.
        let persisted = service
            .sessions()
            .events_for(&outcome.session_id)
            .expect("read");
        assert_eq!(persisted.len(), 3);
        assert!(persisted.iter().all(|e| e.run_id == outcome.run_id));
    }

    struct FailingRouter;

    #[async_trait::async_trait]
    impl DecisionRouter for FailingRouter {
        async fn route(
            &self,
            _: &RoutingRequest,
        ) -> Result<forge_core::RoutingDecision, ForgeError> {
            Err(ForgeError::router("router exploded"))
        }
    }

    #[tokio::test]
    async fn run_failure_appends_error_event() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let service = AgentService::new(
            Arc::new(MockModel::new()),
            Arc::new(FailingRouter),
            Arc::new(MockExecution::new()),
            Arc::new(NullSkillRegistry),
            Arc::new(JsonlSessionStore::new(tmp.path().join("sessions"))),
            Config::default(),
        );

        let err = service.run("doomed").await.expect_err("routing fails");
        assert!(matches!(err, ForgeError::Router(_)));

        let all = service.sessions().events().expect("read all");
        assert_eq!(all.len(), 2);
        assert!(matches!(all[0].kind, EventKind::RunStarted { .. }));
        match &all[1].kind {
            EventKind::Error { message } => assert!(message.contains("router exploded")),
            other => panic!("expected error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cancel_unknown_run_is_typed_error() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let service = test_service(tmp.path());
        let err = service
            .cancel("definitely-unknown-run")
            .expect_err("unknown");
        assert!(matches!(err, ForgeError::Session(_)));
    }

    #[tokio::test]
    async fn cancel_records_event_and_resume_returns_history() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let service = test_service(tmp.path());
        let outcome = service.run("do a thing").await.expect("run");

        service.cancel(&outcome.run_id).expect("cancel");

        let history = service.resume(&outcome.session_id).expect("resume");
        assert_eq!(history.len(), 4);
        assert!(matches!(history[3].kind, EventKind::Cancelled { .. }));

        // Resume also works by run id.
        let by_run = service.resume(&outcome.run_id).expect("resume by run");
        assert_eq!(by_run.len(), 4);

        // events(run_id) filters to the run.
        let run_events = service.events(&outcome.run_id).expect("events");
        assert_eq!(run_events.len(), 4);
        assert!(run_events.iter().all(|e| e.run_id == outcome.run_id));
    }

    #[tokio::test]
    async fn subscribers_receive_live_events() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let service = test_service(tmp.path());

        // Subscribing requires a run id; the CLI won't know it ahead of
        // time, but the server can subscribe right after RunStarted. Here
        // we verify the broadcast seam carries events.
        let outcome = service.run("stream me").await.expect("run");
        let mut rx = service.subscribe(&outcome.run_id);
        service.cancel(&outcome.run_id).expect("cancel");
        let event = rx.try_recv().expect("broadcast delivered");
        assert!(matches!(event.kind, EventKind::Cancelled { .. }));
    }
}
