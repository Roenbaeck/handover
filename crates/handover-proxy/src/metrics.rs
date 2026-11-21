//! Prometheus-style counters (`GET /metrics`).

use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! counters {
    ($($name:ident: $help:literal),* $(,)?) => {
        #[derive(Debug, Default)]
        pub struct Metrics { $(pub $name: AtomicU64,)* }

        impl Metrics {
            pub fn render(&self) -> String {
                let mut out = String::new();
                $(
                    out.push_str(concat!("# HELP handover_", stringify!($name), " ", $help, "\n"));
                    out.push_str(concat!("# TYPE handover_", stringify!($name), " counter\n"));
                    out.push_str(&format!(concat!("handover_", stringify!($name), " {}\n"), self.$name.load(Ordering::Relaxed)));
                )*
                out
            }
        }
    };
}

counters! {
    requests_total: "Requests handled",
    passthrough_total: "Requests forwarded without any rewrite",
    epochs_total: "Epochs opened",
    packets_injected_total: "Upstream requests that carried a handoff packet",
    tool_calls_total: "Tool calls seen",
    blocked_total: "Tool calls blocked",
    held_total: "Tool calls held for confirmation",
    warned_total: "Tool calls forwarded with a warning",
    extractions_total: "Extraction requests completed",
    extraction_failures_total: "Extraction requests that failed",
    extraction_prompt_tokens_total: "Prompt tokens of extraction requests",
    extraction_cached_tokens_total: "Cached prompt tokens of extraction requests",
    deltas_written_total: "Register deltas written",
    deltas_rejected_total: "Register deltas rejected by verification",
    option_a_total: "Synchronous extractions at a boundary",
    option_a_timeouts_total: "Synchronous extractions that hit the deadline",
    unplanned_handovers_total: "Responses served by a model other than the pinned one",
    weight_swaps_total: "Model ids whose /v1/models root changed",
    audits_total: "Audit checks run",
    audit_violations_total: "Audit checks that returned violated",
    upstream_errors_total: "Upstream errors (status >= 400 or transport)",
    lanes_total: "Delegated lanes opened (subagents, side calls)",
    side_requests_total: "Side requests answered without entering their lane (compaction summaries)",
}

pub fn inc(c: &AtomicU64) {
    c.fetch_add(1, Ordering::Relaxed);
}

pub fn add(c: &AtomicU64, n: u64) {
    c.fetch_add(n, Ordering::Relaxed);
}
