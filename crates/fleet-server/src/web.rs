// SPDX-License-Identifier: AGPL-3.0-only
//! Serves the embedded dashboard with its security headers (PLAN §10.1).

use axum::{
    Router,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use fleet_web_assets::Dist;
use sha2::{Digest, Sha256};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::config::Config;

/// The CSP of PLAN §10.1 for the embedded `index.html`, with the usercontent origin (if any)
/// allowed in `img-src` and `connect-src`.
fn csp_header(usercontent: Option<&str>) -> HeaderValue {
    let index = Dist::get("index.html").map(|f| String::from_utf8_lossy(&f.data).into_owned());
    HeaderValue::try_from(csp(index.as_deref().unwrap_or_default(), usercontent))
        .expect("CSP should be ASCII: literals, base64 hashes and a validated origin")
}

/// `scheme://host[:port]` of `usercontent.url`, the one origin the dashboard may show images
/// from and fetch text from (the screenshot and log viewers); `None` when not configured.
pub(crate) fn usercontent_origin(config: &Config) -> Option<String> {
    let url = url::Url::parse(config.usercontent.url.as_deref()?).ok()?;
    Some(url.origin().ascii_serialization())
}

/// Trunk injects one inline module script that loads the wasm, and has no nonce option.
/// Allowing exactly that script by hash keeps `script-src` free of `'unsafe-inline'`.
///
/// Evidence comes from the usercontent origin and only from there (invariant I7): screenshots
/// are images, the maestro log and `commands.json` are fetched as text. No directive allows
/// scripts, frames or styles from it.
fn csp(index_html: &str, usercontent: Option<&str>) -> String {
    let hashes: String = inline_scripts(index_html)
        .map(|body| BASE64_STANDARD.encode(Sha256::digest(body)))
        .map(|hash| " 'sha256-".to_owned() + &hash + "'")
        .collect();
    let origin = usercontent.map(|o| format!(" {o}")).unwrap_or_default();
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{hashes}; style-src 'self'; \
         object-src 'none'; base-uri 'none'; img-src 'self' blob:{origin}; \
         connect-src 'self'{origin}; frame-ancestors 'none'"
    )
}

/// Bodies of `<script>` elements without a `src` attribute.
fn inline_scripts(html: &str) -> impl Iterator<Item = &str> {
    html.split("<script").skip(1).filter_map(|rest| {
        let (tag, rest) = rest.split_once('>')?;
        let (body, _) = rest.split_once("</script>")?;
        (!tag.contains("src=")).then_some(body)
    })
}

pub(crate) fn router(usercontent: Option<&str>) -> Router {
    let headers = [
        (header::CONTENT_SECURITY_POLICY, csp_header(usercontent)),
        (
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ),
        (
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ),
    ];
    headers
        .into_iter()
        .fold(Router::new().fallback(asset), |router, (name, value)| {
            router.layer(SetResponseHeaderLayer::overriding(name, value))
        })
}

/// Serves a file from `dist`. Paths without a file extension are client-side routes and get
/// `index.html`; a missing file with an extension is a 404, not HTML.
async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let is_route = !path.rsplit('/').next().unwrap_or_default().contains('.');
    let (path, file) = match Dist::get(path) {
        Some(file) if !path.is_empty() => (path, file),
        None if !is_route => return StatusCode::NOT_FOUND.into_response(),
        _ => match Dist::get("index.html") {
            Some(file) => ("index.html", file),
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    "dashboard not built: run `cargo xtask web`",
                )
                    .into_response();
            }
        },
    };
    let cache = if path == "index.html" {
        "no-cache"
    } else {
        "public, max-age=31536000, immutable"
    };
    (
        [
            (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
            (header::CACHE_CONTROL, cache.to_owned()),
        ],
        file.data,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn csp_allows_only_the_inline_loader_by_hash() {
        let html = "<script type=\"module\">init()</script><script src=\"/x.js\"></script>";
        let csp = csp(html, None);
        // printf 'init()' | openssl dgst -sha256 -binary | base64
        assert!(csp.contains("script-src 'self' 'wasm-unsafe-eval' 'sha256-w4ujnOpjBoH2vcasx+reJRUwYivG8Q3afx/XevGJod8='; "), "{csp}");
        assert!(!csp.contains("unsafe-inline"));
        assert_eq!(csp.matches("'sha256-").count(), 1, "{csp}");
    }

    #[test]
    fn plugins_and_base_urls_are_refused_outright() {
        // Not covered by default-src 'self' alone: <object> and <base> would still load.
        let csp = csp("", None);
        assert!(csp.contains("object-src 'none'; base-uri 'none';"), "{csp}");
    }

    #[test]
    fn without_a_usercontent_origin_the_dashboard_may_load_nothing_from_elsewhere() {
        let csp = csp("", None);
        assert!(
            csp.contains("img-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'"),
            "{csp}"
        );
    }

    #[test]
    fn the_usercontent_origin_may_serve_images_and_text_to_the_dashboard_and_nothing_else() {
        let origin = "https://fleet-usercontent.example.net";
        let csp = csp("", Some(origin));
        assert!(
            csp.contains(&format!(
                "img-src 'self' blob: {origin}; connect-src 'self' {origin}; "
            )),
            "{csp}"
        );
        // Exactly those two directives name it: no scripts, styles, frames or default from there.
        assert_eq!(csp.matches(origin).count(), 2, "{csp}");
        assert!(!csp.contains('*'), "no wildcard sources: {csp}");
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    }

    #[test]
    fn the_origin_is_the_configured_url_without_its_path() {
        let mut config = Config::default();
        assert_eq!(usercontent_origin(&config), None);
        config.usercontent.url = Some("http://localhost:8443/".into());
        assert_eq!(
            usercontent_origin(&config).as_deref(),
            Some("http://localhost:8443")
        );
        config.usercontent.url = Some("https://uc.example.net".into());
        assert_eq!(
            usercontent_origin(&config).as_deref(),
            Some("https://uc.example.net")
        );
    }

    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        let origin = Some("https://fleet-usercontent.example.net");
        for usercontent in [None, origin] {
            let expected = csp_header(usercontent);
            for path in ["/", "/p/demo/pipelines/1", "/missing.wasm"] {
                let res = router(usercontent)
                    .oneshot(Request::get(path).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                let h = res.headers();
                assert_eq!(h[header::CONTENT_SECURITY_POLICY], expected, "{path}");
                assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
            }
        }
    }
}
