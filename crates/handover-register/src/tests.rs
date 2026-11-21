use chrono::{Duration, TimeZone};
use handover_core::item::Status;
use handover_core::status::{StatusPolicy, derive};
use handover_core::verify::Source;

use super::*;

fn t(min: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 20, 10, 0, 0).unwrap() + Duration::minutes(min)
}

const DEV: &str = "developer:42";
const HARNESS: &str = "harness:generic";

fn auth() -> Vec<String> {
    vec![DEV.to_string(), HARNESS.to_string()]
}

fn add(kind: Kind, strength: Strength, statement: &str, enforce: Option<Value>, turn: usize) -> VerifiedDelta {
    VerifiedDelta {
        op: DeltaOp::Add,
        target: Target::New,
        replaces: None,
        kind,
        strength,
        statement: Some(statement.into()),
        enforce,
        source: Source::Developer,
        certainty: 1.0,
        meta_certainty: 0.9,
        citation: json!({"turn": turn, "quote": statement, "op": "add", "verified": true}),
        requires: None,
        notes: vec![],
    }
}

fn revoke(item_key: &str, turn: usize) -> VerifiedDelta {
    VerifiedDelta {
        op: DeltaOp::Revoke,
        target: Target::Existing(item_key.into()),
        replaces: None,
        kind: Kind::Permission,
        strength: Strength::Hard,
        statement: None,
        enforce: None,
        source: Source::Developer,
        certainty: 0.0,
        meta_certainty: 0.9,
        citation: json!({"turn": turn, "quote": "no more", "op": "revoke", "verified": true}),
        requires: None,
        notes: vec![],
    }
}

fn timed(delta: VerifiedDelta, positor: &str, at: DateTime<Utc>) -> TimedDelta {
    TimedDelta { delta, positor: positor.into(), at }
}

async fn coding_case(reg: &Register) -> (String, Vec<String>) {
    let b = reg.ensure_session("sess-1", DEV, "generic", &auth(), None, t(0)).await.unwrap();
    let keys = reg
        .apply(
            "sess-1",
            &b,
            &[
                timed(
                    add(
                        Kind::Prohibition,
                        Strength::Hard,
                        "Do not deploy to production",
                        Some(json!({"ops": ["exec"], "commands": ["deploy.*prod"]})),
                        1,
                    ),
                    DEV,
                    t(1),
                ),
                timed(
                    add(
                        Kind::Permission,
                        Strength::Hard,
                        "May run database migrations",
                        Some(json!({"ops": ["exec"], "commands": ["migrate"]})),
                        1,
                    ),
                    DEV,
                    t(1),
                ),
                timed(add(Kind::Goal, Strength::Graded, "Fix duplicate retries", None, 1), DEV, t(1)),
            ],
            Some("extractor:qwen"),
            Some(2),
            Some(json!({"through": 2})),
            t(2),
        )
        .await
        .unwrap();
    (b, keys)
}

#[tokio::test]
async fn writes_read_back_as_items_with_history() {
    let reg = Register::open(None).unwrap();
    let (b, keys) = coding_case(&reg).await;
    assert_eq!(keys, vec!["sess-1#1", "sess-1#2", "sess-1#3"]);
    reg.apply("sess-1", &b, &[timed(revoke("sess-1#2", 5), DEV, t(5))], Some("extractor:qwen"), Some(6), None, t(6)).await.unwrap();

    let items = reg.items(&b, None).await.unwrap();
    assert_eq!(items.len(), 3);
    assert_eq!(items[0].statements[0].0, "Do not deploy to production");
    assert_eq!(items[0].enforce, Some(json!({"commands": ["deploy.*prod"], "ops": ["exec"]})));
    assert_eq!(items[0].assertions[0].authority, Authority::Authoritative);
    assert_eq!(items[0].assertions[0].meta_certainty, Some(0.9));
    assert_eq!(items[1].assertions.len(), 2);
    assert_eq!(items[1].citations.last().unwrap().op(), Some("revoke"));
    let derived = derive(&items, &StatusPolicy::default());
    let st: Vec<Status> = derived.iter().map(|i| i.status).collect();
    assert_eq!(st, vec![Status::Active, Status::Revoked, Status::Active]);

    // At a cut before the revocation was extracted, P2 was still active.
    let before = derive(&reg.items(&b, Some(t(5) + Duration::seconds(30))).await.unwrap(), &StatusPolicy::default());
    assert_eq!(before[1].status, Status::Active, "the revocation was not known until its meta-assertion at t(6)");
    // Before anything was extracted the register was empty.
    assert!(reg.items(&b, Some(t(0))).await.unwrap().is_empty());
}

