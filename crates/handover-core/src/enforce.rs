//! Deterministic enforcement of hard constraints on tool calls (PLAN.md §10).
//!
//! Item `enforce` scopes are compiled once per register change. A decision is a
//! pure function of the normalized action and the compiled rules; there is no
//! store access per tool call.

use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::item::{Kind, RegisterItem, Status, Strength};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Read,
    Write,
    Exec,
    Network,
    Other,
}

impl Op {
    pub fn parse(s: &str) -> Option<Op> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "read" => Op::Read,
            "write" | "edit" => Op::Write,
            "exec" | "execute" | "shell" | "run" => Op::Exec,
            "network" | "net" | "fetch" => Op::Network,
            "other" => Op::Other,
            _ => return None,
        })
    }
}

/// A tool call normalized by a harness profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub tool: String,
    pub op: Option<Op>,
    pub paths: Vec<String>,
    pub command: Option<String>,
    pub urls: Vec<String>,
}

impl Action {
    /// Paths named by the action, plus every argument of an exec command (a bare
    /// `migrations` is a path to `rm -rf`), also joined to a `cd` target and to
    /// the action's own paths (a Codex `workdir`).
    fn candidate_paths(&self) -> Vec<String> {
        let mut out = self.paths.clone();
        if let Some(cmd) = &self.command {
            let toks: Vec<&str> = cmd
                .split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '>' | '<' | '(' | ')' | '"' | '\'' | '='))
                .map(str::trim)
                .filter(|t| !t.is_empty() && !t.starts_with('-'))
                .collect();
            let mut bases: Vec<String> = self.paths.clone();
            for w in toks.windows(2) {
                if w[0] == "cd" {
                    bases.push(w[1].to_string());
                }
            }
            for t in toks {
                out.push(t.to_string());
                if !t.starts_with('/') {
                    for b in &bases {
                        out.push(format!("{}/{}", b.trim_end_matches('/'), t.trim_start_matches("./")));
                    }
                }
            }
        }
        out
    }

    pub fn summary(&self) -> String {
        if let Some(c) = &self.command {
            return format!("{}: {}", self.tool, c);
        }
        if !self.paths.is_empty() {
            return format!("{}: {}", self.tool, self.paths.join(", "));
        }
        self.tool.clone()
    }
}

/// A compiled `enforce` scope: `{"ops": [...], "paths": [globs], "commands": [regexes], "tools": [names]}`.
#[derive(Clone, Debug)]
pub struct Scope {
    ops: Vec<Op>,
    paths: Option<GlobSet>,
    commands: Vec<Regex>,
    tools: Vec<String>,
    urls: Vec<Regex>,
}

impl Scope {
    pub fn compile(v: &Value) -> Result<Scope, String> {
        let list = |key: &str| -> Vec<String> {
            match v.get(key) {
                Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
                Some(Value::String(s)) => vec![s.clone()],
                _ => Vec::new(),
            }
        };
        if !v.is_object() {
            return Err("enforce scope must be an object".into());
        }
        let mut ops = Vec::new();
        for o in list("ops") {
            ops.push(Op::parse(&o).ok_or_else(|| format!("unknown op {o:?}"))?);
        }
        let globs = list("paths");
        let paths = if globs.is_empty() {
            None
        } else {
            let mut b = GlobSetBuilder::new();
            for g in &globs {
                b.add(Glob::new(g).map_err(|e| format!("bad glob {g:?}: {e}"))?);
                // `dir/`, `dir` and `dir/**` cover the directory itself and everything below.
                let base = g.strip_suffix("/**").unwrap_or(g).trim_end_matches('/');
                if !base.contains('*') && !base.is_empty() {
                    b.add(Glob::new(base).map_err(|e| e.to_string())?);
                    b.add(Glob::new(&format!("{base}/**")).map_err(|e| e.to_string())?);
                }
            }
            Some(b.build().map_err(|e| e.to_string())?)
        };
        let mut commands = Vec::new();
        for c in list("commands") {
            commands.push(Regex::new(&c).map_err(|e| format!("bad regex {c:?}: {e}"))?);
        }
        let mut urls = Vec::new();
        for u in list("urls") {
            urls.push(Regex::new(&u).map_err(|e| format!("bad regex {u:?}: {e}"))?);
        }
        let tools = list("tools");
        if ops.is_empty() && paths.is_none() && commands.is_empty() && tools.is_empty() && urls.is_empty() {
            return Err("enforce scope selects nothing".into());
        }
        Ok(Scope { ops, paths, commands, tools, urls })
    }

