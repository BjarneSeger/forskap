//! What a daemon run is built from, and running it.
//!
//! A normal run and a dry run (`--dry-run`) differ only in their
//! [`Environment`]: the session to start with, the keychain, the config and
//! whether its file is watched, where the database and the avatars live, and
//! the socket. [`Daemon::start`] builds the same daemon from either, and
//! [`Daemon::serve_until`] serves it.
//!
//! - [`Environment::real`]: the user's daemon. Moves the pre-rename
//!   directories, reads the config file and the keychain, connects to GitLab.
//! - [`Environment::dry_run`]: the demo account of [`crate::demo`], in a
//!   [`Scratch`] directory, on the baked-in config and without a keychain.
//!   No supervisor runs: there is no config file to watch, and the
//!   reconnect and the token rotation live off a keychain.

use std::future::Future;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, RwLock};
use tracing::{info, warn};

use crate::args::Args;
use crate::config::{self, SharedConfig};
use crate::demo::{self, DemoGitlab};
use crate::error::{DormancyReason, Result};
use crate::gitlab::GitlabClient;
use crate::handlers::{ConnState, Handlers, Session, SessionSlot};
use crate::queue::{RetryQueue, SettleHook};
use crate::secrets::Keychain;
use crate::service::ServiceHandler;
use crate::sync::store::SyncStore;
use crate::sync::{AvatarDir, Job, JobStatus, SyncHandle, now_secs};
use crate::usage::UsageStats;
use crate::write::Write;
use crate::{db, migrate, reconnect, reload, rotate, server};

/// Cache keyspaces of the stores the sync layer replaced (plus the unmerged
/// tracked-search branch's); re-fetchable, so dropped at startup. The retry
/// queue, dead letters and open statistics are user data and are never
/// listed here.
const RETIRED_KEYSPACES: [&str; 11] = [
    "issues_cache_v1",
    "issues_cache_v2",
    "project_board_labels_v1",
    "timelog_history_v1",
    "refresh_meta_v1",
    "search_issues_v1",
    "search_mrs_v1",
    "search_projects_v1",
    "search_groups_v1",
    "search_meta_v1",
    "search_tracked_v1",
];

/// Everything a run of the daemon is built from.
pub struct Environment {
    pub config: SharedConfig,
    /// Whether edits to the config file are picked up while running.
    pub watch_config: bool,
    pub keychain: Keychain,
    /// The session to start with.
    pub session: ConnState,
    pub db_dir: PathBuf,
    pub avatar_dir: PathBuf,
    pub listen: Listen,
    /// Print the socket's address on stdout once the first sync is done:
    /// how a dry run's caller learns where it is, and that it is ready.
    pub announce: bool,
}

/// Where the daemon listens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listen {
    /// The socket systemd passed (socket activation).
    Activated,
    /// A socket bound at this path (see [`server::bind`]), removed again on
    /// shutdown.
    Bind(String),
}

/// Why the user's daemon can't be set up.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("failed to load configuration: {0}")]
    Config(#[from] confique::Error),
    #[error("no home directory to put the socket in; name one with --socket")]
    NoSocket,
}

impl Environment {
    /// The user's daemon: their config, keychain, GitLab and directories.
    /// Fails on a config file that doesn't parse, and where nothing names a
    /// socket and there is no home directory for the default one.
    pub async fn real(args: &Args) -> std::result::Result<Self, SetupError> {
        migrate::run();

        let config = config::load_shared()?;
        let listen = if server::is_socket_activated() {
            Listen::Activated
        } else {
            let socket = match &args.socket {
                Some(socket) => socket.clone(),
                None => {
                    let resolved = config.read().unwrap().server.resolved_socket();
                    resolved.ok_or(SetupError::NoSocket)?
                }
            };
            Listen::Bind(socket)
        };
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| "~/.local/share".into())
            .join("forskapd");
        // One-time cleanup of the pre-fjall redb stores; their data was cache and
        // is repopulated by the regular sync.
        for file in [
            "cache.redb",
            "boards.redb",
            "history.redb",
            "queue.redb",
            "dead_letter.redb",
        ] {
            if let Err(e) = std::fs::remove_file(data_dir.join(file))
                && e.kind() != std::io::ErrorKind::NotFound
            {
                warn!(error = %e, file, "failed to remove legacy redb store");
            }
        }
        let db_dir = data_dir.join("db");
        // Re-fetchable, hence the cache directory; launchers read the files.
        let avatar_dir = dirs::cache_dir()
            .unwrap_or_else(|| "~/.cache".into())
            .join("forskapd")
            .join("avatars");