#[tokio::test]
async fn advisory_sources_are_not_authoritative() {
    let reg = Register::open(None).unwrap();
    let b = reg.ensure_session("s2", DEV, "generic", &auth(), None, t(0)).await.unwrap();
    let mut d = add(Kind::Commitment, Strength::Graded, "Rewrite as a queue", None, 2);
    d.source = Source::Model;
    d.certainty = 0.8;
    reg.apply("s2", &b, &[timed(d, "model:qwen", t(1))], Some("extractor:qwen"), None, None, t(2)).await.unwrap();
    let items = reg.items(&b, None).await.unwrap();
    assert_eq!(items[0].assertions[0].authority, Authority::Advisory);
    assert_eq!(derive(&items, &StatusPolicy::default())[0].status, Status::Uncertain);
}

#[tokio::test]
async fn audits_need_readiness_and_find_missing_citations() {
    let reg = Register::open(None).unwrap();
    let (b, keys) = coding_case(&reg).await;
    reg.apply("sess-1", &b, &[timed(revoke(&keys[1], 5), DEV, t(5))], Some("extractor:qwen"), Some(6), None, t(6)).await.unwrap();
    // Epoch 1 cites P1 but forgets that P2 was revoked.
    reg.open_epoch(
        &b,
        &EpochWrite {
            key: "sess-1#e1".into(),
            trigger: "model_change".into(),
            to_model: "model-b".into(),
            packet: Some("<handover-register>…</handover-register>".into()),
            citations: vec![(keys[0].clone(), "hard-active".into())],
            turn: 7,
            through: Some(6),
            ..Default::default()
        },
        t(7),
    )
    .await
    .unwrap();

    let unready = reg.audit(&b, false, false).await.unwrap();
    assert!(unready.iter().all(|a| a.verdict == "indeterminate"), "{unready:?}");

    reg.declare_ready(&b, t(8)).await.unwrap();
    let ready = reg.audit(&b, true, false).await.unwrap();
    let v: Vec<(&str, &str)> = ready.iter().map(|a| (a.name.as_str(), a.verdict.as_str())).collect();
    assert_eq!(v[0].1, "satisfied", "C1: the only active hard item is cited");
    assert_eq!(v[1].1, "violated", "C2: revoked P2 is not cited as revoked");
    assert_eq!(v[2].1, "satisfied", "C3: no actions yet");
    assert_eq!(v[3].1, "satisfied", "C4: no requires pairs");
    assert_eq!(v[4].1, "satisfied", "C5: the revocation of P2 is cited (and P2 is a permission anyway)");

    // A forwarded action that matched the active prohibition violates C3.
    reg.record_action(&b, &json!({"tool": "Bash", "command": "make deploy-prod"}), "forwarded", &[keys[0].clone()], t(9)).await.unwrap();
    reg.record_action(&b, &json!({"tool": "Bash", "command": "make deploy-prod"}), "blocked", &[keys[0].clone()], t(10)).await.unwrap();
    let after = reg.audit(&b, true, true).await.unwrap();
    assert_eq!(after[2].verdict, "violated");
    assert_eq!(after[2].findings, 1);
}

#[tokio::test]
async fn branches_inherit_items_and_diverge() {
    let reg = Register::open(None).unwrap();
    let (root, keys) = coding_case(&reg).await;
    let parent = derive(&reg.items(&root, None).await.unwrap(), &StatusPolicy::default());
    let inherited: Vec<Inherited> =
        parent.iter().map(|i| Inherited { item_key: i.key().to_string(), positions: vec![(DEV.to_string(), 1.0, Some(0.9))] }).collect();
    reg.create_branch("sess-1", &root, "sess-1#b1", 3, &auth(), &inherited, Some(2), t(20)).await.unwrap();
    assert_eq!(reg.session_snapshot("sess-1").await.unwrap().unwrap().branches[1].watermark, Some(2), "fork watermark persists");
    // Revoke in the child only.
    reg.apply("sess-1", "sess-1#b1", &[timed(revoke(&keys[0], 4), DEV, t(21))], None, None, None, t(21)).await.unwrap();
    let child = derive(&reg.items("sess-1#b1", None).await.unwrap(), &StatusPolicy::default());
    let root_now = derive(&reg.items(&root, None).await.unwrap(), &StatusPolicy::default());
    assert_eq!(child.len(), 3);
    assert_eq!(child[0].status, Status::Revoked);
    assert_eq!(root_now[0].status, Status::Active, "the parent branch is unaffected");
    // New items continue the session-wide sequence.
    let k = reg
        .apply("sess-1", "sess-1#b1", &[timed(add(Kind::Goal, Strength::Graded, "Ship it", None, 5), DEV, t(22))], None, None, None, t(22))
        .await
        .unwrap();
    assert_eq!(k, vec!["sess-1#4"]);
}

