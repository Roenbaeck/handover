//! Q3: auditing handover duties with saved Traqula constraints.
//!
//! Questions:
//! - Do the audit bodies validate as saved constraints (`as of ?at`, `not exists`)?
//! - Which verdicts come back without and with a readiness declaration?
//! - Can readiness be scoped per session (parameterized declaration)?
//! - Does `record by` store the report as an audit posit?
//! - Are reports compact (digest + count) unless the full manifest is requested?
//!
//! The bodies use the shape the Q4 benchmark recommends: anchored on the
//! `session_key` literal, with the session owner as the authoritative positor.
//! (In 0.3.2-beta.7 a `where ?s = $session` anchor is lost once the clause has
//! other conjuncts, and `($s, session)` in an appearance does not steer the plan.)
//!
//! Scenario: session sess-1. Hard permission P granted 10:00, revoked 10:30.
//! Hard prohibition H from 10:50. Epoch E1 at 10:15 (P active), E2 at 12:00 (P revoked, H active).

use positorium::{CollectedResultSet, Database, ExecutionOptions, ExecutionParameter};
use positorium_spike::ROLES;
use serde_json::{Value, json};

const C1_CITES_ACTIVE: &str = r#"add constraint +c1 "Handoff packets cite every active hard item" {
    search [{(?s, session_key)}, $session_key, *],
           ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", ?introduced],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?s, session), (?src, developer)}, "owner", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, *, *] }
    where ?introduced < ?at and ?c > 0%
    return ?e, ?i, ?ep, ?m;
};"#;

const C2_CITES_REVOKED: &str = r#"add constraint +c2 "Revoked hard items are cited as revoked" {
    search [{(?s, session_key)}, $session_key, *],
           ?ep = [{(?e, epoch), (?s, session)}, *, ?at],
           ?m = [{(?i, item), (?s, session)}, "registered", *],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, ?revoked_at] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?earlier, ?granted_at],
           [{(?s, session), (?src, developer)}, "owner", *]
    not exists { [{(?e, packet_of), (?i, packet_item)}, "revoked", *] }
    where ?c <= 0% and ?earlier > 0% and ?granted_at < ?revoked_at
    return ?e, ?i, ?ep, ?m;
};"#;

const C3_NO_FORWARDED_MATCH: &str = r#"add constraint +c3 "No forwarded action matched an active hard item" {
    search [{(?s, session_key)}, $session_key, *],
           ?act = [{(?x, action), (?s, session)}, *, ?at],
           [{(?x, decision)}, "forwarded", *],
           [{(?x, action), (?i, matched_item)}, "match", *],
           ?m = [{(?i, item), (?s, session)}, "registered", *],
           [{(?i, strength)}, "hard", *] as of ?at,
           [{(?m, posit), (?src, ascertains)}, ?c, *] as of ?at,
           [{(?s, session), (?src, developer)}, "owner", *]
    where ?c > 0%
    return ?x, ?i, ?act, ?m;
};"#;

fn scenario() -> Database {
    let db = Database::memory().unwrap();
    db.execute(ROLES).unwrap();
    db.execute(
        r#"
        add posit [{(+dev, name)}, "developer:42", '2026-11-20'],
                  [{(+proxy, name)}, "handover", '2026-11-20'],
                  [{(+s, session_key)}, "sess-1", '2026-11-20'],
                  [{(s, session), (dev, developer)}, "owner", '2026-11-20'];
        add posit +mp [{(+p, item), (s, session)}, "registered", '2026-11-20 10:00:00']
                    and assert by dev with 100% at '2026-11-20 10:00:00',
                  [{(p, item_key)}, "P", '2026-11-20 10:00:00'],
                  [{(p, strength)}, "hard", '2026-11-20 10:00:00'];
        add posit [{(mp, posit), (dev, ascertains)}, 0%, '2026-11-20 10:30:00'];
        add posit +mh [{(+h, item), (s, session)}, "registered", '2026-11-20 10:50:00']
                    and assert by dev with 100% at '2026-11-20 10:50:00',
                  [{(h, item_key)}, "H", '2026-11-20 10:50:00'],
                  [{(h, strength)}, "hard", '2026-11-20 10:50:00'];
        add posit [{(+e1, epoch), (s, session)}, "model_change", '2026-11-20 10:15:00'],
                  [{(+e2, epoch), (s, session)}, "model_change", '2026-11-20 12:00:00'],
                  [{(e2, packet_of), (h, packet_item)}, "hard-active", '2026-11-20 12:00:00'];
        "#,
    )
    .unwrap();
    db
}

fn report(set: &CollectedResultSet) -> Value {
    serde_json::from_str(&set.cell(0, "report").unwrap().text).unwrap()
}