        let keychain = Keychain::Os;
        let session = connect(&keychain).await;
        Ok(Self {
            config,
            watch_config: true,
            keychain,
            session,
            db_dir,
            avatar_dir,
            listen,
            announce: false,
        })
    }

    /// A dry run in `scratch`: the demo account, the baked-in config, no
    /// keychain, and its socket in `scratch` unless `--socket` names one.
    /// Reads no file and asks no one.
    pub fn dry_run(scratch: &Scratch, args: &Args) -> Result<Self> {
        let socket = match &args.socket {
            // Absolute, so the address it announces works from anywhere.
            Some(socket) => std::path::absolute(socket)?,
            None => scratch.path().join("forskapd.socket"),
        };
        info!(
            dir = %scratch.path().display(),
            "dry run: serving a demo account from a temporary directory; \
             no keychain, no GitLab, nothing of the real daemon's is touched"
        );
        let gitlab = Arc::new(DemoGitlab::new(now_secs()));
        Ok(Self {
            config: Arc::new(std::sync::RwLock::new(demo::config())),
            watch_config: false,
            keychain: Keychain::disabled(),
            session: ConnState::Connected(gitlab.session()),
            db_dir: scratch.path().join("db"),
            avatar_dir: scratch.path().join("avatars"),
            listen: Listen::Bind(socket.to_string_lossy().into_owned()),
            announce: true,
        })
    }
}

/// The session the daemon starts with.
///
/// Credentials come only from the OS keychain (set via `forskap auth login`). The
/// daemon never refuses to start: any failure here leaves it *dormant*
/// (serving, but returning `NotAuthenticated`). The dormancy reason is kept
/// in the session slot so the CLI can report a specific cause.
async fn connect(keychain: &Keychain) -> ConnState {
    match keychain.load().await {
        Err(e) => {
            warn!(error = %e, "keychain read failed; starting dormant");
            ConnState::Dormant(DormancyReason::KeychainError(e.to_string()))
        }
        Ok(None) => {
            info!("no credentials available; daemon starting dormant (run `forskap auth login`)");
            ConnState::Dormant(DormancyReason::NoCredentials)
        }
        Ok(Some(c)) => match GitlabClient::connect(&c.host, &c.token).await {
            Ok(client) => {
                let s = Session::from_client(client);
                info!(host = %s.host, user_id = s.user_id, "initial GitLab connection ready");
                ConnState::Connected(s)
            }
            Err(e) => {
                warn!(error = %e, host = %c.host, "initial GitLab connection failed; daemon starting dormant");
                ConnState::Dormant(DormancyReason::from_connect_error(&c.host, &e))
            }
        },
    }
}

/// A dry run's private directory (mode 0700) for its database, avatars and
/// socket. Removed with everything in it when dropped; `main` drops it only
/// after the runtime, whose tasks hold the stores in it.
pub struct Scratch(tempfile::TempDir);

/// What a dry run's socket path adds to the temporary directory's:
/// `/forskapd-dry-run.XXXXXX/forskapd.socket`.
const SOCKET_TAIL: usize = 40;

/// The longest Unix socket path every platform binds: `sun_path` holds 104
/// bytes on macOS, 108 on Linux, the closing NUL included.
const MAX_SOCKET_PATH: usize = 103;

