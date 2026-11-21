//! Admin API (PLAN.md §7.6, §8.4, §11): sessions, explicit decisions, audits,
//! the current packet, read-only Traqula and metrics.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use handover_core::enforce::Scope;
use handover_core::item::{Kind, Strength};
use handover_core::verify::{DeltaOp, Source, Target, VerifiedDelta};
use handover_register::TimedDelta;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::pipeline::{SessionRef, Shared, run_audits};
use crate::{bearer, err};

fn session(state: &Shared, key: &str) -> Option<SessionRef> {
    state.sessions.lock().unwrap().get(key).cloned()
}

#[allow(clippy::result_large_err)] // the error is the response itself
async fn authorize(state: &Shared, headers: &HeaderMap, key: &str, write: bool) -> Result<SessionRef, Response> {
    // Admin tokens only: the harness's inference token must not be able to
    // record "developer" decisions (an agent could lift its own constraints).
    let dev = state.admin_for(bearer(headers).as_deref(), write).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "admin token required"))?;
    let key = &state.namespaced(&dev, key);
    let s = session(state, key).ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown session (sessions load on their first request)"))?;
    if s.lock().await.developer != dev {
        return Err(err(StatusCode::FORBIDDEN, "session belongs to another developer"));
    }
    Ok(s)
}

pub async fn list(State(state): State<Shared>, headers: HeaderMap) -> Response {
    let Some(dev) = state.admin_for(bearer(&headers).as_deref(), false) else {
        return err(StatusCode::UNAUTHORIZED, "admin token required");
    };
    let all: Vec<SessionRef> = state.sessions.lock().unwrap().values().cloned().collect();
    let mut out = Vec::new();
    for s in all {
        let s = s.lock().await;
        if s.developer != dev {
            continue;
        }
        let b = s.branch();
        out.push(json!({
            "key": s.client_key, "status": s.light(), "branch": b.key, "epoch": b.epoch.as_ref().map(|e| &e.key),
            "model": b.epoch.as_ref().map(|e| &e.model), "items": b.items.len(), "watermark": b.watermark,
        }));
    }
    Json(json!({"sessions": out})).into_response()
}

