//! `forge acp` — an Agent Client Protocol agent over stdio, so Zed (and
//! any other ACP client) gets forge as a native in-editor agent: streaming
//! responses, visible tool calls, editor-native permission prompts and
//! cancellation.
//!
//! This is the sibling of `forge-mcp`. Both are thin adapters over the one
//! shared [`AgentService`](forge_runtime::AgentService), which the CLI
//! constructs (`commands::service::build_run_service`) — but they expose
//! different things: MCP exposes forge's capabilities *as tools* for
//! another agent to call, while ACP exposes forge *as the agent*. No
//! routing, model or execution logic lives here.
//!
//! # Trust model
//!
//! Loopback-only by construction: the editor launches this process as a
//! subprocess on the same machine, as the same user. There is no network
//! listener and no authentication layer (we advertise no `authMethods`).
//!
//! # SDK decision: we carry the v1 wire types, we do not depend on the SDK
//!
//! Verified 2026-09-24 against `agent-client-protocol` 2.2.0 (published
//! 2026-09-18, the Zed/ACP org's official Rust SDK, repository
//! <https://github.com/agentclientprotocol/rust-sdk>). It *works* — it
//! builds on our stable toolchain (crate edition 2024, MSRV 1.88; ours is
//! 1.98) and its `Agent.builder().connect_to(Stdio::new())` covers the
//! agent side. We still do not use it, and unlike the `rmcp` decision next
//! door the reason is cost, not capability:
//!
//! * **Dependency weight.** It adds **52** crates to this workspace
//!   (measured by resolving it alone and diffing against our `Cargo.lock`),
//!   where `rmcp` added 13. Among them: a second async reactor
//!   (`async-io`, `async-process`, `async-signal`, `polling`, `blocking`)
//!   alongside the tokio runtime `AgentService` already runs on, two more
//!   datetime libraries (`jiff` + `jiff-tzdb`, `time`) alongside our
//!   `chrono`, `defmt` (an *embedded* logging framework), and the
//!   `darling`/`strum`/`serde_with`/`derive_more` proc-macro trees.
//! * **Surface mismatch.** 2.x is a framework — roles, components, proxy
//!   chains, protocol routers, MCP-over-ACP, derive macros for JSON-RPC
//!   traits — and it is moving fast (1.0 to 2.2 in three months, with a
//!   draft protocol v2 behind `unstable_protocol_v2`). What this adapter
//!   needs is one newline-delimited JSON-RPC loop and about a dozen
//!   message types.
//!
//! So [`protocol`] carries those types, transcribed field-by-field from the
//! authoritative schema source (`agent-client-protocol-schema` **1.9.1**,
//! `src/v1/`, the version 2.2.0 pins with `=`). Nothing here is guessed
//! from prose. If the protocol moves in a way this subset cannot follow,
//! adopting the SDK behind these same module boundaries is the intended
//! escape hatch: [`dispatch`] is where the wire meets forge, and it is
//! pure.
//!
//! # Protocol facts this adapter relies on (verified against the schema)
//!
//! * **Protocol version** is a single integer, "only bumped for breaking
//!   changes". `V1` is `LATEST` in 1.9.1; `V2` exists only behind the
//!   SDK's `unstable_protocol_v2` feature, as a draft. We speak 1.
//! * **Framing** is JSON-RPC 2.0, one message per line over stdio — no
//!   `Content-Length` headers (that is LSP). Source:
//!   <https://agentclientprotocol.com/protocol/overview>
//! * **Methods** we implement (agent side): `initialize`, `session/new`,
//!   `session/prompt`, and the `session/cancel` *notification*. Methods we
//!   call (client side): the `session/update` *notification* and the
//!   `session/request_permission` request.
//! * **`session/update` nests its payload**: `params` is
//!   `{ sessionId, update: { sessionUpdate: "<variant>", ... } }`, where
//!   `sessionUpdate` is the internal tag.
//! * **Stop reasons** are `end_turn`, `max_tokens`, `max_turn_requests`,
//!   `refusal`, `cancelled`. `cancelled` MUST be returned after a
//!   `session/cancel` "even if the cancellation causes exceptions in
//!   underlying operations".
//! * **Permission outcomes** are internally tagged with `outcome`:
//!   `{"outcome":"selected","optionId":"..."}` or
//!   `{"outcome":"cancelled"}`. A client that cancels a turn MUST answer
//!   every pending permission request with `cancelled`.
//! * **stdout purity**: stdout is the protocol channel, so no code path in
//!   this process may `println!`. Diagnostics go to stderr, which
//!   `tracing_setup::init` already does, and a test pins it.
//!
//! # What v1 of this adapter deliberately does not do
//!
//! * **It ignores the client's `fs`/`terminal` capabilities.** forge runs
//!   every tool through its own `ExecutionProvider`, rooted at the
//!   session's `cwd`, so it never asks the editor to read or write on its
//!   behalf. That keeps one execution path with one set of risk and
//!   approval rules. Bridging unsaved editor buffers is a follow-up.
//! * **It does not stream tokens.** forge's agent loop produces final text
//!   rather than a token stream, so the answer is sent as one
//!   `agent_message_chunk`. Faking a stream by chopping up finished text
//!   would only look like streaming.
//! * **No `session/load`**, and text-only prompts — both advertised
//!   honestly in `initialize`.

pub mod dispatch;
pub mod protocol;
pub mod server;

pub use server::{ForgeAcpServer, ServiceFactory, serve_stdio};