impl Scratch {
    /// A fresh directory under the temporary directory, or under `/tmp`
    /// when a socket in it would be too long a path to bind.
    pub fn create() -> std::io::Result<Self> {
        let mut base = std::env::temp_dir();
        if base.as_os_str().len() + SOCKET_TAIL > MAX_SOCKET_PATH {
            base = PathBuf::from("/tmp");
        }
        let dir = tempfile::Builder::new()
            .prefix("forskapd-dry-run.")
            // Not the umask's 0755: nobody else lists or connects.
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in(base)?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

/// A started daemon: its stores open, its workers and supervisors running,
/// its socket listening.
pub struct Daemon {
    handlers: Arc<Handlers>,
    listener: tokio::net::UnixListener,
    /// The socket file to remove on shutdown: one bound here, not one
    /// systemd passed.
    socket: Option<String>,
    announce: bool,
    /// Held for the run, as the stores are.
    _db: fjall::Database,
}

/// How long a dry run waits for its first sync before it announces its
/// socket anyway. The demo syncs in milliseconds.
const READY_LIMIT: Duration = Duration::from_secs(10);

impl Daemon {
    /// Open the stores, start the sync and queue workers and the supervisors
    /// `env` has a use for, and listen.
    pub fn start(env: Environment) -> Result<Self> {
        let Environment {
            config,
            watch_config,
            keychain,
            session,
            db_dir,
            avatar_dir,
            listen,
            announce,
        } = env;
        let session: SessionSlot = Arc::new(RwLock::new(session));

        std::fs::create_dir_all(&db_dir)?;
        let db = fjall::Database::builder(&db_dir).open()?;
        match db::drop_keyspaces(&db, &RETIRED_KEYSPACES) {
            Ok(dropped) if !dropped.is_empty() => {
                info!(?dropped, "dropped retired cache keyspaces")
            }
            Ok(_) => {}
            Err(e) => warn!(error = %e, "dropping retired cache keyspaces failed"),
        }
        let store = Arc::new(SyncStore::open(&db)?);
        let usage = Arc::new(UsageStats::open(&db)?);
        // Woken when the sync worker demotes the session to `Dormant(Unreachable)`,
        // so the reconnect supervisor re-engages mid-run and not only at boot.
        let reconnect_signal = Arc::new(Notify::new());
        // The only GitLab reader: fetches on its jittered schedule into `store`,
        // which the handlers serve from.
        let sync = SyncHandle::spawn(
            store,
            AvatarDir::new(avatar_dir),
            Arc::clone(&session),
            Arc::clone(&config),
            Arc::clone(&reconnect_signal),
            reconnect::keychain_probe(keychain.clone()),
        );
        let queue = RetryQueue::new(Arc::clone(&session), &db, Arc::clone(&config))?;
        queue.on_settled(settle_hook(&sync, &config));
        let rotation = Arc::new(rotate::Rotation::default());
        let handlers = Arc::new(Handlers {
            session,
            sync: Arc::clone(&sync),
            usage,
            queue,
            config: Arc::clone(&config),
            reconnect_signal,
            rotation: Arc::clone(&rotation),
            keychain,
        });

        if watch_config {
            reload::spawn(Arc::clone(&config), move || {
                sync.reconfigure();
                rotation.reevaluate();
            });
        }

        // Both live off the keychain: a reconnect reads the token from it, a
        // rotation stores the new one there. Without one they don't run.
        if handlers.keychain.require().is_ok() {
            // If the daemon booted dormant because GitLab was unreachable, retry the
            // connection in the background with exponential backoff. A successful
            // reconnect flips the shared session to `Connected`, which drains the retry
            // queue and wakes the sync worker. No-op when already connected or when
            // dormancy needs the user (bad token / logged out).
            reconnect::spawn(Arc::clone(&handlers));

            // Replaces the token by a fresh one shortly before it expires; idles on
            // a token that doesn't rotate.
            rotate::spawn(Arc::clone(&handlers));
        }

        let (listener, socket) = match listen {
            Listen::Activated => {
                let listener = server::inherited_listener()?;
                info!("starting forskapd from socket");
                (listener, None)
            }
            Listen::Bind(socket) => {
                let listener = server::bind(&socket)?;
                info!(socket = socket, "starting forskapd");
                (listener, Some(socket))
            }
        };
        Ok(Self {
            handlers,
            listener,
            socket,
            announce,
            _db: db,
        })
    }

    pub fn handlers(&self) -> &Arc<Handlers> {
        &self.handlers
    }

    /// The socket file bound; `None` under socket activation.
    pub fn socket(&self) -> Option<&str> {
        self.socket.as_deref()
    }

    /// Serve until `stop` resolves, then remove the socket file bound. An
    /// environment that announces its socket prints it once the first sync
    /// is done, serving already.
    pub async fn serve_until(self, stop: impl Future<Output = ()>) -> Result<()> {
        let Self {
            handlers,
            listener,
            socket,
            announce,
            _db,
        } = self;
        let service = Arc::new(ServiceHandler::new(Arc::clone(&handlers)));
        let to_announce = socket.clone().filter(|_| announce);
        let announced = async {
            if let Some(socket) = to_announce {
                if !synced(&handlers.sync, READY_LIMIT).await {
                    warn!("the first sync isn't done yet; announcing the socket anyway");
                }
                print_address(&socket)?;
                info!("synced; the socket's address is on stdout");
            }
            Ok(())
        };
        let serve = server::serve(service, listener);
        tokio::pin!(serve, announced, stop);
        let mut announcing = true;
        let served = loop {
            tokio::select! {
                served = &mut serve => break served,
                // Nobody to tell where the socket is: no use serving it.
                done = &mut announced, if announcing => match done {
                    Ok(()) => announcing = false,
                    Err(e) => break Err(e),
                },
                () = &mut stop => break Ok(()),
            }
        };
        if let Some(socket) = &socket {
            let _ = std::fs::remove_file(socket);
        }
        served
    }
}

/// The socket's address on stdout, in the form `FORSKAPD_SOCKET` takes.
fn print_address(socket: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "unix:{socket}")?;
    out.flush()
}

/// Wait until the sync worker has run every job it plans and has nothing
/// due, running or demanded: the store then holds all there is. `false` if
/// that takes longer than `limit`.
pub async fn synced(sync: &SyncHandle, limit: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        let jobs = sync.jobs().await.jobs;
        let done = jobs
            .iter()
            .all(|j| j.status == JobStatus::Waiting && j.last_ok > 0);
        if !jobs.is_empty() && done {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Resolves on SIGINT or SIGTERM, logging which. Both are caught from the
/// moment this returns.
pub fn shutdown_signal() -> Result<impl Future<Output = ()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut sigterm = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = sigint.recv() => info!("received SIGINT, shutting down"),
            _ = sigterm.recv() => info!("received SIGTERM, shutting down"),
        }
    })
}

