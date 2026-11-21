//! The bundled scenarios, scored under every packet strategy.

use handover_eval::{STRATEGIES, load, run, table};

fn scenarios() -> Vec<handover_eval::Scenario> {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scenarios");
    let mut files: Vec<_> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).filter(|p| p.extension().is_some_and(|x| x == "toml")).collect();
    files.sort();
    files.iter().map(|f| load(f).unwrap()).collect()
}

#[tokio::test]
async fn status_aware_core_preserves_everything_and_enforcement_holds_regardless() {
    let mut scores = Vec::new();
    for s in scenarios() {
        for strategy in STRATEGIES {
            scores.push(run(&s, strategy).await.unwrap());
        }
    }
    println!("{}", table(&scores));
    for s in &scores {
        // Enforcement does not depend on what the packet says.
        assert_eq!(s.actions_ok.0, s.actions_ok.1, "{s:?}");
        if s.strategy == "status_aware_core" {
            assert_eq!(s.hard_cited.0, s.hard_cited.1, "{s:?}");
            assert_eq!(s.ended_marked.0, s.ended_marked.1, "{s:?}");
            assert_eq!(s.stale, 0, "{s:?}");
        }
    }
    let surface = scores.iter().find(|s| s.scenario == "coding-case" && s.strategy == "surface_rich").unwrap();
    assert_eq!(surface.stale, 1, "surface-rich notes present the revoked permission as if current");
    assert_eq!(surface.audits[1].1, "violated", "audit C2 catches the stale summary: {surface:?}");
    let none = scores.iter().find(|s| s.scenario == "coding-case" && s.strategy == "none").unwrap();
    assert_eq!(none.hard_cited.0, 0);
    assert!(none.audits.iter().any(|(_, v)| v == "violated"), "no packet ⇒ C1/C2 violated: {none:?}");
}
