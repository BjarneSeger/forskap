//! Varlink protocol dispatcher.
//!
//! Splits the framework-level `org.varlink.service.*` methods from the
//! forskapd methods so each match stays short and self-evident.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use tracing::{debug, warn};
use varlink::Reply;
use varlink::sansio::ServerEvent;

use forskap_api::{
    AssignSelf_Args, AsyncCall, Call_AssignSelf, Call_ClearCache, Call_ClearFailures, Call_Close,
    Call_CreateWorkItem, Call_DismissFailure, Call_GetActivity, Call_GetAssignedMergeRequests,
    Call_GetAssignedWorkItems, Call_GetFailures, Call_GetHistory, Call_GetStatus, Call_GetSyncJobs,
    Call_ListWorkItems, Call_Login, Call_Logout, Call_PostTime, Call_RecordOpen, Call_RetryFailure,
    Call_Search, Call_UnassignSelf, Call_WhoAmI, ClearCache_Args, Close_Args, CreateWorkItem_Args,
    DismissFailure_Args, GetActivity_Args, GetAssignedMergeRequests_Args,
    GetAssignedWorkItems_Args, GetHistory_Args, ListWorkItems_Args, Login_Args, PostTime_Args,
    RecordOpen_Args, RetryFailure_Args, Search_Args, UnassignSelf_Args,
    VARLINK_INTERFACE_DESCRIPTION, VarlinkInterface as _,
};

use crate::handlers::Handlers;

const ORG_VARLINK_SERVICE_DESCRIPTION: &str = r#"interface org.varlink.service

method GetInfo() -> (
  vendor: string,
  product: string,
  version: string,
  url: string,
  interfaces: []string
)

method GetInterfaceDescription(interface: string) -> (description: string)

error InterfaceNotFound (interface: string)
error MethodNotFound (method: string)
error MethodNotImplemented (method: string)
error InvalidParameter (parameter: string)
"#;

pub struct ServiceHandler {
    handlers: Arc<Handlers>,
}

impl ServiceHandler {
    pub fn new(handlers: Arc<Handlers>) -> Self {
        ServiceHandler { handlers }
    }
}

#[async_trait::async_trait]
impl varlink::AsyncConnectionHandler for ServiceHandler {
    async fn handle(
        &self,
        server: &mut varlink::sansio::Server,
        _upgraded: Option<String>,
    ) -> varlink::Result<Option<String>> {
        while let Some(event) = server.poll_event() {
            match event {
                ServerEvent::Request { request } => {
                    debug!(method = request.method.as_ref(), "varlink request");
                    let method = request.method.as_ref();
                    let reply = if let Some(reply) = handle_varlink_meta(method, &request) {
                        Some(reply)
                    } else if method.starts_with("org.thehoster.forskapd.") {
                        handle_forskapd(method, request.parameters, &self.handlers).await?
                    } else {
                        warn!(method, "unknown varlink method");
                        Some(Reply::error(
                            "org.varlink.service.MethodNotFound",
                            Some(serde_json::json!({"method": method})),
                        ))
                    };
                    if let Some(reply) = reply {
                        server.send_reply(reply)?;
                    }
                }
                ServerEvent::Upgrade { interface } => return Ok(Some(interface)),
            }
        }
        Ok(None)
    }
}

/// Replies for the framework-level `org.varlink.service.*` methods, or `None`
/// if the method isn't one of them.
fn handle_varlink_meta(method: &str, request: &varlink::Request) -> Option<Reply> {
    match method {
        "org.varlink.service.GetInfo" => Some(Reply::parameters(Some(serde_json::json!({
            "vendor": "org.thehoster",
            "product": "forskapd",
            "version": env!("CARGO_PKG_VERSION"),
            "url": "https://github.com/bjarneseger/forskap",
            "interfaces": ["org.varlink.service", "org.thehoster.forskapd"]
        })))),
        "org.varlink.service.GetInterfaceDescription" => {
            let name = request
                .parameters
                .as_ref()
                .and_then(|p| p.get("interface"))
                .and_then(|v| v.as_str());
            let desc = match name {
                Some("org.varlink.service") => Some(ORG_VARLINK_SERVICE_DESCRIPTION),
                Some("org.thehoster.forskapd") => Some(VARLINK_INTERFACE_DESCRIPTION),
                _ => None,
            };
            Some(match desc {
                Some(d) => Reply::parameters(Some(serde_json::json!({"description": d}))),
                None => Reply::error(
                    "org.varlink.service.InvalidParameter",
                    Some(serde_json::json!({"parameter": "interface"})),
                ),
            })
        }
        _ => None,
    }
}

