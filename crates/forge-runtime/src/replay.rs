//! Reconstructing a session's model conversation from its event log.
//!
//! The append-only JSONL log is the harness's memory: `forge resume` (and,
//! later, the chat UI) rebuilds the `messages` history from it rather than
//! re-seeding the model with a prompt and a truncated summary. Everything
//! here is pure — events in, messages out — so the policy decisions
//! (ordering, pairing, truncation) are unit-testable without a runtime.
//!
//! ## What is replayed
//!
//! Per run, in log order:
//!
//! | event                                | message                              |
//! |--------------------------------------|--------------------------------------|
//! | `run_started { prompt }`             | `user(prompt)`                       |
//! | `input_received { message }`         | `user(message)` (except resume markers) |
//! | `assistant_message { text, calls }`  | `assistant(text, tool_calls)`        |
//! | `tool_result { call_id, output }`    | `tool(call_id, output)`              |
//!
//! `assistant_message`/`tool_result` are v3 kinds. A pre-v3 run has
//! neither, so its assistant turns are replayed from the one thing older
//! logs do carry — the truncated `completed { summary }` — and the replay
//! is flagged [`degraded`](Replay::degraded). That is the honest floor: old
//! logs replay as well as their data allows.

use forge_core::{Event, EventKind, Message, ModelCapabilities, Role};

/// Rough bytes-per-token used to turn a model's token context window into a
/// character budget. Four is the usual English/code approximation; being
/// wrong here costs a little budget, never correctness — the model itself
/// enforces the real limit.
const CHARS_PER_TOKEN: usize = 4;

/// Share of the model's context the replayed history may occupy. The rest
/// is left for the tool definitions, the new prompt, the system context
/// forge seeds, and the model's own answer.
const HISTORY_CONTEXT_SHARE: f64 = 0.5;

/// Floor on the history budget, so a tiny or misreported `max_context`
/// cannot reduce replay to nothing.
const MIN_HISTORY_CHARS: usize = 2_000;

/// Marker inserted where messages were dropped to fit the budget, so the
/// model is told the history is partial instead of silently seeing a
/// conversation that never happened.
const ELISION_NOTE: &str = "[forge: earlier conversation omitted to fit the model's context]";

/// A session's conversation, rebuilt from its events.
#[derive(Debug, Clone, PartialEq)]
pub struct Replay {
    /// The reconstructed history, oldest first.
    pub messages: Vec<Message>,
    /// True when at least one run in the session predates the v3 replay
    /// events, so its assistant turns came from truncated summaries.
    pub degraded: bool,
}

/// Character budget for replayed history, derived from the model's
/// advertised context window.
pub fn history_budget_chars(capabilities: &ModelCapabilities) -> usize {
    let chars = capabilities.max_context.saturating_mul(CHARS_PER_TOKEN);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let budget = (chars as f64 * HISTORY_CONTEXT_SHARE) as usize;
    budget.max(MIN_HISTORY_CHARS)
}

/// Rebuild the model conversation of a whole session (every run, in order).
///
/// `events` is the session's log as stored. Events are consumed in file
/// order: runs of one session are appended sequentially, so file order *is*
/// chronological order across runs.
pub fn conversation_from_events(events: &[Event]) -> Replay {
    let mut messages: Vec<Message> = Vec::new();
    let mut degraded = false;
    // Runs that recorded at least one v3 replay event. A run with none and
    // a `completed` predates v3, so its truncated summary is the only
    // assistant text available.
    let mut replayable_runs: std::collections::HashSet<&str> = std::collections::HashSet::new();

    for event in events {
        match &event.kind {
            EventKind::RunStarted { prompt, .. } => {
                if !prompt.is_empty() {
                    messages.push(Message::user(prompt));
                }
            }

            // Real user input mid-run. The resume marker the runtime writes
            // is bookkeeping, not something the user said.
            EventKind::InputReceived { message } | EventKind::Note { message } => {
                if !is_runtime_marker(message) {
                    messages.push(Message::user(message));
                }
            }

            EventKind::AssistantMessage { text, tool_calls } => {
                replayable_runs.insert(event.run_id.as_str());
                let mut assistant = Message::assistant_tool_calls(tool_calls.clone());
                assistant.content.clone_from(text);
                messages.push(assistant);
            }

            EventKind::ToolResult {
                call_id, output, ..
            } => {
                replayable_runs.insert(event.run_id.as_str());
                messages.push(Message::tool(call_id, output));
            }

            // Pre-v3 fallback: a run that recorded no replay events at all
            // has only its truncated summary to offer.
            EventKind::Completed { summary } => {
                if !replayable_runs.contains(event.run_id.as_str()) {
                    degraded = true;
                    if !summary.is_empty() {
                        messages.push(Message::assistant(summary));
                    }
                }
            }

            // Observability-only, or not part of the model conversation.
            EventKind::RoutingDecisionMade { .. }
            | EventKind::SkillActivated { .. }
            | EventKind::ToolStarted { .. }
            | EventKind::ToolCompleted { .. }
            | EventKind::ToolCallRequested { .. }
            | EventKind::FileChanged { .. }
            | EventKind::ApprovalRequested { .. }
            | EventKind::ApprovalDecided { .. }
            | EventKind::TurnCompleted { .. }
            | EventKind::SessionForked { .. }
            | EventKind::Error { .. }
            | EventKind::Cancelled { .. } => {}
        }
    }

    Replay {
        messages: drop_orphan_tool_messages(messages),
        degraded,
    }
}

