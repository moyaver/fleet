// SPDX-License-Identifier: AGPL-3.0-only
//! S3 settings: credentials never print (I6), and an unusable configuration is refused up front.

use fleet_blob::{BlobError, S3, S3Config};

fn config() -> S3Config {
    S3Config {
        endpoint: "http://garage:3900".into(),
        region: "garage".into(),
        bucket: "fleet-artifacts".into(),
        access_key_id: "GKaccess-id-123".into(),
        secret_access_key: "very-secret-key-456".into(),
        public_endpoint: None,
    }
}

#[test]
fn credentials_never_appear_in_debug_output() {
    let shown = format!("{:?}", config());
    assert!(
        shown.contains("fleet-artifacts") && shown.contains("garage:3900"),
        "{shown}"
    );
    assert!(
        !shown.contains("GKaccess-id-123") && !shown.contains("very-secret-key-456"),
        "{shown}"
    );
    let store = format!("{:?}", S3::new(&config()).unwrap());
    assert!(!store.contains("very-secret-key-456"), "{store}");
}

#[test]
fn an_incomplete_configuration_is_refused_naming_the_field() {
    let mut no_bucket = config();
    no_bucket.bucket = " ".into();
    let err = S3::new(&no_bucket).unwrap_err();
    assert!(
        matches!(&err, BlobError::Config(m) if m.contains("bucket")),
        "{err:?}"
    );
    let mut no_secret = config();
    no_secret.secret_access_key.clear();
    let err = S3::new(&no_secret).unwrap_err();
    assert!(
        matches!(&err, BlobError::Config(m) if m.contains("secret_access_key")),
        "{err:?}"
    );
}
