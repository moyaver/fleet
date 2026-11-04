// SPDX-License-Identifier: AGPL-3.0-only
//! Serves the embedded dashboard with its security headers (PLAN §10.1).

use std::sync::LazyLock;

use axum::{
    Router,
    http::{HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, prelude::BASE64_STANDARD};
use fleet_web_assets::Dist;
use sha2::{Digest, Sha256};
use tower_http::set_header::SetResponseHeaderLayer;

/// The CSP of PLAN §10.1 for the embedded `index.html`. The usercontent origin joins `img-src`
/// once artifacts exist (M5).
static CSP: LazyLock<HeaderValue> = LazyLock::new(|| {
    let index = Dist::get("index.html").map(|f| String::from_utf8_lossy(&f.data).into_owned());
    HeaderValue::try_from(csp(index.as_deref().unwrap_or_default()))
        .expect("CSP should be ASCII: literals plus base64 hashes")
});

/// Trunk injects one inline module script that loads the wasm, and has no nonce option.
/// Allowing exactly that script by hash keeps `script-src` free of `'unsafe-inline'`.
fn csp(index_html: &str) -> String {
    let hashes: String = inline_scripts(index_html)
        .map(|body| BASE64_STANDARD.encode(Sha256::digest(body)))
        .map(|hash| " 'sha256-".to_owned() + &hash + "'")
        .collect();
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{hashes}; style-src 'self'; \
         img-src 'self' blob:; connect-src 'self'; frame-ancestors 'none'"
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

pub(crate) fn router() -> Router {
    let headers = [
        (header::CONTENT_SECURITY_POLICY, CSP.clone()),
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
        let csp = csp(html);
        // printf 'init()' | openssl dgst -sha256 -binary | base64
        assert!(csp.contains("script-src 'self' 'wasm-unsafe-eval' 'sha256-w4ujnOpjBoH2vcasx+reJRUwYivG8Q3afx/XevGJod8='; "), "{csp}");
        assert!(!csp.contains("unsafe-inline"));
        assert_eq!(csp.matches("'sha256-").count(), 1, "{csp}");
    }

    #[tokio::test]
    async fn every_response_carries_the_security_headers() {
        for path in ["/", "/p/demo/pipelines/1", "/missing.wasm"] {
            let res = router()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            let h = res.headers();
            assert_eq!(h[header::CONTENT_SECURITY_POLICY], *CSP, "{path}");
            assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff", "{path}");
        }
    }
}
