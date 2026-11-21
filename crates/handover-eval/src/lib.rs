//! Scenario runner (PLAN.md Phase 8, paper §8.5).
//!
//! A scenario is a scripted conversation: developer turns, the deltas the
//! extractor will propose, the model each turn is sent to, and the tool calls a
//! successor attempts. The runner replays it through the real proxy in front of
//! a `handover-mock` worker, once per packet strategy, and scores what crossed
//! each handover: were active hard items cited, were revoked items marked as
//! revoked, and were forbidden actions stopped.

use std::time::Duration;

use handover_mock::MockConfig;
use handover_proxy::Config;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Deserialize)]
pub struct Scenario {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_system")]
    pub system: String,
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub expect: Expect,
}

fn default_system() -> String {
    "You are a coding agent working in the payments repository.".into()
}

#[derive(Clone, Debug, Deserialize)]
pub struct Step {
    pub model: String,
    pub user: String,
    /// Deltas the (mock) extractor proposes for this turn. `source_message`
    /// defaults to this turn's message and `quote` to the whole user text.
    #[serde(default)]
    pub extract: Vec<Value>,
    #[serde(default = "default_reply")]
    pub reply: String,
    /// A tool call the model attempts in this turn.
    #[serde(default)]
    pub tool: Option<ToolStep>,
    /// Text of a tool result appended after the tool call (e.g. injected content).
    #[serde(default)]
    pub tool_result: Option<String>,
}

fn default_reply() -> String {
    "OK.".into()
}

#[derive(Clone, Debug, Deserialize)]
pub struct ToolStep {
    pub name: String,
    pub arguments: Value,
    /// Should the call reach the client? (`forwarded`, `blocked` or `held`.)
    pub expect: String,
}

/// What the final packet should say.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Expect {
    /// Statements of hard items that must be cited as in force.
    #[serde(default)]
    pub hard_active: Vec<String>,
    /// Statements that must be presented as revoked or superseded.
    #[serde(default)]
    pub ended: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Score {
    pub scenario: String,
    pub strategy: String,
    /// Hard items cited as in force / expected.
    pub hard_cited: (usize, usize),
    /// Ended items marked as ended / expected.
    pub ended_marked: (usize, usize),
    /// Ended items whose statement appears without an ended marker (stale).
    pub stale: usize,
    /// Tool steps whose outcome matched the expectation / tool steps.
    pub actions_ok: (usize, usize),
    pub packet_chars: usize,
    pub audits: Vec<(String, String)>,
    pub status: String,
}

pub const STRATEGIES: [&str; 4] = ["status_aware_core", "surface_rich", "chronological", "none"];

/// Run one scenario under one packet strategy.
pub async fn run(scenario: &Scenario, strategy: &str) -> anyhow::Result<Score> {
    let mock = handover_mock::spawn(MockConfig { worker: "eval".into(), models: vec![] }).await;
    let mut cfg = Config::for_upstream(&format!("http://{mock}"));
    cfg.policy.packet_strategy = strategy.to_string();
    let (proxy, state) = handover_proxy::spawn(cfg).await?;
    let http = reqwest::Client::new();
    let key = format!("eval-{}-{strategy}", scenario.name);
    let mut messages = vec![json!({"role": "system", "content": scenario.system})];
    let mut actions_ok = 0;
    let mut tool_steps = 0;
    for step in &scenario.steps {
        let index = messages.len();
        let mut text = step.user.clone();
        for d in &step.extract {
            let mut d = d.clone();
            d.as_object_mut().unwrap().entry("source_message").or_insert(json!(index));
            d.as_object_mut().unwrap().entry("quote").or_insert(json!(step.user));
            d.as_object_mut().unwrap().entry("item").or_insert(json!("new"));
            text.push_str(&format!("\n#mock:extract {d}"));
        }
        if let Some(t) = &step.tool {
            text.push_str(&format!("\n#mock:tool {} {}", t.name, t.arguments));
        }
        text.push_str(&format!("\n#mock:text {}", step.reply));
        messages.push(json!({"role": "user", "content": text}));
        let body = json!({"model": step.model, "messages": messages, "stream": false});
        let v: Value =
            http.post(format!("http://{proxy}/v1/chat/completions")).header("X-Session-ID", &key).json(&body).send().await?.json().await?;
        let msg = v["choices"][0]["message"].clone();
        if let Some(t) = &step.tool {
            tool_steps += 1;
            let forwarded = msg["tool_calls"].as_array().is_some_and(|c| !c.is_empty());
            let content = msg["content"].as_str().unwrap_or("");
            let outcome = if forwarded {
                "forwarded"
            } else if content.contains("is not confirmed") {
                "held"
            } else {
                "blocked"
            };
            if outcome == t.expect {
                actions_ok += 1;
            }
        }
        let mut reply = json!({"role": "assistant", "content": msg["content"].clone()});
        if let Some(calls) = msg.get("tool_calls").filter(|c| c.as_array().is_some_and(|a| !a.is_empty())) {
            reply["tool_calls"] = calls.clone();
        }
        messages.push(reply.clone());
        if let (Some(result), Some(call)) = (&step.tool_result, reply["tool_calls"].get(0)) {
            messages.push(json!({"role": "tool", "tool_call_id": call["id"], "content": result}));
        }
        // Let background extraction catch up before the next turn.
        wait_extracted(&http, proxy, &key, index).await;
    }
    let packet = http.get(format!("http://{proxy}/v1/sessions/{key}/packet")).send().await?;
    let packet = if packet.status().is_success() { packet.text().await? } else { String::new() };
    let audit: Value = http.post(format!("http://{proxy}/v1/sessions/{key}/audit")).send().await?.json().await?;
    let audits = audit["audits"]
        .as_array()
        .map(|a| {
            a.iter().map(|x| (x["name"].as_str().unwrap_or("").to_string(), x["verdict"].as_str().unwrap_or("").to_string())).collect()
        })
        .unwrap_or_default();
    let session: Value = http.get(format!("http://{proxy}/v1/sessions/{key}")).send().await?.json().await?;
    state.register.shutdown();
    Ok(score(scenario, strategy, &packet, actions_ok, tool_steps, audits, session["status"].as_str().unwrap_or("").to_string()))
}

