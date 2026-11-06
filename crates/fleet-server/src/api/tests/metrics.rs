// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C's series: `GET …/workers/{id}/metrics` and `GET …/devices/{id}/metrics` (acceptance
//! criteria 4 and 5: labels, units, and "unavailable" instead of invented values).

use fleet_protocol::{event::FlowOutcome, frame::Sample};
use serde_json::{Value, json};
use time::{Duration, OffsetDateTime, format_description::well_known::Rfc3339};

use super::rig::{Agent, Rig, rig, sample, telemetry};

/// The start of the test's samples: whole seconds, ten minutes ago, the same for the whole
/// test (each test runs on a thread of its own).
fn base() -> OffsetDateTime {
    thread_local! {
        static BASE: OffsetDateTime =
            OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::minutes(10);
    }
    BASE.with(|b| *b)
}

fn ms(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).unwrap()
}

/// `?from=…&to=…` around the samples, plus `extra` parameters.
fn range(extra: &str) -> String {
    let from = (base() - Duration::minutes(1)).format(&Rfc3339).unwrap();
    let to = (base() + Duration::minutes(30)).format(&Rfc3339).unwrap();
    format!("?from={from}&to={to}{extra}")
}

/// A worker that published `samples`, one every 5 s from [`base`]; returns its ID.
async fn worker_with(rig: &Rig, samples: Vec<Sample>) -> String {
    let mut agent = Agent::new("600", "build", "runner-a");
    rig.publish(&mut agent, vec![telemetry(1, samples)]).await;
    let overview = rig.json("/api/v1/projects/app/overview").await;
    overview["workers"][0]["id"].as_str().unwrap().to_owned()
}

fn at(i: usize) -> OffsetDateTime {
    base() + Duration::seconds(5 * i64::try_from(i).unwrap())
}

fn series<'a>(view: &'a Value, metric: &str) -> &'a Value {
    view["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["metric"] == metric)
        .unwrap_or_else(|| panic!("no series {metric} in {view}"))
}

#[tokio::test]
async fn worker_series_carry_label_unit_scope_and_every_reading_in_time_order() {
    let rig = rig().await;
    let worker = worker_with(
        &rig,
        [10.0, 20.0, 30.0]
            .into_iter()
            .enumerate()
            .map(|(i, cpu)| sample(at(i), cpu))
            .collect(),
    )
    .await;

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/workers/{worker}/metrics{}",
            range("&metrics=host_cpu_pct,host_mem_used_bytes")
        ))
        .await;

    assert_eq!(view["subject"], json!({"kind": "worker", "id": worker}));
    assert_eq!(view["resolution"], "raw");
    assert_eq!(view["truncated"], false);
    assert_eq!(view["series"].as_array().unwrap().len(), 2);
    let cpu = series(&view, "host_cpu_pct");
    assert_eq!(cpu["label"], "Host CPU");
    assert_eq!(cpu["unit"], "percent");
    assert_eq!(cpu["scope"], "host");
    assert_eq!(cpu["available"], true);
    let readings: Vec<(i64, f64)> = cpu["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["at_ms"].as_i64().unwrap(), p["value"].as_f64().unwrap()))
        .collect();
    assert_eq!(
        readings,
        [(ms(at(0)), 10.0), (ms(at(1)), 20.0), (ms(at(2)), 30.0)]
    );
    assert_eq!(cpu["points"][0]["available"], true);
    let mem = series(&view, "host_mem_used_bytes");
    assert_eq!(mem["label"], "Host memory used");
    assert_eq!(mem["unit"], "bytes");
    assert_eq!(mem["points"][0]["value"].as_f64(), Some(4_294_967_296.0));
}

#[tokio::test]
async fn a_metric_the_agent_never_reports_is_unavailable_with_a_reason_never_a_value() {
    let rig = rig().await;
    let worker = worker_with(&rig, vec![sample(at(0), 10.0), sample(at(1), 20.0)]).await;

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/workers/{worker}/metrics{}",
            range("&metrics=host_thermal_c")
        ))
        .await;

    let thermal = series(&view, "host_thermal_c");
    assert_eq!(thermal["label"], "Host temperature");
    assert_eq!(thermal["unit"], "celsius");
    assert_eq!(thermal["available"], false);
    assert_eq!(
        thermal["reason"],
        "the agent does not report this metric yet"
    );
    assert_eq!(thermal["points"], json!([]), "no point, no fake value");
}

