//! `forskap issue create` — file a new issue in a project.
//!
//! Unlike the other writes this one is not queued by the daemon: it either
//! reaches GitLab now or fails, so a failure never turns into an issue later.

use anyhow::Result;
use forskap_api::{VarlinkClientInterface, WorkItemRef};

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
    let labels = (!args.labels.is_empty()).then_some(args.labels);

    let reply = client
        .create_work_item(
            project_id,
            args.title.join(" "),
            args.description,
            labels,
            Some(!args.no_assign),
            parent,
        )
        .call()
        .await
        .map_err(|e| friendly("CreateWorkItem", e))?;

    output::emit(args.output.output, &reply, |created| {
        outln!("{} {}", style::reference('#', created.iid), created.web_url)
    })
}
