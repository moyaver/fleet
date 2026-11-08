// SPDX-License-Identifier: AGPL-3.0-only
//! Test support for other crates (feature `test-util`): a fresh, migrated database per test.
//!
//! Reads `DATABASE_URL` (a server where the user may create databases), creates
//! `fleet_test_<uuid>` and migrates it. The [`TestDatabase`] guard drops the database when it is
//! dropped, unless that happens while the test is panicking: like `#[sqlx::test]`, a failed test
//! keeps its database for inspection.

use uuid::Uuid;

use crate::{Error, PoolOptions, Result, Store};

/// A fresh, migrated database. Dropping the guard drops the database (`WITH (FORCE)`, so a server
/// the test forgot to stop cannot keep it), unless the test is panicking.
#[must_use = "dropping the guard drops the database"]
#[derive(Debug)]
pub struct TestDatabase {
    /// Taken in `drop`, so the pool's connections close before the database is dropped.
    store: Option<Store>,
    url: String,
    name: String,
    /// The server's URL with its database replaced by `postgres`, to run the drop from.
    admin_url: String,
}

impl TestDatabase {
    /// The store on the new database.
    ///
    /// # Panics
    /// Never: the store is only taken when the guard is dropped.
    #[must_use]
    pub fn store(&self) -> &Store {
        self.store
            .as_ref()
            .expect("the store should be present until the guard is dropped")
    }

    /// The new database's URL, so a test can start a second server on it.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The database's name (`fleet_test_<uuid>`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        // Dropping the pool's last handle closes its connections. The pool's own close is async,
        // and `drop` cannot await it.
        drop(self.store.take());
        if std::thread::panicking() {
            return;
        }
        let (admin_url, name) = (self.admin_url.clone(), self.name.clone());
        // `drop` can run inside a runtime, where blocking on a future panics. The drop runs on
        // its own thread with its own runtime, and we wait for it: the process may exit next.
        let dropped = std::thread::spawn({
            let name = name.clone();
            move || drop_database(&admin_url, &name)
        })
        .join();
        match dropped {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%name, %error, "dropping a test database failed"),
            Err(_) => tracing::warn!(%name, "dropping a test database panicked"),
        }
    }
}

/// A fresh, migrated database. Keep the guard until the test is done with the database.
///
/// # Panics
/// Panics when `DATABASE_URL` is unset: these tests need Postgres 18.
pub async fn fresh_database() -> Result<TestDatabase> {
    let admin_url = std::env::var("DATABASE_URL")
        .expect("DATABASE_URL should point at a Postgres 18 server for database tests");
    let name = format!("fleet_test_{}", Uuid::now_v7().simple());
    let admin = Store::connect_lazy(&admin_url, PoolOptions::default())?;
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(admin.pool())
        .await?;
    admin.close().await;
    let url = with_database(&admin_url, &name)?;
    // The guard exists before the migration, so a failed migration drops the database too.
    let db = TestDatabase {
        store: Some(Store::connect_lazy(&url, PoolOptions::default())?),
        url,
        name,
        admin_url,
    };
    db.store().migrate().await?;
    Ok(db)
}

/// Drops `name` on the server at `admin_url`, ending any session still attached to it.
fn drop_database(admin_url: &str, name: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime should build for dropping a test database");
    runtime.block_on(async {
        let admin = Store::connect_lazy(admin_url, PoolOptions::default())?;
        let dropped = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
        )))
        .execute(admin.pool())
        .await;
        admin.close().await;
        dropped?;
        Ok(())
    })
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
    use super::{TestDatabase, drop_database, fresh_database, with_database};
    use crate::{PoolOptions, Store};

    /// Whether `name` exists on the server at `admin_url`.
    async fn exists(admin_url: &str, name: &str) -> bool {
        let admin = Store::connect_lazy(admin_url, PoolOptions::default()).unwrap();
        let found: bool =
            sqlx::query_scalar("select exists(select 1 from pg_database where datname = $1)")
                .bind(name)
                .fetch_one(admin.pool())
                .await
                .unwrap();
        admin.close().await;
        found
    }

    fn admin_url() -> String {
        std::env::var("DATABASE_URL").unwrap()
    }

    #[tokio::test]
    async fn a_fresh_database_is_gone_once_its_guard_is_dropped() {
        let db = fresh_database().await.unwrap();
        let name = db.name().to_owned();
        assert!(exists(&admin_url(), &name).await, "created");
        drop(db);
        assert!(!exists(&admin_url(), &name).await, "dropped with its guard");
    }

    #[test]
    fn a_fresh_database_is_gone_when_its_guard_drops_outside_a_runtime() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db: TestDatabase = runtime.block_on(fresh_database()).unwrap();
        let name = db.name().to_owned();
        // The guard drops here, with no runtime entered on this thread.
        drop(db);
        assert!(!runtime.block_on(exists(&admin_url(), &name)), "dropped");
    }

    #[test]
    fn a_failed_test_keeps_its_database_for_inspection() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let db = runtime.block_on(fresh_database()).unwrap();
        let name = db.name().to_owned();
        let kept = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _db = db;
            panic!("a failing test");
        }));
        assert!(kept.is_err());
        assert!(runtime.block_on(exists(&admin_url(), &name)), "kept");
        // Clean up what the failing test left behind.
        drop_database(&admin_url(), &name).unwrap();
    }

    #[test]
    fn database_names_replace_the_path() {
        assert_eq!(
            with_database("postgres://u:p@h:5432/fleet?sslmode=disable", "x").unwrap(),
            "postgres://u:p@h:5432/x?sslmode=disable"
        );
        assert_eq!(
            with_database("postgres://h", "x").unwrap(),
            "postgres://h/x"
        );
    }
}