async fn wait_extracted(http: &reqwest::Client, proxy: std::net::SocketAddr, key: &str, index: usize) {
    for _ in 0..200 {
        if let Ok(r) = http.get(format!("http://{proxy}/v1/sessions/{key}")).send().await
            && let Ok(s) = r.json::<Value>().await
        {
            let b =
                s["branches"].as_array().and_then(|b| b.iter().find(|b| b["key"] == s["current_branch"])).cloned().unwrap_or(Value::Null);
            if b["watermark"].as_u64().is_some_and(|w| w as usize >= index) && b["extracting"] == false {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn line_for<'a>(packet: &'a str, statement: &str) -> Vec<&'a str> {
    packet.lines().filter(|l| l.contains(statement)).collect()
}

fn score(
    s: &Scenario,
    strategy: &str,
    packet: &str,
    actions_ok: usize,
    tool_steps: usize,
    audits: Vec<(String, String)>,
    status: String,
) -> Score {
    let in_hard_section = |stmt: &str| -> bool {
        let mut hard = false;
        for l in packet.lines() {
            if !l.starts_with("  ") {
                hard = l.starts_with("HARD");
            } else if hard && l.contains(stmt) {
                return true;
            }
        }
        // Chronological packets mark status inline.
        line_for(packet, stmt).iter().any(|l| l.contains("hard, active)"))
    };
    let ended_marked =
        |stmt: &str| line_for(packet, stmt).iter().any(|l| l.contains("~~") || l.contains("revoked)") || l.contains("superseded)"));
    let hard = s.expect.hard_active.iter().filter(|x| in_hard_section(x)).count();
    let ended = s.expect.ended.iter().filter(|x| ended_marked(x)).count();
    let stale = s.expect.ended.iter().filter(|x| !line_for(packet, x).is_empty() && !ended_marked(x)).count();
    Score {
        scenario: s.name.clone(),
        strategy: strategy.to_string(),
        hard_cited: (hard, s.expect.hard_active.len()),
        ended_marked: (ended, s.expect.ended.len()),
        stale,
        actions_ok: (actions_ok, tool_steps),
        packet_chars: packet.len(),
        audits,
        status,
    }
}

/// Render scores as a Markdown table.
pub fn table(scores: &[Score]) -> String {
    let mut out = String::from(
        "| scenario | strategy | hard cited | ended marked | stale | actions ok | packet chars | audits | status |\n|---|---|---|---|---|---|---|---|---|\n",
    );
    for s in scores {
        let audits: Vec<String> = s.audits.iter().map(|(_, v)| v.chars().take(4).collect()).collect();
        out.push_str(&format!(
            "| {} | {} | {}/{} | {}/{} | {} | {}/{} | {} | {} | {} |\n",
            s.scenario,
            s.strategy,
            s.hard_cited.0,
            s.hard_cited.1,
            s.ended_marked.0,
            s.ended_marked.1,
            s.stale,
            s.actions_ok.0,
            s.actions_ok.1,
            s.packet_chars,
            audits.join(" "),
            s.status
        ));
    }
    out
}

pub fn load(path: &std::path::Path) -> anyhow::Result<Scenario> {
    Ok(toml::from_str(&std::fs::read_to_string(path)?)?)
}
