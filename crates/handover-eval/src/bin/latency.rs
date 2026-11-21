//! Added latency of the proxy on the hot path (PLAN.md Phase 0 exit criterion:
//! p50 below 1 ms per chunk). Streams the same responses from a mock worker
//! directly and through the proxy, and compares chunk arrival times.
//!
//! Usage: [STORE=<dir>] latency [requests] [chars-per-reply]   (run with --release)

use std::time::{Duration, Instant};

use handover_mock::MockConfig;
use handover_proxy::Config;
use serde_json::json;

async fn arrivals(http: &reqwest::Client, url: &str, session: &str, text: &str) -> Vec<Duration> {
    let body = json!({"model": "model-a", "stream": true, "messages": [{"role": "user", "content": format!("go\n#mock:text {text}")}]});
    let start = Instant::now();
    let mut resp = http.post(url).header("X-Session-ID", session).json(&body).send().await.unwrap();
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await.unwrap() {
        let events = chunk.windows(2).filter(|w| w == b"\n\n").count();
        for _ in 0..events {
            out.push(start.elapsed());
        }
    }
    out
}

fn pct(mut v: Vec<f64>, q: f64) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[((v.len() as f64 - 1.0) * q).round() as usize]
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200);
    let chars: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(2000);
    let mock = handover_mock::spawn(MockConfig { worker: "lat".into(), models: vec![] }).await;
    let mut cfg = Config::for_upstream(&format!("http://{mock}"));
    cfg.extractor.enabled = false; // measure the hot path only
    // `STORE=<dir>` measures against a persistent store (durable commits on new sessions).
    cfg.store.path = std::env::var_os("STORE").map(Into::into);
    let (proxy, _state) = handover_proxy::spawn(cfg).await.unwrap();
    let http = reqwest::Client::new();
    let text: String = "lorem ipsum dolor sit amet ".chars().cycle().take(chars).collect();
    let direct_url = format!("http://{mock}/v1/chat/completions");
    let proxy_url = format!("http://{proxy}/v1/chat/completions");
    // Warm up connections and code paths.
    for i in 0..10 {
        arrivals(&http, &direct_url, &format!("w{i}"), &text).await;
        arrivals(&http, &proxy_url, &format!("w{i}"), &text).await;
    }
    let mut first = Vec::new();
    let mut per_chunk = Vec::new();
    let mut total = Vec::new();
    let mut chunks = 0;
    for i in 0..n {
        // Each proxy request is a new session's first turn: session start, epoch write included.
        let d = arrivals(&http, &direct_url, &format!("d{i}"), &text).await;
        // `SAME=1`: one continuing session (no store writes before forwarding).
        let session = if std::env::var_os("SAME").is_some() { "same".to_string() } else { format!("p{i}") };
        let p = arrivals(&http, &proxy_url, &session, &text).await;
        let k = d.len().min(p.len());
        chunks = k;
        first.push((p[0].as_secs_f64() - d[0].as_secs_f64()) * 1e3);
        total.push((p[k - 1].as_secs_f64() - d[k - 1].as_secs_f64()) * 1e3);
        // Added delay of each chunk relative to the first one (streaming overhead).
        for j in 1..k {
            let dp = (p[j] - p[0]).as_secs_f64() - (d[j] - d[0]).as_secs_f64();
            per_chunk.push(dp * 1e3 / j as f64);
        }
    }
    println!("{n} requests, {chunks} SSE events each ({chars} chars of text)");
    let what = if std::env::var_os("SAME").is_some() { "continuing session" } else { "new session" };
    println!("added time to first event ({what}):  p50 {:.3} ms  p95 {:.3} ms", pct(first.clone(), 0.5), pct(first, 0.95));
    println!("added streaming delay per event:          p50 {:.4} ms  p95 {:.4} ms", pct(per_chunk.clone(), 0.5), pct(per_chunk, 0.95));
    println!("added total time per response:            p50 {:.3} ms  p95 {:.3} ms", pct(total.clone(), 0.5), pct(total, 0.95));
}
