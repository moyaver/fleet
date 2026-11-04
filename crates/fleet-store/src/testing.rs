// SPDX-License-Identifier: AGPL-3.0-only
//! Test support for other crates (feature `test-util`): a fresh, migrated database per test.
//!
//! Reads `DATABASE_URL` (a server where the user may create databases), creates
//! `fleet_test_<uuid>` and migrates it. Databases are left behind for inspection; CI's database
//! is throwaway.

use uuid::Uuid;

use crate::{Error, PoolOptions, Result, Store};

/// A fresh, migrated database and its URL (so a test can start a second server on it).
///
/// # Panics
/// Panics when `DATABASE_URL` is unset: these tests need Postgres 18.
pub async fn fresh_database() -> Result<(Store, String)> {
    let base = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL should point at a Postgres 18 server for database tests");
    let name = format!("fleet_test_{}", Uuid::now_v7().simple());
    let admin = Store::connect_lazy(&base, PoolOptions::default())?;
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(admin.pool())
        .await?;
    admin.close().await;
    let url = with_database(&base, &name)?;
    let store = Store::connect_lazy(&url, PoolOptions::default())?;
    store.migrate().await?;
    Ok((store, url))
}

fn with_database(base: &str, name: &str) -> Result<String> {
    let (head, query) = base
        .split_once('?')
        .map_or((base, None), |(h, q)| (h, Some(q)));
    let scheme_end = head
        .find("://")
        .ok_or_else(|| Error::Corrupt("DATABASE_URL has no scheme".into()))?
        + 3;
    let path_start = head
        .get(scheme_end..)
        .and_then(|rest| rest.find('/'))
        .map_or(head.len(), |i| scheme_end + i);
    let mut url = format!("{}/{name}", head.get(..path_start).unwrap_or(head));
    if let Some(q) = query {
        url.push('?');
        url.push_str(q);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    #[test]
    fn database_names_replace_the_path() {
        assert_eq!(
            super::with_database("postgres://u:p@h:5432/fleet?sslmode=disable", "x").unwrap(),
            "postgres://u:p@h:5432/x?sslmode=disable"
        );
        assert_eq!(
            super::with_database("postgres://h", "x").unwrap(),
            "postgres://h/x"
        );
    }
}