    /// An action matches when its op is listed (or no ops are listed) and at least
    /// one selector matches (or there are no selectors).
    pub fn matches(&self, a: &Action) -> bool {
        // A shell command that writes files is a write too (`sed -i`, `rm`, `>` …).
        let exec_writes = a.op == Some(Op::Exec) && self.ops.contains(&Op::Write) && a.command.as_deref().is_some_and(writes_files);
        if !self.ops.is_empty() && !a.op.is_some_and(|op| self.ops.contains(&op)) && !exec_writes {
            return false;
        }
        let has_selectors = self.paths.is_some() || !self.commands.is_empty() || !self.tools.is_empty() || !self.urls.is_empty();
        if !has_selectors {
            return true;
        }
        if self.tools.iter().any(|t| t.eq_ignore_ascii_case(&a.tool)) {
            return true;
        }
        if let Some(cmd) = &a.command
            && self.commands.iter().any(|r| r.is_match(cmd))
        {
            return true;
        }
        if self.urls.iter().any(|r| a.urls.iter().any(|u| r.is_match(u))) {
            return true;
        }
        if let Some(set) = &self.paths {
            for p in a.candidate_paths() {
                if path_suffixes(&p).any(|s| set.is_match(s)) {
                    return true;
                }
            }
        }
        false
    }
}

/// Does a shell command obviously modify files? (Conservative heuristics; the
/// command's path-like tokens are then matched against write scopes.)
fn writes_files(cmd: &str) -> bool {
    use std::sync::OnceLock;
    static FD: OnceLock<Regex> = OnceLock::new();
    // `2>&1`, `>&2`, `&>/dev/null`, `2>/dev/null` move output, they write no file.
    let fd = FD.get_or_init(|| Regex::new(r"(\d*>>?&\d+|&>>?\s*/dev/null|\d*>>?\s*/dev/null)").expect("valid"));
    let cleaned = fd.replace_all(cmd, " ");
    let c = format!(" {} ", cleaned.replace(['\n', '\t', ';', '&', '|', '(', ')'], " "));
    const HINTS: [&str; 25] = [
        " apply_patch ",
        " tee ",
        " rm ",
        " mv ",
        " cp ",
        " truncate ",
        " touch ",
        " patch ",
        " git checkout ",
        " git restore ",
        " git rm ",
        " git mv ",
        " git apply ",
        " chmod ",
        " dd ",
        " install ",
        " ln ",
        " mkdir ",
        " rmdir ",
        " unlink ",
        " shred ",
        " -delete ",
        " rsync ",
        " tar -x",
        " unzip ",
    ];
    if HINTS.iter().any(|h| c.contains(h)) || cleaned.contains('>') {
        return true;
    }
    // `sed -i`, `sed -E -i`, `perl -pi -e` …: an in-place flag anywhere after sed/perl.
    let toks: Vec<&str> = c.split_whitespace().collect();
    toks.iter().any(|t| *t == "sed" || *t == "perl")
        && toks
            .iter()
            .any(|t| t.starts_with('-') && !t.starts_with("--") && t[1..].chars().all(|ch| ch.is_ascii_alphabetic()) && t.contains('i'))
}

