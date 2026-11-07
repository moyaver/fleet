// SPDX-License-Identifier: AGPL-3.0-only
//! The agent protocol, end to end over WebSocket and HTTPS batch.

use std::time::Duration;

use fleet_protocol::{
    PROTOCOL_VERSION,
    event::{EventBody, JournalGap, RunSignal},
    frame::{
        BatchRequest, BatchResponse, ClientFrame, ErrorCode, GoawayReason, ResumeOutcome, Sample,
        ServerFrame, Telemetry,
    },
    journal::{ClockSample, EventEnvelope, Provenance, SourceKind, Subject},
};
use time::OffsetDateTime;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use crate::support::{Agent, Client, Harness, Received, post_json};

fn welcome(frame: &ServerFrame) -> (u64, u32, ResumeOutcome) {
    match frame {
        ServerFrame::Welcome(w) => (w.committed_through, w.epoch, w.resume),
        other => panic!("expected welcome, got {other:?}"),
    }
}

#[tokio::test]
async fn a_run_streams_and_resumes_where_the_server_says() {
    let h = Harness::start().await;
    let mut agent = Agent::new("1", "e2e");
    let events = agent.suite(&[("login", true), ("pay", true)]);

    let mut c = h.connect(None).await;
    assert_eq!(
        welcome(&c.hello(&agent, Some(1)).await),
        (0, 1, ResumeOutcome::New)
    );
    assert_eq!(c.events(events[..5].to_vec()).await, (5, 0));
    drop(c);

    // The agent never saw that ack: it reconnects holding everything. The server's answer, not
    // the agent's memory, decides what to resend (research 05 §3.4).
    let mut c = h.connect(None).await;
    let (committed, epoch, resume) = welcome(&c.hello(&agent, Some(1)).await);
    assert_eq!((committed, epoch, resume), (5, 2, ResumeOutcome::Resumed));
    // Resending the whole stream is harmless: the first five are duplicates.
    assert_eq!(c.events(events.clone()).await, (agent.seq, 5));
    c.send(&ClientFrame::Bye).await;
    assert_eq!(c.recv().await.close_code(), Some(CloseCode::Normal));
}

