//! Wire protocols: SSE framing, Chat Completions request rewriting, the
//! streaming tee with per-call tool buffering (PLAN.md §5 step 5, §10), and the
//! Anthropic Messages adapter (§13).

pub mod chat;
pub mod messages;
pub mod responses;
pub mod sse;
pub mod stream;

pub use sse::{SseEvent, SseParser};
pub use stream::{CallDecision, StreamFilter, ToolCall, Transcript};