/// Once a queued write settles, show it: an applied one is noted for the
/// read-time overlay, and either way the jobs displaying it rerun.
fn settle_hook(sync: &Arc<SyncHandle>, config: &config::SharedConfig) -> SettleHook {
    let sync = Arc::clone(sync);
    let config = Arc::clone(config);
    Arc::new(move |write: &Write, queued_at: u64, applied: bool| {
        if applied {
            sync.note_write(write);
        }
        let jobs = Job::affected_by_replay(write, queued_at, &config.read().unwrap(), now_secs());
        sync.refresh_soon(&jobs);
    })
}

/// Dry runs, driven through their socket like any client drives them.
#[cfg(test)]
mod tests {
    use forskap_api::admin::{self, VarlinkClientInterface as _};
    use forskap_api::{
        ErrorKind as WireError, IssuableKind, NewWorkItem, Scope, SearchOptions, VarlinkClient,
        VarlinkClientInterface, WorkItem, WorkItemFilter, WorkItemRef, WorkItemRole,
    };
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;
    use varlink::AsyncConnection;

    use super::*;
    use crate::demo::{ARCHIVED_PROJECT, AVATAR_PROJECT, HOST, USER_ID, USERNAME};

    fn dry_args() -> Args {
        Args {
            dry_run: true,
            socket: None,
        }
    }

    /// A dry run serving in `scratch` until its stop is dropped or sent.
    struct DryRun {
        client: VarlinkClient,
        /// The admin interface, on the same connection.
        admin: admin::VarlinkClient,
        handlers: Arc<Handlers>,
        keychain: Keychain,
        stop: oneshot::Sender<()>,
        served: JoinHandle<Result<()>>,
        socket: String,
    }