/// The call's arguments, or the `InvalidParameter` reply saying why they
/// don't parse (a missing field, an unknown enum value) or naming a field the
/// method doesn't have: a newer client's argument this daemon would ignore
/// unseen. An omitted `parameters` block reads as an empty one: a valid call
/// of a method whose arguments are all optional.
fn parse_args<T: DeserializeOwned>(params: Option<serde_json::Value>) -> Result<T, Reply> {
    let invalid = |parameter: String| {
        Reply::error(
            "org.varlink.service.InvalidParameter",
            Some(serde_json::json!({ "parameter": parameter })),
        )
    };
    let params = params.unwrap_or_else(|| serde_json::json!({}));
    let mut unknown = None;
    let args = serde_ignored::deserialize(params, |path| {
        unknown.get_or_insert_with(|| field_name(&path));
    })
    .map_err(|e| invalid(e.to_string()))?;
    match unknown {
        Some(field) => Err(invalid(field)),
        None => Ok(args),
    }
}

/// `scope.groups`, `parent.iid`: serde_ignored's path without the `?` it puts
/// in for an optional's content.
fn field_name(path: &serde_ignored::Path) -> String {
    use serde_ignored::Path;
    let (parent, name) = match path {
        Path::Root => return String::new(),
        Path::Seq { parent, index } => (parent, index.to_string()),
        Path::Map { parent, key } => (parent, key.clone()),
        Path::Some { parent }
        | Path::NewtypeStruct { parent }
        | Path::NewtypeVariant { parent } => return field_name(parent),
    };
    match field_name(parent) {
        parent if parent.is_empty() => name,
        parent => format!("{parent}.{name}"),
    }
}