fn summary(label: &str, set: &CollectedResultSet) -> (String, Vec<(String, String)>) {
    let r = report(set);
    let findings: Vec<(String, String)> = r["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| (f["subject"]["text"].as_str().unwrap_or("?").to_string(), f["status"].as_str().unwrap().to_string()))
        .collect();
    println!(
        "{label}: verdict={} findings={findings:?} report_bytes={} keys={:?}",
        r["verdict"],
        set.cell(0, "report").unwrap().text.len(),
        r.as_object().unwrap().keys().collect::<Vec<_>>()
    );
    (r["verdict"].as_str().unwrap().to_string(), findings)
}

/// Readiness scope for a check whose only parameter is `$session_key`. Must equal
/// `{"ready": <the check's scope>}` exactly; see `pending_deliveries` in Positorium.
fn scope_for(session_key: &str) -> String {
    let token = serde_json::Value::String(session_key.into()).to_string();
    json!({"ready": {"parameters": {"session_key": {"kind": "literal", "text": token}}, "clock": null}}).to_string()
}

/// Record an extraction run as the delivery, link it from a manifest entry, and
/// declare the revision ready for this session, all in one command. Returns the
/// declaration reference. The revision goes straight into the appearance as `($rev, policy)`.
fn declare_ready(db: &Database, revision: &ExecutionParameter, session_key: &str) -> ExecutionParameter {
    let sets = db
        .execute_with_options(
            r#"
            strict mutations;
            search [{(?s, session_key)}, $session_key, *] or add posit [{(+s, session_key)}, $session_key, @NOW];
            add posit +run [{(+r, extraction), (s, session)}, {"through": 41}, @NOW],
                      +entry [{(run, delivery)}, "included", @NOW],
                      [{($rev, policy), (entry, manifest)}, $scope, @NOW];
            search ?ready = [{($rev, policy), (?entry, manifest)}, *, *] return ?ready;
            "#,
            ExecutionOptions::default()
                .bind("rev", revision.clone())
                .bind("session_key", session_key)
                .bind("scope", ExecutionParameter::Literal { text: scope_for(session_key) }),
        )
        .unwrap();
    let set = sets.last().unwrap();
    let ready = set.cell(set.rows.len() - 1, "ready").unwrap().text.clone();
    ExecutionParameter::Posit { text: format!("{}/{}", db.store_id(), ready) }
}

fn save_constraint(db: &Database, body: &str) -> ExecutionParameter {
    db.execute_one(body).unwrap().reference(0, "revision").unwrap()
}

fn check(db: &Database, rev: &ExecutionParameter, key: &str, ready: Option<&ExecutionParameter>) -> CollectedResultSet {
    let mut opts = ExecutionOptions::default().bind("rev", rev.clone()).bind("session_key", key);
    let script = match ready {
        Some(r) => {
            opts = opts.bind("ready", r.clone());
            "check constraint $rev with readiness $ready;"
        }
        None => "check constraint $rev;",
    };
    db.execute_one_with_options(script, opts).unwrap()
}

#[test]
fn c1_needs_readiness_to_turn_a_missing_citation_into_a_violation() {
    let db = scenario();
    let rev = save_constraint(&db, C1_CITES_ACTIVE);

    let (verdict, findings) = summary("C1 no readiness", &check(&db, &rev, "sess-1", None));
    assert_eq!(verdict, "indeterminate");
    assert_eq!(findings.len(), 1, "E1 does not cite P");
    assert_eq!(findings[0].1, "indeterminate");

    let ready = declare_ready(&db, &rev, "sess-1");
    let (verdict, findings) = summary("C1 with readiness", &check(&db, &rev, "sess-1", Some(&ready)));
    assert_eq!(verdict, "violated");
    assert_eq!(findings[0].1, "violated");

    // The proxy fixes E1 by citing P. `{(?e, epoch)}` would match only unary sets;
    // the open set `{(?e, epoch), ...}` matches the stored {epoch, session}.
    db.execute(
        r#"strict mutations;
           search [{(?e, epoch), ...}, *, '2026-11-20 10:15:00'] or add posit [{(+e, epoch)}, "x", @NOW];
           search [{(?p, item_key)}, "P", *] or add posit [{(+p, item_key)}, "P", @NOW];
           add posit [{(e, packet_of), (p, packet_item)}, "hard-active", '2026-11-20 10:15:00'];"#,
    )
    .unwrap();
    let (verdict, findings) = summary("C1 after fix", &check(&db, &rev, "sess-1", Some(&ready)));
    assert!(findings.is_empty());
    assert_eq!(verdict, "satisfied");
}

#[test]
fn readiness_is_bound_to_the_session_parameter() {
    let db = scenario();
    let rev = save_constraint(&db, C1_CITES_ACTIVE);
    let ready = declare_ready(&db, &rev, "sess-1");
    let err = db
        .execute_one_with_options(
            "check constraint $rev with readiness $ready;",
            ExecutionOptions::default().bind("rev", rev).bind("session_key", "sess-2").bind("ready", ready),
        )
        .unwrap_err();
    println!("readiness for another session -> {err}");
    assert!(err.to_string().contains("does not cover the check parameters"));
}

#[test]
fn c2_finds_revoked_items_not_cited_as_revoked() {
    let db = scenario();
    let rev = save_constraint(&db, C2_CITES_REVOKED);
    let ready = declare_ready(&db, &rev, "sess-1");
    let (verdict, findings) = summary("C2", &check(&db, &rev, "sess-1", Some(&ready)));
    assert_eq!(verdict, "violated");
    assert_eq!(findings.len(), 1, "E2 does not cite P as revoked; E1 predates the revocation");
}

#[test]
fn c3_flags_a_forwarded_action_that_matched_an_active_hard_item() {
    let db = scenario();
    db.execute(
        r#"
        search [{(?s, session_key)}, "sess-1", *] or add posit [{(+s, session_key)}, "sess-1", @NOW];
        search [{(?h, item_key)}, "H", *] or add posit [{(+h, item_key)}, "H", @NOW];
        add posit [{(+x, action), (s, session)}, {"turn":44,"op":"write","paths":["migrations/002.sql"]}, '2026-11-20 11:00:00'],
                  [{(x, decision)}, "forwarded", '2026-11-20 11:00:00'],
                  [{(x, action), (h, matched_item)}, "match", '2026-11-20 11:00:00'],
                  [{(+y, action), (s, session)}, {"turn":44,"op":"write","paths":["migrations/003.sql"]}, '2026-11-20 11:00:01'],
                  [{(y, decision)}, "blocked", '2026-11-20 11:00:01'],
                  [{(y, action), (h, matched_item)}, "match", '2026-11-20 11:00:01'];
        "#,
    )
    .unwrap();
    let rev = save_constraint(&db, C3_NO_FORWARDED_MATCH);
    let ready = declare_ready(&db, &rev, "sess-1");
    let (verdict, findings) = summary("C3", &check(&db, &rev, "sess-1", Some(&ready)));
    assert_eq!(verdict, "violated");
    assert_eq!(findings.len(), 1, "only the forwarded action, not the blocked one");
}

#[test]
fn record_by_saves_the_report_as_an_audit_posit() {
    let db = scenario();
    let rev = save_constraint(&db, C1_CITES_ACTIVE);
    let ready = declare_ready(&db, &rev, "sess-1");
    let sets = db
        .execute_with_options(
            r#"search [{(?proxy, name)}, "handover", *] or add posit [{(+proxy, name)}, "handover", @NOW];
               check constraint $rev with readiness $ready record by proxy;"#,
            ExecutionOptions::default().bind("rev", rev).bind("session_key", "sess-1").bind("ready", ready),
        )
        .unwrap();
    let set = sets.last().unwrap();
    assert_eq!(set.columns, ["policy", "revision", "store", "report", "audit"]);
    let stored = db
        .execute_one(
            r#"search ?a = [{(?rev, posit), (?proxy, ascertains)}, ?c, *], [{(?proxy, name)}, "handover", *]
               return ?a, ?c;"#,
        )
        .unwrap();
    assert_eq!(stored.rows.len(), 1, "the audit report is ascertained by the evaluator");
    assert_eq!(stored.rows[0][1].text, "100%");
}

#[test]
fn reports_carry_a_digest_by_default_and_the_full_manifest_on_request() {
    let db = scenario();
    let rev = save_constraint(&db, C1_CITES_ACTIVE);
    let ready = declare_ready(&db, &rev, "sess-1");
    let compact = report(&check(&db, &rev, "sess-1", Some(&ready)));
    println!("compact snapshot: {}", compact["snapshot"]);
    assert!(compact["snapshot"]["digest"].is_string() || compact["snapshot"]["digest"].is_number());
    assert!(compact["snapshot"]["count"].as_u64().unwrap() > 0);
    assert!(compact["snapshot"].get("posits").is_none(), "no manifest by default");
    assert!(!compact["read_posits"].as_array().unwrap().is_empty());

    let full = db
        .execute_one_with_options(
            "check constraint $rev with readiness $ready with full manifest;",
            ExecutionOptions::default().bind("rev", rev).bind("session_key", "sess-1").bind("ready", ready),
        )
        .unwrap();
    let full = report(&full);
    let posits = full["snapshot"]["posits"].as_array().unwrap();
    assert_eq!(posits.len() as u64, full["snapshot"]["count"].as_u64().unwrap());
    assert_eq!(full["snapshot"]["digest"], compact["snapshot"]["digest"]);
}