    impl DryRun {
        async fn start(scratch: &Scratch) -> Self {
            let mut env = Environment::dry_run(scratch, &dry_args()).unwrap();
            // The test harness doesn't capture a direct write to stdout.
            env.announce = false;
            let keychain = env.keychain.clone();
            let daemon = Daemon::start(env).unwrap();
            let handlers = Arc::clone(daemon.handlers());
            let socket = daemon
                .socket()
                .expect("a dry run binds its socket")
                .to_string();
            let (stop, stopped) = oneshot::channel::<()>();
            let served = tokio::spawn(daemon.serve_until(async {
                let _ = stopped.await;
            }));
            let conn = AsyncConnection::with_address(format!("unix:{socket}"))
                .await
                .unwrap();
            Self {
                client: VarlinkClient::new(Arc::clone(&conn)),
                admin: admin::VarlinkClient::new(conn),
                handlers,
                keychain,
                stop,
                served,
                socket,
            }
        }

        /// Stop serving; the dry run must not have asked the keychain once.
        async fn stop(self) {
            assert_eq!(self.keychain.refused(), 0, "a keychain call was attempted");
            let _ = self.stop.send(());
            self.served.await.unwrap().unwrap();
            assert!(!Path::new(&self.socket).exists(), "the socket is removed");
        }

        async fn assigned_work_items(&self) -> Vec<WorkItem> {
            let call = self.client.get_assigned_work_items(None).call().await;
            call.unwrap().work_items
        }
    }

