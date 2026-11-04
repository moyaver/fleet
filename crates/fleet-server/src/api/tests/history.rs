// SPDX-License-Identifier: AGPL-3.0-only
//! Screen E's REST reads: `GET /api/v1/projects/{project}/history` and
//! `GET /api/v1/projects/{project}/flows/{name}/stats` (seam S12). Every execution is produced
//! through the real ingest path; expected values come from ADR-0009 and worked literal examples.

use fleet_protocol::event::{FlowOutcome, StepStatus};
use serde_json::{Value, json};

use super::rig::{Agent, Rig, rig};

pub(super) const MAIN: &str = "refs/heads/main";
pub(super) const FEATURE: &str = "refs/heads/feat/pay";

/// One flow of a [`Run`].
struct Flow {
    name: String,
    outcome: FlowOutcome,
    failure: Option<String>,
    ms: u64,
}

/// One CI run, reported by a final JUnit report, so every outcome is verified (unless the run is
/// [`live`](Run::live)). By default it ran on a feature branch (flaky v1's flip rate only looks at
/// the default one) on an `x86_64` runner, and every flow took 1.5 s.
pub(super) struct Run {
    ci: String,
    commit: String,
    git_ref: &'static str,
    flows: Vec<Flow>,
    arch: &'static str,
    live: bool,
}

impl Run {
    /// A run in which `flow` passes.
    pub(super) fn passed(ci_run: &str, commit: &str, flow: &str) -> Self {
        Self {
            ci: ci_run.into(),
            commit: commit.into(),
            git_ref: FEATURE,
            flows: Vec::new(),
            arch: "x86_64",
            live: false,
        }
        .and_passed(flow)
    }

    /// A run in which `flow` fails with `message`.
    pub(super) fn failed(ci_run: &str, commit: &str, flow: &str, message: &str) -> Self {
        Self::passed(ci_run, commit, flow).replace_with_failure(message)
    }

    fn replace_with_failure(mut self, message: &str) -> Self {
        if let Some(f) = self.flows.last_mut() {
            f.outcome = FlowOutcome::Failed;
            f.failure = Some(message.into());
        }
        self
    }

    /// Another flow of the run that passes.
    pub(super) fn and_passed(mut self, flow: &str) -> Self {
        self.flows.push(Flow {
            name: flow.into(),
            outcome: FlowOutcome::Passed,
            failure: None,
            ms: 1500,
        });
        self
    }

    /// Another flow of the run that fails with `message`.
    pub(super) fn and_failed(self, flow: &str, message: &str) -> Self {
        self.and_passed(flow).replace_with_failure(message)
    }

    pub(super) fn on(self, git_ref: &'static str) -> Self {
        Self { git_ref, ..self }
    }

    pub(super) fn arch(self, arch: &'static str) -> Self {
        Self { arch, ..self }
    }

    /// How long the last added flow took.
    pub(super) fn taking(mut self, ms: u64) -> Self {
        if let Some(f) = self.flows.last_mut() {
            f.ms = ms;
        }
        self
    }

    /// The run is still open: outcomes come from the live console only, provisional.
    pub(super) fn live(self) -> Self {
        Self { live: true, ..self }
    }

    /// Publishes the run through the real ingest path. Returns the first flow execution's ID.
    pub(super) async fn publish(self, rig: &Rig) -> String {
        self.publish_all(rig).await.remove(0)
    }

    /// Publishes the run; returns every flow execution's ID, in flow order.
    pub(super) async fn publish_all(self, rig: &Rig) -> Vec<String> {
        let mut agent = Agent::new(&self.ci, "build", "runner-a")
            .on_commit(&self.commit, self.git_ref)
            .on_arch(self.arch);
        agent.started();
        let names: Vec<&str> = self.flows.iter().map(|f| f.name.as_str()).collect();
        let shard = agent.shard("emulator-5554", &names);
        let mut started = Vec::new();
        for (i, flow) in (0..).zip(&self.flows) {
            let ids = agent.flow_started(&shard, &flow.name, i);
            agent.step(&ids, 0, "Launch app", StepStatus::Completed);
            if flow.failure.is_some() {
                agent.step(&ids, 1, "Tap on \"Pay\"", StepStatus::Failed);
            }
            agent.set_flow_time(flow.ms);
            agent.flow_ended(&ids, flow.outcome, flow.failure.as_deref());
            started.push(ids);
        }
        if !self.live {
            for (ids, flow) in started.iter().zip(&self.flows) {
                agent.set_flow_time(flow.ms);
                agent.flow_reported(ids, &flow.name, flow.outcome, flow.failure.as_deref());
            }
            agent.suite_finished(&shard, self.flows.iter().any(|f| f.failure.is_some()));
        }
        rig.publish(&mut agent, vec![]).await;
        let mut ids = Vec::new();
        for flow in &self.flows {
            ids.push(flow_of(rig, &self.ci, &flow.name).await);
        }
        ids
    }
}

