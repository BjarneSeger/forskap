//! `forskap issue create` — file a new issue in a project.
//!
//! Unlike the other writes this one is not queued by the daemon: it either
//! reaches GitLab now or fails, so a failure never turns into an issue later.

use anyhow::Result;
use forskap_api::{ErrorKind, NewWorkItem, VarlinkClientInterface, WorkItemRef};

use crate::cli::CreateArgs;
use crate::cmd::{epic, project};
use crate::friendly::friendly;
use crate::{client, output, style};

pub async fn run(args: CreateArgs) -> Result<()> {
    let client = client::connect_default().await?;
    let project_id = project::by_arg(&client, &args.project).await?;
    let parent = match args.epic {
        Some(iid) => {
            let epic = epic::resolve(&client, iid, args.group.as_deref()).await?;
            Some(WorkItemRef {
                project_id: None,
                group_id: epic.group_id,
                iid: epic.iid,
                r#type: Some(epic.r#type),
                title: None,
                web_url: None,
            })
        }
        None => None,
    };
    let item = NewWorkItem {
        title: args.title.join(" "),
        description: args.description,
        labels: (!args.labels.is_empty()).then_some(args.labels),
        assign_self: Some(!args.no_assign),
        parent,
    };

    let reply = client
        .create_work_item(project_id, item)
        .call()
        .await
        .map_err(failed)?;

    output::emit(args.output.output, &reply, |created| {
        match (created.iid, &created.web_url) {
            (Some(iid), Some(url)) => outln!("{} {url}", style::reference('#', iid)),
            (Some(iid), None) => outln!("{}", style::reference('#', iid)),
            (None, Some(url)) => outln!("{url}"),
            (None, None) => outln!("created, but GitLab's answer doesn't say which issue it is"),
        }
    })
}

/// The failure as the user reads it: where GitLab's outcome is unknown, the
/// issue may exist all the same, and creating it again files it twice.
fn failed(e: forskap_api::Error) -> anyhow::Error {
    if matches!(e.kind(), ErrorKind::GitlabUnavailable(_)) {
        let unknown =
            "GitLab may have created the issue all the same: look for it before creating it again";
        return friendly("CreateWorkItem", e).context(unknown);
    }
    friendly("CreateWorkItem", e)
}

#[cfg(test)]
mod tests {
    use forskap_api::{GitlabError_Args, GitlabUnavailable_Args};

    use super::*;

    #[test]
    fn only_an_unknown_outcome_warns_of_a_second_issue() {
        let unavailable = ErrorKind::GitlabUnavailable(Some(GitlabUnavailable_Args {
            message: "network error: connection reset".into(),
        }));
        let shown = format!("{:#}", failed(unavailable.into()));
        assert!(
            shown.starts_with("GitLab may have created the issue all the same")
                && shown.contains("connection reset"),
            "{shown}"
        );

        let refused = ErrorKind::GitlabError(Some(GitlabError_Args {
            message: "GitLab error: 403 Forbidden".into(),
            status: Some(403),
        }));
        let shown = format!("{:#}", failed(refused.into()));
        assert!(shown.starts_with("CreateWorkItem failed"), "{shown}");
    }
}