#[tokio::test]
async fn a_sample_without_a_reading_is_an_unavailable_point_not_a_zero() {
    let rig = rig().await;
    let mut samples: Vec<Sample> = (0..3).map(|i| sample(at(i), 10.0)).collect();
    samples[1].maestro_cpu_pct = None;
    let worker = worker_with(&rig, samples).await;

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/workers/{worker}/metrics{}",
            range("&metrics=maestro_cpu_pct")
        ))
        .await;

    let cpu = series(&view, "maestro_cpu_pct");
    assert_eq!(cpu["scope"], "maestro");
    assert_eq!(cpu["available"], true, "two of three readings exist");
    let points = cpu["points"].as_array().unwrap();
    assert_eq!(points.len(), 3);
    assert_eq!(points[1]["at_ms"], ms(at(1)));
    assert_eq!(points[1]["available"], false);
    assert_eq!(points[1]["value"], json!(null));
    assert_eq!(points[1]["reason"], "the agent reported no value");
    assert_eq!(points[0]["value"], 80.0);
}

#[tokio::test]
async fn a_range_without_samples_says_so_for_every_metric() {
    let rig = rig().await;
    let worker = worker_with(&rig, vec![sample(at(0), 10.0)]).await;
    let long_ago = (base() - Duration::hours(5)).format(&Rfc3339).unwrap();
    let a_bit_later = (base() - Duration::hours(4)).format(&Rfc3339).unwrap();

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/workers/{worker}/metrics?from={long_ago}&to={a_bit_later}"
        ))
        .await;

    let all = view["series"].as_array().unwrap();
    assert_eq!(all.len(), 15, "every host metric is listed: {view}");
    for s in all {
        assert_eq!(s["available"], false, "{s}");
        assert_eq!(s["points"], json!([]));
    }
    assert_eq!(
        series(&view, "host_cpu_pct")["reason"],
        "no samples in this range"
    );
}

/// Load, cores, swap and pressure reach the store through ingest and come back as series and as
/// the worker's latest readings; a sample that predates them has no value, not a zero.
#[tokio::test]
async fn load_cores_swap_and_pressure_round_trip_and_are_unavailable_when_absent() {
    let rig = rig().await;
    let mut samples: Vec<Sample> = (0..3).map(|i| sample(at(i), 10.0)).collect();
    samples[2].host_load_1m = Some(1.5);
    samples[2].host_cpu_cores = Some(8);
    samples[2].host_swap_used_bytes = Some(1 << 30);
    samples[2].host_swap_total_bytes = Some(2 << 30);
    samples[2].host_psi_cpu_pct = Some(0.25);
    let worker = worker_with(&rig, samples).await;
    let metrics = "host_load_1m,host_cpu_cores,host_swap_used_bytes,host_swap_total_bytes,\
                   host_psi_cpu_pct,host_psi_memory_pct";

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/workers/{worker}/metrics{}",
            range(&format!("&metrics={metrics}"))
        ))
        .await;

    let load = series(&view, "host_load_1m");
    assert_eq!(load["label"], "Host load (1 min)");
    assert_eq!(load["unit"], "number");
    assert_eq!(load["scope"], "host");
    assert_eq!(
        load["points"][0]["available"], false,
        "older sample: no value"
    );
    assert_eq!(load["points"][2]["value"], 1.5);
    assert_eq!(series(&view, "host_cpu_cores")["points"][2]["value"], 8.0);
    assert_eq!(
        series(&view, "host_swap_used_bytes")["points"][2]["value"],
        1_073_741_824.0
    );
    let psi = series(&view, "host_psi_cpu_pct");
    assert_eq!(psi["label"], "Host CPU pressure");
    assert_eq!(psi["unit"], "percent");
    assert_eq!(psi["points"][2]["value"], 0.25);
    let memory_psi = series(&view, "host_psi_memory_pct");
    assert_eq!(memory_psi["available"], false, "never reported: not zero");
    assert_eq!(
        memory_psi["reason"],
        "the agent reported no value in this range"
    );

    let overview = rig.json("/api/v1/projects/app/overview").await;
    let readings = overview["workers"][0]["latest"]["readings"]
        .as_array()
        .unwrap();
    let latest = |metric: &str| {
        readings
            .iter()
            .find(|r| r["metric"] == metric)
            .unwrap_or_else(|| panic!("no reading {metric}"))
            .clone()
    };
    assert_eq!(latest("host_cpu_cores")["value"], 8.0);
    assert_eq!(latest("host_cpu_cores")["unit"], "number");
    assert_eq!(latest("host_load_1m")["value"], 1.5);
    assert_eq!(latest("host_psi_io_pct")["available"], false);
}