#[tokio::test]
async fn persistent_store_resumes_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("register.store");
    {
        let reg = Register::open(Some(&path)).unwrap();
        let (b, keys) = coding_case(&reg).await;
        reg.record_turn(&b, 1, "digest-1", t(1)).await.unwrap();
        reg.record_turn(&b, 3, "digest-3", t(3)).await.unwrap();
        reg.record_served_model(&b, "model-a", t(3)).await.unwrap();
        reg.open_epoch(
            &b,
            &EpochWrite {
                key: "sess-1#e0".into(),
                trigger: "session_start".into(),
                to_model: "model-a".into(),
                packet: None,
                citations: vec![],
                turn: 0,
                through: None,
                ..Default::default()
            },
            t(0),
        )
        .await
        .unwrap();
        reg.open_epoch(
            &b,
            &EpochWrite {
                key: "sess-1#e1".into(),
                trigger: "model_change".into(),
                to_model: "model-b".into(),
                packet: Some("PACKET \"bytes\"\n".into()),
                drop_tools: vec!["Bash".into()],
                citations: vec![(keys[0].clone(), "hard-active".into())],
                turn: 4,
                through: Some(3),
            },
            t(4),
        )
        .await
        .unwrap();
        reg.shutdown();
    }
    let reg = Register::open(Some(&path)).unwrap();
    let snap = reg.session_snapshot("sess-1").await.unwrap().unwrap();
    assert_eq!(snap.branches.len(), 1);
    let b = &snap.branches[0];
    assert_eq!(b.key, "sess-1#0");
    assert_eq!(b.anchors, vec![(1, "digest-1".into(), clock::format(t(1))), (3, "digest-3".into(), clock::format(t(3)))]);
    assert_eq!(b.epochs.len(), 2);
    assert_eq!(b.epochs[1].packet.as_deref(), Some("PACKET \"bytes\"\n"));
    assert_eq!(b.epochs[1].model, "model-b");
    assert_eq!((b.epochs[1].turn, b.epochs[1].through), (4, 3));
    assert_eq!(b.epochs[1].drop_tools, vec!["Bash"]);
    assert!(b.epochs[0].drop_tools.is_empty());
    assert_eq!(b.watermark, Some(2));
    assert_eq!(b.served_model.as_deref(), Some("model-a"));
    assert!(reg.session_snapshot("nope").await.unwrap().is_none());
    // Sequence numbers continue after a restart.
    let k = reg
        .apply("sess-1", "sess-1#0", &[timed(add(Kind::Goal, Strength::Graded, "More", None, 5), DEV, t(30))], None, None, None, t(30))
        .await
        .unwrap();
    assert_eq!(k, vec!["sess-1#4"]);
    // Audits still work against the re-added constraint revisions.
    assert_eq!(reg.audit("sess-1#0", false, false).await.unwrap().len(), 5);
}

#[tokio::test]
async fn admin_query_is_read_only() {
    let reg = Register::open(None).unwrap();
    coding_case(&reg).await;
    let sets = reg.query("search [{(?i, item_key)}, ?k, *] return ?k;").await.unwrap();
    assert_eq!(sets[0].rows.len(), 3);
    assert!(reg.query("add posit [{(+x, name)}, \"x\", @NOW];").await.is_err());
    assert!(reg.query("search [{(?x, name)}, \"y\", *] or add posit [{(+x, name)}, \"y\", @NOW];").await.is_err());
    // Review finding: any spacing or comment between `or` and `add` is still rejected.
    for sneaky in [
        "search [{(?x, name)}, \"y\", *]\nor add posit [{(+x, name)}, \"y\", @NOW];",
        "search [{(?x, name)}, \"y\", *] or/**/add posit [{(+x, name)}, \"y\", @NOW];",
        "search [{(?x, name)}, \"y\", *]\tor\n\tadd posit [{(+x, name)}, \"y\", @NOW];",
        "search [{(?i, item_key)}, ?k, *] return ?k; check constraint \"x\";",
        "search [{(?x, `name\\`)}, *, *] return ?x; add posit [{(+n, name)}, \"pwned\", @NOW]; search [{(?y, `name`)}, *, *] return ?y;",
    ] {
        assert!(reg.query(sneaky).await.is_err(), "{sneaky}");
    }
    let names = reg.query("search [{(?p, name)}, ?n, *] return ?n;").await.unwrap();
    assert!(names[0].rows.iter().all(|r| r[0].text != "\"y\""), "nothing was written");
    // Keywords inside string literals are fine.
    assert!(reg.query("search [{(?i, statement)}, \"add a check; record it\", *] return ?i;").await.is_ok());
}

