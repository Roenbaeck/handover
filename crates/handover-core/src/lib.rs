//! Pure, synchronous building blocks of the handover proxy (PLAN.md §5–§10).
//!
//! Nothing here performs I/O. The proxy feeds requests and register rows in and
//! gets classifications, packets, enforcement decisions and verified deltas out.

pub mod classify;
pub mod clock;
pub mod conversation;
pub mod enforce;
pub mod extraction;
pub mod item;
pub mod packet;
pub mod profile;
pub mod status;
pub mod verify;

pub use classify::{Classification, Incoming, LaneView, Route, classify, route};
pub use clock::Clock;
pub use conversation::{CanonMessage, Conversation, Label, Span, canonical_json};
pub use enforce::{Action, Decision, Enforcer, Op, Scope, Verdict};
pub use item::{Assertion, Authority, Citation, ItemRecord, Kind, RegisterItem, Status, Strength};
pub use packet::{Packet, PacketInput, PacketStrategy};
pub use profile::Profile;
pub use verify::{Delta, DeltaOp, Source, TrustPolicy, VerifiedDelta};
