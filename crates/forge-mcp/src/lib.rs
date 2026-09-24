//! `forge mcp` — a Model Context Protocol server over stdio, exposing
//! forge's project intelligence (graph, skills, doctor) and agent runs as
//! MCP tools to any MCP client (Claude Code, VS Code, Cursor, other
//! harnesses).
//!
//! This crate is the stdio sibling of `forge-server`: both are thin
//! adapters over the one shared [`AgentService`] runtime, which the CLI
//! constructs (`commands::service::build_run_service`). No routing, model
//! or execution logic lives here.
//!
//! # Trust model
//!
//! Loopback-only by construction: the client launches this process as a
//! subprocess on the same machine, as the same user. There is no network
//! listener and no authentication layer.
//!
//! # SDK decision: the official `rmcp` crate (verified 2026-09-24)
//!
//! We use `rmcp` 3.4 (modelcontextprotocol/rust-sdk), default features
//! off, `server` + `transport-io` only. Evidence for the choice:
//!
//! * It builds on our stable edition-2024 workspace (crate edition 2024,
//!   MSRV 1.88; our toolchain is 1.98).
//! * The stdio-only feature slice is small — 13 new transitive crates,
//!   none of them a runtime we did not already have. `macros`/`schemars`
//!   stay off because our tool schemas are hand-written `serde_json`.
//! * It is *dual-era*, which hand-rolling would have made our problem.
//!   MCP revision `2026-07-28` (the current one) replaced the `initialize`
//!   handshake with per-request `_meta` metadata plus a mandatory
//!   `server/discover` RPC, while `2025-11-25` and earlier still expect
//!   `initialize`. rmcp's `ProtocolVersion` carries `V_2025_06_18`,
//!   `V_2025_11_25` and `V_2026_07_28`, and its stdio serve loop picks the
//!   era from how the client opens (an `initialize` first request selects
//!   legacy; anything else enters the stateless modern loop) — so one
//!   `forge mcp` process serves both kinds of client. It also answers
//!   `initialize` *after* a `server/discover` opener, which is exactly what
//!   current Claude Code does.
//!
//! # Protocol facts this adapter relies on (verified against the spec)
//!
//! * **Framing**: stdio MCP is newline-delimited JSON-RPC — one message
//!   per line, no `Content-Length` headers (that is LSP, not MCP). Source:
//!   <https://modelcontextprotocol.io/specification/2026-07-28/basic/transports/stdio>
//! * **stdout purity**: "The server MUST NOT write anything to its
//!   `stdout` that is not a valid MCP message", and it MAY write anything
//!   to `stderr`. All forge diagnostics therefore go to stderr
//!   (`tracing_setup::init` already writes there) and no code path on this
//!   side may `println!`.
//! * **Shutdown**: "Servers SHOULD exit promptly when their standard input
//!   is closed or reads return end-of-file" — [`serve_stdio`] returns on
//!   EOF.
//! * **Tool results**: a `content` array (we send one text item holding
//!   compact JSON) plus `structuredContent` with the same value, and
//!   `isError: true` for tool execution errors. Unknown tool names are
//!   protocol errors instead. rmcp adds/strips the `resultType`
//!   discriminator per negotiated version.

mod runs;
pub mod server;
pub mod tools;

pub use server::{ForgeMcpServer, serve_stdio};
pub use tools::{Diagnostics, ForgeTools, ToolDef, ToolError, ToolOutcome, definitions};