pub async fn show(State(state): State<Shared>, headers: HeaderMap, Path(key): Path<String>) -> Response {
    let s = match authorize(&state, &headers, &key, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let s = s.lock().await;
    let b = s.branch();
    let items: Vec<Value> = b
        .items
        .iter()
        .map(|i| {
            json!({
                "handle": i.handle(), "key": i.key(), "kind": i.kind(), "strength": i.strength(), "status": i.status,
                "statement": i.statement, "enforce": i.record.enforce, "superseded_by": i.superseded_by,
                "assertions": i.record.assertions, "citations": i.record.citations,
            })
        })
        .collect();
    Json(json!({
        "key": s.client_key, "developer": s.developer, "profile": s.profile, "weak_identity": s.weak_identity,
        "status": s.light(), "current_branch": b.key,
        "branches": s.branches.iter().map(|b| json!({
            "key": b.key, "parent": b.parent, "fork_common": b.fork_common, "epochs": b.epochs,
            "epoch": b.epoch, "watermark": b.watermark, "head": b.head, "served_model": b.served_model,
            "unplanned": b.unplanned, "hold_pending": b.hold_pending, "audits": b.audits,
            "all_epochs_complete": b.all_epochs_complete, "extracting": b.extracting,
        })).collect::<Vec<_>>(),
        "lanes": s.lanes,
        "items": items,
        "uncompiled_enforce": b.enforcer.uncompiled,
    }))
    .into_response()
}

#[derive(Deserialize)]
pub struct Decision {
    op: String,
    #[serde(default)]
    item: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    strength: Option<String>,
    #[serde(default)]
    statement: Option<String>,
    #[serde(default)]
    enforce: Option<Value>,
    #[serde(default)]
    replaces: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// Handle of an item this new item requires.
    #[serde(default)]
    requires: Option<String>,
}

/// Record an explicit decision as the authoritative developer (§7.6).
pub async fn decide(State(state): State<Shared>, headers: HeaderMap, Path(key): Path<String>, Json(d): Json<Decision>) -> Response {
    let sref = match authorize(&state, &headers, &key, true).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let mut s = sref.lock().await;
    let Some(op) = DeltaOp::parse(&d.op) else { return err(StatusCode::BAD_REQUEST, "unknown op") };
    let items = s.branch().items.clone();
    let find = |h: &str| items.iter().find(|i| i.handle().eq_ignore_ascii_case(h) || i.key() == h).cloned();
    let existing = d.item.as_deref().filter(|h| !h.eq_ignore_ascii_case("new")).map(find);
    if matches!(existing, Some(None)) {
        return err(StatusCode::NOT_FOUND, "unknown item");
    }
    let existing = existing.flatten();
    let replaces = match d.replaces.as_deref() {
        Some(h) => match find(h) {
            Some(i) => Some(i.key().to_string()),
            None => return err(StatusCode::NOT_FOUND, "unknown item in replaces"),
        },
        None => None,
    };
    let requires = match d.requires.as_deref() {
        Some(h) => match find(h) {
            Some(i) => Some(i.key().to_string()),
            None => return err(StatusCode::NOT_FOUND, "unknown item in requires"),
        },
        None => None,
    };
    let target = match (op, &existing) {
        (DeltaOp::Add | DeltaOp::Supersede, _) => Target::New,
        (_, Some(i)) => Target::Existing(i.key().to_string()),
        (_, None) => return err(StatusCode::BAD_REQUEST, "this op needs an existing item"),
    };
    let kind = match (&existing, d.kind.as_deref()) {
        (_, Some(k)) => match Kind::parse(k) {
            Some(k) => k,
            None => return err(StatusCode::BAD_REQUEST, "unknown kind"),
        },
        (Some(i), None) => i.kind(),
        (None, None) => return err(StatusCode::BAD_REQUEST, "kind is required"),
    };
    let strength = d.strength.as_deref().and_then(Strength::parse).or(existing.as_ref().map(|i| i.strength())).unwrap_or(Strength::Graded);
    if target == Target::New && d.statement.as_deref().is_none_or(|s| s.trim().is_empty()) {
        return err(StatusCode::BAD_REQUEST, "statement is required");
    }
    if let Some(e) = &d.enforce
        && let Err(e) = Scope::compile(e)
    {
        return err(StatusCode::BAD_REQUEST, &format!("enforce: {e}"));
    }
    let certainty = match op {
        DeltaOp::Revoke => 0.0,
        DeltaOp::Dispute => -1.0,
        _ => 1.0,
    };
    let now = state.clock.now();
    let delta = VerifiedDelta {
        op,
        target,
        replaces,
        kind,
        strength,
        statement: d.statement.clone(),
        enforce: d.enforce.clone(),
        source: Source::Developer,
        certainty,
        meta_certainty: 1.0,
        citation: json!({"turn": s.branch().head, "quote": d.note.clone().unwrap_or_default(), "op": op.as_str(), "verified": true, "via": "admin"}),
        requires,
        notes: vec![],
    };
    let branch = s.branch().key.clone();
    let td = TimedDelta { delta, positor: s.developer.clone(), at: now };
    let keys = match state.register.apply(&s.key, &branch, &[td], None, None, None, now).await {
        Ok(k) => k,
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    match state.refresh(&branch).await {
        Ok((items, enforcer)) => {
            let b = s.branch_mut();
            b.items = items;
            b.enforcer = enforcer;
        }
        Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
    let handles: Vec<String> = s.branch().items.iter().filter(|i| keys.contains(&i.record.key)).map(|i| i.handle()).collect();
    (StatusCode::CREATED, Json(json!({"created": keys, "handles": handles, "status": s.light()}))).into_response()
}

pub async fn audit(State(state): State<Shared>, headers: HeaderMap, Path(key): Path<String>) -> Response {
    let sref = match authorize(&state, &headers, &key, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let (internal, branch, ready) = {
        let s = sref.lock().await;
        let b = s.branch();
        (s.key.clone(), b.key.clone(), b.all_epochs_complete && b.epochs > 0)
    };
    if ready && let Err(e) = state.register.declare_ready(&branch, state.clock.now()).await {
        return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    match run_audits(&state, &internal, &branch, ready).await {
        Ok(results) => {
            Json(json!({"branch": branch, "ready": ready, "audits": results, "status": sref.lock().await.light()})).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub async fn packet(State(state): State<Shared>, headers: HeaderMap, Path(key): Path<String>) -> Response {
    let sref = match authorize(&state, &headers, &key, false).await {
        Ok(s) => s,
        Err(r) => return r,
    };
    let s = sref.lock().await;
    match s.branch().epoch.as_ref().and_then(|e| e.packet.clone()) {
        Some(p) => ([("content-type", "text/plain; charset=utf-8")], p).into_response(),
        None => err(StatusCode::NOT_FOUND, "the current epoch has no packet"),
    }
}

/// Read-only Traqula against the live store, with budgets (§8.4).
pub async fn traqula(State(state): State<Shared>, headers: HeaderMap, body: String) -> Response {
    if !state.operator_ok(bearer(&headers).as_deref()) {
        return err(StatusCode::UNAUTHORIZED, "operator token required");
    }
    match state.register.query(&body).await {
        Ok(sets) => Json(json!(sets
            .iter()
            .map(|s| json!({"columns": s.columns, "rows": s.rows.iter().map(|r| r.iter().map(|c| c.text.clone()).collect::<Vec<_>>()).collect::<Vec<_>>()}))
            .collect::<Vec<_>>()))
        .into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()),
    }
}
