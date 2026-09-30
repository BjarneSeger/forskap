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
    Call_DismissFailure, Call_GetActivity, Call_GetAssignedIssues, Call_GetAssignedMergeRequests,
    Call_GetFailures, Call_GetHistory, Call_GetSyncJobs, Call_Login, Call_Logout, Call_PostTime,
    Call_RecordEpicOpen, Call_RecordOpen, Call_RetryFailure, Call_Search, Call_UnassignSelf,
    Call_WhoAmI, ClearCache_Args, Close_Args, DismissFailure_Args, GetActivity_Args,
    GetAssignedIssues_Args, GetAssignedMergeRequests_Args, GetHistory_Args, Login_Args,
    PostTime_Args, RecordEpicOpen_Args, RecordOpen_Args, RetryFailure_Args, Search_Args,
    UnassignSelf_Args, VARLINK_INTERFACE_DESCRIPTION, VarlinkInterface as _,
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
/// don't parse (a missing field, an unknown enum value). An omitted
/// `parameters` block reads as an empty one: a valid call of a method whose
/// arguments are all optional.
fn parse_args<T: DeserializeOwned>(params: Option<serde_json::Value>) -> Result<T, Reply> {
    let params = params.unwrap_or_else(|| serde_json::json!({}));
    serde_json::from_value(params).map_err(|e| {
        Reply::error(
            "org.varlink.service.InvalidParameter",
            Some(serde_json::json!({"parameter": e.to_string()})),
        )
    })
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
        "org.thehoster.forskapd.GetAssignedIssues" => {
            let args: GetAssignedIssues_Args = args!();
            handlers
                .get_assigned_issues(&mut call as &mut dyn Call_GetAssignedIssues, args.groups)
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
        "org.thehoster.forskapd.Search" => {
            let args: Search_Args = args!();
            handlers
                .search(
                    &mut call as &mut dyn Call_Search,
                    args.query,
                    args.kinds,
                    args.limit,
                    args.scope,
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
                    args.project_id,
                    args.iid,
                    args.kind,
                )
                .await?;
        }
        "org.thehoster.forskapd.RecordEpicOpen" => {
            let args: RecordEpicOpen_Args = args!();
            handlers
                .record_epic_open(
                    &mut call as &mut dyn Call_RecordEpicOpen,
                    args.group_id,
                    args.iid,
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
        let reply = handle_forskapd(
            "org.thehoster.forskapd.Search",
            Some(serde_json::json!({"query": "x"})),
            &handlers,
        )
        .await
        .unwrap()
        .expect("a reply");
        assert_ne!(
            reply.error.as_deref(),
            Some("org.varlink.service.MethodNotFound"),
            "Search is missing its dispatch arm in handle_forskapd"
        );
    }

    /// Unparseable arguments are answered, not punished by a dropped
    /// connection: an enum value the interface doesn't have, a missing
    /// required field.
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
                "RecordOpen",
                Some(serde_json::json!({"project_id": 1, "iid": 2, "kind": "epic"})),
                "epic",
            ),
            ("Search", None, "query"),
            ("Search", Some(serde_json::json!({"kinds": []})), "query"),
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

    #[tokio::test]
    async fn dispatch_has_an_arm_for_record_open() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let reply = handle_forskapd(
            "org.thehoster.forskapd.RecordOpen",
            Some(serde_json::json!({"project_id": 1, "iid": 2, "kind": "issue"})),
            &handlers,
        )
        .await
        .unwrap()
        .expect("a reply");
        assert!(
            reply.error.is_none(),
            "RecordOpen is missing its dispatch arm or rejected valid args: {:?}",
            reply.error
        );
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

    #[tokio::test]
    async fn dispatch_has_an_arm_for_record_epic_open() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let reply = handle_forskapd(
            "org.thehoster.forskapd.RecordEpicOpen",
            Some(serde_json::json!({"group_id": 1, "iid": 2})),
            &handlers,
        )
        .await
        .unwrap()
        .expect("a reply");
        assert!(
            reply.error.is_none(),
            "RecordEpicOpen is missing its dispatch arm or rejected valid args: {:?}",
            reply.error
        );
    }
}
