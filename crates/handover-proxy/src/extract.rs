//! Register extraction: Option B in the background on the hot prefix, Option A
//! synchronously at a boundary, Option C on a dedicated extractor (PLAN.md §7).

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use handover_core::extraction;
use handover_core::item::RegisterItem;
use handover_core::verify::{TrustPolicy, verify};
use handover_protocol::chat;
use handover_register::TimedDelta;
use serde_json::{Value, json};

use crate::config::Affinity;
use crate::metrics::{add, inc};
use crate::pipeline::{SessionRef, Shared, positor_name};
use crate::session::{LastTurn, Session};

/// Everything an extraction needs, copied out of the session so the HTTP call
/// runs without holding the session lock.
struct Job {
    session_key: String,
    developer: String,
    profile: String,
    branch_key: String,
    new_from: usize,
    items: Vec<RegisterItem>,
    input: LastTurn,
    /// Appearance time of each message of `input.conv` (distinct and ordered).
    times: Vec<DateTime<Utc>>,
}

impl Job {
    fn new(s: &Session, branch_key: &str, input: LastTurn) -> Option<Job> {
        let b = s.branches.iter().find(|b| b.key == branch_key)?;
        Some(Job {
            session_key: s.key.clone(),
            developer: s.developer.clone(),
            profile: s.profile.clone(),
            branch_key: branch_key.to_string(),
            new_from: b.next_new_from(),
            items: b.items.clone(),
            times: b.message_times(input.conv.len(), chrono::Utc::now()),
            input,
        })
    }
}

pub struct Outcome {
    pub through: usize,
    pub written: usize,
}

/// Option A: extract synchronously for the current branch (caller holds the lock).
/// The deadline covers only the model call: once deltas are verified, the write
/// and the refresh always complete, so memory and store never diverge. Returns
/// `None` when the deadline passed.
pub async fn run_extraction(state: &Shared, s: &mut Session, input: LastTurn, deadline: Duration) -> anyhow::Result<Option<Outcome>> {
    let key = s.branch().key.clone();
    let job = Job::new(s, &key, input).ok_or_else(|| anyhow::anyhow!("no branch"))?;
    let fetched = match tokio::time::timeout(deadline, fetch(state, &job)).await {
        Ok(f) => f?,
        Err(_) => return Ok(None),
    };
    let out = store(state, &job, fetched).await?;
    apply_outcome(state, s, &key, &out).await?;
    Ok(Some(out))
}

/// Option B: after the debounce, extract on the prefix the worker just served.
/// A newer request aborts the debounce or the model call, never the write.
pub async fn schedule(state: Shared, session: SessionRef, branch_key: String) {
    let delay = Duration::from_millis(state.config.extractor.debounce_ms);
    let (st, sess, bk) = (state.clone(), session.clone(), branch_key.clone());
    let handle = tokio::spawn(async move {
        tokio::time::sleep(delay).await;
        let Ok(permit) = st.extract_permits.clone().acquire_owned().await else { return };
        if st.overloaded_until.lock().unwrap().is_some_and(|t| t > std::time::Instant::now()) {
            tracing::info!(branch = %bk, "skipping background extraction: next hop is overloaded");
            return;
        }
        let job = {
            let mut s = sess.lock().await;
            let Some(b) = s.branch_by_key(&bk) else { return };
            let Some(input) = b.last_turn.clone() else { return };
            b.extracting = true;
            Job::new(&s, &bk, input)
        };
        let Some(job) = job else { return };
        let fetched = if job.new_from < job.input.conv.len() { fetch(&st, &job).await.map(Some) } else { Ok(None) };
        // From here on the work is detached from the abortable task.
        tokio::spawn(async move {
            let _permit = permit;
            let stored = match fetched {
                Ok(Some(f)) => store(&st, &job, f).await.map(Some),
                Ok(None) => Ok(None),
                Err(e) => Err(e),
            };
            let mut s = sess.lock().await;
            if let Some(b) = s.branch_by_key(&bk) {
                b.extracting = false;
                b.extraction = None;
            }
            match stored {
                Ok(Some(out)) => {
                    if let Err(e) = apply_outcome(&st, &mut s, &bk, &out).await {
                        tracing::warn!(session = %s.key, "refresh after extraction: {e}");
                    }
                }
                Ok(None) => {}
                Err(e) => tracing::warn!(session = %s.key, branch = %bk, "extraction failed: {e}"),
            }
        });
    });
    let mut s = session.lock().await;
    if let Some(b) = s.branch_by_key(&branch_key)
        && let Some(old) = b.extraction.replace(handle)
    {
        old.abort();
    }
}

async fn apply_outcome(state: &Shared, s: &mut Session, branch_key: &str, out: &Outcome) -> anyhow::Result<()> {
    let (items, enforcer) = state.refresh(branch_key).await?;
    if let Some(b) = s.branch_by_key(branch_key) {
        b.watermark = Some(b.watermark.map_or(out.through, |w| w.max(out.through)));
        b.items = items;
        b.enforcer = enforcer;
    }
    Ok(())
}

/// Verified deltas from one model call, ready to be written.
struct Fetched {
    timed: Vec<TimedDelta>,
    run: Value,
    through: usize,
    extractor: String,
    cached: u64,
}

