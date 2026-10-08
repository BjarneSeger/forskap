//! Varlink protocol dispatcher.
//!
//! Splits the framework-level `org.varlink.service.*` methods from the
//! methods of `org.thehoster.forskapd` and of `org.thehoster.forskapd.admin`
//! so each match stays short and self-evident.

use std::sync::Arc;

use serde::de::DeserializeOwned;
use tracing::{debug, warn};
use varlink::Reply;
use varlink::sansio::ServerEvent;

use forskap_api::admin::{
    self, Call_ClearCache, Call_GetSyncJobs, Call_Login, Call_Logout, ClearCache_Args,
    GetSyncJobs_Args, Login_Args, Logout_Args,
};
use forskap_api::{
    AssignSelf_Args, AsyncCall, Call_AssignSelf, Call_ClearFailures, Call_Close,
    Call_CreateWorkItem, Call_DismissFailure, Call_GetActivity, Call_GetAssignedMergeRequests,
    Call_GetAssignedWorkItems, Call_GetDescriptionTemplates, Call_GetFailures, Call_GetHistory,
    Call_GetQueue, Call_GetStatus, Call_ListWorkItems, Call_PostTime, Call_RecordOpen,
    Call_RetryFailure, Call_Search, Call_UnassignSelf, Call_WhoAmI, ClearFailures_Args, Close_Args,
    CreateWorkItem_Args, DismissFailure_Args, GetActivity_Args, GetAssignedMergeRequests_Args,
    GetAssignedWorkItems_Args, GetDescriptionTemplates_Args, GetFailures_Args, GetHistory_Args,
    GetQueue_Args, GetStatus_Args, ListWorkItems_Args, PostTime_Args, RecordOpen_Args,
    RetryFailure_Args, Search_Args, UnassignSelf_Args, VARLINK_INTERFACE_DESCRIPTION,
    VarlinkInterface as _, WhoAmI_Args,
};

use crate::handlers::Handlers;