/// The ID of flow `name` in the attempt of CI run `ci_run`.
pub(super) async fn flow_of(rig: &Rig, ci_run: &str, name: &str) -> String {
    let attempt = attempt_of(rig, ci_run).await;
    let snapshot = rig
        .json(&format!("/api/v1/projects/app/attempts/{attempt}"))
        .await;
    snapshot["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("no flow {name} in run {ci_run}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The pipeline attempt of CI run `ci_run`.
pub(super) async fn attempt_of(rig: &Rig, ci_run: &str) -> String {
    let attempts = rig.json("/api/v1/projects/app/attempts").await;
    attempts
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["run_key"] == format!("o/r:{ci_run}"))
        .unwrap_or_else(|| panic!("no attempt for run {ci_run}"))["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn ids(items: &Value) -> Vec<&str> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["flow_id"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn history_lists_runs_newest_first_with_their_evidence_and_marks_a_pass_and_a_fail_on_one_commit_flaky()
 {
    let rig = rig().await;
    let passed = Run::passed("900", "abc123", "checkout").publish(&rig).await;
    let failed = Run::failed("901", "abc123", "checkout", "Element not found: Done")
        .publish(&rig)
        .await;

    let page = rig.json("/api/v1/projects/app/history").await;

    assert_eq!(ids(&page["items"]), [failed.as_str(), passed.as_str()]);
    let (latest, earlier) = (&page["items"][0], &page["items"][1]);
    assert_eq!(latest["flow"], "flows/checkout.yaml");
    assert_eq!(latest["name"], "checkout");
    assert_eq!(latest["state"], "failed");
    assert_eq!(latest["outcome_source"], "junit_final");
    assert_eq!(latest["finality"], "verified");
    assert_eq!(latest["commit_sha"], "abc123");
    assert_eq!(latest["git_ref"], FEATURE);
    assert_eq!(latest["failure_message"], "Element not found: Done");
    assert_eq!(earlier["state"], "passed");
    assert_eq!(earlier["failure_fingerprint"], Value::Null);
    // ADR-0009: failure fingerprints are `v1:` and 16 hex digits; a condition fingerprint is a
    // SHA-256, and the two runs ran under the same conditions.
    let failure = latest["failure_fingerprint"].as_str().unwrap();
    assert!(
        failure.starts_with("v1:") && failure.len() == 19,
        "{failure}"
    );
    let condition = latest["condition_fingerprint"].as_str().unwrap();
    assert_eq!(condition.len(), 64);
    assert_eq!(earlier["condition_fingerprint"], condition);

    // ADR-0009 "confirmed": passed and failed on one commit under one condition, with the runs
    // named as evidence.
    assert_eq!(
        page["flows"],
        json!([{
            "flow": "flows/checkout.yaml",
            "flakiness": "confirmed",
            "evidence": [{
                "kind": "passed_and_failed",
                "commit": "abc123",
                "condition": condition,
                "passed": [passed],
                "failed": [failed],
            }]
        }])
    );
}

fn verdict<'a>(page: &'a Value, flow: &str) -> &'a Value {
    page["flows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["flow"] == flow)
        .unwrap_or_else(|| panic!("no verdict for {flow}: {page}"))
}

#[tokio::test]
async fn alternating_default_branch_results_are_suspected_flaky_and_steady_ones_are_not() {
    let rig = rig().await;
    // Four default-branch runs on four commits flip pass, fail, pass, fail: 3 flips over 3 pairs.
    let mut login = Vec::new();
    for i in 0..4 {
        let (run, commit) = (format!("91{i}"), format!("c{i}"));
        let run = if i % 2 == 0 {
            Run::passed(&run, &commit, "login")
        } else {
            Run::failed(&run, &commit, "login", "Element not found")
        };
        login.push(run.on(MAIN).publish(&rig).await);
    }
    for i in 0..4 {
        let (run, commit) = (format!("92{i}"), format!("d{i}"));
        Run::passed(&run, &commit, "search")
            .on(MAIN)
            .publish(&rig)
            .await;
    }

    let page = rig.json("/api/v1/projects/app/history").await;

    let suspected = verdict(&page, "flows/login.yaml");
    assert_eq!(suspected["flakiness"], "suspected");
    let evidence = &suspected["evidence"][0];
    assert_eq!(evidence["kind"], "flips");
    assert_eq!(evidence["flips"], 3);
    assert_eq!(
        evidence["runs"],
        json!(login),
        "the runs behind it, oldest first"
    );
    let steady = verdict(&page, "flows/search.yaml");
    assert_eq!(steady["flakiness"], "not_flaky");
    assert_eq!(steady["evidence"], json!([]));
}

#[tokio::test]
async fn an_inferred_run_is_shown_as_inferred_and_never_as_a_pass_or_flakiness() {
    let rig = rig().await;
    Run::passed("930", "e0", "profile").publish(&rig).await;
    // The agent of a second run on the same commit vanished mid-flow: the server's sweep infers
    // an `unknown` outcome; nothing observed one.
    let mut agent = Agent::new("931", "build", "runner-a").on_commit("e0", FEATURE);
    agent.started();
    let shard = agent.shard("emulator-5554", &["profile"]);
    agent.flow_started(&shard, "profile", 0);
    agent.suite_finished(&shard, true);
    rig.publish(&mut agent, vec![]).await;
    rig.store
        .sweep(std::time::Duration::ZERO, std::time::Duration::ZERO)
        .await
        .unwrap();

    let page = rig.json("/api/v1/projects/app/history").await;

    let inferred = &page["items"][0];
    assert_eq!(inferred["state"], "unknown");
    assert_eq!(inferred["finality"], "inferred");
    assert_eq!(inferred["outcome_source"], "server_inferred");
    assert_eq!(inferred["condition_fingerprint"], Value::Null, "not final");
    assert_eq!(
        verdict(&page, "flows/profile.yaml")["flakiness"],
        "not_flaky"
    );
    // Filtering for passes finds the one real pass, not the inferred run.
    let passes = rig
        .json("/api/v1/projects/app/history?outcome=passed")
        .await;
    assert_eq!(passes["items"].as_array().unwrap().len(), 1);
}

/// Three runs: `login` passes on main, `login` fails on a feature branch, `Search` passes on main.
async fn mixed(rig: &Rig) -> [String; 3] {
    [
        Run::passed("940", "a1", "login")
            .on(MAIN)
            .publish(rig)
            .await,
        Run::failed("941", "b2", "login", "Nope").publish(rig).await,
        Run::passed("942", "c3", "Search")
            .on(MAIN)
            .publish(rig)
            .await,
    ]
}

#[tokio::test]
async fn filters_narrow_the_history_by_flow_branch_and_outcome() {
    let rig = rig().await;
    let [login_main, login_feature, search] = mixed(&rig).await;
    let list = |query: &str| {
        let path = format!("/api/v1/projects/app/history?{query}");
        let rig = &rig;
        async move {
            let page = rig.json(&path).await;
            ids(&page["items"])
                .into_iter()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        }
    };

    assert_eq!(
        list("").await,
        [search.clone(), login_feature.clone(), login_main.clone()]
    );
    // A flow filter is text the path or name contains, ignoring case.
    assert_eq!(
        list("flow=LOGIN").await,
        [login_feature.clone(), login_main.clone()]
    );
    assert_eq!(
        list("flow=flows/search").await,
        std::slice::from_ref(&search)
    );
    // A branch is `main` or its full ref.
    assert_eq!(
        list("branch=main").await,
        [search.clone(), login_main.clone()]
    );
    assert_eq!(
        list("branch=refs/heads/feat/pay").await,
        std::slice::from_ref(&login_feature)
    );
    assert_eq!(
        list("outcome=failed").await,
        std::slice::from_ref(&login_feature)
    );
    assert_eq!(
        list("outcome=passed&branch=main&flow=login").await,
        std::slice::from_ref(&login_main)
    );
    assert_eq!(list("outcome=skipped").await, Vec::<String>::new());
}

#[tokio::test]
async fn the_time_window_filters_and_is_bounded() {
    let rig = rig().await;
    let [_, _, search] = mixed(&rig).await;
    let now = time::OffsetDateTime::now_utc();
    let rfc = |t: time::OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
            .replace('+', "%2B")
    };
    let get = |query: String| {
        let rig = &rig;
        async move {
            rig.get(&format!("/api/v1/projects/app/history?{query}"))
                .await
        }
    };
    let hour = time::Duration::hours(1);

    // Everything ran just now: a window that ended an hour ago is empty, one around now is not.
    let past = get(format!(
        "from={}&to={}",
        rfc(now - time::Duration::days(2)),
        rfc(now - hour)
    ))
    .await;
    assert_eq!(past.status_code(), 200);
    assert_eq!(past.json::<Value>()["items"], json!([]));
    let around = get(format!("from={}&to={}", rfc(now - hour), rfc(now + hour))).await;
    assert_eq!(
        around.json::<Value>()["items"][0]["flow_id"],
        search.as_str()
    );

    // The window is bounded to 90 days and must be well formed.
    for (query, why) in [
        (
            format!(
                "from={}&to={}",
                rfc(now - time::Duration::days(91)),
                rfc(now)
            ),
            "91 days",
        ),
        (
            format!("from={}&to={}", rfc(now), rfc(now - hour)),
            "from after to",
        ),
        ("from=yesterday".to_owned(), "not a time"),
        ("outcome=great".to_owned(), "unknown outcome"),
        ("cursor=garbage".to_owned(), "unknown cursor"),
    ] {
        let res = get(query).await;
        assert_eq!(res.status_code(), 400, "{why}: {}", res.text());
        assert!(res.json::<Value>()["error"].is_string(), "{why}");
    }
}

#[tokio::test]
async fn pages_follow_a_cursor_newest_first_without_gaps_or_repeats() {
    let rig = rig().await;
    let mut all = Vec::new();
    for i in 0..5 {
        let (run, commit) = (format!("95{i}"), format!("p{i}"));
        all.push(
            Run::passed(&run, &commit, "login")
                .on(MAIN)
                .publish(&rig)
                .await,
        );
    }
    all.reverse();

    let mut seen = Vec::new();
    let mut cursor = String::new();
    let mut pages = 0;
    loop {
        // The first request has an empty `cursor=`, as a form without a value sends.
        let path = format!("/api/v1/projects/app/history?limit=2&cursor={cursor}");
        let page = rig.json(&path).await;
        seen.extend(ids(&page["items"]).into_iter().map(str::to_owned));
        pages += 1;
        match page["next_cursor"].as_str() {
            Some(next) => next.clone_into(&mut cursor),
            None => break,
        }
    }

    assert_eq!(pages, 3, "5 executions at 2 per page");
    assert_eq!(seen, all);
}

pub(super) const DEATH_A: &str = "maestro.android.DeviceServerDiedException: Device server died during \
    'viewHierarchy' on emulator-5602 (2267ms since last byte, connection age 34368ms): \
    StatusRuntimeException: UNAVAILABLE";
pub(super) const DEATH_B: &str = "maestro.android.DeviceServerDiedException: Device server died during \
    'viewHierarchy' on emulator-5554 (991ms since last byte, connection age 20211ms): \
    StatusRuntimeException: UNAVAILABLE";
const ASSERTION: &str = r#"Assertion is false: "Order confirmed" is visible"#;

#[tokio::test]
async fn a_flows_stats_count_outcomes_summarize_durations_and_group_failures_by_fingerprint() {
    let rig = rig().await;
    // Five passes of 1, 2, 3, 4 and 10 s; the first shares a commit with a failure.
    let mut passes = Vec::new();
    for (i, ms) in [1000, 2000, 3000, 4000, 10_000].into_iter().enumerate() {
        let (run, commit) = (format!("96{i}"), format!("s{i}"));
        let run = Run::passed(&run, &commit, "checkout").taking(ms);
        passes.push(run.publish(&rig).await);
    }
    // Two failures that are one failure (the emulator and the timings differ), then another.
    let death_a = Run::failed("970", "s0", "checkout", DEATH_A)
        .publish(&rig)
        .await;
    let death_b = Run::failed("971", "s9", "checkout", DEATH_B)
        .publish(&rig)
        .await;
    let assertion = Run::failed("972", "s8", "checkout", ASSERTION)
        .publish(&rig)
        .await;
    // Another flow's runs don't count.
    Run::failed("973", "s7", "login", "Nope")
        .publish(&rig)
        .await;

    let stats = rig
        .json("/api/v1/projects/app/flows/flows%2Fcheckout.yaml/stats")
        .await;

    assert_eq!(stats["flow"], "flows/checkout.yaml");
    assert_eq!(
        stats["counts"],
        json!({"passed": 5, "failed": 3, "other": 0})
    );
    // p50 of the passes' 1, 2, 3, 4 and 10 s is 3 s; p90 interpolates 60 % of the way from 4 s
    // to 10 s.
    assert_eq!(
        stats["duration"],
        json!({"samples": 5, "p50_ms": 3000, "p90_ms": 7600})
    );
    // The chart's points: every settled pass and failure, oldest first.
    let points = stats["durations"].as_array().unwrap();
    let series: Vec<_> = points
        .iter()
        .map(|p| {
            (
                p["flow_id"].as_str().unwrap(),
                p["duration_ms"].as_i64().unwrap(),
                p["state"].as_str().unwrap(),
            )
        })
        .collect();
    let expected: Vec<_> = passes
        .iter()
        .zip([1000, 2000, 3000, 4000, 10_000])
        .map(|(id, ms)| (id.as_str(), ms, "passed"))
        .chain([&death_a, &death_b, &assertion].map(|id| (id.as_str(), 1500, "failed")))
        .collect();
    assert_eq!(series, expected);

    // Failures group by fingerprint, the most recent group first; the two device deaths are one.
    let groups = stats["recent_failures"].as_array().unwrap();
    assert_eq!(groups.len(), 2, "{groups:?}");
    assert_eq!(groups[0]["count"], 1);
    assert_eq!(groups[0]["flow_ids"], json!([assertion]));
    let death = &groups[1];
    assert_eq!(death["count"], 2);
    assert_eq!(death["flow_ids"], json!([death_b, death_a]), "newest first");
    assert_eq!(death["step_kind"], "Tap on \"Pay\"");
    assert_eq!(
        death["message"],
        "maestro.android.DeviceServerDiedException: Device server died during 'viewHierarchy' on \
         <serial> (<dur> since last byte, connection age <dur>): StatusRuntimeException: UNAVAILABLE"
    );
    // The group's fingerprint is the one the history shows on its executions.
    let history = rig
        .json("/api/v1/projects/app/history?outcome=failed")
        .await;
    let entry = history["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["flow_id"] == death_a.as_str())
        .unwrap();
    assert_eq!(death["fingerprint"], entry["failure_fingerprint"]);

    // The first pass and the first device death share commit `s0` and conditions: flaky.
    assert_eq!(stats["flaky_commits"], 1);
    assert_eq!(stats["flakiness"]["flakiness"], "confirmed");
    assert_eq!(
        stats["flakiness"]["evidence"][0]["passed"],
        json!([passes[0]])
    );
    assert_eq!(
        stats["flakiness"]["evidence"][0]["failed"],
        json!([death_a])
    );
}

#[tokio::test]
async fn stats_of_an_unknown_flow_are_missing_and_of_a_quiet_window_are_empty() {
    let rig = rig().await;
    Run::passed("980", "q1", "login").publish(&rig).await;

    let missing = rig
        .get("/api/v1/projects/app/flows/flows%2Fnope.yaml/stats")
        .await;
    assert_eq!(missing.status_code(), 404);

    let now = time::OffsetDateTime::now_utc();
    let rfc = |t: time::OffsetDateTime| {
        t.format(&time::format_description::well_known::Rfc3339)
            .unwrap()
            .replace('+', "%2B")
    };
    let quiet = rig
        .json(&format!(
            "/api/v1/projects/app/flows/flows%2Flogin.yaml/stats?from={}&to={}",
            rfc(now - time::Duration::days(10)),
            rfc(now - time::Duration::days(5)),
        ))
        .await;
    assert_eq!(
        quiet["counts"],
        json!({"passed": 0, "failed": 0, "other": 0})
    );
    assert_eq!(
        quiet["duration"],
        json!({"samples": 0, "p50_ms": null, "p90_ms": null}),
        "no samples is not a zero duration"
    );
    assert_eq!(quiet["recent_failures"], json!([]));
    assert_eq!(quiet["flakiness"]["flakiness"], "not_flaky");
}
