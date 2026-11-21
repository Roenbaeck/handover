//! The constraint register on Positorium (PLAN.md §8, §11).
//!
//! One process owns the store. Writes run in order on a dedicated writer thread,
//! each as one Traqula command (one durable commit) with existing Things passed
//! as identity parameters and `strict mutations;` (spike R1–R6). Reads run on
//! snapshot readers in `spawn_blocking`, anchored on the `branch_key` literal.

mod schema;
mod value;

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};

use chrono::{DateTime, Utc};
use handover_core::clock;
use handover_core::item::{Assertion, Authority, Citation, ItemRecord, Kind, Strength};
use handover_core::verify::{DeltaOp, Target, VerifiedDelta};
use positorium::{CollectedResultSet, Database, ExecutionOptions, ExecutionParameter, QueryLimits};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::oneshot;

pub use schema::{AUDITS, ROLES};
use value::*;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("positorium: {0}")]
    Db(#[from] positorium::DatabaseError),
    #[error("register writer stopped")]
    WriterGone,
    #[error("register: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

type Job = Box<dyn FnOnce(&Database) + Send>;

/// One audit's outcome.
#[derive(Clone, Debug, Serialize)]
pub struct AuditResult {
    pub name: String,
    pub verdict: String,
    pub findings: usize,
    pub ready: bool,
    pub missing_inputs: usize,
}

/// A recorded epoch, as read back at restart.
#[derive(Clone, Debug, Serialize)]
pub struct EpochSnapshot {
    pub key: String,
    pub trigger: String,
    pub at: String,
    pub model: String,
    pub packet: Option<String>,
    pub turn: i64,
    pub through: i64,
    pub drop_tools: Vec<String>,
}

/// A branch, as read back at restart.
#[derive(Clone, Debug, Serialize)]
pub struct BranchSnapshot {
    pub key: String,
    pub fork_common: i64,
    pub created: String,
    /// `(message index, chain digest, time)` of human-facing turn anchors.
    pub anchors: Vec<(usize, String, String)>,
    pub epochs: Vec<EpochSnapshot>,
    pub watermark: Option<i64>,
    pub served_model: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionSnapshot {
    pub key: String,
    /// The owning developer's positor name.
    pub owner: Option<String>,
    pub branches: Vec<BranchSnapshot>,
}

/// A branch's inherited item: the item key plus every source's latest position at the fork.
#[derive(Clone, Debug)]
pub struct Inherited {
    pub item_key: String,
    /// `(positor name, certainty, meta-certainty)`.
    pub positions: Vec<(String, f64, Option<f64>)>,
}

/// A verified change with the time of the message it came from.
#[derive(Clone, Debug)]
pub struct TimedDelta {
    pub delta: VerifiedDelta,
    /// Positor name the change is attributed to.
    pub positor: String,
    pub at: DateTime<Utc>,
}

/// One enforcement decision for `record_exchange`.
#[derive(Clone, Debug)]
pub struct ActionRecord {
    pub action: Value,
    /// `forwarded`, `warned`, `held` or `blocked`.
    pub decision: String,
    /// Keys of the items whose scopes matched.
    pub matched: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct EpochWrite {
    pub key: String,
    pub trigger: String,
    pub to_model: String,
    pub packet: Option<String>,
    pub citations: Vec<(String, String)>,
    pub turn: usize,
    pub through: Option<usize>,
    /// Tool definitions removed from upstream requests for the whole epoch.
    pub drop_tools: Vec<String>,
}

pub struct Register {
    /// `None` after `shutdown`, so the store's owner lock is released.
    db: std::sync::RwLock<Option<Arc<Database>>>,
    writer: Mutex<Option<mpsc::Sender<Job>>>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    store: String,
    ids: Mutex<HashMap<String, String>>,
    /// Constraint revision references, in `AUDITS` order.
    revisions: Vec<ExecutionParameter>,
}

impl Register {
    /// Open (or create) a persistent store, or an in-memory one with `None`.
    pub fn open(path: Option<&Path>) -> Result<Arc<Register>> {
        let db = match path {
            Some(p) => Database::open(p)?,
            None => Database::memory()?,
        };
        if !db.contains_role("branch_key")? {
            db.execute(ROLES)?;
        }
        if !db.contains_role("complement")? {
            db.execute("add role complement, complement_of;")?; // stores from before C4
        }
        let mut revisions = Vec::new();
        for a in AUDITS {
            // Re-adding an identical definition returns its canonical revision.
            let set = db.execute_one(a.source)?;
            revisions.push(set.reference(0, "revision")?);
        }
        let store = db.store_id().to_string();
        let db = Arc::new(db);
        let (tx, rx) = mpsc::channel::<Job>();
        let wdb = db.clone();
        let thread = std::thread::Builder::new()
            .name("register-writer".into())
            .spawn(move || {
                while let Ok(job) = rx.recv() {
                    job(&wdb);
                }
            })
            .map_err(|e| Error::Invalid(e.to_string()))?;
        Ok(Arc::new(Register {
            db: std::sync::RwLock::new(Some(db)),
            writer: Mutex::new(Some(tx)),
            thread: Mutex::new(Some(thread)),
            store,
            ids: Mutex::new(HashMap::new()),
            revisions,
        }))
    }

    /// Stop the writer after its queued jobs, wait for it, and release the
    /// store (its owner lock goes once in-flight reads finish).
    pub fn shutdown(&self) {
        self.writer.lock().unwrap().take();
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
        self.db.write().unwrap().take();
    }

    fn db(&self) -> Result<Arc<Database>> {
        self.db.read().unwrap().clone().ok_or(Error::WriterGone)
    }

    pub fn store_id(&self) -> &str {
        &self.store
    }

    fn thing(&self, id: &str) -> ExecutionParameter {
        ExecutionParameter::Thing { text: format!("{}/{}", self.store, id) }
    }

    fn posit(&self, id: &str) -> ExecutionParameter {
        ExecutionParameter::Posit { text: format!("{}/{}", self.store, id) }
    }

    fn cached(&self, key: &str) -> Option<String> {
        self.ids.lock().unwrap().get(key).cloned()
    }

    fn cache(&self, key: String, id: String) {
        self.ids.lock().unwrap().insert(key, id);
    }

    /// Run a write script on the writer thread.
    async fn write(&self, script: String, opts: ExecutionOptions) -> Result<Vec<CollectedResultSet>> {
        let (tx, rx) = oneshot::channel();
        let job: Job = Box::new(move |db: &Database| {
            let _ = tx.send(db.execute_with_options(&script, opts));
        });
        self.writer.lock().unwrap().as_ref().ok_or(Error::WriterGone)?.send(job).map_err(|_| Error::WriterGone)?;
        Ok(rx.await.map_err(|_| Error::WriterGone)??)
    }

    /// Run a read script on a snapshot reader.
    async fn read(&self, script: String, opts: ExecutionOptions) -> Result<Vec<CollectedResultSet>> {
        let db = self.db()?;
        tokio::task::spawn_blocking(move || db.execute_with_options(&script, opts))
            .await
            .map_err(|e| Error::Invalid(e.to_string()))?
            .map_err(Error::from)
    }

    /// Read-only Traqula for the admin endpoint: searches and explains only, with budgets.
    pub async fn query(&self, script: &str) -> Result<Vec<CollectedResultSet>> {
        if !is_read_only(script) {
            return Err(Error::Invalid("only search and explain are allowed".into()));
        }
        let opts = ExecutionOptions {
            timeout: Some(std::time::Duration::from_secs(5)),
            max_rows_per_search: Some(10_000),
            query_limits: QueryLimits { max_work_units: Some(50_000_000), max_memory_bytes: Some(256 << 20) },
            ..Default::default()
        };
        self.read(script.to_string(), opts).await
    }

    // ---------------------------------------------------------------- identities

    /// Get-or-create a positor by name; returns its Thing id.
    pub async fn positor(&self, name: &str) -> Result<String> {
        let key = format!("p:{name}");
        if let Some(id) = self.cached(&key) {
            return Ok(id);
        }
        let sets = self
            .write(
                "strict mutations;
                 search [{(?p, name)}, $n, *] or add posit [{(+p, name)}, $n, @NOW];
                 search [{(?p, name)}, $n, *] return ?p;"
                    .into(),
                ExecutionOptions::default().bind("n", name),
            )
            .await?;
        let id = first_cell(&sets, "p")?;
        self.cache(key, id.clone());
        Ok(id)
    }

    async fn branch_id(&self, branch_key: &str) -> Result<String> {
        let key = format!("b:{branch_key}");
        if let Some(id) = self.cached(&key) {
            return Ok(id);
        }
        let sets =
            self.read("search [{(?b, branch_key)}, $k, *] return ?b;".into(), ExecutionOptions::default().bind("k", branch_key)).await?;
        let id = first_cell(&sets, "b").map_err(|_| Error::Invalid(format!("unknown branch {branch_key}")))?;
        self.cache(key, id.clone());
        Ok(id)
    }

    async fn session_id(&self, session_key: &str) -> Result<String> {
        let key = format!("s:{session_key}");
        if let Some(id) = self.cached(&key) {
            return Ok(id);
        }
        let sets =
            self.read("search [{(?s, session_key)}, $k, *] return ?s;".into(), ExecutionOptions::default().bind("k", session_key)).await?;
        let id = first_cell(&sets, "s").map_err(|_| Error::Invalid(format!("unknown session {session_key}")))?;
        self.cache(key, id.clone());
        Ok(id)
    }

    /// Cache item Thing ids and this branch's membership posits.
    async fn load_memberships(&self, branch_key: &str) -> Result<HashMap<String, (String, String)>> {
        let sets = self
            .read(
                "search [{(?b, branch_key)}, $bk, *],
                        ?m = [{(?i, item), (?b, branch)}, \"registered\", *],
                        [{(?i, item_key)}, ?k, *]
                 return ?k, ?i, ?m;"
                    .into(),
                ExecutionOptions::default().bind("bk", branch_key),
            )
            .await?;
        let mut out = HashMap::new();
        if let Some(set) = sets.last() {
            for r in 0..set.rows.len() {
                let k = lit_str(cell(set, r, "k"));
                let i = cell(set, r, "i").to_string();
                let m = cell(set, r, "m").to_string();
                self.cache(format!("i:{k}"), i.clone());
                out.insert(k, (i, m));
            }
        }
        Ok(out)
    }

    async fn item_id(&self, item_key: &str) -> Result<String> {
        let key = format!("i:{item_key}");
        if let Some(id) = self.cached(&key) {
            return Ok(id);
        }
        let sets = self.read("search [{(?i, item_key)}, $k, *] return ?i;".into(), ExecutionOptions::default().bind("k", item_key)).await?;
        let id = first_cell(&sets, "i").map_err(|_| Error::Invalid(format!("unknown item {item_key}")))?;
        self.cache(key, id.clone());
        Ok(id)
    }

    /// Next session-wide item sequence number.
    async fn next_seq(&self, session_key: &str) -> Result<u64> {
        let key = format!("seq:{session_key}");
        if let Some(n) = self.cached(&key) {
            let n: u64 = n.parse().unwrap_or(0) + 1;
            self.cache(key, n.to_string());
            return Ok(n);
        }
        let sets = self
            .read(
                "search [{(?s, session_key)}, $sk, *],
                        [{(?b, branch), (?s, session)}, *, *],
                        [{(?i, item), (?b, branch)}, *, *],
                        [{(?i, item_key)}, ?k, *]
                 return distinct ?k;"
                    .into(),
                ExecutionOptions::default().bind("sk", session_key),
            )
            .await?;
        let max =
            sets.last().map(|set| (0..set.rows.len()).filter_map(|r| seq_of(&lit_str(cell(set, r, "k")))).max().unwrap_or(0)).unwrap_or(0);
        let n = max + 1;
        self.cache(key, n.to_string());
        Ok(n)
    }

    // ---------------------------------------------------------------- writes

    /// Create the session, its owner link, its root branch `<key>#0` with the
    /// given authoritative positors, and optionally its first epoch — all in one
    /// command (one durable commit). Idempotent: an existing session is left alone.
    pub async fn ensure_session(
        &self,
        session_key: &str,
        developer: &str,
        harness: &str,
        authorities: &[String],
        first_epoch: Option<&EpochWrite>,
        at: DateTime<Utc>,
    ) -> Result<String> {
        let root = root_branch(session_key);
        if self.cached(&format!("b:{root}")).is_some() {
            return Ok(root);
        }
        let dev = self.positor(developer).await?;
        let mut opts = ExecutionOptions::default()
            .bind("sk", session_key)
            .bind("bk", root.as_str())
            .bind("harness", harness)
            .bind("dev", self.thing(&dev))
            .bind("t", time_param(at));
        let mut parts = vec![
            "[{(+s, session_key)}, $sk, $t]".to_string(),
            "[{(s, session), ($dev, developer)}, \"owner\", $t]".to_string(),
            "[{(s, harness)}, $harness, $t]".to_string(),
            "[{(+b, branch_key)}, $bk, $t]".to_string(),
            "[{(b, branch), (s, session)}, 0, $t]".to_string(),
        ];
        for (n, a) in authorities.iter().enumerate() {
            let id = self.positor(a).await?;
            opts = opts.bind(format!("a{n}"), self.thing(&id));
            parts.push(format!("[{{(b, branch), ($a{n}, authority)}}, \"authoritative\", $t]"));
        }
        if let Some(e) = first_epoch {
            opts = opts
                .bind("ek", e.key.as_str())
                .bind("trigger", e.trigger.as_str())
                .bind("model", e.to_model.as_str())
                .bind("turn", e.turn as i64)
                .bind("through", e.through.map(|w| w as i64).unwrap_or(-1));
            parts.extend([
                "[{(+e, epoch_key)}, $ek, $t]".to_string(),
                "[{(e, epoch), (b, branch)}, $trigger, $t]".to_string(),
                "[{(e, to_model)}, $model, $t]".to_string(),
                "[{(e, turn)}, $turn, $t]".to_string(),
                "[{(e, extracted_through)}, $through, $t]".to_string(),
            ]);
        }
        // Concurrent first requests serialize on the writer: the second finds the session.
        let script = format!(
            "strict mutations;
             search [{{(?s, session_key)}}, $sk, *] or add posit {};
             search [{{(?s, session_key)}}, $sk, *], [{{(?b, branch_key)}}, $bk, *] return ?s, ?b;",
            parts.join(",\n  ")
        );
        let sets = self.write(script, opts).await?;
        self.cache(format!("s:{session_key}"), first_cell(&sets, "s")?);
        self.cache(format!("b:{root}"), first_cell(&sets, "b")?);
        Ok(root)
    }

    /// Everything a finished turn leaves behind, in one command: the turn anchor
    /// (human-facing turns only), a changed served model, and every enforcement
    /// decision with the items it matched (§5 step 6).
    pub async fn record_exchange(
        &self,
        branch_key: &str,
        anchor: Option<(usize, &str)>,
        served_model: Option<&str>,
        actions: &[ActionRecord],
        at: DateTime<Utc>,
    ) -> Result<()> {
        if anchor.is_none() && served_model.is_none() && actions.is_empty() {
            return Ok(());
        }
        let b = self.branch_id(branch_key).await?;
        let mut opts = ExecutionOptions::default().bind("b", self.thing(&b)).bind("t", time_param(at));
        let mut parts = Vec::new();
        if let Some((idx, digest)) = anchor {
            opts = opts.bind("idx", idx as i64).bind("d", digest);
            parts.push("[{(+turn0, turn), ($b, branch)}, $idx, $t]".to_string());
            parts.push("[{(turn0, digest)}, $d, $t]".to_string());
        }
        if let Some(m) = served_model {
            opts = opts.bind("sm", m);
            parts.push("[{($b, served_model)}, $sm, $t]".to_string());
        }
        for (n, a) in actions.iter().enumerate() {
            opts = opts.bind(format!("act{n}"), json_param(&a.action)).bind(format!("dec{n}"), a.decision.as_str());
            parts.push(format!("[{{(+x{n}, action), ($b, branch)}}, $act{n}, $t]"));
            parts.push(format!("[{{(x{n}, decision)}}, $dec{n}, $t]"));
            for (j, k) in a.matched.iter().enumerate() {
                let i = self.item_id(k).await?;
                opts = opts.bind(format!("i{n}_{j}"), self.thing(&i));
                parts.push(format!("[{{(x{n}, action), ($i{n}_{j}, matched_item)}}, \"match\", $t]"));
            }
        }
        self.write(format!("strict mutations;\nadd posit {};", parts.join(",\n  ")), opts).await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // one call site per argument group; a struct would only rename them
    /// Create a child branch that forks `parent` after `fork_common` messages and
    /// inherits the given items with their positions (one command).
    pub async fn create_branch(
        &self,
        session_key: &str,
        parent_key: &str,
        child_key: &str,
        fork_common: usize,
        authorities: &[String],
        inherited: &[Inherited],
        watermark: Option<usize>,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let s = self.session_id(session_key).await?;
        let parent = self.branch_id(parent_key).await?;
        let mut opts = ExecutionOptions::default()
            .bind("bk", child_key)
            .bind("s", self.thing(&s))
            .bind("parent", self.thing(&parent))
            .bind("fork", fork_common as i64)
            .bind("t", time_param(at));
        let mut parts = vec![
            "[{(+b, branch_key)}, $bk, $t]".to_string(),
            "[{(b, branch), ($s, session)}, $fork, $t]".to_string(),
            "[{(b, branch), ($parent, parent)}, $fork, $t]".to_string(),
        ];
        // What the parent had already extracted of the shared prefix (survives a restart).
        if let Some(w) = watermark {
            opts = opts.bind("w", w as i64);
            parts.push("[{(b, extracted_through)}, $w, $t]".to_string());
        }
        for (n, a) in authorities.iter().enumerate() {
            let id = self.positor(a).await?;
            parts.push(format!("[{{(b, branch), ($a{n}, authority)}}, \"authoritative\", $t]"));
            opts = opts.bind(format!("a{n}"), self.thing(&id));
        }
        let extractor = self.positor("handover:inheritance").await?;
        opts = opts.bind("inh", self.thing(&extractor));
        for (k, item) in inherited.iter().enumerate() {
            let i = self.item_id(&item.item_key).await?;
            opts = opts.bind(format!("i{k}"), self.thing(&i));
            parts.push(format!("+m{k} [{{($i{k}, item), (b, branch)}}, \"registered\", $t]"));
            // Provenance of the inherited positions (audit C5 expects one per assertion time).
            opts = opts.bind(format!("cite{k}"), json_param(&json!({"op": "inherit", "from": parent_key, "fork_common": fork_common})));
            parts.push(format!("[{{($i{k}, cited_turn)}}, $cite{k}, $t]"));
            for (j, (positor, c, meta)) in item.positions.iter().enumerate() {
                let p = self.positor(positor).await?;
                opts = opts.bind(format!("p{k}_{j}"), self.thing(&p)).bind(format!("c{k}_{j}"), cert(*c));
                parts.push(format!("+a{k}_{j} [{{(m{k}, posit), ($p{k}_{j}, ascertains)}}, $c{k}_{j}, $t]"));
                if let Some(mc) = meta {
                    opts = opts.bind(format!("mc{k}_{j}"), cert(*mc));
                    parts.push(format!("[{{(a{k}_{j}, posit), ($inh, ascertains)}}, $mc{k}_{j}, $t]"));
                }
            }
        }
        let script = format!("strict mutations;\nadd posit {};\nsearch [{{(?b, branch_key)}}, $bk, *] return ?b;", parts.join(",\n  "));
        let sets = self.write(script, opts).await?;
        self.cache(format!("b:{child_key}"), first_cell(&sets, "b")?);
        Ok(())
    }

    /// Turn anchor at a human-facing turn: message index and chain digest.
    pub async fn record_turn(&self, branch_key: &str, index: usize, digest: &str, at: DateTime<Utc>) -> Result<()> {
        let b = self.branch_id(branch_key).await?;
        self.write(
            "add posit [{(+t, turn), ($b, branch)}, $idx, $at], [{(t, digest)}, $d, $at];".into(),
            ExecutionOptions::default().bind("b", self.thing(&b)).bind("idx", index as i64).bind("d", digest).bind("at", time_param(at)),
        )
        .await?;
        Ok(())
    }

    pub async fn record_served_model(&self, branch_key: &str, model: &str, at: DateTime<Utc>) -> Result<()> {
        let b = self.branch_id(branch_key).await?;
        self.write(
            "add posit [{($b, served_model)}, $m, $at];".into(),
            ExecutionOptions::default().bind("b", self.thing(&b)).bind("m", model).bind("at", time_param(at)),
        )
        .await?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    /// Apply verified deltas and move the watermark, all in one command (spike R6).
    /// `extractor` names the positor of the meta-assertions; `None` for explicit
    /// decisions, which carry no extraction provenance. Returns the keys of new items.
    pub async fn apply(
        &self,
        session_key: &str,
        branch_key: &str,
        deltas: &[TimedDelta],
        extractor: Option<&str>,
        through: Option<usize>,
        run: Option<Value>,
        now: DateTime<Utc>,
    ) -> Result<Vec<String>> {
        let b = self.branch_id(branch_key).await?;
        let members = self.load_memberships(branch_key).await?;
        let mut opts = ExecutionOptions::default().bind("b", self.thing(&b)).bind("now", time_param(now));
        let mut parts: Vec<String> = Vec::new();
        if let Some(name) = extractor {
            let ext = self.positor(name).await?;
            opts = opts.bind("ext", self.thing(&ext));
        }
        let mut new_keys = Vec::new();
        for (k, td) in deltas.iter().enumerate() {
            let d = &td.delta;
            let p = self.positor(&td.positor).await?;
            opts = opts
                .bind(format!("p{k}"), self.thing(&p))
                .bind(format!("t{k}"), time_param(td.at))
                .bind(format!("c{k}"), cert(d.certainty))
                .bind(format!("cite{k}"), json_param(&d.citation));
            let meta = |parts: &mut Vec<String>, opts: ExecutionOptions| -> ExecutionOptions {
                if extractor.is_some() {
                    parts.push(format!("[{{(a{k}, posit), ($ext, ascertains)}}, $mc{k}, $now]"));
                    opts.bind(format!("mc{k}"), cert(d.meta_certainty))
                } else {
                    opts
                }
            };
            match &d.target {
                Target::New => {
                    let seq = self.next_seq(session_key).await?;
                    let key = format!("{session_key}#{seq}");
                    opts = opts
                        .bind(format!("key{k}"), key.as_str())
                        .bind(format!("kind{k}"), d.kind.as_str())
                        .bind(format!("str{k}"), d.strength.as_str())
                        .bind(format!("st{k}"), d.statement.clone().unwrap_or_default());
                    parts.push(format!("[{{(+i{k}, item_key)}}, $key{k}, $t{k}]"));
                    parts.push(format!("[{{(i{k}, kind)}}, $kind{k}, $t{k}]"));
                    parts.push(format!("[{{(i{k}, strength)}}, $str{k}, $t{k}]"));
                    parts.push(format!("[{{(i{k}, statement)}}, $st{k}, $t{k}]"));
                    parts.push(format!("[{{(i{k}, cited_turn)}}, $cite{k}, $t{k}]"));
                    if let Some(e) = &d.enforce {
                        opts = opts.bind(format!("en{k}"), json_param(e));
                        parts.push(format!("[{{(i{k}, enforce)}}, $en{k}, $t{k}]"));
                    }
                    parts.push(format!("+m{k} [{{(i{k}, item), ($b, branch)}}, \"registered\", $t{k}]"));
                    parts.push(format!("+a{k} [{{(m{k}, posit), ($p{k}, ascertains)}}, $c{k}, $t{k}]"));
                    opts = meta(&mut parts, opts);
                    if let Some(req) = &d.requires {
                        let req_id = self.item_id(req).await?;
                        opts = opts.bind(format!("req{k}"), self.thing(&req_id));
                        parts.push(format!("[{{(i{k}, complement), ($req{k}, complement_of)}}, \"requires\", $t{k}]"));
                    }
                    if let Some(old) = &d.replaces {
                        let old_id = self.item_id(old).await?;
                        opts = opts.bind(format!("old{k}"), self.thing(&old_id));
                        parts.push(format!("[{{(i{k}, replacement), ($old{k}, replaced)}}, \"supersedes\", $t{k}]"));
                    }
                    new_keys.push(key);
                }
                Target::Existing(item_key) => {
                    let (i, m) = members
                        .get(item_key)
                        .cloned()
                        .ok_or_else(|| Error::Invalid(format!("item {item_key} is not in branch {branch_key}")))?;
                    opts = opts.bind(format!("i{k}"), self.thing(&i)).bind(format!("m{k}"), self.posit(&m));
                    parts.push(format!("+a{k} [{{($m{k}, posit), ($p{k}, ascertains)}}, $c{k}, $t{k}]"));
                    opts = meta(&mut parts, opts);
                    parts.push(format!("[{{($i{k}, cited_turn)}}, $cite{k}, $t{k}]"));
                    if d.op == DeltaOp::Restate
                        && let Some(st) = &d.statement
                    {
                        opts = opts.bind(format!("st{k}"), st.as_str());
                        parts.push(format!("[{{($i{k}, statement)}}, $st{k}, $t{k}]"));
                    }
                }
            }
        }
        if let Some(w) = through {
            opts = opts.bind("through", w as i64);
            parts.push("[{($b, extracted_through)}, $through, $now]".into());
        }
        if let Some(r) = &run {
            opts = opts.bind("run", json_param(r));
            parts.push("[{(+run, extraction), ($b, branch)}, $run, $now]".into());
        }
        if parts.is_empty() {
            return Ok(new_keys);
        }
        let script = format!("strict mutations;\nadd posit {};", parts.join(",\n  "));
        self.write(script, opts).await?;
        Ok(new_keys)
    }

    /// Persist an epoch, its packet bytes and what the packet cited (one command).
    pub async fn open_epoch(&self, branch_key: &str, e: &EpochWrite, at: DateTime<Utc>) -> Result<()> {
        let b = self.branch_id(branch_key).await?;
        let mut opts = ExecutionOptions::default()
            .bind("b", self.thing(&b))
            .bind("ek", e.key.as_str())
            .bind("trigger", e.trigger.as_str())
            .bind("model", e.to_model.as_str())
            .bind("turn", e.turn as i64)
            .bind("through", e.through.map(|w| w as i64).unwrap_or(-1))
            .bind("t", time_param(at));
        let mut parts = vec![
            "[{(+e, epoch_key)}, $ek, $t]".to_string(),
            "[{(e, epoch), ($b, branch)}, $trigger, $t]".to_string(),
            "[{(e, to_model)}, $model, $t]".to_string(),
            "[{(e, turn)}, $turn, $t]".to_string(),
            "[{(e, extracted_through)}, $through, $t]".to_string(),
        ];
        if let Some(p) = &e.packet {
            opts = opts.bind("packet", p.as_str());
            parts.push("[{(e, packet)}, $packet, $t]".into());
        }
        if !e.drop_tools.is_empty() {
            opts = opts.bind("drop", json_param(&json!({"drop_tools": e.drop_tools})));
            parts.push("[{(e, enforce)}, $drop, $t]".into());
        }
        for (n, (item_key, label)) in e.citations.iter().enumerate() {
            let i = self.item_id(item_key).await?;
            opts = opts.bind(format!("i{n}"), self.thing(&i)).bind(format!("l{n}"), label.as_str());
            parts.push(format!("[{{(e, packet_of), ($i{n}, packet_item)}}, $l{n}, $t]"));
        }
        self.write(format!("strict mutations;\nadd posit {};", parts.join(",\n  ")), opts).await?;
        Ok(())
    }

    /// Record an enforcement decision and the items it matched.
    pub async fn record_action(
        &self,
        branch_key: &str,
        action: &Value,
        decision: &str,
        matched: &[String],
        at: DateTime<Utc>,
    ) -> Result<()> {
        let b = self.branch_id(branch_key).await?;
        let mut opts = ExecutionOptions::default()
            .bind("b", self.thing(&b))
            .bind("act", json_param(action))
            .bind("d", decision)
            .bind("t", time_param(at));
        let mut parts = vec!["[{(+x, action), ($b, branch)}, $act, $t]".to_string(), "[{(x, decision)}, $d, $t]".to_string()];
        for (n, k) in matched.iter().enumerate() {
            let i = self.item_id(k).await?;
            opts = opts.bind(format!("i{n}"), self.thing(&i));
            parts.push(format!("[{{(x, action), ($i{n}, matched_item)}}, \"match\", $t]"));
        }
        self.write(format!("strict mutations;\nadd posit {};", parts.join(",\n  ")), opts).await?;
        Ok(())
    }

    fn scope(branch_key: &str) -> String {
        json!({"ready": {"parameters": {"branch_key": {"kind": "literal", "text": Value::String(branch_key.into()).to_string()}}, "clock": null}})
            .to_string()
    }

    /// Declare every audit revision ready for this branch (once). The proxy only
    /// passes the declaration to checks when every epoch was built from a complete
    /// register (§11).
    pub async fn declare_ready(&self, branch_key: &str, at: DateTime<Utc>) -> Result<()> {
        if self.cached(&format!("ready:{branch_key}")).is_some() {
            return Ok(());
        }
        let b = self.branch_id(branch_key).await?;
        let scope = Self::scope(branch_key);
        let mut opts = ExecutionOptions::default()
            .bind("b", self.thing(&b))
            .bind("scope", ExecutionParameter::Literal { text: scope.clone() })
            .bind("t", time_param(at));
        let mut parts = vec![
            "+run [{(+r, extraction), ($b, branch)}, {\"readiness\": true}, $t]".to_string(),
            "+entry [{(run, delivery)}, \"included\", $t]".to_string(),
        ];
        let mut searches = String::new();
        for (n, rev) in self.revisions.iter().enumerate() {
            opts = opts.bind(format!("rev{n}"), rev.clone());
            parts.push(format!("[{{($rev{n}, policy), (entry, manifest)}}, $scope, $t]"));
            searches.push_str(&format!("search ?ready = [{{($rev{n}, policy), (?entry, manifest)}}, $scope, *] return ?ready;\n"));
        }
        let script = format!("strict mutations;\nadd posit {};\n{searches}", parts.join(",\n  "));
        let sets = self.write(script, opts).await?;
        for (n, set) in sets.iter().rev().take(self.revisions.len()).rev().enumerate() {
            let last = set.rows.len().checked_sub(1).ok_or_else(|| Error::Invalid("readiness not found".into()))?;
            self.cache(format!("ready:{branch_key}:{n}"), cell(set, last, "ready").to_string());
        }
        self.cache(format!("ready:{branch_key}"), "1".into());
        Ok(())
    }

    /// Run every audit for a branch. With `ready`, checks use the branch's
    /// readiness declaration; without, they honestly return `indeterminate`.
    /// With `record`, each report is saved as an audit posit by the proxy.
    pub async fn audit(&self, branch_key: &str, ready: bool, record: bool) -> Result<Vec<AuditResult>> {
        let proxy = if record { Some(self.positor("handover").await?) } else { None };
        let mut out = Vec::new();
        for (n, (a, rev)) in AUDITS.iter().zip(&self.revisions).enumerate() {
            let ready_ref = if ready { self.cached(&format!("ready:{branch_key}:{n}")) } else { None };
            let mut opts = ExecutionOptions::default().bind("rev", rev.clone()).bind("branch_key", branch_key);
            let mut script = "check constraint $rev".to_string();
            if let Some(r) = &ready_ref {
                script.push_str(" with readiness $ready");
                opts = opts.bind("ready", self.posit(r));
            }
            if let Some(p) = &proxy {
                script.push_str(" record by $proxy");
                opts = opts.bind("proxy", self.thing(p));
            }
            script.push(';');
            let sets = if record { self.write(script, opts).await? } else { self.read(script, opts).await? };
            let set = sets.last().ok_or_else(|| Error::Invalid("no check result".into()))?;
            let report: Value = serde_json::from_str(cell(set, 0, "report")).unwrap_or(Value::Null);
            out.push(AuditResult {
                name: a.name.to_string(),
                verdict: report["verdict"].as_str().unwrap_or("unknown").to_string(),
                findings: report["findings"].as_array().map(Vec::len).unwrap_or(0),
                ready: ready_ref.is_some(),
                missing_inputs: report["missing_inputs"].as_array().map(Vec::len).unwrap_or(0),
            });
        }
        Ok(out)
    }

    // ---------------------------------------------------------------- reads

    /// Every item of a branch with its full history up to `cut` (all if `None`).
    /// An assertion that carries extraction provenance counts only once the
    /// extractor knew it (meta-assertion time ≤ cut): what the proxy could know.
    pub async fn items(&self, branch_key: &str, cut: Option<DateTime<Utc>>) -> Result<Vec<ItemRecord>> {
        let script = r#"
            search [{(?b, branch_key)}, $bk, *],
                   ?m = [{(?i, item), (?b, branch)}, "registered", ?introduced],
                   [{(?i, item_key)}, ?key, *], [{(?i, kind)}, ?kind, *], [{(?i, strength)}, ?strength, *]
            return ?m, ?i, ?key, ?kind, ?strength, ?introduced;
            search [{(?b, branch_key)}, $bk, *], [{(?i, item), (?b, branch)}, "registered", *],
                   [{(?i, statement)}, ?st, ?t]
            return ?i, ?st, ?t;
            search [{(?b, branch_key)}, $bk, *], [{(?i, item), (?b, branch)}, "registered", *],
                   [{(?i, cited_turn)}, ?cite, ?t]
            return ?i, ?cite, ?t;
            search [{(?b, branch_key)}, $bk, *], [{(?i, item), (?b, branch)}, "registered", *],
                   [{(?i, enforce)}, ?en, ?t]
            return ?i, ?en, ?t;
            search [{(?b, branch_key)}, $bk, *], ?m = [{(?i, item), (?b, branch)}, "registered", *],
                   ?a = [{(?m, posit), (?src, ascertains)}, ?c, ?t], [{(?src, name)}, ?who, *]
            return ?m, ?a, ?src, ?who, ?c, ?t;
            search [{(?b, branch_key)}, $bk, *], ?m = [{(?i, item), (?b, branch)}, "registered", *],
                   ?a = [{(?m, posit), (?src, ascertains)}, *, *],
                   [{(?a, posit), (?ext, ascertains)}, ?mc, ?mt]
            return ?a, ?mc, ?mt;
            search [{(?b, branch_key)}, $bk, *], [{(?b, branch), (?p, authority)}, "authoritative", *]
            return ?p;
            search [{(?b, branch_key)}, $bk, *], [{(?n, item), (?b, branch)}, "registered", *],
                   [{(?n, replacement), (?o, replaced)}, *, ?t], [{(?n, item_key)}, ?nk, *]
            return ?o, ?nk, ?t;
            search [{(?b, branch_key)}, $bk, *], [{(?i, item), (?b, branch)}, "registered", *],
                   [{(?i, complement), (?c, complement_of)}, *, ?t], [{(?c, item_key)}, ?ck, *]
            return ?i, ?ck, ?t;
        "#;
        let sets = self.read(script.into(), ExecutionOptions::default().bind("bk", branch_key)).await?;
        if sets.len() != 9 {
            return Err(Error::Invalid(format!("expected 9 result sets, got {}", sets.len())));
        }
        let within = |t: &str| cut.is_none_or(|c| t <= clock::format(c).as_str());
        let authorities: std::collections::HashSet<String> = (0..sets[6].rows.len()).map(|r| cell(&sets[6], r, "p").to_string()).collect();

        let mut meta: HashMap<String, (String, f64)> = HashMap::new();
        for r in 0..sets[5].rows.len() {
            let a = cell(&sets[5], r, "a").to_string();
            let t = time_text(cell(&sets[5], r, "mt"));
            let mc = certainty(cell(&sets[5], r, "mc"));
            if within(&t) && meta.get(&a).is_none_or(|(pt, _)| *pt < t) {
                meta.insert(a, (t, mc));
            }
        }

        let mut records: BTreeMap<String, ItemRecord> = BTreeMap::new();
        let mut by_member: HashMap<String, String> = HashMap::new();
        let s = &sets[0];
        for r in 0..s.rows.len() {
            let introduced = time_text(cell(s, r, "introduced"));
            if !within(&introduced) {
                continue;
            }
            let i = cell(s, r, "i").to_string();
            let key = lit_str(cell(s, r, "key"));
            by_member.insert(cell(s, r, "m").to_string(), i.clone());
            records.insert(
                i,
                ItemRecord {
                    seq: seq_of(&key).unwrap_or(0),
                    key,
                    kind: Kind::parse(&lit_str(cell(s, r, "kind"))).unwrap_or(Kind::Goal),
                    strength: Strength::parse(&lit_str(cell(s, r, "strength"))).unwrap_or(Strength::Graded),
                    statements: Vec::new(),
                    enforce: None,
                    citations: Vec::new(),
                    assertions: Vec::new(),
                    replaced_by: Vec::new(),
                    requires: Vec::new(),
                    introduced,
                },
            );
        }
        for r in 0..sets[1].rows.len() {
            let t = time_text(cell(&sets[1], r, "t"));
            if let Some(rec) = records.get_mut(cell(&sets[1], r, "i")).filter(|_| within(&t)) {
                rec.statements.push((lit_str(cell(&sets[1], r, "st")), t));
            }
        }
        for r in 0..sets[2].rows.len() {
            let t = time_text(cell(&sets[2], r, "t"));
            if let Some(rec) = records.get_mut(cell(&sets[2], r, "i")).filter(|_| within(&t)) {
                rec.citations.push(Citation { time: t, value: lit_json(cell(&sets[2], r, "cite")) });
            }
        }
        let mut enforce_t: HashMap<String, String> = HashMap::new();
        for r in 0..sets[3].rows.len() {
            let i = cell(&sets[3], r, "i").to_string();
            let t = time_text(cell(&sets[3], r, "t"));
            if !within(&t) || enforce_t.get(&i).is_some_and(|pt| *pt > t) {
                continue;
            }
            if let Some(rec) = records.get_mut(&i) {
                rec.enforce = Some(lit_json(cell(&sets[3], r, "en")));
                enforce_t.insert(i, t);
            }
        }
        for r in 0..sets[4].rows.len() {
            let s4 = &sets[4];
            let t = time_text(cell(s4, r, "t"));
            let a = cell(s4, r, "a").to_string();
            let m = meta.get(&a).cloned();
            // Bitemporal: an extracted assertion exists for us once its meta-assertion does.
            let known = match &m {
                Some(_) => true,
                None => !has_meta_anywhere(&sets[5], &a),
            };
            if !within(&t) || !known {
                continue;
            }
            let Some(i) = by_member.get(cell(s4, r, "m")) else { continue };
            if let Some(rec) = records.get_mut(i) {
                let src = cell(s4, r, "src").to_string();
                rec.assertions.push(Assertion {
                    source: lit_str(cell(s4, r, "who")),
                    authority: if authorities.contains(&src) { Authority::Authoritative } else { Authority::Advisory },
                    certainty: certainty(cell(s4, r, "c")),
                    time: t,
                    meta_certainty: m.map(|(_, c)| c),
                });
            }
        }
        for r in 0..sets[7].rows.len() {
            let t = time_text(cell(&sets[7], r, "t"));
            if let Some(rec) = records.get_mut(cell(&sets[7], r, "o")).filter(|_| within(&t)) {
                rec.replaced_by.push(lit_str(cell(&sets[7], r, "nk")));
            }
        }
        for r in 0..sets[8].rows.len() {
            let t = time_text(cell(&sets[8], r, "t"));
            if let Some(rec) = records.get_mut(cell(&sets[8], r, "i")).filter(|_| within(&t)) {
                rec.requires.push(lit_str(cell(&sets[8], r, "ck")));
            }
        }
        let mut out: Vec<ItemRecord> = records.into_values().collect();
        for rec in &mut out {
            rec.statements.sort_by(|a, b| a.1.cmp(&b.1));
            rec.citations.sort_by(|a, b| a.time.cmp(&b.time));
            rec.assertions.sort_by(|a, b| a.time.cmp(&b.time));
        }
        out.sort_by_key(|r| r.seq);
        Ok(out)
    }

    /// Everything needed to resume a session after a restart.
    pub async fn session_snapshot(&self, session_key: &str) -> Result<Option<SessionSnapshot>> {
        let sets = self
            .read(
                "search [{(?s, session_key)}, $sk, *], [{(?b, branch), (?s, session)}, ?fork, ?t], [{(?b, branch_key)}, ?bk, *]
                 return ?b, ?bk, ?fork, ?t;"
                    .into(),
                ExecutionOptions::default().bind("sk", session_key),
            )
            .await?;
        let Some(set) = sets.last().filter(|s| !s.rows.is_empty()) else { return Ok(None) };
        let set = set.clone();
        let set = &set;
        let owner_sets = self
            .read(
                "search [{(?s, session_key)}, $sk, *], [{(?s, session), (?d, developer)}, \"owner\", *], [{(?d, name)}, ?dn, *] return ?dn;".into(),
                ExecutionOptions::default().bind("sk", session_key),
            )
            .await?;
        let owner = owner_sets.last().filter(|s| !s.rows.is_empty()).map(|s| lit_str(cell(s, 0, "dn")));
        let mut branches = Vec::new();
        for r in 0..set.rows.len() {
            let key = lit_str(cell(set, r, "bk"));
            self.cache(format!("b:{key}"), cell(set, r, "b").to_string());
            branches.push((key, cell(set, r, "fork").to_string().parse::<i64>().unwrap_or(0), time_text(cell(set, r, "t"))));
        }
        let mut out = Vec::new();
        for (key, fork_common, created) in branches {
            let sets = self
                .read(
                    r#"search [{(?b, branch_key)}, $bk, *], [{(?t, turn), (?b, branch)}, ?idx, ?time], [{(?t, digest)}, ?d, *]
                       return ?idx, ?d, ?time;
                       search [{(?b, branch_key)}, $bk, *], [{(?e, epoch), (?b, branch)}, ?trigger, ?at],
                              [{(?e, epoch_key)}, ?ek, *], [{(?e, to_model)}, ?model, *], [{(?e, turn)}, ?turn, *],
                              [{(?e, extracted_through)}, ?through, *]
                       return ?e, ?ek, ?trigger, ?at, ?model, ?turn, ?through;
                       search [{(?b, branch_key)}, $bk, *], [{(?e, epoch), (?b, branch)}, *, *], [{(?e, packet)}, ?p, *]
                       return ?e, ?p;
                       search [{(?b, branch_key)}, $bk, *], [{(?b, extracted_through)}, ?w, ?t] return ?w, ?t;
                       search [{(?b, branch_key)}, $bk, *], [{(?b, served_model)}, ?sm, ?t] return ?sm, ?t;
                       search [{(?b, branch_key)}, $bk, *], [{(?e, epoch), (?b, branch)}, *, *], [{(?e, enforce)}, ?en, *]
                       return ?e, ?en;"#
                        .into(),
                    ExecutionOptions::default().bind("bk", key.as_str()),
                )
                .await?;
            let mut anchors: Vec<(usize, String, String)> = (0..sets[0].rows.len())
                .map(|r| {
                    (
                        cell(&sets[0], r, "idx").to_string().parse().unwrap_or(0),
                        lit_str(cell(&sets[0], r, "d")),
                        time_text(cell(&sets[0], r, "time")),
                    )
                })
                .collect();
            anchors.sort();
            let drops: HashMap<String, Vec<String>> = (0..sets[5].rows.len())
                .map(|r| {
                    let v = lit_json(cell(&sets[5], r, "en"));
                    let tools = v["drop_tools"]
                        .as_array()
                        .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                        .unwrap_or_default();
                    (cell(&sets[5], r, "e").to_string(), tools)
                })
                .collect();
            let packets: HashMap<String, String> =
                (0..sets[2].rows.len()).map(|r| (cell(&sets[2], r, "e").to_string(), lit_str(cell(&sets[2], r, "p")))).collect();
            let mut epochs: Vec<EpochSnapshot> = (0..sets[1].rows.len())
                .map(|r| {
                    let s = &sets[1];
                    EpochSnapshot {
                        key: lit_str(cell(s, r, "ek")),
                        trigger: lit_str(cell(s, r, "trigger")),
                        at: time_text(cell(s, r, "at")),
                        model: lit_str(cell(s, r, "model")),
                        packet: packets.get(cell(s, r, "e")).cloned(),
                        turn: cell(s, r, "turn").to_string().parse().unwrap_or(0),
                        through: cell(s, r, "through").to_string().parse().unwrap_or(-1),
                        drop_tools: drops.get(cell(s, r, "e")).cloned().unwrap_or_default(),
                    }
                })
                .collect();
            epochs.sort_by(|a, b| a.at.cmp(&b.at));
            let latest = |set: &CollectedResultSet, col: &str| -> Option<String> {
                (0..set.rows.len()).max_by_key(|r| time_text(cell(set, *r, "t"))).map(|r| cell(set, r, col).to_string())
            };
            out.push(BranchSnapshot {
                key,
                fork_common,
                created,
                anchors,
                epochs,
                watermark: latest(&sets[3], "w").and_then(|w| w.parse().ok()),
                served_model: latest(&sets[4], "sm").map(|s| lit_str_text(&s)),
            });
        }
        out.sort_by(|a, b| a.created.cmp(&b.created));
        Ok(Some(SessionSnapshot { key: session_key.to_string(), owner, branches: out }))
    }
}

pub fn root_branch(session_key: &str) -> String {
    format!("{session_key}#0")
}

fn has_meta_anywhere(meta_set: &CollectedResultSet, a: &str) -> bool {
    (0..meta_set.rows.len()).any(|r| cell(meta_set, r, "a") == a)
}

fn seq_of(item_key: &str) -> Option<u64> {
    item_key.rsplit('#').next()?.parse().ok()
}

/// Conservative read-only check for the admin endpoint: with comments and
/// string literals removed, every statement starts with `search` or `explain`
/// and no mutating keyword (`add`, `record`, `check`) appears anywhere, however
/// it is spaced (`search …\nor add posit` included).
fn is_read_only(script: &str) -> bool {
    let mut code = String::with_capacity(script.len());
    let mut chars = script.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
                code.push(' ');
            }
            // As the grammar has it: only `"` strings take escapes; quoted roles
            // (backticks) and time literals (') end at the next delimiter.
            '"' => {
                let mut escaped = false;
                for d in chars.by_ref() {
                    if escaped {
                        escaped = false;
                    } else if d == '\\' {
                        escaped = true;
                    } else if d == '"' {
                        break;
                    }
                }
                code.push(' ');
            }
            '`' | '\'' => {
                for d in chars.by_ref() {
                    if d == c {
                        break;
                    }
                }
                code.push(' ');
            }
            _ => code.push(c),
        }
    }
    let words = code.split(|c: char| !(c.is_alphanumeric() || c == '_')).map(str::to_ascii_lowercase);
    if words.clone().any(|w| matches!(w.as_str(), "add" | "record" | "check" | "strict")) {
        return false;
    }
    let statements: Vec<&str> = code.split(';').filter(|s| !s.trim().is_empty()).collect();
    !statements.is_empty()
        && statements.iter().all(|s| {
            let head = s.trim_start().to_ascii_lowercase();
            head.starts_with("search") || head.starts_with("explain")
        })
}

fn first_cell(sets: &[CollectedResultSet], col: &str) -> Result<String> {
    let set = sets.last().ok_or_else(|| Error::Invalid("no result".into()))?;
    if set.rows.is_empty() {
        return Err(Error::Invalid(format!("no row for {col}")));
    }
    Ok(cell(set, 0, col).to_string())
}

#[cfg(test)]
mod tests;
