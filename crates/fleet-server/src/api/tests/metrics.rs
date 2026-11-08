// SPDX-License-Identifier: AGPL-3.0-only
//! Screen C's series: `GET …/workers/{id}/metrics` and `GET …/devices/{id}/metrics` (acceptance
//! criteria 4 and 5: labels, units, and "unavailable" instead of invented values).

use fleet_protocol::{
    event::{AdbState, FlowOutcome},
    frame::{DeviceSample, DeviceUnavailable, Sample, SampleUnavailable, Telemetry, Unavailable},
    journal::ClockSample,
};
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

/// The runner's newest sample says why each host reading is unknown, in words for the machine: no
/// pressure information on macOS, a probe that failed, and the first CPU sample. A reading the agent
/// sent without a reason says that no value was reported, and the metric it never reports keeps its
/// own reason.
#[tokio::test]
async fn the_latest_host_sample_says_why_each_unknown_reading_is_unknown() {
    let rig = rig().await;
    let mut newest = sample(at(0), 10.0);
    newest.host_psi_cpu_pct = None;
    newest.host_psi_memory_pct = None;
    newest.host_psi_io_pct = None;
    newest.host_load_1m = None;
    newest.host_cpu_cores = None;
    newest.unavailable = SampleUnavailable {
        host_psi_cpu_pct: Some(Unavailable::NoPsi),
        host_psi_memory_pct: Some(Unavailable::NoPsi),
        host_psi_io_pct: Some(Unavailable::NoPsi),
        host_load_1m: Some(Unavailable::ProbeFailed),
        ..SampleUnavailable::default()
    };
    let worker = worker_with(&rig, vec![newest]).await;
    let overview = rig.json("/api/v1/projects/app/overview").await;
    assert_eq!(overview["workers"][0]["id"], worker);
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

    assert_eq!(
        latest("host_psi_cpu_pct")["reason"],
        "the kernel has no pressure information (PSI), as on macOS"
    );
    assert_eq!(latest("host_psi_io_pct")["available"], false);
    assert_eq!(
        latest("host_load_1m")["reason"],
        "the probe failed: the agent could not read it"
    );
    assert_eq!(
        latest("host_cpu_cores")["reason"],
        "the agent reported no value",
        "a reading with no reason from an older agent says so"
    );
    assert_eq!(
        latest("host_thermal_c")["reason"],
        "the agent does not report this metric yet"
    );
    let cpu = latest("host_cpu_pct");
    assert_eq!(
        (cpu["value"].as_f64(), cpu.get("reason")),
        (Some(10.0), None)
    );
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
            "adb_state",
            "boot_completed",
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
async fn device_samples_the_agent_sent_are_served_for_their_device_session() {
    let rig = rig().await;
    let mut agent = Agent::new("620", "build", "runner-a");
    agent.started();
    let device = agent
        .shard("emulator-5584", &["login"])
        .device_session_id
        .expect("a shard names its device session");
    let t0 = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::minutes(5);
    let device_at = |secs: i64| DeviceSample {
        model: None,
        os_version: None,
        at: ClockSample {
            wall: t0 + Duration::seconds(secs),
            mono_ms: 0,
        },
        device_session_id: device,
        adb_state: Some(AdbState::Device),
        boot_completed: Some(true),
        guest_cpu_pct: Some(12.5),
        guest_mem_available_bytes: Some(2 << 30),
        emu_cpu_pct: None,
        emu_rss_bytes: None,
        avd_name: None,
        api_level: None,
        foreground_activity: None,
        shard: None,
        unavailable: DeviceUnavailable::default(),
    };
    let later = DeviceSample {
        model: None,
        os_version: None,
        guest_cpu_pct: Some(40.0),
        guest_mem_available_bytes: None,
        ..device_at(30)
    };
    // A probe that did not answer: adb's state only, every reading unknown.
    let unknown = DeviceSample {
        model: None,
        os_version: None,
        adb_state: None,
        boot_completed: None,
        guest_cpu_pct: None,
        guest_mem_available_bytes: None,
        ..device_at(60)
    };
    rig.publish(
        &mut agent,
        vec![Telemetry {
            tseq_from: 1,
            tseq_to: 1,
            dropped_since_last: 0,
            samples: vec![super::rig::sample(t0, 10.0)],
            devices: vec![device_at(0), later, unknown],
        }],
    )
    .await;

    let view = rig
        .json(&format!(
            "/api/v1/projects/app/devices/{device}/metrics?from={}&to={}",
            (t0 - Duration::minutes(1)).format(&Rfc3339).unwrap(),
            (t0 + Duration::minutes(10)).format(&Rfc3339).unwrap()
        ))
        .await;

    let values = |metric: &str| -> Vec<Value> {
        series(&view, metric)["points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["value"].clone())
            .collect()
    };
    let guest = series(&view, "guest_cpu_pct");
    assert_eq!(
        (guest["available"].clone(), guest["scope"].clone()),
        (json!(true), json!("guest"))
    );
    assert_eq!(
        values("guest_cpu_pct"),
        [json!(12.5), json!(40.0), Value::Null]
    );
    assert_eq!(
        series(&view, "guest_cpu_pct")["points"][2]["reason"],
        "the agent reported no value"
    );
    assert_eq!(
        values("guest_mem_available_bytes"),
        [json!(2_147_483_648.0), Value::Null, Value::Null]
    );
    assert_eq!(values("adb_state"), [json!(0.0), json!(0.0), Value::Null]);
    assert_eq!(
        values("boot_completed"),
        [json!(1.0), json!(1.0), Value::Null]
    );
}

/// The overview's "active emulators" readout (issue #123): an emulator counts when adb reported it
/// online in the last two minutes. A physical device, an offline emulator and a stale one do not.
#[tokio::test]
async fn the_overview_counts_the_emulators_online_now() {
    let rig = rig().await;
    let mut agent = Agent::new("621", "build", "runner-a");
    agent.started();
    let mut session = |serial: &str| {
        agent
            .shard(serial, &["login"])
            .device_session_id
            .expect("a shard names its device session")
    };
    let online = session("emulator-5584");
    let physical = session("R58M");
    let offline = session("emulator-5586");
    let stale = session("emulator-5588");
    let now = OffsetDateTime::now_utc();
    let at = |session, adb_state, seconds_ago: i64| DeviceSample {
        model: None,
        os_version: None,
        at: ClockSample {
            wall: now - Duration::seconds(seconds_ago),
            mono_ms: 0,
        },
        device_session_id: session,
        adb_state: Some(adb_state),
        boot_completed: None,
        guest_cpu_pct: None,
        guest_mem_available_bytes: None,
        emu_cpu_pct: None,
        emu_rss_bytes: None,
        avd_name: None,
        api_level: None,
        foreground_activity: None,
        shard: None,
        unavailable: DeviceUnavailable::default(),
    };
    rig.publish(
        &mut agent,
        vec![Telemetry {
            tseq_from: 1,
            tseq_to: 1,
            dropped_since_last: 0,
            samples: Vec::new(),
            devices: vec![
                at(online, AdbState::Device, 5),
                at(physical, AdbState::Device, 5),
                at(offline, AdbState::Offline, 5),
                at(stale, AdbState::Device, 10 * 60),
            ],
        }],
    )
    .await;

    let overview = rig.json("/api/v1/projects/app/overview").await;
    assert_eq!(overview["active_emulators"], 1);
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

/// `(at_ms, value)` of every point of a series.
fn points(s: &Value) -> Vec<(i64, Option<f64>)> {
    s["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["at_ms"].as_i64().unwrap(), p["value"].as_f64()))
        .collect()
}

/// The series of `metric` about one agent run.
fn run_series<'a>(view: &'a Value, metric: &str, run: uuid::Uuid) -> &'a Value {
    view["series"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["metric"] == metric && s["run_id"] == json!(run.to_string()))
        .unwrap_or_else(|| panic!("no {metric} series for run {run} in {view}"))
}

/// Two agent runs on one worker (one runner) report at the same instants. Each run's Maestro
/// figures stay its own, one series per run, from raw samples and from the minute and hour
/// rollups; the host's figures stay one series per worker.
#[tokio::test]
async fn concurrent_runs_on_one_worker_each_keep_their_own_maestro_series() {
    let rig = rig().await;
    let now = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap();
    // Twenty hours ago, on a whole minute: closed minute and hour buckets for the rollups.
    let m0 = (now - Duration::hours(20)).replace_second(0).unwrap();
    let minute_of = |maestro_cpu: f32, maestro_rss: u64| -> Vec<Sample> {
        (0..12)
            .map(|i| {
                let mut s = sample(m0 + Duration::seconds(5 * i), 10.0);
                s.maestro_cpu_pct = Some(maestro_cpu);
                s.maestro_rss_bytes = Some(maestro_rss);
                s
            })
            .collect()
    };
    let mut a = Agent::new("930", "build", "runner-a");
    let mut b = Agent::new("931", "build", "runner-a");
    rig.publish(&mut a, vec![telemetry(1, minute_of(100.0, 200 << 20))])
        .await;
    rig.publish(&mut b, vec![telemetry(1, minute_of(300.0, 400 << 20))])
        .await;
    rig.store.roll_up(now).await.unwrap();
    let overview = rig.json("/api/v1/projects/app/overview").await;
    let worker = overview["workers"][0]["id"].as_str().unwrap().to_owned();
    let path = |from: OffsetDateTime, to: OffsetDateTime, metrics: &str| {
        format!(
            "/api/v1/projects/app/workers/{worker}/metrics?from={}&to={}&metrics={metrics}",
            from.format(&Rfc3339).unwrap(),
            to.format(&Rfc3339).unwrap()
        )
    };

    let raw = rig
        .json(&path(
            m0 - Duration::minutes(1),
            m0 + Duration::minutes(2),
            "maestro_cpu_pct,host_cpu_pct",
        ))
        .await;
    assert_eq!(raw["resolution"], "raw");
    assert_eq!(
        points(run_series(&raw, "maestro_cpu_pct", a.id)).len(),
        12,
        "run A's own readings"
    );
    assert!(
        points(run_series(&raw, "maestro_cpu_pct", a.id))
            .iter()
            .all(|(_, v)| *v == Some(100.0)),
        "{raw}"
    );
    assert!(
        points(run_series(&raw, "maestro_cpu_pct", b.id))
            .iter()
            .all(|(_, v)| *v == Some(300.0)),
        "{raw}"
    );
    let host_series = raw["series"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["metric"] == "host_cpu_pct")
        .count();
    assert_eq!(host_series, 1, "host figures stay one series per worker");

    let minutes = rig
        .json(&path(
            now - Duration::days(3),
            now,
            "maestro_cpu_pct,maestro_rss_bytes",
        ))
        .await;
    assert_eq!(minutes["resolution"], "1m");
    assert_eq!(
        points(run_series(&minutes, "maestro_cpu_pct", a.id)),
        [(ms(m0), Some(100.0))]
    );
    assert_eq!(
        points(run_series(&minutes, "maestro_cpu_pct", b.id)),
        [(ms(m0), Some(300.0))],
        "not averaged with run A"
    );
    assert_eq!(
        points(run_series(&minutes, "maestro_rss_bytes", b.id)),
        [(ms(m0), Some(f64::from(400 << 20)))]
    );

    let hours = rig
        .json(&path(now - Duration::days(40), now, "maestro_cpu_pct"))
        .await;
    assert_eq!(hours["resolution"], "1h");
    let hour = m0.replace_minute(0).unwrap();
    assert_eq!(
        points(run_series(&hours, "maestro_cpu_pct", a.id)),
        [(ms(hour), Some(100.0))]
    );
    assert_eq!(
        points(run_series(&hours, "maestro_cpu_pct", b.id)),
        [(ms(hour), Some(300.0))]
    );
}

/// The device page's newest sample: `GET …/devices/{id}` carries its shard and every reading, and
/// each reading the agent did not take says why in words. The newest sample is the one that counts,
/// whatever order the agent sent them in.
#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one device's newest sample, built and read back reading by reading"
)]
async fn a_device_detail_shows_its_newest_sample_with_the_shard_and_a_reason_per_unknown_reading() {
    let rig = rig().await;
    let mut agent = Agent::new("621", "build", "runner-a");
    agent.started();
    let device = agent
        .shard("emulator-5584", &["login"])
        .device_session_id
        .expect("a shard names its device session");
    let t0 = OffsetDateTime::now_utc().replace_nanosecond(0).unwrap() - Duration::minutes(5);
    let older = DeviceSample {
        model: None,
        os_version: None,
        at: ClockSample {
            wall: t0,
            mono_ms: 0,
        },
        device_session_id: device,
        adb_state: Some(AdbState::Device),
        boot_completed: Some(true),
        guest_cpu_pct: Some(12.5),
        guest_mem_available_bytes: Some(2 << 30),
        emu_cpu_pct: Some(80.0),
        emu_rss_bytes: Some(3 << 30),
        avd_name: Some("Pixel_10a_atd".into()),
        api_level: Some(37),
        foreground_activity: None,
        shard: Some(1),
        unavailable: DeviceUnavailable::default(),
    };
    // Newest: the host was under pressure, so the guest was not read; the emulator's process could
    // not be found, and its API level could not be read.
    let newest = DeviceSample {
        model: None,
        os_version: None,
        at: ClockSample {
            wall: t0 + Duration::seconds(30),
            mono_ms: 0,
        },
        guest_cpu_pct: None,
        guest_mem_available_bytes: None,
        emu_cpu_pct: None,
        emu_rss_bytes: None,
        avd_name: None,
        api_level: None,
        shard: Some(2),
        unavailable: DeviceUnavailable {
            guest_cpu_pct: Some(Unavailable::Throttled),
            guest_mem_available_bytes: Some(Unavailable::Throttled),
            emu_cpu_pct: Some(Unavailable::NoEmulatorProcess),
            emu_rss_bytes: Some(Unavailable::NoEmulatorProcess),
            avd_name: Some(Unavailable::NoEmulatorProcess),
            api_level: Some(Unavailable::ProbeFailed),
            ..DeviceUnavailable::default()
        },
        ..older.clone()
    };
    rig.publish(
        &mut agent,
        vec![Telemetry {
            tseq_from: 1,
            tseq_to: 1,
            dropped_since_last: 0,
            samples: Vec::new(),
            devices: vec![newest, older],
        }],
    )
    .await;

    let view = rig
        .json(&format!("/api/v1/projects/app/devices/{device}"))
        .await;
    let latest = &view["latest"];
    assert_eq!(latest["shard"], 2, "the newest sample's shard: {latest}");
    let reading = |metric: &str| -> Value {
        latest["readings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["metric"] == metric)
            .unwrap_or_else(|| panic!("no reading {metric}: {latest}"))
            .clone()
    };
    assert_eq!(
        (
            reading("adb_state")["text"].clone(),
            reading("adb_state")["available"].clone()
        ),
        (json!("device"), json!(true)),
        "the probe's adb state comes from the device list"
    );
    assert_eq!(
        (
            reading("boot_completed")["available"].clone(),
            reading("boot_completed")["value"].clone()
        ),
        (json!(true), json!(1.0)),
        "a reading that was taken carries its value and no reason"
    );
    assert!(reading("boot_completed")["reason"].is_null());
    let guest = reading("guest_cpu_pct");
    assert_eq!(
        (
            guest["available"].clone(),
            guest["value"].clone(),
            guest["reason"].clone(),
            guest["scope"].clone()
        ),
        (
            json!(false),
            Value::Null,
            json!("throttled: the host is under pressure"),
            json!("guest")
        ),
    );
    assert_eq!(
        reading("emu_rss_bytes")["reason"],
        json!("no emulator process maps to this device")
    );
    assert_eq!(
        reading("avd_name")["reason"],
        json!("no emulator process maps to this device")
    );
    assert_eq!(
        reading("api_level")["reason"],
        json!("the probe failed: adb did not answer it")
    );
    assert_eq!(
        reading("api_level")["unit"],
        json!("number"),
        "a number keeps its unit"
    );
}
