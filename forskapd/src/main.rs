//! `forskapd` — GitLab time-tracking varlink daemon.

use std::sync::Arc;
use tokio::sync::{Notify, RwLock};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use forskapd::error::{DormancyReason, Result};
use forskapd::gitlab::GitlabClient;
use forskapd::handlers::{ConnState, Handlers, Session, SessionSlot};
use forskapd::queue::{RetryQueue, SettleHook};
use forskapd::service::ServiceHandler;
use forskapd::sync::store::SyncStore;
use forskapd::sync::{AvatarDir, Job, SyncHandle};
use forskapd::usage::UsageStats;
use forskapd::write::Write;
use forskapd::{config, db, migrate, reconnect, reload, rotate, secrets, server};

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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("FORSKAPD_LOG")
                .or_else(|_| EnvFilter::try_from_env("GITLAB_TRACKRD_LOG"))
                .unwrap_or_else(|_| EnvFilter::new("forskapd=info")),
        )
        .init();

    migrate::run();

    let config = match config::load_shared() {
        Ok(config) => config,
        Err(e) => {
            error!(error = %e, "failed to load configuration");
            std::process::exit(1);
        }
    };
    let socket = config.read().unwrap().server.resolved_socket();
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

    // Credentials come only from the OS keychain (set via `forskap auth login`). The
    // daemon never refuses to start: any failure here leaves it *dormant*
    // (serving, but returning `NotAuthenticated`). The dormancy reason is kept
    // in the session slot so the CLI can report a specific cause.
    let initial: ConnState = match secrets::load().await {
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
    };
    let session: SessionSlot = Arc::new(RwLock::new(initial));

    std::fs::create_dir_all(&db_dir)?;
    let db = fjall::Database::builder(&db_dir).open()?;
    match db::drop_keyspaces(&db, &RETIRED_KEYSPACES) {
        Ok(dropped) if !dropped.is_empty() => info!(?dropped, "dropped retired cache keyspaces"),
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
        reconnect::keychain_probe(),
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
    });

    reload::spawn(Arc::clone(&config), move || {
        sync.reconfigure();
        rotation.reevaluate();
    });

    // If the daemon booted dormant because GitLab was unreachable, retry the
    // connection in the background with exponential backoff. A successful
    // reconnect flips the shared session to `Connected`, which drains the retry
    // queue and wakes the sync worker. No-op when already connected or when
    // dormancy needs the user (bad token / logged out).
    reconnect::spawn(Arc::clone(&handlers));

    // Replaces the token by a fresh one shortly before it expires; idles on
    // a token that doesn't rotate.
    rotate::spawn(Arc::clone(&handlers));

    let listener = server::make_listener(&socket)?;

    if server::is_socket_activated() {
        info!("starting forskapd from socket");
    } else {
        info!(socket = socket, "starting forskapd");
    }

    let serve = server::serve(Arc::new(ServiceHandler::new(handlers)), listener);

    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate())?;

    tokio::select! {
        result = serve => result?,
        _ = tokio::signal::ctrl_c() => { info!("received SIGINT, shutting down"); }
        _ = sigterm.recv() => { info!("received SIGTERM, shutting down"); }
    }

    if !server::is_socket_activated() {
        let _ = std::fs::remove_file(&socket);
    }
    Ok(())
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let jobs = Job::affected_by_replay(write, queued_at, &config.read().unwrap(), now);
        sync.refresh_soon(&jobs);
    })
}