/// Runtime bookkeeping written as `input_received`, which must not be
/// replayed as something the user typed.
fn is_runtime_marker(message: &str) -> bool {
    message.starts_with("resume of run ")
}

/// Keep the history a provider will accept: a `Role::Tool` message is only
/// legal when the assistant message before it requested that call id.
///
/// This matters for logs where a run was cancelled or errored between the
/// assistant's tool call and the tool's result, and again after
/// [`fit_to_budget`] has dropped messages from the front.
fn drop_orphan_tool_messages(messages: Vec<Message>) -> Vec<Message> {
    let mut announced: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        match message.role {
            Role::Assistant => {
                for call in &message.tool_calls {
                    announced.insert(call.id.clone());
                }
                out.push(message);
            }
            Role::Tool => {
                let known = message
                    .tool_call_id
                    .as_ref()
                    .is_some_and(|id| announced.contains(id));
                if known {
                    out.push(message);
                }
            }
            _ => out.push(message),
        }
    }
    out
}

/// Fit a replayed history into `budget_chars`.
///
/// Policy, in order:
///  1. The **first** message is always kept — it is the session's original
///     ask, and dropping it loses the task itself.
///  2. Beyond that, the **most recent** messages are kept: drop from the
///     front (oldest first) until the rest fits.
///  3. A dropped assistant message takes its tool results with it — an
///     orphan `Role::Tool` message is rejected by real providers.
///  4. When anything was dropped, an [`ELISION_NOTE`] system message is
///     inserted after the first message, so the model knows.
///  5. If the first message alone exceeds the budget, it is kept and its
///     content is cut (with a marker): a replay that cannot fit is better
///     partial than empty.
///
/// The budget is counted in characters over message content plus a small
/// per-message overhead, which is an approximation of tokens — see
/// [`history_budget_chars`].
pub fn fit_to_budget(messages: Vec<Message>, budget_chars: usize) -> Vec<Message> {
    if total_cost(&messages) <= budget_chars {
        return messages;
    }
    let mut messages = messages;
    if messages.is_empty() {
        return messages;
    }

    let first = messages.remove(0);
    let first_cost = message_cost(&first);
    let note = Message::system(ELISION_NOTE);
    let reserved = first_cost.saturating_add(message_cost(&note));
    if reserved >= budget_chars {
        // Even the anchor does not fit: keep it, cut its content.
        return vec![truncate_message(first, budget_chars)];
    }

    let mut tail_budget = budget_chars - reserved;
    // Walk backwards, keeping whole messages while they fit.
    let mut kept: Vec<Message> = Vec::new();
    for message in messages.into_iter().rev() {
        let cost = message_cost(&message);
        if cost > tail_budget {
            break;
        }
        tail_budget -= cost;
        kept.push(message);
    }
    kept.reverse();

    let mut out = vec![first, note];
    out.extend(drop_orphan_tool_messages(kept));
    out
}

/// Per-message cost: content plus serialized tool-call arguments plus a
/// small allowance for role/framing the provider adds.
fn message_cost(message: &Message) -> usize {
    let calls: usize = message
        .tool_calls
        .iter()
        .map(|call| call.name.len() + call.arguments.to_string().len() + 16)
        .sum();
    message.content.chars().count() + calls + 16
}

fn total_cost(messages: &[Message]) -> usize {
    messages.iter().map(message_cost).sum()
}

fn truncate_message(mut message: Message, budget_chars: usize) -> Message {
    const MARKER: &str = "\n[forge: message truncated to fit the model's context]";
    let room = budget_chars.saturating_sub(MARKER.len() + 16);
    if message.content.chars().count() <= room {
        return message;
    }
    let head: String = message.content.chars().take(room).collect();
    message.content = format!("{head}{MARKER}");
    message.tool_calls.clear();
    message
}

#[cfg(test)]
mod tests;