#[tokio::test]
async fn a_long_series_is_cut_to_the_requested_points_keeping_first_last_and_the_spike() {
    let rig = rig().await;
    let samples: Vec<Sample> = (0..100)
        .map(|i| sample(at(i), if i == 47 { 99.0 } else { 10.0 }))
        .collect();
    let worker = worker_with(&rig, samples).await;

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/workers/{worker}/metrics{}",
            range("&metrics=host_cpu_pct&points=10")
        ))
        .await;

    let points = series(&view, "host_cpu_pct")["points"]
        .as_array()
        .unwrap()
        .clone();
    assert!(points.len() <= 10, "{} points", points.len());
    assert_eq!(points.first().unwrap()["at_ms"], ms(at(0)), "first kept");
    assert_eq!(points.last().unwrap()["at_ms"], ms(at(99)), "last kept");
    assert!(
        points.iter().any(|p| p["value"] == 99.0),
        "the spike survives downsampling: {points:?}"
    );
    let times: Vec<i64> = points
        .iter()
        .map(|p| p["at_ms"].as_i64().unwrap())
        .collect();
    assert!(
        times.windows(2).all(|w| w[0] < w[1]),
        "ascending: {times:?}"
    );
}

#[tokio::test]
async fn a_gap_of_missing_readings_survives_downsampling_as_one_unavailable_marker() {
    let rig = rig().await;
    let samples: Vec<Sample> = (0..60)
        .map(|i| {
            let mut s = sample(at(i), 10.0);
            if (20..30).contains(&i) {
                s.maestro_cpu_pct = None;
            }
            s
        })
        .collect();
    let worker = worker_with(&rig, samples).await;

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/workers/{worker}/metrics{}",
            range("&metrics=maestro_cpu_pct&points=12")
        ))
        .await;

    let points = series(&view, "maestro_cpu_pct")["points"]
        .as_array()
        .unwrap()
        .clone();
    assert!(points.len() <= 12, "{} points", points.len());
    let gaps: Vec<&Value> = points.iter().filter(|p| p["available"] == false).collect();
    assert_eq!(gaps.len(), 1, "one marker for the whole gap: {points:?}");
    assert_eq!(gaps[0]["at_ms"], ms(at(20)), "where the gap begins");
    assert_eq!(gaps[0]["value"], json!(null));
    assert!(
        points
            .iter()
            .all(|p| p["available"] == false || p["value"] == 80.0),
        "no value that wasn't reported"
    );
}

#[tokio::test]
async fn the_source_follows_the_range_raw_up_to_a_day_then_minutes_then_hours() {
    let rig = rig().await;
    let worker = worker_with(&rig, vec![sample(at(0), 10.0)]).await;
    let resolution = |from: Duration| {
        let rig = &rig;
        let worker = &worker;
        async move {
            let to = OffsetDateTime::now_utc();
            let path = format!(
                "/api/v1/projects/app/workers/{worker}/metrics?from={}&to={}",
                (to - from).format(&Rfc3339).unwrap(),
                to.format(&Rfc3339).unwrap()
            );
            rig.json(&path).await["resolution"].clone()
        }
    };

    assert_eq!(resolution(Duration::hours(24)).await, "raw");
    assert_eq!(resolution(Duration::hours(25)).await, "1m");
    assert_eq!(resolution(Duration::days(30)).await, "1m");
    assert_eq!(resolution(Duration::days(31)).await, "1h");
}