#[tokio::test]
async fn c5_flags_an_uncited_authoritative_lift() {
    let reg = Register::open(None).unwrap();
    let (b, keys) = coding_case(&reg).await;
    // A cited revocation of the hard prohibition: fine.
    let mut cited = revoke(&keys[0], 5);
    cited.kind = Kind::Prohibition;
    reg.apply("sess-1", &b, &[timed(cited, DEV, t(5))], None, None, None, t(5)).await.unwrap();
    reg.declare_ready(&b, t(6)).await.unwrap();
    assert_eq!(reg.audit(&b, true, false).await.unwrap()[4].verdict, "satisfied");
    // A bare authoritative 0% written around the register's typed writes: violated.
    let id = reg.positor(DEV).await.unwrap();
    let members = reg.load_memberships(&b).await.unwrap();
    let (_, m) = members[&keys[0]].clone();
    reg.write(
        "add posit [{($m, posit), ($dev, ascertains)}, 0%, $t];".into(),
        ExecutionOptions::default().bind("m", reg.posit(&m)).bind("dev", reg.thing(&id)).bind("t", value::time_param(t(7))),
    )
    .await
    .unwrap();
    let a = reg.audit(&b, true, false).await.unwrap();
    assert_eq!(a[4].verdict, "violated", "{a:?}");
    assert_eq!(a[4].findings, 1);
}

#[tokio::test]
async fn a_new_session_and_its_first_turn_take_two_commits() {
    let reg = Register::open(None).unwrap();
    let first = EpochWrite {
        key: "n#e1".into(),
        trigger: "session_start".into(),
        to_model: "m".into(),
        packet: None,
        citations: vec![],
        turn: 0,
        through: None,
        ..Default::default()
    };
    let b = reg.ensure_session("n", DEV, "generic", &auth(), Some(&first), t(0)).await.unwrap();
    // A second (concurrent) first request finds the session instead of duplicating it.
    reg.ids.lock().unwrap().clear();
    assert_eq!(reg.ensure_session("n", DEV, "generic", &auth(), Some(&first), t(0)).await.unwrap(), b);
    let sessions = reg.query("search [{(?s, session_key)}, \"n\", *] return ?s;").await.unwrap();
    assert_eq!(sessions[0].rows.len(), 1);
    reg.record_exchange(
        &b,
        Some((0, "d0")),
        Some("m"),
        &[ActionRecord { action: json!({"tool": "Bash"}), decision: "forwarded".into(), matched: vec![] }],
        t(1),
    )
    .await
    .unwrap();
    let snap = reg.session_snapshot("n").await.unwrap().unwrap();
    assert_eq!(snap.branches[0].epochs.len(), 1);
    assert_eq!(snap.branches[0].epochs[0].trigger, "session_start");
    assert_eq!(snap.branches[0].anchors, vec![(0, "d0".to_string(), clock::format(t(1)))]);
    assert_eq!(snap.branches[0].served_model.as_deref(), Some("m"));
}

#[tokio::test]
async fn c4_requires_pairs_to_be_cited_together() {
    let reg = Register::open(None).unwrap();
    let (b, keys) = coding_case(&reg).await;
    let mut dependent = add(Kind::Commitment, Strength::Graded, "Run migrations after each deploy", None, 3);
    dependent.requires = Some(keys[1].clone());
    let k = reg.apply("sess-1", &b, &[timed(dependent, DEV, t(3))], None, None, None, t(3)).await.unwrap();
    let items = reg.items(&b, None).await.unwrap();
    assert_eq!(items[3].requires, vec![keys[1].clone()]);
    // The packet cites the commitment but not the permission it requires.
    let e = EpochWrite {
        key: "sess-1#e1".into(),
        trigger: "model_change".into(),
        to_model: "b".into(),
        packet: Some("p".into()),
        citations: vec![(keys[0].clone(), "hard-active".into()), (k[0].clone(), "active".into())],
        turn: 4,
        through: Some(3),
        ..Default::default()
    };
    reg.open_epoch(&b, &e, t(4)).await.unwrap();
    reg.declare_ready(&b, t(5)).await.unwrap();
    let a = reg.audit(&b, true, false).await.unwrap();
    assert_eq!(a[3].name, "Required items are cited together");
    assert_eq!(a[3].verdict, "violated", "{a:?}");
}
