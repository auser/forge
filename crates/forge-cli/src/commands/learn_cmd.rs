use chrono::{DateTime, Utc};
use forge_context::{FsLearningStore, LearningScope, LearningSession, ProposalStatus};
use forge_core::ForgeError;
use forge_session::JsonlSessionStore;

use crate::{
    cli::{LearnCommand, LearnScopeArgs},
    commands::Context,
};

pub fn run(ctx: &Context, command: LearnCommand) -> Result<(), ForgeError> {
    let root = ctx.project_root()?;
    let sessions = JsonlSessionStore::new(root.join(".forge/sessions"));
    let store = FsLearningStore::new(root.join(".forge/context"));
    match command {
        LearnCommand::Propose { scope } => {
            let scope = scope_from(&scope)?;
            let evidence = evidence_for_scope(&sessions, scope.session_id())?;
            let proposals = store
                .propose(&evidence, &scope, sessions.redactor())
                .map_err(learning_error)?;
            output(&serde_json::json!({
                "storage": ".forge/context/learning/index.json",
                "proposals": proposals,
            }))
        }
        LearnCommand::List => output(&store.list().map_err(learning_error)?),
        LearnCommand::Show { id } => output(&store.get(&id).map_err(learning_error)?),
        LearnCommand::Apply { id, yes } => {
            if !yes {
                return Err(ForgeError::session(
                    "learning proposal application requires --yes",
                ));
            }
            output(
                &store
                    .decide(&id, ProposalStatus::Accepted, Utc::now())
                    .map_err(learning_error)?,
            )
        }
        LearnCommand::Reject { id } => output(
            &store
                .decide(&id, ProposalStatus::Rejected, Utc::now())
                .map_err(learning_error)?,
        ),
        LearnCommand::Metrics { scope } => {
            let scope = scope_from(&scope)?;
            let evidence = evidence_for_scope(&sessions, scope.session_id())?;
            output(
                &store
                    .metrics(&evidence, &scope, sessions.redactor())
                    .map_err(learning_error)?,
            )
        }
    }
}

fn scope_from(args: &LearnScopeArgs) -> Result<LearningScope, ForgeError> {
    let mut scope = LearningScope::new();
    if let Some(session) = &args.session {
        scope = scope.session(session);
    }
    if let Some(since) = &args.since {
        scope = scope.since(timestamp(since)?);
    }
    if let Some(until) = &args.until {
        scope = scope.until(timestamp(until)?);
    }
    Ok(scope)
}

fn timestamp(value: &str) -> Result<DateTime<Utc>, ForgeError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| ForgeError::session("learning time bounds must be RFC 3339 timestamps"))
}

fn evidence_for_scope(
    sessions: &JsonlSessionStore,
    session: Option<&str>,
) -> Result<Vec<LearningSession>, ForgeError> {
    if let Some(session) = session {
        if !sessions
            .list_sessions()?
            .iter()
            .any(|info| info.session_id == session)
        {
            return Err(ForgeError::session("unknown session"));
        }
        return Ok(vec![LearningSession::new(
            session,
            sessions.events_for(session)?,
        )]);
    }
    let mut evidence = Vec::new();
    for info in sessions.list_sessions()? {
        let events = sessions.events_for(&info.session_id)?;
        evidence.push(LearningSession::new(info.session_id, events));
    }
    // The core scope applies session/time filters. Uniform enumeration here
    // also lets it deduplicate copied fork evidence by source identity.
    Ok(evidence)
}

fn output(value: &impl serde::Serialize) -> Result<(), ForgeError> {
    let rendered = serde_json::to_string_pretty(value)
        .map_err(|_| ForgeError::session("learning output unavailable"))?;
    println!("{rendered}");
    Ok(())
}

fn learning_error(error: forge_context::LearningError) -> ForgeError {
    ForgeError::session(error.to_string())
}