/// The model call: build the request on the hot prefix (or the dedicated
/// extractor's minimal context), parse and verify the answer.
async fn fetch(state: &Shared, job: &Job) -> anyhow::Result<Fetched> {
    let cfg = &state.config.extractor;
    let up = state.config.upstream();
    let prompt = extraction::prompt(&job.items, &job.input.conv, job.new_from, job.input.offset, 600);
    let prompt_msg = json!({"role": "user", "content": prompt});
    let dedicated = cfg.mode == "dedicated";
    let (url, model, api_key, messages) = if dedicated {
        (
            cfg.url.clone().ok_or_else(|| anyhow::anyhow!("extractor.url is required for mode = \"dedicated\""))?,
            cfg.model.clone().unwrap_or_else(|| job.input.model.clone()),
            cfg.api_key.clone(),
            vec![prompt_msg],
        )
    } else {
        let mut m = job.input.upstream_messages.clone();
        if !job.input.assistant.is_null() {
            m.push(job.input.assistant.clone());
        }
        m.push(prompt_msg);
        (up.base_url.clone(), job.input.model.clone(), up.api_key.clone(), m)
    };
    let mut body = json!({
        "model": model,
        "messages": messages,
        "response_format": extraction::response_format(),
        "temperature": 0,
        "max_tokens": cfg.max_tokens,
        "stream": false,
    });
    // Chat templates render tool definitions at the top of the prompt: resend
    // exactly the served ones, or the cached prefix would differ from token 0.
    if !dedicated && let Some(tools) = &job.input.tools {
        body["tools"] = tools.clone();
    }
    if let Some(p) = cfg.priority {
        body["priority"] = json!(p);
    }
    let mut req = state.http.post(format!("{}/v1/chat/completions", url.trim_end_matches('/'))).json(&body);
    if let Affinity::Header { name } = &up.affinity {
        req = req.header(name.as_str(), job.session_key.as_str());
    }
    match (api_key, &job.input.auth) {
        (Some(k), _) => req = req.bearer_auth(k),
        (None, Some(a)) if !dedicated => req = req.header("authorization", a.clone()),
        _ => {}
    }
    let started = Instant::now();
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            inc(&state.metrics.extraction_failures_total);
            anyhow::bail!("extractor transport: {e}");
        }
    };
    if !resp.status().is_success() {
        inc(&state.metrics.extraction_failures_total);
        anyhow::bail!("extractor returned {}", resp.status());
    }
    let v: Value = resp.json().await?;
    let usage = v.get("usage");
    let cached = chat::cached_tokens(usage).unwrap_or(0);
    let prompt_tokens = usage.and_then(|u| u.get("prompt_tokens")).and_then(Value::as_u64).unwrap_or(0);
    add(&state.metrics.extraction_cached_tokens_total, cached);
    add(&state.metrics.extraction_prompt_tokens_total, prompt_tokens);
    let content = v.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("");
    let deltas = match extraction::parse(content) {
        Ok(d) => d,
        Err(e) => {
            inc(&state.metrics.extraction_failures_total);
            anyhow::bail!("extractor output: {e}");
        }
    };
    let policy = TrustPolicy {
        window_start: job.new_from,
        message_times: job.times.iter().map(|t| handover_core::clock::format(*t)).collect(),
        ..TrustPolicy::default()
    };
    let (ok, rejected) = verify(deltas, &job.input.conv, &job.items, &policy);
    add(&state.metrics.deltas_rejected_total, rejected.len() as u64);
    for r in &rejected {
        tracing::info!(session = %job.session_key, reason = %r.reason, "delta rejected");
    }
    let now = state.clock.now();
    let through = job.input.conv.len().saturating_sub(1);
    let timed: Vec<TimedDelta> = ok
        .into_iter()
        .map(|d| {
            let turn = d.citation.get("turn").and_then(Value::as_u64).map(|t| t as usize);
            let at = turn.and_then(|t| job.times.get(t).copied()).unwrap_or(now);
            let positor = positor_name(&job.developer, &job.profile, d.source, &job.input.model);
            TimedDelta { delta: d, positor, at }
        })
        .collect();
    let run = json!({
        "model": model, "mode": cfg.mode, "new_from": job.new_from, "through": through,
        "deltas": timed.len(), "rejected": rejected.len(),
        "cached_tokens": cached, "prompt_tokens": prompt_tokens, "ms": started.elapsed().as_millis() as u64,
    });
    Ok(Fetched { timed, run, through, extractor: format!("extractor:{model}"), cached })
}

/// The write: every delta, the watermark and the run record in one commit.
async fn store(state: &Shared, job: &Job, f: Fetched) -> anyhow::Result<Outcome> {
    let now = state.clock.now();
    state.register.apply(&job.session_key, &job.branch_key, &f.timed, Some(&f.extractor), Some(f.through), Some(f.run), now).await?;
    inc(&state.metrics.extractions_total);
    add(&state.metrics.deltas_written_total, f.timed.len() as u64);
    tracing::info!(session = %job.session_key, branch = %job.branch_key, through = f.through, deltas = f.timed.len(), cached = f.cached, "extraction");
    Ok(Outcome { through: f.through, written: f.timed.len() })
}
