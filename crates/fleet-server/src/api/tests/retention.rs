// SPDX-License-Identifier: AGPL-3.0-only
//! Retention and garbage collection (seam S10) through the real router and a real blob
//! directory: artifacts come in through the upload protocol, time is injected into the job
//! functions the leader calls (`now` is a parameter, nothing sleeps), and the effects are read
//! back the way the dashboard and an operator would see them: the flow's artifact list, the
//! blob directory, the metrics endpoint.

use std::fs::File;
use std::time::{Duration, SystemTime};

use fleet_blob::LocalFs;
use fleet_protocol::{artifact::UploadResult, event::FlowOutcome, frame::Sample};
use serde_json::Value;
use time::{Duration as Time, OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use super::{
    artifacts::{Dirs, artifact_rig, request, stored_blobs},
    rig::{Agent, Rig, sample, telemetry},
};
use crate::{config::Retention, retention};

/// An agent run whose one flow passed.
struct PassingRun {
    run: Uuid,
    flow: Uuid,
}

async fn passing_run(rig: &Rig, ci_run: &str) -> PassingRun {
    let mut agent = Agent::new(ci_run, "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let flow = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&flow, FlowOutcome::Passed, None);
    rig.publish(&mut agent, vec![]).await;
    PassingRun {
        run: agent.id,
        flow: flow.subject.flow_execution_id.unwrap(),
    }
}

/// A JUnit report: valid as `junit`, and plain text, so also valid as a Maestro log.
const XML: &[u8] = b"<?xml version=\"1.0\"?><testsuites/>";

fn run_level(
    run: &PassingRun,
    class: &str,
    ctype: &str,
    bytes: &[u8],
) -> fleet_protocol::artifact::UploadRequest {
    let mut req = request(
        &super::artifacts::Run {
            run: run.run,
            flow: run.flow,
        },
        class,
        "report/file",
        ctype,
        bytes,
    );
    req.flow_execution_id = None;
    req
}

async fn available(rig: &Rig, req: &fleet_protocol::artifact::UploadRequest, bytes: &[u8]) -> Uuid {
    match rig.upload(req, bytes).await {
        UploadResult::Available { artifact_id } => artifact_id,
        other @ UploadResult::Rejected { .. } => panic!("the upload was refused: {other:?}"),
    }
}

fn store_of(dirs: &Dirs) -> LocalFs {
    LocalFs::new(&dirs.blobs).unwrap()
}

async fn collect(rig: &Rig, dirs: &Dirs, now: OffsetDateTime) {
    retention::collect_garbage(&rig.store, &store_of(dirs), &Retention::default(), now).await;
}

#[tokio::test]
async fn an_expired_artifact_leaves_the_flow_and_its_blob_is_deleted_after_the_grace_period() {
    let (rig, dirs) = artifact_rig(|_| {}).await;
    let run = passing_run(&rig, "700").await;
    let mut req = run_level(&run, "maestro_log", "text/plain", b"log line\n");
    req.flow_execution_id = Some(run.flow);
    available(&rig, &req, b"log line\n").await;
    assert_eq!(
        rig.artifacts_of(run.flow).await["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let now = OffsetDateTime::now_utc();

    // Within its 30 days (a Maestro log of a passing run): untouched.
    collect(&rig, &dirs, now + Time::days(29)).await;
    assert_eq!(
        rig.artifacts_of(run.flow).await["artifacts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    // Past it, the artifact is gone from the flow, but the bytes wait out the grace period.
    let expired = now + Time::days(31);
    collect(&rig, &dirs, expired).await;
    assert_eq!(
        rig.artifacts_of(run.flow).await["artifacts"],
        Value::Array(vec![])
    );
    assert_eq!(
        stored_blobs(&dirs).await.len(),
        1,
        "within the grace period"
    );
    collect(&rig, &dirs, expired + Time::hours(23)).await;
    assert_eq!(
        stored_blobs(&dirs).await.len(),
        1,
        "still within the grace period"
    );

    collect(&rig, &dirs, expired + Time::hours(25)).await;
    assert_eq!(stored_blobs(&dirs).await, [], "the object is deleted");
}

#[tokio::test]
async fn a_blob_two_artifacts_share_survives_until_the_last_of_them_expires() {
    let (rig, dirs) = artifact_rig(|_| {}).await;
    let run = passing_run(&rig, "710").await;
    // The same bytes as a log (30 days) and as a report (1 year): one blob.
    let log = run_level(&run, "maestro_log", "text/plain", XML);
    let report = run_level(&run, "junit", "application/xml", XML);
    available(&rig, &log, XML).await;
    available(&rig, &report, XML).await;
    assert_eq!(stored_blobs(&dirs).await.len(), 1);
    let now = OffsetDateTime::now_utc();

    // The log expires; the report still needs the bytes, however long we wait.
    collect(&rig, &dirs, now + Time::days(40)).await;
    collect(&rig, &dirs, now + Time::days(60)).await;
    assert_eq!(
        stored_blobs(&dirs).await.len(),
        1,
        "the report still refers to it"
    );
    let listed = rig.artifacts_of(run.flow).await;
    let classes: Vec<&str> = listed["artifacts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["class"].as_str().unwrap())
        .collect();
    assert_eq!(classes, ["junit"]);

    // The report expires (a year); its grace period passes; now the object goes.
    let year = now + Time::days(366);
    collect(&rig, &dirs, year).await;
    assert_eq!(stored_blobs(&dirs).await.len(), 1, "grace period");
    collect(&rig, &dirs, year + Time::hours(25)).await;
    assert_eq!(stored_blobs(&dirs).await, []);
}

#[tokio::test]
async fn leftover_temp_files_of_crashed_uploads_are_swept_but_a_fresh_one_is_left_alone() {
    let (rig, dirs) = artifact_rig(|_| {}).await;
    let tmp = dirs.blobs.join(".tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    for (name, age) in [("crashed", 3 * 86_400), ("uploading", 5)] {
        File::create(tmp.join(name))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(age))
            .unwrap();
    }

    collect(&rig, &dirs, OffsetDateTime::now_utc()).await;

    let mut left: Vec<_> = std::fs::read_dir(&tmp)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    assert_eq!(left, ["uploading"]);
}

fn ms(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap()
}

/// Twelve samples (5 s apart) in the minute starting at `minute`.
fn minute_of_samples(minute: OffsetDateTime, make: impl Fn(&mut Sample, usize)) -> Vec<Sample> {
    (0..12)
        .map(|i| {
            let mut s = sample(minute + Time::seconds(5 * i64::try_from(i).unwrap()), 0.0);
            make(&mut s, i);
            s
        })
        .collect()
}

#[tokio::test]
async fn a_three_day_range_is_served_from_rollups_and_missing_readings_stay_missing() {
    let rig = super::rig::rig().await;
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    // Yesterday-ish (inside the 24 h clock-skew window of ingest, 3 minutes apart so the
    // minutes are not adjacent): three minutes of samples.
    let m0 = (now - Time::hours(20)).replace_second(0).unwrap();
    let (m1, m2) = (m0 + Time::minutes(1), m0 + Time::minutes(3));
    let mut samples = minute_of_samples(m0, |s, i| {
        s.host_cpu_pct = Some(if i % 2 == 0 { 10.0 } else { 30.0 });
    });
    // Minute two: Maestro reported nothing at all.
    samples.extend(minute_of_samples(m1, |s, _| {
        s.host_cpu_pct = Some(50.0);
        s.maestro_cpu_pct = None;
    }));
    // Minute three: Maestro reported in the last four samples only.
    samples.extend(minute_of_samples(m2, |s, i| {
        s.host_cpu_pct = Some(60.0);
        s.maestro_cpu_pct = (i >= 8).then_some(40.0);
    }));
    let mut agent = Agent::new("720", "build", "runner-a");
    rig.publish(&mut agent, vec![telemetry(1, samples)]).await;
    let overview = rig.json("/api/v1/projects/app/overview").await;
    let worker = overview["workers"][0]["id"].as_str().unwrap().to_owned();

    // The leader's rollup job, at a time when those minutes are long closed.
    rig.store.roll_up(now).await.unwrap();

    let path = format!(
        "/api/v1/projects/app/workers/{worker}/metrics?from={}&to={}&metrics=host_cpu_pct,maestro_cpu_pct",
        (now - Time::days(3)).format(&Rfc3339).unwrap(),
        now.format(&Rfc3339).unwrap()
    );
    let view = rig.json(&path).await;

    assert_eq!(
        view["resolution"], "1m",
        "three days are served from minute rollups"
    );
    let series = |metric: &str| -> Vec<(i64, Option<f64>, bool)> {
        view["series"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["metric"] == metric)
            .unwrap()["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["at_ms"].as_i64().unwrap(),
                    p["value"].as_f64(),
                    p["available"].as_bool().unwrap(),
                )
            })
            .collect()
    };
    // One point per minute that has samples, nothing in between (no zero-filled gaps); the
    // value is the average of the readings that exist.
    assert_eq!(
        series("host_cpu_pct"),
        [
            (ms(m0), Some(20.0), true),
            (ms(m1), Some(50.0), true),
            (ms(m2), Some(60.0), true)
        ]
    );
    // Maestro CPU: 80 % in minute one; unavailable (not 0) in minute two; the average of the
    // four readings there are in minute three.
    assert_eq!(
        series("maestro_cpu_pct"),
        [
            (ms(m0), Some(80.0), true),
            (ms(m1), None, false),
            (ms(m2), Some(40.0), true)
        ]
    );
    let cpu = view["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["metric"] == "maestro_cpu_pct")
        .unwrap();
    assert_eq!(cpu["aggregate"], "avg");
}