/// `/a/b/c` → `/a/b/c`, `a/b/c`, `b/c`, `c`: relative globs match absolute paths.
fn path_suffixes(p: &str) -> impl Iterator<Item = &str> {
    let p = p.strip_prefix("./").unwrap_or(p);
    std::iter::once(p).chain(p.match_indices('/').map(move |(i, _)| &p[i + 1..]).filter(|s| !s.is_empty()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Forward,
    Warn,
    Hold,
    Block,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Forward => "forwarded",
            Decision::Warn => "warned",
            Decision::Hold => "held",
            Decision::Block => "blocked",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub item_key: String,
    pub handle: String,
    pub statement: String,
    pub effect: Decision,
    scope: Scope,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Matched {
    pub item_key: String,
    pub handle: String,
    pub statement: String,
    pub effect: Decision,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Verdict {
    pub decision: Decision,
    pub matched: Vec<Matched>,
}

impl Verdict {
    /// The assistant text that replaces a blocked or held call.
    pub fn message(&self, action: &Action) -> Option<String> {
        let top = self.matched.iter().find(|m| m.effect == self.decision)?;
        match self.decision {
            Decision::Block => Some(format!(
                "Blocked by session constraint {}: {} Ask the developer if this has changed. (Blocked call: {})",
                top.handle,
                with_period(&top.statement),
                action.summary()
            )),
            Decision::Hold => Some(format!(
                "Before running {}: session constraint {} may apply (\"{}\"), but it is not confirmed. Is it still in force?",
                action.summary(),
                top.handle,
                top.statement.trim_end_matches('.')
            )),
            _ => None,
        }
    }
}

fn with_period(s: &str) -> String {
    let s = s.trim();
    if s.ends_with(['.', '!', '?']) { s.to_string() } else { format!("{s}.") }
}

#[derive(Clone, Debug)]
pub struct EnforcePolicy {
    /// Kinds whose scopes restrict actions (prohibition, role_boundary).
    pub enforce_kinds: Vec<Kind>,
    /// Effect of an uncertain or disputed hard item (`Hold` by default).
    pub on_uncertain_hard: Decision,
    /// Effect of an active graded item (`Warn` by default).
    pub on_graded: Decision,
}

impl Default for EnforcePolicy {
    fn default() -> Self {
        EnforcePolicy {
            enforce_kinds: vec![Kind::Prohibition, Kind::RoleBoundary],
            on_uncertain_hard: Decision::Hold,
            on_graded: Decision::Warn,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Enforcer {
    pub rules: Vec<Rule>,
    /// Items whose `enforce` failed to compile (prompt-only).
    pub uncompiled: Vec<(String, String)>,
}

impl Enforcer {
    pub fn from_register(items: &[RegisterItem], policy: &EnforcePolicy) -> Enforcer {
        let mut e = Enforcer::default();
        for i in items {
            let Some(scope_json) = &i.record.enforce else { continue };
            let effect = if policy.enforce_kinds.contains(&i.kind()) {
                match (i.strength(), i.status) {
                    (Strength::Hard, Status::Active) => Some(Decision::Block),
                    (Strength::Hard, Status::Uncertain | Status::Disputed) => Some(policy.on_uncertain_hard),
                    (Strength::Graded, Status::Active) => Some(policy.on_graded),
                    _ => None,
                }
            } else if i.kind() == Kind::Permission && i.status == Status::Revoked {
                // A revoked permission forbids what it used to allow (the paper's coding case).
                Some(if i.is_hard() { Decision::Block } else { policy.on_graded })
            } else if i.kind() == Kind::Permission
                && matches!(i.status, Status::Uncertain | Status::Disputed)
                && i.record.assertions.iter().any(|a| a.authority == crate::item::Authority::Authoritative && a.certainty <= 0.0)
            {
                // A revoked permission that is only uncertainly back: ask first.
                Some(if i.is_hard() { policy.on_uncertain_hard } else { policy.on_graded })
            } else {
                None
            };
            let Some(effect) = effect else { continue };
            match Scope::compile(scope_json) {
                Ok(scope) => {
                    e.rules.push(Rule { item_key: i.key().to_string(), handle: i.handle(), statement: i.statement.clone(), effect, scope })
                }
                Err(err) => e.uncompiled.push((i.key().to_string(), err)),
            }
        }
        e
    }

    pub fn decide(&self, a: &Action) -> Verdict {
        let matched: Vec<Matched> = self
            .rules
            .iter()
            .filter(|r| r.scope.matches(a))
            .map(|r| Matched { item_key: r.item_key.clone(), handle: r.handle.clone(), statement: r.statement.clone(), effect: r.effect })
            .collect();
        let decision = matched.iter().map(|m| m.effect).max().unwrap_or(Decision::Forward);
        Verdict { decision, matched }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn exec(cmd: &str) -> Action {
        Action { tool: "Bash".into(), op: Some(Op::Exec), command: Some(cmd.into()), ..Default::default() }
    }

    fn write(path: &str) -> Action {
        Action { tool: "Edit".into(), op: Some(Op::Write), paths: vec![path.into()], ..Default::default() }
    }

    #[test]
    fn scopes() {
        let deploy = Scope::compile(&json!({"ops": ["exec"], "commands": ["deploy.*prod"]})).unwrap();
        assert!(deploy.matches(&exec("make deploy-prod")));
        assert!(!deploy.matches(&exec("make test")));
        assert!(!deploy.matches(&write("deploy-prod.sh")));

        let mig = Scope::compile(&json!({"ops": ["write", "exec"], "paths": ["migrations/"]})).unwrap();
        assert!(mig.matches(&write("/home/dev/repo/migrations/002.sql")));
        assert!(mig.matches(&write("migrations/002.sql")));
        assert!(mig.matches(&exec("rm -rf migrations/002.sql")));
        assert!(!mig.matches(&write("src/migrations.rs")));
        assert!(!mig.matches(&Action {
            tool: "Read".into(),
            op: Some(Op::Read),
            paths: vec!["migrations/1.sql".into()],
            ..Default::default()
        }));

        let tool = Scope::compile(&json!({"tools": ["WebFetch"]})).unwrap();
        assert!(tool.matches(&Action { tool: "webfetch".into(), op: Some(Op::Network), ..Default::default() }));
        assert!(Scope::compile(&json!({})).is_err());
        assert!(Scope::compile(&json!({"commands": ["("]})).is_err());
    }

    #[test]
    fn write_scopes_catch_shell_writes() {
        let mig = Scope::compile(&json!({"ops": ["write"], "paths": ["migrations/"]})).unwrap();
        assert!(mig.matches(&exec("sed -i s/a/b/ migrations/002.sql")));
        assert!(mig.matches(&exec("echo x > migrations/002.sql")));
        assert!(mig.matches(&exec("rm migrations/002.sql")));
        assert!(!mig.matches(&exec("cat migrations/002.sql")), "reading is not writing");
        assert!(!mig.matches(&exec("rm src/x.rs")));
    }

    /// Review finding: a revoked permission that is only uncertainly back holds.
    #[test]
    fn an_uncertainly_restored_permission_holds() {
        use crate::item::{Assertion, Authority, ItemRecord};
        let a = |c: f64, t: &str, meta: f64| Assertion {
            source: "developer:1".into(),
            authority: Authority::Authoritative,
            certainty: c,
            time: format!("2026-11-20 {t}"),
            meta_certainty: Some(meta),
        };
        let rec = ItemRecord {
            key: "s#1".into(),
            seq: 1,
            kind: Kind::Permission,
            strength: Strength::Hard,
            statements: vec![("May deploy to production".into(), "t".into())],
            enforce: Some(json!({"ops": ["exec"], "commands": ["deploy.*prod"]})),
            citations: vec![],
            assertions: vec![a(1.0, "10:00", 0.9), a(0.0, "10:30", 0.9), a(1.0, "11:00", 0.3)],
            replaced_by: vec![],
            requires: vec![],
            introduced: "t".into(),
        };
        let items = crate::status::derive(&[rec], &Default::default());
        assert_eq!(items[0].status, Status::Uncertain);
        let e = Enforcer::from_register(&items, &EnforcePolicy::default());
        assert_eq!(e.decide(&exec("make deploy-prod")).decision, Decision::Hold);
    }

    /// Third review: common shell writes are caught, redirections of output are not writes.
    #[test]
    fn shell_writes_and_reads() {
        let mig = Scope::compile(&json!({"ops": ["write"], "paths": ["migrations/"]})).unwrap();
        for w in [
            "rm -rf migrations",
            "rm -rf ./migrations",
            "mv migrations /tmp/old",
            "git checkout -- migrations",
            "cd migrations && rm 002.sql",
            "find migrations -name '*.sql' -delete",
            "sed -E -i s/a/b/ migrations/002.sql",
            "perl -pi -e s/a/b/ migrations/002.sql",
        ] {
            assert!(mig.matches(&exec(w)), "{w}");
        }
        let in_workdir = Action {
            tool: "shell".into(),
            op: Some(Op::Exec),
            command: Some("rm 002.sql".into()),
            paths: vec!["/repo/migrations".into()],
            ..Default::default()
        };
        assert!(mig.matches(&in_workdir));
        for r in [
            "cat migrations/002.sql 2>&1",
            "grep -r x migrations 2>/dev/null",
            "ls migrations/ 2>&1 | head",
            "sed -n 1,5p migrations/002.sql",
        ] {
            assert!(!mig.matches(&exec(r)), "{r}");
        }
        let star = Scope::compile(&json!({"ops": ["write"], "paths": ["migrations/**"]})).unwrap();
        assert!(star.matches(&exec("rm -rf migrations")));
    }
}
