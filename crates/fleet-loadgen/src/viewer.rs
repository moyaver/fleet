// SPDX-License-Identifier: AGPL-3.0-only
//! Simulated dashboard viewers and the ingest-to-live latency they measure (PLAN §7.3, §13).
//!
//! A viewer is a browser's live socket: it subscribes to a project's channel and reads the deltas
//! the server pushes after each commit. Every agent records when it sends a flow's live end, and
//! every viewer records when a delta first shows that flow finished. The difference is the
//! ingest-to-live latency of the frame: from the event's send to the live frame a viewer receives.
//! Agents and viewers of one run share one clock (the same process), so the difference is exact.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use fleet_protocol::live::{EntityView, LiveFrame, Routed};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

use crate::{Options, connect_ws};

/// When each flow's live end was sent, and how long each viewer took to see it finished.
#[derive(Debug, Default)]
pub(crate) struct Probe {
    sent: Mutex<HashMap<Uuid, Instant>>,
    samples: Mutex<Vec<Duration>>,
}

impl Probe {
    /// Records the send of a flow's live end. The first send counts, so a resent event does not
    /// shorten the latency.
    pub(crate) fn sent(&self, flow: Uuid, at: Instant) {
        if let Ok(mut sent) = self.sent.lock() {
            sent.entry(flow).or_insert(at);
        }
    }

    /// Records that a viewer saw `flow` finished at `at`. Flows that were never sent give no sample.
    pub(crate) fn seen(&self, flow: Uuid, at: Instant) {
        let sent = self
            .sent
            .lock()
            .ok()
            .and_then(|sent| sent.get(&flow).copied());
        if let (Some(sent), Ok(mut samples)) = (sent, self.samples.lock()) {
            samples.push(at.saturating_duration_since(sent));
        }
    }

    /// Flow ends sent, and the latency samples of the viewers that saw them.
    pub(crate) fn summary(&self) -> (usize, Vec<Duration>) {
        let flows = self.sent.lock().map_or(0, |sent| sent.len());
        let samples = self
            .samples
            .lock()
            .map_or_else(|_| Vec::new(), |s| s.clone());
        (flows, samples)
    }
}

/// What the viewers received, counted as they go.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    /// Viewers whose subscription was confirmed.
    pub(crate) subscribed: AtomicU64,
    frames: AtomicU64,
    resyncs: AtomicU64,
    errors: AtomicU64,
    failed: AtomicU64,
}

/// The viewers' numbers of one run.
#[derive(Debug, Clone, PartialEq)]
pub struct ViewerReport {
    /// Live sockets opened.
    pub viewers: usize,
    /// Live sockets that were refused or dropped before the end (their subscription never ran).
    pub failed: u64,
    /// Frames received, over all viewers.
    pub frames: u64,
    /// `resync` frames: a viewer fell behind and must fetch snapshots again.
    pub resyncs: u64,
    /// Error frames the server sent.
    pub errors: u64,
    /// Flow ends the agents sent.
    pub flows_sent: usize,
    /// Latency samples: one per viewer and flow end the viewer saw finished.
    pub samples: usize,
    /// Ingest-to-live latency p50, p95, p99 and max, in milliseconds (`None` without samples).
    pub latency_ms: Option<[f64; 4]>,
}