const FORSKAPD: &str = "org.thehoster.forskapd";
const ADMIN: &str = "org.thehoster.forskapd.admin";

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
                    // The varlink crate doesn't hold the reply back: a oneway
                    // caller would read it as the answer to its next call.
                    let oneway = request.oneway.unwrap_or(false);
                    let method = request.method.as_ref();
                    let reply = match handle_varlink_meta(method, &request) {
                        Some(reply) => Some(reply),
                        None => dispatch(method, request.parameters, &self.handlers).await?,
                    };
                    if let Some(reply) = reply.filter(|_| !oneway) {
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
            "interfaces": ["org.varlink.service", FORSKAPD, ADMIN]
        })))),
        "org.varlink.service.GetInterfaceDescription" => {
            let name = request
                .parameters
                .as_ref()
                .and_then(|p| p.get("interface"))
                .and_then(|v| v.as_str());
            let desc = match name {
                Some("org.varlink.service") => Some(ORG_VARLINK_SERVICE_DESCRIPTION),
                Some(FORSKAPD) => Some(VARLINK_INTERFACE_DESCRIPTION),
                Some(ADMIN) => Some(admin::VARLINK_INTERFACE_DESCRIPTION),
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

/// Hands a call to the interface its method is named under.
async fn dispatch(
    method: &str,
    params: Option<serde_json::Value>,
    handlers: &Handlers,
) -> varlink::Result<Option<Reply>> {
    if in_interface(method, ADMIN) {
        handle_admin(method, params, handlers).await
    } else if in_interface(method, FORSKAPD) {
        handle_forskapd(method, params, handlers).await
    } else {
        warn!(method, "unknown varlink method");
        Ok(Some(method_not_found(method)))
    }
}

/// Whether `method` is one of `interface`'s: the interface's name, a dot and
/// the method's own name. The admin interface's name begins with the other's.
fn in_interface(method: &str, interface: &str) -> bool {
    method
        .strip_prefix(interface)
        .and_then(|rest| rest.strip_prefix('.'))
        .is_some_and(|name| !name.contains('.'))
}

fn method_not_found(method: &str) -> Reply {
    Reply::error(
        "org.varlink.service.MethodNotFound",
        Some(serde_json::json!({"method": method})),
    )
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

/// The arguments of the call of `method`, or return the `InvalidParameter`
/// reply from the dispatcher: returning the error instead would drop the
/// connection without a reply.
macro_rules! args {
    ($method:ident, $params:ident) => {
        match parse_args($params) {
            Ok(args) => args,
            Err(reply) => {
                warn!(method = $method, "invalid varlink parameters");
                return Ok(Some(reply));
            }
        }
    };
}

async fn handle_forskapd(
    method: &str,
    params: Option<serde_json::Value>,
    handlers: &Handlers,
) -> varlink::Result<Option<Reply>> {
    let mut call = AsyncCall::default();
    match method {
        "org.thehoster.forskapd.GetHistory" => {
            let args: GetHistory_Args = args!(method, params);
            handlers
                .get_history(&mut call as &mut dyn Call_GetHistory, args.days)
                .await?;
        }
        "org.thehoster.forskapd.GetActivity" => {
            let args: GetActivity_Args = args!(method, params);
            handlers
                .get_activity(&mut call as &mut dyn Call_GetActivity, args.days)
                .await?;
        }
        "org.thehoster.forskapd.GetDescriptionTemplates" => {
            let args: GetDescriptionTemplates_Args = args!(method, params);
            handlers
                .get_description_templates(
                    &mut call as &mut dyn Call_GetDescriptionTemplates,
                    args.project_id,
                    args.kind,
                )
                .await?;
        }
        "org.thehoster.forskapd.GetQueue" => {
            let GetQueue_Args {} = args!(method, params);
            handlers
                .get_queue(&mut call as &mut dyn Call_GetQueue)
                .await?;
        }
        "org.thehoster.forskapd.GetFailures" => {
            let GetFailures_Args {} = args!(method, params);
            handlers
                .get_failures(&mut call as &mut dyn Call_GetFailures)
                .await?;
        }
        "org.thehoster.forskapd.GetStatus" => {
            let GetStatus_Args {} = args!(method, params);
            handlers
                .get_status(&mut call as &mut dyn Call_GetStatus)
                .await?;
        }
        "org.thehoster.forskapd.RetryFailure" => {
            let args: RetryFailure_Args = args!(method, params);
            handlers
                .retry_failure(&mut call as &mut dyn Call_RetryFailure, args.id)
                .await?;
        }
        "org.thehoster.forskapd.DismissFailure" => {
            let args: DismissFailure_Args = args!(method, params);
            handlers
                .dismiss_failure(&mut call as &mut dyn Call_DismissFailure, args.id)
                .await?;
        }
        "org.thehoster.forskapd.ClearFailures" => {
            let ClearFailures_Args {} = args!(method, params);
            handlers
                .clear_failures(&mut call as &mut dyn Call_ClearFailures)
                .await?;
        }
        "org.thehoster.forskapd.GetAssignedWorkItems" => {
            let args: GetAssignedWorkItems_Args = args!(method, params);
            handlers
                .get_assigned_work_items(
                    &mut call as &mut dyn Call_GetAssignedWorkItems,
                    args.scope,
                )
                .await?;
        }
        "org.thehoster.forskapd.GetAssignedMergeRequests" => {
            let args: GetAssignedMergeRequests_Args = args!(method, params);
            handlers
                .get_assigned_merge_requests(
                    &mut call as &mut dyn Call_GetAssignedMergeRequests,
                    args.scope,
                )
                .await?;
        }
        "org.thehoster.forskapd.ListWorkItems" => {
            let args: ListWorkItems_Args = args!(method, params);
            handlers
                .list_work_items(&mut call as &mut dyn Call_ListWorkItems, args.filter)
                .await?;
        }
        "org.thehoster.forskapd.Search" => {
            let args: Search_Args = args!(method, params);
            handlers
                .search(&mut call as &mut dyn Call_Search, args.query, args.options)
                .await?;
        }
        "org.thehoster.forskapd.PostTime" => {
            let args: PostTime_Args = args!(method, params);
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
            let args: Close_Args = args!(method, params);
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
            let args: RecordOpen_Args = args!(method, params);
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
            let args: AssignSelf_Args = args!(method, params);
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
            let args: UnassignSelf_Args = args!(method, params);
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
            let args: CreateWorkItem_Args = args!(method, params);
            handlers
                .create_work_item(
                    &mut call as &mut dyn Call_CreateWorkItem,
                    args.project_id,
                    args.item,
                )
                .await?;
        }
        "org.thehoster.forskapd.WhoAmI" => {
            let WhoAmI_Args {} = args!(method, params);
            handlers.who_am_i(&mut call as &mut dyn Call_WhoAmI).await?;
        }
        _ => {
            warn!(method, "unknown forskapd method");
            return Ok(Some(method_not_found(method)));
        }
    }
    Ok(call.take_reply())
}

async fn handle_admin(
    method: &str,
    params: Option<serde_json::Value>,
    handlers: &Handlers,
) -> varlink::Result<Option<Reply>> {
    use admin::VarlinkInterface as _;

    let mut call = admin::AsyncCall::default();
    match method {
        "org.thehoster.forskapd.admin.ClearCache" => {
            let args: ClearCache_Args = args!(method, params);
            handlers
                .clear_cache(&mut call as &mut dyn Call_ClearCache, args.scope)
                .await?;
        }
        "org.thehoster.forskapd.admin.GetSyncJobs" => {
            let GetSyncJobs_Args {} = args!(method, params);
            handlers
                .get_sync_jobs(&mut call as &mut dyn Call_GetSyncJobs)
                .await?;
        }
        "org.thehoster.forskapd.admin.Login" => {
            let args: Login_Args = args!(method, params);
            handlers
                .login(&mut call as &mut dyn Call_Login, args.host, args.token)
                .await?;
        }
        "org.thehoster.forskapd.admin.Logout" => {
            let Logout_Args {} = args!(method, params);
            handlers.logout(&mut call as &mut dyn Call_Logout).await?;
        }
        _ => {
            warn!(method, "unknown forskapd admin method");
            return Ok(Some(method_not_found(method)));
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
        let typed = serde_json::json!({
            "query": "x",
            "options": {"kinds": ["work_items"], "types": ["epic"]},
        });
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

    /// A value inside an argument struct is named by its path.
    #[tokio::test]
    async fn a_refused_option_is_named_by_its_path() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for (method, params, argument) in [
            (
                "Search",
                serde_json::json!({"query": "x", "options": {"limit": 0}}),
                "options.limit",
            ),
            (
                "CreateWorkItem",
                serde_json::json!({"project_id": 1, "item": {"title": " "}}),
                "item.title",
            ),
        ] {
            let reply = handle_forskapd(
                &format!("org.thehoster.forskapd.{method}"),
                Some(params),
                &handlers,
            )
            .await
            .unwrap()
            .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.thehoster.forskapd.InvalidArgument"),
                "{method}"
            );
            assert_eq!(reply.parameters.unwrap()["argument"], argument);
        }
    }

    /// The arm hands `types` and `exclude_types` on, each to its own end.
    #[tokio::test]
    async fn dispatch_passes_the_search_type_filters_on() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        crate::handlers::tests::seed_corpus(&handlers);
        let found = async |filter: serde_json::Value| -> Vec<String> {
            let mut options = serde_json::json!({"kinds": ["work_items"]});
            options
                .as_object_mut()
                .unwrap()
                .extend(filter.as_object().unwrap().clone());
            let params = serde_json::json!({"query": "i", "options": options});
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

    /// The replies `calls` get on one connection, through the whole service.
    async fn serve(
        handlers: &Arc<Handlers>,
        calls: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        use varlink::AsyncConnectionHandler as _;

        let service = ServiceHandler::new(Arc::clone(handlers));
        let mut server = varlink::sansio::Server::new();
        for call in calls {
            let mut message = serde_json::to_vec(call).unwrap();
            message.push(0);
            server.handle_input(&message).unwrap();
        }
        service.handle(&mut server, None).await.unwrap();
        std::iter::from_fn(|| server.poll_transmit())
            .map(|t| serde_json::from_slice(t.payload.strip_suffix(&[0]).unwrap()).unwrap())
            .collect()
    }

    /// A oneway call runs and gets no reply, not even an error: the one
    /// answer on the connection is the next call's. The admin interface's
    /// calls too.
    #[tokio::test]
    async fn a_oneway_call_gets_no_reply() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let handlers = Arc::new(handlers);
        let open = |iid| serde_json::json!({"kind": "work_item", "iid": iid, "project_id": 1});
        let sent = serve(
            &handlers,
            &[
                serde_json::json!({"method": "org.thehoster.forskapd.RecordOpen", "parameters": open(2), "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.NoSuchMethod", "oneway": true}),
                serde_json::json!({"method": "org.example.Other", "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.Close", "parameters": {"project_id": 1}, "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.WhoAmI", "oneway": true}),
                serde_json::json!({"method": "org.varlink.service.GetInfo", "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.admin.ClearCache", "parameters": {"scope": ["usage"]}, "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.admin.Logout", "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.admin.GetSyncJobs", "parameters": {"key": "events"}, "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.admin.NoSuchMethod", "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.RecordOpen", "parameters": open(3), "oneway": true}),
                serde_json::json!({"method": "org.thehoster.forskapd.GetStatus", "oneway": false}),
            ],
        )
        .await;

        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            sent[0]["parameters"]["api_version"],
            forskap_api::API_VERSION,
            "{sent:?}"
        );
        // The clear ran between the two opens.
        let entries = handlers.usage.snapshot().unwrap().entries;
        assert_eq!(entries.len(), 1, "{entries:?}");
    }

    /// `GetInfo` names both interfaces, and each describes itself.
    #[tokio::test]
    async fn the_service_offers_both_interfaces() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let handlers = Arc::new(handlers);
        let describe = |interface: &str| {
            serde_json::json!({
                "method": "org.varlink.service.GetInterfaceDescription",
                "parameters": {"interface": interface},
            })
        };
        let sent = serve(
            &handlers,
            &[
                serde_json::json!({"method": "org.varlink.service.GetInfo"}),
                describe(FORSKAPD),
                describe(ADMIN),
            ],
        )
        .await;
        assert_eq!(
            sent[0]["parameters"]["interfaces"],
            serde_json::json!(["org.varlink.service", FORSKAPD, ADMIN])
        );
        let description = |i: usize| sent[i]["parameters"]["description"].as_str().unwrap();
        assert_eq!(description(1), VARLINK_INTERFACE_DESCRIPTION);
        assert_eq!(description(2), admin::VARLINK_INTERFACE_DESCRIPTION);
        assert!(description(2).contains("interface org.thehoster.forskapd.admin\n"));
    }

    #[test]
    fn a_method_belongs_to_the_interface_it_is_named_under() {
        let admin = "org.thehoster.forskapd.admin.Login";
        assert!(in_interface(admin, ADMIN));
        assert!(!in_interface(admin, FORSKAPD));
        let main = "org.thehoster.forskapd.WhoAmI";
        assert!(in_interface(main, FORSKAPD));
        assert!(!in_interface(main, ADMIN));
        assert!(!in_interface("org.thehoster.forskapdx.WhoAmI", FORSKAPD));
        assert!(!in_interface("org.thehoster.forskapd", FORSKAPD));
    }

    const MOVED: [&str; 4] = ["ClearCache", "GetSyncJobs", "Login", "Logout"];

    /// The methods that moved to the admin interface are gone from the main
    /// one, and the admin one has none of the main one's.
    #[tokio::test]
    async fn each_interface_answers_its_own_methods_only() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let not_found = async |method: String| {
            let reply = dispatch(&method, None, &handlers).await.unwrap();
            let reply = reply.expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.varlink.service.MethodNotFound"),
                "{method}"
            );
            assert_eq!(reply.parameters.unwrap()["method"], method.as_str());
        };
        for method in MOVED {
            not_found(format!("{FORSKAPD}.{method}")).await;
            let declared = format!("method {method}(");
            assert!(
                !VARLINK_INTERFACE_DESCRIPTION.contains(&declared),
                "{method}"
            );
            assert!(admin::VARLINK_INTERFACE_DESCRIPTION.contains(&declared));
        }
        for method in ["WhoAmI", "GetStatus", "Search", "GetFailures"] {
            not_found(format!("{ADMIN}.{method}")).await;
        }
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
                Some(serde_json::json!({"query": "x", "options": {"kinds": ["boards"]}})),
                "boards",
            ),
            (
                "admin.ClearCache",
                Some(serde_json::json!({"scope": ["everything"]})),
                "everything",
            ),
            (
                "admin.ClearCache",
                Some(serde_json::json!({"scope": ["usage"], "wait": false})),
                r#""wait""#,
            ),
            (
                "admin.Login",
                Some(serde_json::json!({"host": "x"})),
                "token",
            ),
            (
                "admin.Login",
                Some(serde_json::json!({"host": "x", "token": "y", "user": "z"})),
                r#""user""#,
            ),
            (
                "Search",
                Some(serde_json::json!({"query": "x", "options": {"kinds": ["epics"]}})),
                "epics",
            ),
            (
                "RecordOpen",
                Some(serde_json::json!({"project_id": 1, "iid": 2, "kind": "issue"})),
                "issue",
            ),
            (
                "ListWorkItems",
                Some(serde_json::json!({"filter": {"role": "reviewer"}})),
                "reviewer",
            ),
            (
                "ListWorkItems",
                Some(serde_json::json!({"filter": {"states": ["merged"]}})),
                "merged",
            ),
            ("Search", None, "query"),
            ("GetDescriptionTemplates", None, "project_id"),
            (
                "GetDescriptionTemplates",
                Some(serde_json::json!({"project_id": 1, "kind": "epic"})),
                "epic",
            ),
            (
                "GetDescriptionTemplates",
                Some(serde_json::json!({"project_id": 1, "type": "issues"})),
                "type",
            ),
            (
                "CreateWorkItem",
                Some(serde_json::json!({"project_id": 1})),
                "item",
            ),
            (
                "CreateWorkItem",
                Some(serde_json::json!({"project_id": 1, "item": {}})),
                "title",
            ),
            (
                "CreateWorkItem",
                Some(serde_json::json!({
                    "project_id": 1,
                    "item": {"title": "x", "parent": {"group_id": 3}},
                })),
                "iid",
            ),
            (
                "Search",
                Some(serde_json::json!({"options": {"kinds": []}})),
                "query",
            ),
            (
                "Search",
                Some(serde_json::json!({"query": "x", "options": {"labels": ["bug"]}})),
                r#""options.labels""#,
            ),
            (
                "Search",
                Some(serde_json::json!({
                    "query": "x",
                    "options": {"scope": {"projects": [1], "users": [2]}},
                })),
                r#""options.scope.users""#,
            ),
            // The arguments the options replaced.
            (
                "Search",
                Some(serde_json::json!({"query": "x", "limit": 5})),
                r#""limit""#,
            ),
            (
                "GetAssignedWorkItems",
                Some(serde_json::json!({"groups": ["team"]})),
                r#""groups""#,
            ),
            (
                "GetAssignedMergeRequests",
                Some(serde_json::json!({"scope": {"groups": ["team"], "users": [2]}})),
                r#""scope.users""#,
            ),
            (
                "ListWorkItems",
                Some(serde_json::json!({"role": "author"})),
                r#""role""#,
            ),
            (
                "CreateWorkItem",
                Some(serde_json::json!({"project_id": 1, "title": "x", "item": {"title": "x"}})),
                r#""title""#,
            ),
            (
                "CreateWorkItem",
                Some(serde_json::json!({
                    "project_id": 1,
                    "item": {
                        "title": "x",
                        "parent": {"group_id": 3, "iid": 5, "state": "opened"},
                    },
                })),
                r#""item.parent.state""#,
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
            // The methods without arguments as well.
            (
                "GetFailures",
                Some(serde_json::json!({"op": "Close"})),
                r#""op""#,
            ),
            (
                "GetQueue",
                Some(serde_json::json!({"op": "Close"})),
                r#""op""#,
            ),
            (
                "ClearFailures",
                Some(serde_json::json!({"ids": [1]})),
                r#""ids""#,
            ),
            (
                "admin.GetSyncJobs",
                Some(serde_json::json!({"key": "events"})),
                r#""key""#,
            ),
            (
                "GetStatus",
                Some(serde_json::json!({"verbose": true})),
                r#""verbose""#,
            ),
            (
                "WhoAmI",
                Some(serde_json::json!({"host": "x"})),
                r#""host""#,
            ),
            (
                "admin.Logout",
                Some(serde_json::json!({"forget": true})),
                r#""forget""#,
            ),
        ] {
            let reply = dispatch(&format!("{FORSKAPD}.{method}"), params, &handlers)
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

    /// Every argument of these is optional, or they have none, so a call
    /// without a `parameters` block or with an empty one is valid: answered,
    /// dormant, as the method answers it.
    #[tokio::test]
    async fn optional_arguments_may_be_omitted() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        for (method, error) in [
            ("admin.ClearCache", None),
            ("GetHistory", None),
            ("GetFailures", None),
            ("GetQueue", None),
            ("ClearFailures", None),
            ("admin.GetSyncJobs", None),
            ("GetStatus", None),
            ("WhoAmI", Some("org.thehoster.forskapd.NotAuthenticated")),
            // The tests' disabled keychain turns it down.
            (
                "admin.Logout",
                Some("org.thehoster.forskapd.admin.Internal"),
            ),
        ] {
            for params in [None, Some(serde_json::json!({}))] {
                let reply = dispatch(&format!("{FORSKAPD}.{method}"), params.clone(), &handlers)
                    .await
                    .unwrap()
                    .expect("a reply");
                assert_eq!(reply.error.as_deref(), error, "{method} {params:?}");
            }
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
    async fn dispatch_has_an_arm_for_get_queue() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let reply = handle_forskapd("org.thehoster.forskapd.GetQueue", None, &handlers)
            .await
            .unwrap()
            .expect("a reply");
        assert!(
            reply.error.is_none(),
            "GetQueue is missing its dispatch arm: {:?}",
            reply.error
        );
        assert!(reply.parameters.unwrap()["writes"].is_array());
    }

    #[tokio::test]
    async fn dispatch_has_an_arm_for_get_sync_jobs() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let reply = handle_admin("org.thehoster.forskapd.admin.GetSyncJobs", None, &handlers)
            .await
            .unwrap()
            .expect("a reply");
        assert!(
            reply.error.is_none(),
            "GetSyncJobs is missing its dispatch arm: {:?}",
            reply.error
        );
        assert!(reply.parameters.unwrap()["jobs"].is_array());
    }

    /// Dormant, it clears and replies at once, every scope or some.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_clear_cache() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let scoped = serde_json::json!({"scope": ["assigned", "usage"]});
        for params in [None, Some(scoped)] {
            let reply = handle_admin("org.thehoster.forskapd.admin.ClearCache", params, &handlers)
                .await
                .unwrap()
                .expect("a reply");
            assert!(
                reply.error.is_none(),
                "ClearCache is missing its dispatch arm: {:?}",
                reply.error
            );
        }
    }

    /// The tests' disabled keychain turns both down, as the admin
    /// interface's own `Internal`.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_login_and_logout() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let login = serde_json::json!({"host": "gitlab.invalid", "token": "glpat-x"});
        for (method, params) in [("Login", Some(login)), ("Logout", None)] {
            let reply = handle_admin(&format!("{ADMIN}.{method}"), params, &handlers)
                .await
                .unwrap()
                .expect("a reply");
            assert_eq!(
                reply.error.as_deref(),
                Some("org.thehoster.forskapd.admin.Internal"),
                "{method} is missing its dispatch arm"
            );
        }
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
        let scoped = serde_json::json!({"scope": {"projects": [1], "groups": ["team"]}});
        for params in [None, Some(scoped)] {
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
    /// `MethodNotFound`.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_get_work_item_templates() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let params = Some(serde_json::json!({"project_id": 7}));
        let reply = handle_forskapd(
            "org.thehoster.forskapd.GetDescriptionTemplates",
            params,
            &handlers,
        )
        .await
        .unwrap()
        .expect("a reply");
        assert_eq!(
            reply.error.as_deref(),
            Some("org.thehoster.forskapd.NotAuthenticated"),
            "GetDescriptionTemplates is missing its dispatch arm in handle_forskapd"
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

    /// Dormant and never synced: the arm answers `NotAuthenticated`, not
    /// `MethodNotFound`, with and without parameters.
    #[tokio::test]
    async fn dispatch_has_an_arm_for_list_work_items() {
        let (handlers, _dir) = crate::handlers::tests::dormant_handlers();
        let filtered = serde_json::json!({"filter": {
            "role": "author",
            "updated_after": 1_782_900_000,
            "states": ["opened", "closed"],
        }});
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
            "item": {
                "title": "x",
                "description": "y",
                "labels": ["bug"],
                "assign_self": true,
                "parent": {"group_id": 3, "iid": 5, "type": "epic"},
            },
        });
        let bare = serde_json::json!({"project_id": 1, "item": {"title": "x"}});
        for params in [bare, full] {
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
