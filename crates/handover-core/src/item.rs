//! Register items: the paper's inventory items ⟨content, source, time, status,
//! confidence, role⟩ (PLAN.md §3, §8).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Permission,
    Prohibition,
    Commitment,
    Goal,
    RoleBoundary,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Permission => "permission",
            Kind::Prohibition => "prohibition",
            Kind::Commitment => "commitment",
            Kind::Goal => "goal",
            Kind::RoleBoundary => "role_boundary",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "permission" => Kind::Permission,
            "prohibition" => Kind::Prohibition,
            "commitment" => Kind::Commitment,
            "goal" => Kind::Goal,
            "role_boundary" | "role boundary" | "roleboundary" => Kind::RoleBoundary,
            _ => return None,
        })
    }

    /// Handle prefix in packets: P for permissions and prohibitions, C, G, R.
    pub fn letter(self) -> char {
        match self {
            Kind::Permission | Kind::Prohibition => 'P',
            Kind::Commitment => 'C',
            Kind::Goal => 'G',
            Kind::RoleBoundary => 'R',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strength {
    Hard,
    Graded,
}

impl Strength {
    pub fn as_str(self) -> &'static str {
        match self {
            Strength::Hard => "hard",
            Strength::Graded => "graded",
        }
    }

    pub fn parse(s: &str) -> Option<Strength> {
        match s.trim().to_ascii_lowercase().as_str() {
            "hard" => Some(Strength::Hard),
            "graded" | "soft" => Some(Strength::Graded),
            _ => None,
        }
    }
}

/// Derived status (never stored; PLAN.md §8.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Active,
    Superseded,
    Revoked,
    Disputed,
    Uncertain,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Superseded => "superseded",
            Status::Revoked => "revoked",
            Status::Disputed => "disputed",
            Status::Uncertain => "uncertain",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    Authoritative,
    Advisory,
}

/// One source's position on an item's membership posit at some time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Assertion {
    /// Positor name, e.g. `developer:42`, `harness:claude-code`, `model:qwen3`.
    pub source: String,
    pub authority: Authority,
    /// Signed certainty in [-1, 1]; 0 is a withdrawal.
    pub certainty: f64,
    /// Traqula time text of the assertion.
    pub time: String,
    /// The extractor's certainty that this assertion was really made, if any.
    pub meta_certainty: Option<f64>,
}

/// Provenance attached by one register change: the turn, the quote and the op.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Citation {
    pub time: String,
    pub value: Value,
}

impl Citation {
    pub fn turn(&self) -> Option<u64> {
        self.value.get("turn").and_then(Value::as_u64)
    }
    pub fn quote(&self) -> Option<&str> {
        self.value.get("quote").and_then(Value::as_str)
    }
    pub fn op(&self) -> Option<&str> {
        self.value.get("op").and_then(Value::as_str)
    }
}

/// Everything the register store returns about one item at a cut.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ItemRecord {
    /// Unique key, `<session key>#<seq>`.
    pub key: String,
    /// Session-wide sequence number (the handle number).
    pub seq: u64,
    pub kind: Kind,
    pub strength: Strength,
    /// Every wording with its time, oldest first.
    pub statements: Vec<(String, String)>,
    pub enforce: Option<Value>,
    /// Provenance of each change, oldest first.
    pub citations: Vec<Citation>,
    /// Full assertion history up to the cut, oldest first.
    pub assertions: Vec<Assertion>,
    /// Keys of items that replace this one.
    pub replaced_by: Vec<String>,
    /// Keys of items this one requires: the pair is presented together or not at all.
    #[serde(default)]
    pub requires: Vec<String>,
    /// Time the item was registered in this branch.
    pub introduced: String,
}

impl ItemRecord {
    pub fn handle(&self) -> String {
        handle(self.kind, self.seq)
    }
}

pub fn handle(kind: Kind, seq: u64) -> String {
    format!("{}{}", kind.letter(), seq)
}

/// An item with its derived status, as packets and enforcement see it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisterItem {
    pub record: ItemRecord,
    pub status: Status,
    /// Latest wording.
    pub statement: String,
    /// Other wordings asserted at the same latest time (disputed wording).
    pub conflicting_statements: Vec<String>,
    /// Key of the active item that supersedes this one, if any.
    pub superseded_by: Option<String>,
}

impl RegisterItem {
    pub fn handle(&self) -> String {
        self.record.handle()
    }
    pub fn key(&self) -> &str {
        &self.record.key
    }
    pub fn kind(&self) -> Kind {
        self.record.kind
    }
    pub fn strength(&self) -> Strength {
        self.record.strength
    }
    pub fn is_hard(&self) -> bool {
        self.record.strength == Strength::Hard
    }
}