/// The flow ids a live frame shows finished: a flow whose state is `passed` or `failed`.
#[must_use]
pub(crate) fn finished_flows(frame: &LiveFrame) -> Vec<Uuid> {
    match frame {
        LiveFrame::Delta { entities, .. } => entities
            .iter()
            .filter_map(|e| match e {
                EntityView::Flow(f) if matches!(f.state.as_str(), "passed" | "failed") => {
                    Some(f.id)
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The `percent`th percentile (nearest rank) of `sorted`, which must not be empty.
fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    let rank = (percent * sorted.len()).div_ceil(100);
    sorted[rank.clamp(1, sorted.len()) - 1]
}

impl ViewerReport {
    /// The viewers' numbers as JSON; latencies are in milliseconds.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let latency = self.latency_ms.map(|[p50, p95, p99, max]| {
            serde_json::json!({ "p50": p50, "p95": p95, "p99": p99, "max": max })
        });
        serde_json::json!({
            "viewers": self.viewers,
            "failed": self.failed,
            "frames": self.frames,
            "resyncs": self.resyncs,
            "errors": self.errors,
            "flows_sent": self.flows_sent,
            "samples": self.samples,
            "latency_ms": latency,
        })
    }
}

/// Summarises a run's viewers.
pub(crate) fn report(viewers: usize, probe: &Probe, stats: &Stats) -> ViewerReport {
    let (flows_sent, mut samples) = probe.summary();
    samples.sort_unstable();
    let latency_ms = (!samples.is_empty())
        .then(|| [50, 95, 99, 100].map(|p| percentile(&samples, p).as_secs_f64() * 1000.0));
    let get = |n: &AtomicU64| n.load(Ordering::Relaxed);
    ViewerReport {
        viewers,
        failed: get(&stats.failed),
        frames: get(&stats.frames),
        resyncs: get(&stats.resyncs),
        errors: get(&stats.errors),
        flows_sent,
        samples: samples.len(),
        latency_ms,
    }
}

/// One live socket: subscribes to the project, then reads frames until the socket closes.
pub(crate) async fn live(
    opts: std::sync::Arc<Options>,
    probe: std::sync::Arc<Probe>,
    stats: std::sync::Arc<Stats>,
) {
    if live_once(&opts, &probe, &stats).await.is_err() {
        stats.failed.fetch_add(1, Ordering::Relaxed);
    }
}

async fn live_once(opts: &Options, probe: &Probe, stats: &Stats) -> anyhow::Result<()> {
    let mut ws = connect_ws(opts, "/api/v1/live", &opts.viewer_token, false).await?;
    let subscribe = serde_json::json!({ "op": "subscribe", "project": opts.project });
    ws.send(Message::text(subscribe.to_string())).await?;
    let mut seen = HashSet::new();
    while let Some(msg) = ws.next().await {
        let Message::Text(text) = msg? else {
            continue;
        };
        let Ok(routed) = serde_json::from_str::<Routed>(text.as_str()) else {
            continue;
        };
        let at = Instant::now();
        stats.frames.fetch_add(1, Ordering::Relaxed);
        match routed.frame {
            LiveFrame::Subscribed { .. } => {
                stats.subscribed.fetch_add(1, Ordering::Relaxed);
            }
            LiveFrame::Resync { .. } => {
                stats.resyncs.fetch_add(1, Ordering::Relaxed);
            }
            LiveFrame::Error { .. } => {
                stats.errors.fetch_add(1, Ordering::Relaxed);
            }
            ref frame @ LiveFrame::Delta { .. } => {
                for flow in finished_flows(frame) {
                    if seen.insert(flow) {
                        probe.seen(flow, at);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLOW: &str = "00000000-0000-7000-8000-00000000000a";

    /// A delta that shows one flow in `state`, in the form the server sends it.
    fn delta(state: &str) -> LiveFrame {
        let wire = format!(
            r#"{{"op":"delta","project_id":"00000000-0000-7000-8000-000000000001",
              "attempt_id":"00000000-0000-7000-8000-000000000002","version":3,
              "entities":[{{"kind":"flow","entity":{{"id":"{FLOW}",
              "job_id":"00000000-0000-7000-8000-000000000003",
              "suite_execution_id":"00000000-0000-7000-8000-000000000004",
              "agent_run_id":"00000000-0000-7000-8000-000000000005",
              "name":"flow-0-0","state":"{state}","finality":"provisional",
              "has_conflict":false,"steps":2,"rev":4}}}}]}}"#
        );
        serde_json::from_str(&wire).expect("a delta in the documented wire form")
    }

    #[test]
    fn a_delta_shows_a_flow_finished_only_in_a_terminal_state() {
        let flow = FLOW.parse::<Uuid>().expect("a uuid");
        assert_eq!(finished_flows(&delta("passed")), [flow]);
        assert_eq!(finished_flows(&delta("failed")), [flow]);
        assert_eq!(finished_flows(&delta("running")), Vec::<Uuid>::new());
        assert_eq!(
            finished_flows(&LiveFrame::Resync {
                reason: fleet_protocol::live::ResyncReason::Lagged,
            }),
            Vec::<Uuid>::new()
        );
    }

    #[test]
    fn latency_is_measured_from_the_first_send_to_each_viewer() {
        let probe = Probe::default();
        let flow = Uuid::from_u128(7);
        let t0 = Instant::now();
        probe.sent(flow, t0);
        // A resent event does not restart the clock.
        probe.sent(flow, t0 + Duration::from_millis(9));
        probe.seen(flow, t0 + Duration::from_millis(5));
        probe.seen(flow, t0 + Duration::from_millis(20));
        // A flow never sent gives no sample.
        probe.seen(Uuid::from_u128(8), t0);
        let (flows, samples) = probe.summary();
        assert_eq!(flows, 1);
        assert_eq!(
            samples,
            [Duration::from_millis(5), Duration::from_millis(20)]
        );
    }

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let sorted: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
        assert_eq!(percentile(&sorted, 50), Duration::from_millis(50));
        assert_eq!(percentile(&sorted, 95), Duration::from_millis(95));
        assert_eq!(percentile(&sorted, 100), Duration::from_millis(100));
    }
}
