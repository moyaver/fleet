// SPDX-License-Identifier: AGPL-3.0-only
//! The `fleet api-key create` subcommand on a project the server already configured.

use fleet_server::{ApiKeyAction, Cli, Command};
use sqlx::PgPool;

use crate::support::Harness;

#[tokio::test]
async fn api_key_create_keeps_the_configured_project_name() {
    // The harness serves project `app`, configured with the name "App".
    let harness = Harness::start().await;
    let url = harness
        .config
        .database
        .url
        .as_ref()
        .unwrap()
        .expose()
        .to_owned();
    let cli = Cli {
        config: None,
        command: Command::ApiKey {
            action: ApiKeyAction::Create {
                project: "app".into(),
                name: "second-key".into(),
            },
        },
    };
    fleet_server::run(cli, vec![("FLEET_DATABASE__URL".into(), url.clone())])
        .await
        .unwrap();

    let pool = PgPool::connect(&url).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM projects WHERE slug = 'app'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(name, "App", "creating a key must not rename the project");
}
