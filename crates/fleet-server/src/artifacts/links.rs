// SPDX-License-Identifier: AGPL-3.0-only
//! Signed, short-lived links to artifacts on the usercontent origin (research 06 §6.2, T15).
//!
//! ```text
//! <usercontent url>/u/<project>/<artifact>/<expires>/<signature>/<file name>
//! ```
//!
//! `signature` is the unpadded base64url HMAC-SHA256, under the configured key, of
//! `fleet-usercontent-v1`, the project id, the artifact id and the expiry (Unix seconds), one per
//! line. The link therefore names exactly one artifact of one project until one moment: change
//! any of the three and the signature no longer matches. The file name is only for people (and
//! for the browser's "save as"); the server ignores it.
//!
//! The app mints links after it has checked that the caller may read artifacts. The usercontent
//! origin has no session and checks nothing else.

use base64::Engine as _;
use ring::hmac;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::config::Usercontent;

const CONTEXT: &str = "fleet-usercontent-v1";

/// Why a link is not honoured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The signature doesn't match: tampered, minted for something else, or signed by another key.
    BadSignature,
    /// Genuine, and past its expiry.
    Expired,
}

/// A link that checks out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Granted {
    pub(crate) project: Uuid,
    pub(crate) artifact: Uuid,
}

/// Signs and verifies links with the configured key.
#[derive(Clone)]
pub(crate) struct Signer {
    key: hmac::Key,
    base: url::Url,
    ttl_seconds: i64,
}

impl std::fmt::Debug for Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The key never prints (I6).
        f.debug_struct("Signer")
            .field("base", &self.base.as_str())
            .finish_non_exhaustive()
    }
}

impl Signer {
    /// `None` unless `usercontent.url` and `usercontent.signing_key` are set (validation makes
    /// them go together).
    pub(crate) fn from_config(c: &Usercontent) -> Option<Self> {
        let base = url::Url::parse(c.url.as_deref()?).ok()?;
        let key = hmac::Key::new(
            hmac::HMAC_SHA256,
            c.signing_key.as_ref()?.expose().as_bytes(),
        );
        Some(Self {
            key,
            base,
            ttl_seconds: i64::try_from(c.link_ttl_seconds).unwrap_or(300),
        })
    }

    fn message(project: Uuid, artifact: Uuid, expires: i64) -> String {
        format!(
            "{CONTEXT}\n{}\n{}\n{expires}",
            project.hyphenated(),
            artifact.hyphenated()
        )
    }

    fn mac(&self, project: Uuid, artifact: Uuid, expires: i64) -> String {
        let tag = hmac::sign(
            &self.key,
            Self::message(project, artifact, expires).as_bytes(),
        );
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag.as_ref())
    }

    /// A link to `artifact` of `project` that works for the configured lifetime from `now`;
    /// returns it with its expiry.
    pub(crate) fn link(
        &self,
        project: Uuid,
        artifact: Uuid,
        name: &str,
        now: OffsetDateTime,
    ) -> (String, OffsetDateTime) {
        let expires = now.unix_timestamp().saturating_add(self.ttl_seconds);
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.clear().extend([
                "u",
                &project.hyphenated().to_string(),
                &artifact.hyphenated().to_string(),
                &expires.to_string(),
                &self.mac(project, artifact, expires),
                name,
            ]);
        }
        let at = OffsetDateTime::from_unix_timestamp(expires).unwrap_or(now);
        (url.into(), at)
    }

    /// Checks a link's parts: the signature first (compared in constant time), then the expiry.
    pub(crate) fn verify(
        &self,
        project: &str,
        artifact: &str,
        expires: &str,
        signature: &str,
        now: OffsetDateTime,
    ) -> Result<Granted, Refusal> {
        let parsed = (
            Uuid::parse_str(project),
            Uuid::parse_str(artifact),
            expires.parse::<i64>(),
            base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(signature),
        );
        let (Ok(project), Ok(artifact), Ok(expires), Ok(tag)) = parsed else {
            return Err(Refusal::BadSignature);
        };
        // `ring` compares in constant time.
        hmac::verify(
            &self.key,
            Self::message(project, artifact, expires).as_bytes(),
            &tag,
        )
        .map_err(|_| Refusal::BadSignature)?;
        if now.unix_timestamp() >= expires {
            return Err(Refusal::Expired);
        }
        Ok(Granted { project, artifact })
    }
}