#[tokio::test]
async fn a_restart_loses_no_committed_event() {
    let mut h = Harness::start().await;
    let mut agent = Agent::new("2", "e2e");
    let events = agent.suite(&[("login", true)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    let (through, _) = c.events(events[..4].to_vec()).await;
    assert_eq!(through, 4);

    h.restart().await;
    assert!(
        matches!(
            c.recv().await,
            Received::Frame(ServerFrame::Goaway {
                reason: GoawayReason::Shutdown,
                ..
            })
        ),
        "goaway before the restart"
    );
    let mut c = h.connect(None).await;
    let (committed, _, resume) = welcome(&c.hello(&agent, Some(1)).await);
    assert_eq!(
        (committed, resume),
        (through, ResumeOutcome::Resumed),
        "everything acked before the restart is still committed"
    );
    assert_eq!(c.events(events[4..].to_vec()).await.0, agent.seq);
}

#[tokio::test]
async fn shutdown_sends_goaway_the_final_telemetry_ack_and_1012() {
    let mut h = Harness::start_with(|c| c.ingest.telemetry_flush_ms = 60_000).await;
    let agent = Agent::new("3", "e2e");
    let mut c = h.connect(None).await;
    c.hello(&agent, None).await;
    let at = ClockSample {
        wall: OffsetDateTime::now_utc(),
        mono_ms: 1,
    };
    c.send(&ClientFrame::Telemetry(Telemetry {
        tseq_from: 1,
        tseq_to: 2,
        dropped_since_last: 0,
        devices: Vec::new(),
        samples: vec![
            Sample {
                at,
                host_cpu_pct: Some(12.5),
                host_mem_used_bytes: Some(1 << 30),
                host_mem_total_bytes: Some(1 << 34),
                maestro_cpu_pct: None,
                maestro_rss_bytes: None,
                agent_cpu_pct: Some(0.1),
                agent_rss_bytes: Some(1 << 20),
                host_load_1m: None,
                host_cpu_cores: None,
                host_swap_used_bytes: None,
                host_swap_total_bytes: None,
                host_psi_cpu_pct: None,
                host_psi_memory_pct: None,
                host_psi_io_pct: None,
            },
            Sample {
                at: ClockSample {
                    wall: at.wall + time::Duration::seconds(5),
                    mono_ms: 5001,
                },
                host_cpu_pct: None,
                host_mem_used_bytes: None,
                host_mem_total_bytes: None,
                maestro_cpu_pct: None,
                maestro_rss_bytes: None,
                agent_cpu_pct: None,
                agent_rss_bytes: None,
                host_load_1m: None,
                host_cpu_cores: None,
                host_swap_used_bytes: None,
                host_swap_total_bytes: None,
                host_psi_cpu_pct: None,
                host_psi_memory_pct: None,
                host_psi_io_pct: None,
            },
        ],
    }))
    .await;
    // Let the frame land in the buffer; the checkpoint is a minute away.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stop = tokio::spawn(async move { h.stop().await });
    let mut got = Vec::new();
    loop {
        match c.recv().await {
            Received::Frame(f) => got.push(f),
            closed @ Received::Closed(_) => {
                assert_eq!(closed.close_code(), Some(CloseCode::Restart), "1012");
                break;
            }
        }
    }
    assert!(matches!(
        got[0],
        ServerFrame::Goaway {
            reason: GoawayReason::Shutdown,
            ..
        }
    ));
    assert!(
        got.contains(&ServerFrame::TelemetryAck { through: 2 }),
        "the final checkpoint is acked before the close: {got:?}"
    );
    stop.await.unwrap();
}

#[tokio::test]
async fn telemetry_is_acked_after_its_checkpoint() {
    let h = Harness::start().await;
    let agent = Agent::new("4", "e2e");
    let mut c = h.connect(None).await;
    c.hello(&agent, None).await;
    c.send(&ClientFrame::Telemetry(Telemetry {
        tseq_from: 7,
        tseq_to: 7,
        dropped_since_last: 6,
        devices: Vec::new(),
        samples: vec![Sample {
            at: ClockSample {
                wall: OffsetDateTime::now_utc(),
                mono_ms: 1,
            },
            host_cpu_pct: Some(1.0),
            host_mem_used_bytes: None,
            host_mem_total_bytes: None,
            maestro_cpu_pct: None,
            maestro_rss_bytes: None,
            agent_cpu_pct: None,
            agent_rss_bytes: None,
            host_load_1m: None,
            host_cpu_cores: None,
            host_swap_used_bytes: None,
            host_swap_total_bytes: None,
            host_psi_cpu_pct: None,
            host_psi_memory_pct: None,
            host_psi_io_pct: None,
        }],
    }))
    .await;
    assert_eq!(c.frame().await, ServerFrame::TelemetryAck { through: 7 });
}

#[tokio::test]
async fn a_newer_connection_supersedes_the_old_one() {
    let h = Harness::start().await;
    let mut agent = Agent::new("5", "e2e");
    let events = agent.suite(&[("login", true)]);
    let mut old = h.connect(None).await;
    old.hello(&agent, Some(1)).await;
    old.events(events[..2].to_vec()).await;
    let mut new = h.connect(None).await;
    assert_eq!(welcome(&new.hello(&agent, Some(3)).await).1, 2);

    old.send(&ClientFrame::Events {
        events: events[2..].to_vec(),
    })
    .await;
    assert!(matches!(
        old.frame().await,
        ServerFrame::Error {
            code: ErrorCode::Superseded,
            fatal: true,
            ..
        }
    ));
    assert_eq!(old.recv().await.close_code(), Some(CloseCode::Policy));
    assert_eq!(new.events(events[2..].to_vec()).await.0, agent.seq);
}

#[tokio::test]
async fn gaps_ask_for_a_resend_without_closing() {
    let h = Harness::start().await;
    let mut agent = Agent::new("6", "e2e");
    let events = agent.suite(&[("login", true)]);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    c.send(&ClientFrame::Events {
        events: events[3..].to_vec(),
    })
    .await;
    assert!(matches!(
        c.frame().await,
        ServerFrame::Error {
            code: ErrorCode::SeqGap,
            fatal: false,
            expected_seq: Some(1),
            ..
        }
    ));
    assert_eq!(
        c.events(events).await.0,
        agent.seq,
        "the same socket carries on"
    );
}

fn gap_marker(seq: u64, from: u64, to: u64) -> EventEnvelope {
    EventEnvelope::new(
        seq,
        ClockSample {
            wall: OffsetDateTime::now_utc(),
            mono_ms: 0,
        },
        Subject::default(),
        Provenance::new(SourceKind::AgentObserved),
        &EventBody::JournalGap(JournalGap {
            from_seq: from,
            to_seq: to,
            reason: "spool_full".into(),
        }),
    )
}

#[tokio::test]
async fn a_journal_gap_sequenced_in_its_hole_moves_the_cursor_past_it() {
    let h = Harness::start().await;
    let mut agent = Agent::new("8", "e2e");
    let events = agent.suite(&[("login", true), ("pay", true), ("logout", true)]);
    assert!(events.len() > 8);
    let mut c = h.connect(None).await;
    c.hello(&agent, Some(1)).await;
    // The agent dropped events 4..=6: it keeps their numbers and sequences the marker at 6.
    let mut batch = events[..3].to_vec();
    batch.push(gap_marker(6, 4, 6));
    batch.extend_from_slice(&events[6..]);
    assert_eq!(c.events(batch).await, (agent.seq, 0));
    // A marker that does not start right after the cursor is a plain sequence gap.
    let next = agent.seq + 1;
    c.send(&ClientFrame::Events {
        events: vec![gap_marker(next + 4, next + 1, next + 4)],
    })
    .await;
    assert!(matches!(
        c.frame().await,
        ServerFrame::Error {
            code: ErrorCode::SeqGap,
            fatal: false,
            expected_seq: Some(e),
            ..
        } if e == next
    ));
}

#[tokio::test]
async fn bad_credentials_and_protocols_are_refused_for_good() {
    let h = Harness::start().await;
    let agent = Agent::new("7", "e2e");
    for token in [None, Some("mf_live_wrong")] {
        let mut c = Client::connect(h.addr(), token, true).await;
        assert!(matches!(
            c.frame().await,
            ServerFrame::Reject {
                code: ErrorCode::Unauthorized,
                fatal: true,
                ..
            }
        ));
        assert_eq!(c.recv().await.close_code(), Some(CloseCode::Policy));
    }
    let mut c = Client::connect(h.addr(), Some(&h.key), false).await;
    assert!(matches!(
        c.frame().await,
        ServerFrame::Reject {
            code: ErrorCode::UnsupportedProtocol,
            ..
        }
    ));
    let mut c = h.connect(None).await;
    let mut hello = agent.hello(None);
    hello.protocol.min = PROTOCOL_VERSION + 1;
    hello.protocol.max = PROTOCOL_VERSION + 1;
    c.send(&ClientFrame::Hello(Box::new(hello))).await;
    assert!(matches!(
        c.frame().await,
        ServerFrame::Reject {
            code: ErrorCode::UnsupportedProtocol,
            fatal: true,
            ..
        }
    ));
}

#[tokio::test]
async fn a_silent_socket_is_closed_and_its_run_detached() {
    let h = Harness::start_with(|c| {
        c.ingest.idle_timeout_ms = 1_200;
        c.ingest.heartbeat_interval_ms = 400;
        // Pongs prove the peer is alive too; keep pings out of the way to test silence.
        c.ingest.ping_interval_ms = 60_000;
    })
    .await;
    let mut agent = Agent::new("8", "e2e");
    let mut c = h.connect(None).await;
    c.hello(&agent, None).await;
    // Heartbeats keep it alive past the idle timeout.
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        c.send(&ClientFrame::Heartbeat {
            clock: ClockSample {
                wall: OffsetDateTime::now_utc(),
                mono_ms: 1,
            },
            unacked: 0,
        })
        .await;
    }
    let e = agent.event(
        Subject::default(),
        SourceKind::AgentObserved,
        &EventBody::RunSignal(RunSignal { signal: 15 }),
    );
    assert_eq!(c.events(vec![e]).await.0, 1, "still connected");
    assert_eq!(
        c.recv().await.close_code(),
        Some(CloseCode::Away),
        "then silence closes it"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut again = h.connect(None).await;
    assert_eq!(
        welcome(&again.hello(&agent, None).await),
        (1, 2, ResumeOutcome::Resumed)
    );
}

#[tokio::test]
async fn the_batch_fallback_speaks_the_same_protocol() {
    let h = Harness::start().await;
    let mut agent = Agent::new("9", "e2e");
    let events = agent.suite(&[("login", true)]);
    let req = BatchRequest {
        hello: agent.hello(Some(1)),
        events: events[..3].to_vec(),
        telemetry: vec![],
    };
    let (status, body) = post_json(
        h.addr(),
        "/api/v1/agent/batch",
        Some(&h.key),
        &serde_json::to_string(&req).unwrap(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let r: BatchResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(welcome(&r.frames[0]).0, 0);
    assert!(matches!(r.frames[1], ServerFrame::Ack { through: 3, .. }));

    // The next post resends from the start: duplicates are acked again, nothing is applied twice.
    let req = BatchRequest {
        hello: agent.hello(Some(1)),
        events: events.clone(),
        telemetry: vec![],
    };
    let (_, body) = post_json(
        h.addr(),
        "/api/v1/agent/batch",
        Some(&h.key),
        &serde_json::to_string(&req).unwrap(),
    )
    .await;
    let r: BatchResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(welcome(&r.frames[0]).0, 3);
    assert!(matches!(
        r.frames[1],
        ServerFrame::Ack { through, duplicates: 3, .. } if through == agent.seq
    ));

    let (status, body) = post_json(
        h.addr(),
        "/api/v1/agent/batch",
        None,
        &serde_json::to_string(&req).unwrap(),
    )
    .await;
    assert_eq!(status, 401);
    assert!(body.contains("unauthorized"), "{body}");
}

#[tokio::test]
async fn health_endpoints_follow_readiness() {
    let mut h = Harness::start().await;
    let addr = h.addr();
    let get = |path: &'static str| async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        out
    };
    assert!(get("/livez").await.starts_with("HTTP/1.1 200"));
    assert!(get("/readyz").await.starts_with("HTTP/1.1 200"));
    fleet_server::run(
        fleet_server::Cli {
            config: None,
            command: fleet_server::Command::Healthcheck,
        },
        vec![("FLEET_LISTEN".into(), addr.to_string())],
    )
    .await
    .unwrap();
    h.stop().await;
    assert!(
        fleet_server::run(
            fleet_server::Cli {
                config: None,
                command: fleet_server::Command::Healthcheck,
            },
            vec![("FLEET_LISTEN".into(), addr.to_string())],
        )
        .await
        .is_err(),
        "a stopped server is not healthy"
    );
}
