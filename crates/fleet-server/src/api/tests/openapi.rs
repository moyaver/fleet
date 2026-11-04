// SPDX-License-Identifier: AGPL-3.0-only
//! The OpenAPI document lists every route of screens A–D with the DTO it returns.

use serde_json::Value;

use super::rig::rig;

#[tokio::test]
async fn every_screen_route_is_documented_with_its_response_schema() {
    let rig = rig().await;
    let res = rig.app.get("/api/v1/openapi.json").await;
    assert_eq!(res.status_code(), 200);
    let doc: Value = res.json();
    let base = "/api/v1/projects/{project}";

    for (path, schema) in [
        ("overview", "OverviewView"),
        ("attempts/{attempt}/timeline", "TimelineView"),
        ("workers/{worker}", "WorkerView"),
        ("workers/{worker}/runs", "WorkerRunPage"),
        ("workers/{worker}/metrics", "MetricsView"),
        ("devices/{device}", "DeviceDetailView"),
        ("devices/{device}/metrics", "MetricsView"),
        ("flows/{execution_id}", "FlowDetailView"),
    ] {
        let full = format!("{base}/{path}");
        let get = &doc["paths"][&full]["get"];
        assert!(get.is_object(), "{full} is documented");
        assert_eq!(
            get["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}"),
            "{full} returns {schema}"
        );
        assert!(
            get["responses"]["404"].is_object(),
            "{full} documents that a project the caller can't read is missing"
        );
        let params: Vec<&str> = get["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap())
            .collect();
        assert!(params.contains(&"project"), "{full}: {params:?}");
    }

    let schemas = &doc["components"]["schemas"];
    for (schema, property) in [
        ("SeriesView", "available"),
        ("MetricPoint", "reason"),
        ("BarView", "outcome_source"),
        ("ClaimView", "winning"),
        ("FlowDetailView", "claims"),
        ("WorkerSummaryView", "latest"),
        ("OverviewView", "worker_counts"),
    ] {
        assert!(
            schemas[schema]["properties"][property].is_object(),
            "{schema}.{property} is documented"
        );
    }
}