    /// Poll `probe` every 20 ms until it yields, failing after 5 s: a dry
    /// run fills its store in well under a second, but test binaries share
    /// the machine.
    async fn until<T>(what: &str, mut probe: impl AsyncFnMut() -> Option<T>) -> T {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(found) = probe().await {
                return found;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The issues the user authored.
    fn authored() -> WorkItemFilter {
        WorkItemFilter {
            role: Some(WorkItemRole::author),
            ..Default::default()
        }
    }

    /// An issue with just a title.
    fn new_work_item(title: &str) -> NewWorkItem {
        NewWorkItem {
            title: title.into(),
            description: None,
            labels: None,
            assign_self: None,
            parent: None,
        }
    }

    fn keys(items: &[WorkItem]) -> Vec<(i64, i64)> {
        let key = |i: &WorkItem| (i.project_id.unwrap_or_default(), i.iid);
        items.iter().map(key).collect()
    }

    /// The message and status of a `GitlabError`.
    fn gitlab_error(e: forskap_api::Error) -> (String, Option<i64>) {
        match e.kind() {
            WireError::GitlabError(Some(args)) => (args.message.clone(), args.status),
            other => panic!("expected a GitlabError, got {other:?}"),
        }
    }

    fn internal(e: admin::Error) -> String {
        match e.kind() {
            admin::ErrorKind::Internal(Some(args)) => args.message.clone(),
            other => panic!("expected an Internal, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_dry_run_keeps_to_its_scratch_directory() {
        let scratch = Scratch::create().unwrap();
        let dir = scratch.path();
        let mode = std::fs::metadata(dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "private, whatever the umask");
        let env = Environment::dry_run(&scratch, &dry_args()).unwrap();
        assert!(matches!(env.keychain, Keychain::Disabled(_)));
        assert!(!env.watch_config);
        assert!(env.announce);
        assert!(env.db_dir.starts_with(dir));
        assert!(env.avatar_dir.starts_with(dir));
        let Listen::Bind(socket) = &env.listen else {
            panic!("a dry run binds its own socket");
        };
        assert!(Path::new(socket).starts_with(dir));
        assert!(socket.len() <= MAX_SOCKET_PATH, "{socket}");
        assert_eq!(
            env.config.read().unwrap().server.socket,
            None,
            "no file read"
        );
        let ConnState::Connected(session) = &env.session else {
            panic!("a dry run is logged in");
        };
        assert_eq!(session.host, HOST);

        // What it wrote, once it ran: all of it in there.
        let run = DryRun::start(&scratch).await;
        assert!(synced(&run.handlers.sync, Duration::from_secs(5)).await);
        let avatars: Vec<_> = std::fs::read_dir(dir.join("avatars")).unwrap().collect();
        assert_eq!(avatars.len(), 1);
        assert!(dir.join("db").is_dir());
        run.stop().await;
    }

    /// What a dry run announces its socket after: once [`synced`], every
    /// read serves the whole demo at once.
    #[tokio::test]
    async fn a_dry_run_serves_the_fixture_once_synced() {
        let scratch = Scratch::create().unwrap();
        let run = DryRun::start(&scratch).await;
        let client = &run.client;

        let me = client.who_am_i().call().await.unwrap();
        assert_eq!((me.host.as_str(), me.user_id), (HOST, USER_ID));
        assert_eq!(me.username, USERNAME);
        assert_eq!(me.token_expires_at, None);

        assert!(synced(&run.handlers.sync, Duration::from_secs(5)).await);

        let issues = run.assigned_work_items().await;
        assert_eq!(issues.len(), 6);
        assert!(issues.iter().all(|i| i.board_column.is_some()), "boards");
        let rate_limit = issues.iter().find(|i| i.iid == 12).unwrap();
        assert_eq!(rate_limit.board_column.as_deref(), Some("Doing"));
        assert_eq!(
            rate_limit.namespace_path.as_deref(),
            Some("acme/backend/api")
        );
        assert!(rate_limit.web_url.starts_with("https://dry-run.invalid/"));
        let invoice = issues
            .iter()
            .find(|i| (i.project_id, i.iid) == (Some(102), 7))
            .unwrap();
        assert_eq!(invoice.time_spent, Some(3600));

        // A scope keeps to its projects and groups, subgroups included.
        let scoped = async |scope: Scope| {
            let call = client.get_assigned_work_items(Some(scope)).call().await;
            keys(&call.unwrap().work_items)
        };
        let under = |path: &str| {
            let under = |i: &&WorkItem| i.namespace_path.as_deref().unwrap().starts_with(path);
            let found: Vec<_> = issues.iter().filter(under).cloned().collect();
            keys(&found)
        };
        let api = Scope {
            projects: Some(vec![AVATAR_PROJECT]),
            groups: None,
        };
        assert_eq!(scoped(api).await, under("acme/backend/api"));
        let backend = Scope {
            projects: None,
            groups: Some(vec!["acme/backend".into()]),
        };
        let backend = scoped(backend).await;
        assert!(backend.len() > 1 && backend.len() < issues.len());
        assert_eq!(backend, under("acme/backend/"));

        let mrs = client.get_assigned_merge_requests(None).call().await;
        let mrs = mrs.unwrap().merge_requests;
        assert_eq!(mrs.iter().map(|m| m.iid).collect::<Vec<_>>(), [31, 12, 44]);

        let search = |query: &str| client.search(query.into(), None);
        let found = search("billing").call().await.unwrap();
        let (epics, issues): (Vec<_>, Vec<_>) =
            found.work_items.iter().partition(|w| w.r#type == "epic");
        assert_eq!(issues.len(), 3);
        assert_eq!(found.merge_requests[0].iid, 12);
        assert_eq!(found.projects[0].full_path, "acme/backend/billing");
        assert_eq!(epics[0].title, "Self-service billing");
        assert_eq!((epics[0].id, epics[0].group_id), (8101, Some(10)));
        let featured = issues.iter().find(|i| i.iid == 8).unwrap();
        let parent = featured.parent.as_ref().unwrap();
        assert_eq!((parent.group_id, parent.iid), (Some(10), 1));
        assert_eq!(
            parent.web_url.as_deref(),
            Some("https://dry-run.invalid/groups/acme/-/epics/1")
        );
        let tasks = SearchOptions {
            types: Some(vec!["task".into()]),
            ..Default::default()
        };
        let mut tasks = client.search("webhook".into(), Some(tasks));
        let tasks = tasks.call().await.unwrap().work_items;
        assert_eq!(keys(&tasks), [(101, 16)]);
        let no_epics = SearchOptions {
            exclude_types: Some(vec!["epic".to_string()]),
            ..Default::default()
        };
        let mut issues = client.search("billing".into(), Some(no_epics));
        let issues = issues.call().await.unwrap().work_items;
        assert_eq!(issues.len(), 3);
        assert!(issues.iter().all(|w| w.r#type == "issue"));
        let archived = search("legacy").call().await.unwrap();
        assert!(archived.projects[0].archived);
        let api = search("API").call().await.unwrap().projects;
        let api = api.iter().find(|p| p.id == AVATAR_PROJECT).unwrap();
        let avatar = api.avatar.as_deref().unwrap();
        assert!(avatar.starts_with(scratch.path().to_str().unwrap()));
        let others = search("acme").call().await.unwrap().projects;
        assert!(
            others
                .iter()
                .all(|p| (p.id == AVATAR_PROJECT) == p.avatar.is_some())
        );

        let mut mine = client.list_work_items(Some(authored()));
        let mine = mine.call().await.unwrap().work_items;
        assert_eq!(mine.len(), 7);
        assert!(keys(&mine).contains(&(103, 22)));

        let history = client.get_history(Some(7)).call().await.unwrap().events;
        assert_eq!(history.len(), 8, "the ninth is older than a week");
        assert!(history.windows(2).all(|w| w[0].timestamp >= w[1].timestamp));

        let activity = client.get_activity(Some(7)).call().await.unwrap().events;
        let pushed = activity.iter().find(|e| e.action == "pushed to").unwrap();
        assert_eq!(pushed.project_path.as_deref(), Some("acme/backend/api"));
        assert_eq!(pushed.r#ref.as_deref(), Some("rate-limit-token"));

        let jobs = run.admin.get_sync_jobs().call().await.unwrap().jobs;
        assert!(jobs.iter().any(|j| j.key == "project/101/issues"));
        assert!(jobs.iter().any(|j| j.key == "group/10/epics"));
        assert!(jobs.iter().all(|j| j.failures == 0), "{jobs:?}");

        run.stop().await;
    }

    #[tokio::test]
    async fn writes_round_trip_in_a_dry_run() {
        let scratch = Scratch::create().unwrap();
        let run = DryRun::start(&scratch).await;
        let client = &run.client;
        until("the assigned issues", async || {
            (run.assigned_work_items().await.len() == 6).then_some(())
        })
        .await;

        // A closed issue leaves the list at once and reads closed once the
        // sync picked it up.
        client
            .close(101, 12, IssuableKind::work_item)
            .call()
            .await
            .unwrap();
        assert!(!keys(&run.assigned_work_items().await).contains(&(101, 12)));
        until("the closed issue", async || {
            let found = client.search("#12".into(), None).call().await.ok()?;
            let issue = found
                .work_items
                .into_iter()
                .find(|i| i.project_id == Some(101))?;
            (issue.state == "closed").then_some(())
        })
        .await;

        // Logged time shows in the history.
        let summary = Some("Dry run".to_string());
        let mut log = client.post_time(103, 21, IssuableKind::work_item, "45m".into(), summary);
        log.call().await.unwrap();
        until("the logged time", async || {
            let history = client.get_history(Some(1)).call().await.ok()?.events;
            history
                .iter()
                .any(|e| e.summary.as_deref() == Some("Dry run") && e.time_spent == Some(45 * 60))
                .then_some(())
        })
        .await;

        // A created issue is mine, assigned and listed.
        let item = NewWorkItem {
            labels: Some(vec!["demo".into()]),
            assign_self: Some(true),
            ..new_work_item("Try the dry run")
        };
        let created = client.create_work_item(102, item).call().await.unwrap();
        assert_eq!(created.iid, Some(11));
        assert_eq!(
            created.web_url.as_deref(),
            Some("https://dry-run.invalid/acme/backend/billing/-/issues/11")
        );
        until("the created issue", async || {
            let listed = keys(&run.assigned_work_items().await).contains(&(102, 11));
            let mine = client.list_work_items(Some(authored())).call().await;
            (listed && keys(&mine.ok()?.work_items).contains(&(102, 11))).then_some(())
        })
        .await;

        // One under an epic lands with it as its parent.
        let audit = WorkItemRef {
            project_id: None,
            group_id: Some(12),
            iid: 1,
            r#type: Some("epic".into()),
            title: None,
            web_url: None,
        };
        let item = NewWorkItem {
            parent: Some(audit),
            ..new_work_item("Audit the forms")
        };
        let mut create = client.create_work_item(103, item);
        let created = create.call().await.unwrap();
        until("the created task's parent", async || {
            let mine = client.list_work_items(Some(authored())).call().await.ok()?;
            let item = mine
                .work_items
                .into_iter()
                .find(|i| Some(i.iid) == created.iid && i.project_id == Some(103))?;
            let parent = item.parent?;
            assert_eq!((parent.group_id, parent.iid), (Some(12), 1));
            parent.web_url
        })
        .await;

        // Assigning and unassigning.
        client
            .assign_self(103, 22, IssuableKind::work_item)
            .call()
            .await
            .unwrap();
        until("the assigned issue", async || {
            keys(&run.assigned_work_items().await)
                .contains(&(103, 22))
                .then_some(())
        })
        .await;
        let mut unassign = client.unassign_self(101, 31, IssuableKind::merge_request);
        unassign.call().await.unwrap();
        let mrs = client
            .get_assigned_merge_requests(None)
            .call()
            .await
            .unwrap();
        assert!(mrs.merge_requests.iter().all(|m| m.iid != 31));

        // GitLab's refusals come back as GitLab's.
        let archived = client
            .close(ARCHIVED_PROJECT, 2, IssuableKind::work_item)
            .call()
            .await;
        let (refused, status) = gitlab_error(archived.unwrap_err());
        assert!(refused.contains("archived"), "{refused}");
        assert_eq!(status, Some(403));

        run.stop().await;
    }

    #[tokio::test]
    async fn login_and_logout_are_turned_down_and_the_demo_stays() {
        let scratch = Scratch::create().unwrap();
        let run = DryRun::start(&scratch).await;
        let client = &run.client;

        let login = run
            .admin
            .login("gitlab.com".into(), "glpat-not-a-token".into())
            .call()
            .await;
        let refused = internal(login.unwrap_err());
        assert!(
            refused.contains("logging in is disabled") && refused.contains("dry run"),
            "{refused}"
        );
        let logout = run.admin.logout().call().await;
        assert!(internal(logout.unwrap_err()).contains("logging out is disabled"));

        let me = client.who_am_i().call().await.unwrap();
        assert_eq!(me.host, HOST, "still the demo account");
        // `stop` checks the keychain was never asked.
        run.stop().await;
    }

    #[tokio::test]
    async fn clear_cache_refills_from_the_fixture() {
        let scratch = Scratch::create().unwrap();
        let run = DryRun::start(&scratch).await;
        until("the assigned issues", async || {
            (run.assigned_work_items().await.len() == 6).then_some(())
        })
        .await;

        run.admin.clear_cache(None).call().await.unwrap();
        // The foreground views refill before the reply.
        assert_eq!(run.assigned_work_items().await.len(), 6);
        until("the search corpus", async || {
            let found = run.client.search("billing".into(), None).call().await;
            (!found.ok()?.work_items.is_empty()).then_some(())
        })
        .await;
        run.stop().await;
    }

    /// The order `main` keeps: serve, drop the runtime, drop the directory.
    #[test]
    fn a_dry_run_leaves_nothing_behind() {
        let scratch = Scratch::create().unwrap();
        let dir = scratch.path().to_path_buf();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let run = DryRun::start(&scratch).await;
            until("the assigned issues", async || {
                (run.assigned_work_items().await.len() == 6).then_some(())
            })
            .await;
            run.stop().await;
        });
        drop(runtime);
        assert!(dir.join("db").is_dir(), "the database was in there");
        drop(scratch);
        assert!(!dir.exists(), "{} is left", dir.display());
    }
}