async fn handle_forskapd(
    method: &str,
    params: Option<serde_json::Value>,
    handlers: &Handlers,
) -> varlink::Result<Option<Reply>> {
    let mut call = AsyncCall::default();
    // Returning the error instead would drop the connection without a reply.
    macro_rules! args {
        () => {
            match parse_args(params) {
                Ok(args) => args,
                Err(reply) => {
                    warn!(method, "invalid varlink parameters");
                    return Ok(Some(reply));
                }
            }
        };
    }
    match method {
        "org.thehoster.forskapd.ClearCache" => {
            let args: ClearCache_Args = args!();
            handlers
                .clear_cache(&mut call as &mut dyn Call_ClearCache, args.scope)
                .await?;
        }
        "org.thehoster.forskapd.GetHistory" => {
            let args: GetHistory_Args = args!();
            handlers
                .get_history(&mut call as &mut dyn Call_GetHistory, args.days)
                .await?;
        }
        "org.thehoster.forskapd.GetActivity" => {
            let args: GetActivity_Args = args!();
            handlers
                .get_activity(&mut call as &mut dyn Call_GetActivity, args.days)
                .await?;
        }
        "org.thehoster.forskapd.GetFailures" => {
            handlers
                .get_failures(&mut call as &mut dyn Call_GetFailures)
                .await?;
        }
        "org.thehoster.forskapd.GetSyncJobs" => {
            handlers
                .get_sync_jobs(&mut call as &mut dyn Call_GetSyncJobs)
                .await?;
        }
        "org.thehoster.forskapd.GetStatus" => {
            handlers
                .get_status(&mut call as &mut dyn Call_GetStatus)
                .await?;
        }
        "org.thehoster.forskapd.RetryFailure" => {
            let args: RetryFailure_Args = args!();
            handlers
                .retry_failure(&mut call as &mut dyn Call_RetryFailure, args.id)
                .await?;
        }
        "org.thehoster.forskapd.DismissFailure" => {
            let args: DismissFailure_Args = args!();
            handlers
                .dismiss_failure(&mut call as &mut dyn Call_DismissFailure, args.id)
                .await?;
        }
        "org.thehoster.forskapd.ClearFailures" => {
            handlers
                .clear_failures(&mut call as &mut dyn Call_ClearFailures)
                .await?;
        }
        "org.thehoster.forskapd.GetAssignedWorkItems" => {
            let args: GetAssignedWorkItems_Args = args!();
            handlers
                .get_assigned_work_items(
                    &mut call as &mut dyn Call_GetAssignedWorkItems,
                    args.groups,
                )
                .await?;
        }
        "org.thehoster.forskapd.GetAssignedMergeRequests" => {
            let args: GetAssignedMergeRequests_Args = args!();
            handlers
                .get_assigned_merge_requests(
                    &mut call as &mut dyn Call_GetAssignedMergeRequests,
                    args.groups,
                )
                .await?;
        }
        "org.thehoster.forskapd.ListWorkItems" => {
            let args: ListWorkItems_Args = args!();
            handlers
                .list_work_items(
                    &mut call as &mut dyn Call_ListWorkItems,
                    args.role,
                    args.updated_after,
                    args.states,
                )
                .await?;
        }
        "org.thehoster.forskapd.Search" => {
            let args: Search_Args = args!();
            handlers
                .search(
                    &mut call as &mut dyn Call_Search,
                    args.query,
                    args.kinds,
                    args.limit,
                    args.scope,
                    args.types,
                    args.exclude_types,
                )
                .await?;
        }
        "org.thehoster.forskapd.PostTime" => {
            let args: PostTime_Args = args!();
            handlers
                .post_time(
                    &mut call as &mut dyn Call_PostTime,
                    args.project_id,
                    args.iid,
                    args.kind,
                    args.duration,
                    args.summary,
                )
                .await?;
        }
        "org.thehoster.forskapd.Close" => {
            let args: Close_Args = args!();
            handlers
                .close(
                    &mut call as &mut dyn Call_Close,
                    args.project_id,
                    args.iid,
                    args.kind,
                )
                .await?;
        }
        "org.thehoster.forskapd.RecordOpen" => {
            let args: RecordOpen_Args = args!();
            handlers
                .record_open(
                    &mut call as &mut dyn Call_RecordOpen,
                    args.kind,
                    args.iid,
                    args.project_id,
                    args.group_id,
                )
                .await?;
        }
        "org.thehoster.forskapd.AssignSelf" => {
            let args: AssignSelf_Args = args!();
            handlers
                .assign_self(
                    &mut call as &mut dyn Call_AssignSelf,
                    args.project_id,
                    args.iid,
                    args.kind,
                )
                .await?;
        }
        "org.thehoster.forskapd.UnassignSelf" => {
            let args: UnassignSelf_Args = args!();
            handlers
                .unassign_self(
                    &mut call as &mut dyn Call_UnassignSelf,
                    args.project_id,
                    args.iid,
                    args.kind,
                )
                .await?;
        }
        "org.thehoster.forskapd.CreateWorkItem" => {
            let args: CreateWorkItem_Args = args!();
            handlers
                .create_work_item(
                    &mut call as &mut dyn Call_CreateWorkItem,
                    args.project_id,
                    args.title,
                    args.description,
                    args.labels,
                    args.assign_self,
                    args.parent,
                )
                .await?;
        }
        "org.thehoster.forskapd.Login" => {
            let args: Login_Args = args!();
            handlers
                .login(&mut call as &mut dyn Call_Login, args.host, args.token)
                .await?;
        }
        "org.thehoster.forskapd.Logout" => {
            handlers.logout(&mut call as &mut dyn Call_Logout).await?;
        }
        "org.thehoster.forskapd.WhoAmI" => {
            handlers.who_am_i(&mut call as &mut dyn Call_WhoAmI).await?;
        }
        _ => {
            warn!(method, "unknown forskapd method");
            return Ok(Some(Reply::error(
                "org.varlink.service.MethodNotFound",
                Some(serde_json::json!({"method": method})),
            )));
        }
    }
    Ok(call.take_reply())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the hand-written dispatch above: a method that exists in the
    /// generated `VarlinkInterface` trait but has no arm in `handle_forskapd`
    /// compiles fine and only fails at runtime as `MethodNotFound` — this
    /// test turns that silent trap into a red test.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_search() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let typed = serde_json::json!({"query": "x", "kinds": ["work_items"], "types": ["epic"]});
        for params in [serde_json::json!({"query": "x"}), typed] {
            let reply = handle_forskapd("org.thehoster.forskapd.Search", Some(params), &handlers)
                .await
                .unwrap()
                .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.thehoster.forskapd.NotAuthenticated"),
                "Search is missing its dispatch arm in handle_forskapd"
            );
        }
    }

    /// The arm hands `types` and `exclude_types` on, each to its own end.
    #[tokio::test]
    async fn dispatch_passes_the_search_type_filters_on() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        crate::handlers::tests::seed_corpus(&handlers);
        let found = async |filter: serde_json::Value| -> Vec<String> {
            let mut params = serde_json::json!({"query": "i", "kinds": ["work_items"]});
            params
                .as_object_mut()
                .unwrap()
                .extend(filter.as_object().unwrap().clone());
            let reply = handle_forskapd("org.thehoster.forskapd.Search", Some(params), &handlers)
                .await
                .unwrap()
                .expect("a reply");
            let items = &reply.parameters.expect("a result")["work_items"];
            let items = items.as_array().unwrap().iter();
            items
                .map(|w| w["type"].as_str().unwrap().to_string())
                .collect()
        };
        assert_eq!(
            found(serde_json::json!({})).await,
            ["issue", "epic", "epic"]
        );
        let only = found(serde_json::json!({"types": ["epic"]})).await;
        assert_eq!(only, ["epic", "epic"]);
        let excluded = found(serde_json::json!({"exclude_types": ["epic"]})).await;
        assert_eq!(excluded, ["issue"]);
    }

    /// The methods the work items replaced are gone, not answered.
    #[tokio::test]
    async fn removed_methods_are_not_found() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for method in [
            "GetAssignedIssues",
            "ListIssues",
            "CreateIssue",
            "RecordEpicOpen",
        ] {
            let reply = handle_forskapd(
                &format!("org.thehoster.forskapd.{method}"),
                Some(serde_json::json!({"group_id": 1, "iid": 2})),
                &handlers,
            )
            .await
            .unwrap()
            .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.varlink.service.MethodNotFound"),
                "{method}"
            );
        }
    }

    /// Unparseable arguments are answered, not punished by a dropped
    /// connection: an enum value the interface doesn't have, a missing
    /// required field, a field the method doesn't have (named exactly, in
    /// quotes), nested ones too.
    #[tokio::test]
    async fn invalid_arguments_get_an_invalid_parameter_reply() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for (method, params, names) in [
            (
                "Search",
                Some(serde_json::json!({"query": "x", "kinds": ["boards"]})),
                "boards",
            ),
            (
                "ClearCache",
                Some(serde_json::json!({"scope": ["everything"]})),
                "everything",
            ),
            (
                "Search",
                Some(serde_json::json!({"query": "x", "kinds": ["epics"]})),
                "epics",
            ),
            (
                "RecordOpen",
                Some(serde_json::json!({"project_id": 1, "iid": 2, "kind": "issue"})),
                "issue",
            ),
            (
                "ListWorkItems",
                Some(serde_json::json!({"role": "reviewer"})),
                "reviewer",
            ),
            (
                "ListWorkItems",
                Some(serde_json::json!({"states": ["merged"]})),
                "merged",
            ),
            ("Search", None, "query"),
            (
                "CreateWorkItem",
                Some(serde_json::json!({"project_id": 1})),
                "title",
            ),
            (
                "CreateWorkItem",
                Some(serde_json::json!({"project_id": 1, "title": "x", "parent": {"group_id": 3}})),
                "iid",
            ),
            ("Search", Some(serde_json::json!({"kinds": []})), "query"),
            (
                "Search",
                Some(serde_json::json!({"query": "x", "labels": ["bug"]})),
                r#""labels""#,
            ),
            (
                "Search",
                Some(serde_json::json!({"query": "x", "scope": {"projects": [1], "users": [2]}})),
                r#""scope.users""#,
            ),
            (
                "CreateWorkItem",
                Some(serde_json::json!({
                    "project_id": 1,
                    "title": "x",
                    "parent": {"group_id": 3, "iid": 5, "state": "opened"},
                })),
                r#""parent.state""#,
            ),
            (
                "RecordOpen",
                Some(serde_json::json!({"kind": "work_item", "iid": 2, "project_id": 1, "at": 0})),
                r#""at""#,
            ),
            (
                "GetHistory",
                Some(serde_json::json!({"since": 0})),
                r#""since""#,
            ),
        ] {
            let reply = handle_forskapd(
                &format!("org.thehoster.forskapd.{method}"),
                params,
                &handlers,
            )
            .await
            .unwrap()
            .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.varlink.service.InvalidParameter"),
                "{method}"
            );
            let parameter = reply.parameters.unwrap()["parameter"].to_string();
            assert!(parameter.contains(names), "{method}: {parameter}");
        }
    }

    /// Every argument of these is optional, so a call without a
    /// `parameters` block is valid.
    #[tokio::test]
    async fn optional_arguments_may_be_omitted() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for method in ["ClearCache", "GetHistory"] {
            let reply =
                handle_forskapd(&format!("org.thehoster.forskapd.{method}"), None, &handlers)
                    .await
                    .unwrap()
                    .expect("a reply");
            assert!(reply.error.is_none(), "{method}: {:?}", reply.error);
        }
    }

    /// Answered whatever the session is, without arguments.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_get_status() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let reply = handle_forskapd("org.thehoster.forskapd.GetStatus", None, &handlers)
            .await
            .unwrap()
            .expect("a reply");
        assert!(
            reply.error.is_none(),
            "GetStatus is missing its dispatch arm: {:?}",
            reply.error
        );
        let status = reply.parameters.expect("a result");
        assert_eq!(status["api_version"], forskap_api::API_VERSION);
        assert_eq!(status["connected"], false);
    }

    #[tokio::test]
    async fn dispatch_has_an_arm_for_get_sync_jobs() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let reply = handle_forskapd("org.thehoster.forskapd.GetSyncJobs", None, &handlers)
            .await
            .unwrap()
            .expect("a reply");
        assert!(
            reply.error.is_none(),
            "GetSyncJobs is missing its dispatch arm: {:?}",
            reply.error
        );
    }

    /// A project's work item, a group's and a merge request.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_record_open() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for params in [
            serde_json::json!({"kind": "work_item", "iid": 2, "project_id": 1}),
            serde_json::json!({"kind": "work_item", "iid": 2, "group_id": 1}),
            serde_json::json!({"kind": "merge_request", "iid": 2, "project_id": 1}),
        ] {
            let reply = handle_forskapd(
                "org.thehoster.forskapd.RecordOpen",
                Some(params.clone()),
                &handlers,
            )
            .await
            .unwrap()
            .expect("a reply");
            assert!(
                reply.error.is_none(),
                "RecordOpen is missing its dispatch arm or rejected {params}: {:?}",
                reply.error
            );
        }
    }

    /// Dormant and never synced: the arm answers `NotAuthenticated`, not
    /// `MethodNotFound`, with and without parameters.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_get_assigned_work_items() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for params in [None, Some(serde_json::json!({"groups": ["team"]}))] {
            let reply = handle_forskapd(
                "org.thehoster.forskapd.GetAssignedWorkItems",
                params,
                &handlers,
            )
            .await
            .unwrap()
            .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.thehoster.forskapd.NotAuthenticated"),
                "GetAssignedWorkItems is missing its dispatch arm in handle_forskapd"
            );
        }
    }

    /// Dormant and never synced: the arm answers `NotAuthenticated`, not
    /// `MethodNotFound`, with and without parameters.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_get_activity() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for params in [None, Some(serde_json::json!({"days": 3}))] {
            let reply = handle_forskapd("org.thehoster.forskapd.GetActivity", params, &handlers)
                .await
                .unwrap()
                .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.thehoster.forskapd.NotAuthenticated"),
                "GetActivity is missing its dispatch arm in handle_forskapd"
            );
        }
    }

    /// Dormant and never synced: the arm answers `NotAuthenticated`, not
    /// `MethodNotFound`, with and without parameters.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_list_work_items() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let filtered = serde_json::json!({
            "role": "author",
            "updated_after": 1_782_900_000,
            "states": ["opened", "closed"],
        });
        for params in [None, Some(filtered)] {
            let reply = handle_forskapd("org.thehoster.forskapd.ListWorkItems", params, &handlers)
                .await
                .unwrap()
                .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.thehoster.forskapd.NotAuthenticated"),
                "ListWorkItems is missing its dispatch arm in handle_forskapd"
            );
        }
    }

    /// Dormant: the arm answers `NotAuthenticated`, not `MethodNotFound`,
    /// with the optional arguments and without.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_create_work_item() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let full = serde_json::json!({
            "project_id": 1,
            "title": "x",
            "description": "y",
            "labels": ["bug"],
            "assign_self": true,
            "parent": {"group_id": 3, "iid": 5, "type": "epic"},
        });
        for params in [serde_json::json!({"project_id": 1, "title": "x"}), full] {
            let reply = handle_forskapd(
                "org.thehoster.forskapd.CreateWorkItem",
                Some(params),
                &handlers,
            )
            .await
            .unwrap()
            .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.thehoster.forskapd.NotAuthenticated"),
                "CreateWorkItem is missing its dispatch arm in handle_forskapd"
            );
        }
    }
}
