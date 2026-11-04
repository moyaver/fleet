// SPDX-License-Identifier: AGPL-3.0-only
//! Which project a project-wide page (history, compare) is about.
//!
//! REST reads are per project, so these pages take `?project=<slug>` like the worker, device and
//! flow pages. Without it, a deployment with one project means that project; with several, the
//! page offers them as links instead of guessing.

#![expect(
    clippy::must_use_candidate,
    reason = "Leptos's #[component] macro generates the props structs and the components return views"
)]

use fleet_protocol::live::ProjectView;
use fleet_ui::components::display::{EmptyState, Skeleton};
use leptos::prelude::*;
use leptos_router::hooks::use_query_map;

use crate::{api, app::ErrorNotice, paths::escape, remote::remote};

/// What to do about the project of a page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectChoice {
    /// Show this project.
    Use(String),
    /// Ask which of these.
    Pick(Vec<String>),
    /// There is no project to show.
    NoProjects,
}

/// The project to show: the one the address names, else the only one there is.
#[must_use]
pub fn choose_project(requested: Option<&str>, projects: &[ProjectView]) -> ProjectChoice {
    if let Some(slug) = requested.filter(|s| !s.is_empty()) {
        return ProjectChoice::Use(slug.to_owned());
    }
    match projects {
        [] => ProjectChoice::NoProjects,
        [only] => ProjectChoice::Use(only.slug.clone()),
        many => ProjectChoice::Pick(many.iter().map(|p| p.slug.clone()).collect()),
    }
}

/// Runs `body` with the page's project. `path` is where the page lives, for the project links.
#[component]
pub fn InProject(
    /// The page's path, such as `/history`.
    path: &'static str,
    /// Builds the page for a project slug.
    body: Callback<String, AnyView>,
) -> impl IntoView {
    let query = use_query_map();
    move || {
        let requested = query.with(|q| q.get("project"));
        match requested.filter(|p| !p.is_empty()) {
            Some(project) => body.run(project),
            None => view! { <ChooseProject path body /> }.into_any(),
        }
    }
}

/// Fetches the projects to find the one to show.
#[component]
fn ChooseProject(path: &'static str, body: Callback<String, AnyView>) -> impl IntoView {
    let projects = remote(1000, api::projects);
    move || {
        match projects.data.get() {
        Some(list) => match choose_project(None, &list) {
            ProjectChoice::Use(slug) => body.run(slug),
            ProjectChoice::NoProjects => view! {
                <EmptyState title="No projects" description="A project appears when an agent first reports in." />
            }
            .into_any(),
            ProjectChoice::Pick(slugs) => view! {
                <div class="space-y-2" data-testid="choose-project">
                    <p>"Which project?"</p>
                    <ul class="list-disc pl-5">
                        {slugs
                            .into_iter()
                            .map(|s| {
                                let href = format!("{path}?project={}", escape(&s));
                                view! { <li><a class="text-primary underline" href=href>{s}</a></li> }
                            })
                            .collect_view()}
                    </ul>
                </div>
            }
            .into_any(),
        },
        None => view! {
            <ErrorNotice error=projects.error />
            {move || projects.error.with(Option::is_none).then(|| view! { <Skeleton lines=3 /> })}
        }
        .into_any(),
    }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn project(slug: &str) -> ProjectView {
        ProjectView {
            id: Uuid::nil(),
            slug: slug.to_owned(),
            name: slug.to_owned(),
        }
    }

    #[test]
    fn the_address_decides_and_otherwise_the_only_project_does() {
        let one = [project("shop")];
        let two = [project("shop"), project("ops")];
        assert_eq!(
            choose_project(Some("ops"), &two),
            ProjectChoice::Use("ops".to_owned()),
            "the address wins, and an unknown slug is the server's 404 to report"
        );
        assert_eq!(
            choose_project(None, &one),
            ProjectChoice::Use("shop".to_owned())
        );
        assert_eq!(
            choose_project(Some(""), &one),
            ProjectChoice::Use("shop".to_owned()),
            "an empty ?project= is no project"
        );
        assert_eq!(
            choose_project(None, &two),
            ProjectChoice::Pick(vec!["shop".to_owned(), "ops".to_owned()])
        );
        assert_eq!(choose_project(None, &[]), ProjectChoice::NoProjects);
    }
}