#[tokio::test]
async fn bad_metric_requests_are_refused_with_a_reason() {
    let rig = rig().await;
    let worker = worker_with(&rig, vec![sample(at(0), 10.0)]).await;
    let base_path = format!("/api/v1/projects/app/workers/{worker}/metrics");
    let expect_400 = |query: String, message: &'static str| {
        let rig = &rig;
        let path = format!("{base_path}{query}");
        async move {
            let res = rig.get(&path).await;
            assert_eq!(res.status_code(), 400, "{path}");
            let body: Value = res.json();
            assert!(
                body["error"].as_str().unwrap().contains(message),
                "{path}: {body}"
            );
        }
    };

    expect_400(range("&metrics=host_cpu"), "unknown metric host_cpu").await;
    expect_400(range("&points=3"), "points must be between 4 and 5000").await;
    expect_400("?from=yesterday".into(), "from must be an RFC 3339 time").await;
    let t = base().format(&Rfc3339).unwrap();
    expect_400(format!("?from={t}&to={t}"), "from must be before to").await;
    // A device metric on a worker is not a worker metric.
    expect_400(range("&metrics=emu_cpu_pct"), "unknown metric emu_cpu_pct").await;
}

#[tokio::test]
async fn an_unknown_worker_has_no_metrics() {
    let rig = rig().await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/workers/{}/metrics",
            uuid::Uuid::now_v7()
        ))
        .await;

    assert_eq!(res.status_code(), 404);
    assert_eq!(res.json::<Value>(), json!({"error": "no such worker"}));
}

#[tokio::test]
async fn device_series_are_unavailable_when_no_device_readings_were_reported() {
    let rig = rig().await;
    let mut agent = Agent::new("610", "build", "runner-a");
    agent.started();
    let shard = agent.shard("emulator-5554", &["login"]);
    let login = agent.flow_started(&shard, "login", 0);
    agent.flow_ended(&login, FlowOutcome::Passed, None);
    rig.publish(&mut agent, vec![]).await;
    let overview = rig.json("/api/v1/projects/app/overview").await;
    let worker = overview["workers"][0]["id"].as_str().unwrap().to_owned();
    let worker = rig
        .json(&format!("/api/v1/projects/app/workers/{worker}"))
        .await;
    let device = worker["devices"][0]["device"]["id"].as_str().unwrap();

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/devices/{device}/metrics?from={}&to={}",
            (OffsetDateTime::now_utc() - Duration::hours(1))
                .format(&Rfc3339)
                .unwrap(),
            OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
        ))
        .await;

    assert_eq!(view["subject"], json!({"kind": "device", "id": device}));
    let ids: Vec<&str> = view["series"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["metric"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "emu_cpu_pct",
            "emu_rss_bytes",
            "guest_cpu_pct",
            "guest_mem_available_bytes",
            "device_thermal_c"
        ]
    );
    let guest = series(&view, "guest_cpu_pct");
    assert_eq!(guest["scope"], "guest", "never mixed with host CPU");
    assert_eq!(guest["label"], "Guest CPU");
    assert_eq!(guest["available"], false);
    assert_eq!(guest["reason"], "no samples in this range");
    assert_eq!(
        series(&view, "device_thermal_c")["reason"],
        "the agent does not report this metric yet"
    );
    for s in view["series"].as_array().unwrap() {
        assert_eq!(s["points"], json!([]));
    }
}

#[tokio::test]
async fn an_unknown_device_has_no_metrics() {
    let rig = rig().await;

    let res = rig
        .get(&format!(
            "/api/v1/projects/app/devices/{}/metrics",
            uuid::Uuid::now_v7()
        ))
        .await;

    assert_eq!(res.status_code(), 404);
    assert_eq!(res.json::<Value>(), json!({"error": "no such device"}));
}
