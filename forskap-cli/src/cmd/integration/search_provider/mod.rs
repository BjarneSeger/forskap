//! `forskap integration search-provider` — serve `forskap search` to GNOME Shell and
//! KRunner.
//!
//! One process owns the bus name [`BUS_NAME`] and exposes two objects: the
//! GNOME `org.gnome.Shell.SearchProvider2` interface at [`GNOME_PATH`] and
//! the KRunner `org.kde.krunner1` interface at [`KRUNNER_PATH`]. The session
//! bus starts it on demand through the `dbus-1/services` file that
//! `install` writes, and it exits after [`IDLE`] without a
//! call, so nothing runs while no launcher is open.
//!
//! Everything comes from the daemon (`Search`, `RecordOpen`, `WhoAmI`), the
//! same calls the noctalia plugin makes through `forskap search` / `forskap issue open`; the
//! query grammar ([`query`]) and result ids ([`results`]) are shared with it.

mod gnome;
mod install;
mod krunner;
mod query;
mod results;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use forskap_api::{ErrorKind, IssuableKind, VarlinkClientInterface};
use tokio::signal::unix::{SignalKind, signal};
use zbus::zvariant::{OwnedValue, Value};

use crate::cli::SearchProviderCommand;
use crate::cmd::search::wire_filter;
use crate::cmd::{epic, item};
use crate::friendly::friendly;
use crate::{client, config, refspec};
use results::{Row, Target};

pub const BUS_NAME: &str = "org.thehoster.forskap.SearchProvider";
pub const GNOME_PATH: &str = "/org/thehoster/forskap/SearchProvider";
pub const KRUNNER_PATH: &str = "/org/thehoster/forskap/Runner";

/// Exit after this long without a D-Bus call; the bus re-activates us for
/// the next search, so idling costs nothing.
const IDLE: Duration = Duration::from_secs(120);

/// Per-kind cap handed to `Search`; the shells show only a handful per
/// provider anyway.
const PER_KIND_LIMIT: i64 = 10;

pub async fn run(command: SearchProviderCommand) -> Result<()> {
    match command {
        SearchProviderCommand::Serve => serve().await,
        SearchProviderCommand::Install { prefix } => install::run(prefix),
        SearchProviderCommand::Launch => Provider::new()?.launch_search("").await,
    }
}

async fn serve() -> Result<()> {
    let provider = Provider::new()?;
    let conn = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(GNOME_PATH, gnome::SearchProvider2(provider.clone()))?
        .serve_at(KRUNNER_PATH, krunner::Runner(provider.clone()))?
        .build()
        .await
        .context("registering on the session bus")?;

    let mut sigterm = signal(SignalKind::terminate())?;
    tokio::select! {
        _ = idle(&provider) => {}
        _ = sigterm.recv() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    // Give the name back before shutting down so a call racing our exit
    // re-activates a fresh instance instead of getting no reply.
    conn.release_name(BUS_NAME).await?;
    conn.graceful_shutdown().await;
    Ok(())
}

async fn idle(provider: &Provider) {
    loop {
        let left = IDLE.saturating_sub(provider.idle_for());
        if left.is_zero() {
            return;
        }
        tokio::time::sleep(left).await;
    }
}

/// Shared between the two interfaces: daemon address, trigger word and the
/// rows of recent searches, which the shells refer back to by id.
#[derive(Clone)]
pub(super) struct Provider {
    socket: String,
    trigger_word: Option<String>,
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    rows: HashMap<String, Row>,
    last_activity: Instant,
    /// XDG activation token KRunner hands over before `Run`, consumed by the
    /// next browser launch so the compositor lets it take focus.
    activation_token: Option<String>,
}

/// A search's rows plus whether the query was an exact `#42`/`!42`/`&42` form.
pub(super) struct Hits {
    pub rows: Vec<Row>,
    pub exact: bool,
}

impl Provider {
    fn new() -> Result<Self> {
        let cfg = config::load()?;
        let socket = client::socket(&cfg)?;
        let trigger_word = cfg
            .search_provider
            .trigger_word
            .map(|w| w.trim().to_string())
            .filter(|w| !w.is_empty());
        Ok(Self {
            socket,
            trigger_word,
            inner: Arc::new(Mutex::new(Inner {
                rows: HashMap::new(),
                last_activity: Instant::now(),
                activation_token: None,
            })),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn touch(&self) {
        self.lock().last_activity = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        self.lock().last_activity.elapsed()
    }

    fn trigger_word(&self) -> Option<&str> {
        self.trigger_word.as_deref()
    }

    /// Run the daemon search for what the user typed. Input that is not for
    /// us (see [`query::interpret`]) and a dormant daemon yield no rows rather
    /// than an error, since the shells log provider errors on every keystroke.
    async fn search(&self, text: &str) -> Result<Hits> {
        let Some(parsed) = query::interpret(text, self.trigger_word()) else {
            return Ok(Hits {
                rows: vec![],
                exact: false,
            });
        };
        let client = client::connect(&self.socket).await?;
        let (kinds, types) = wire_filter(parsed.kind.as_slice());
        let reply = match client
            .search(parsed.query, kinds, Some(PER_KIND_LIMIT), None, types)
            .call()
            .await
        {
            Ok(reply) => reply,
            Err(e) if matches!(e.kind(), ErrorKind::NotAuthenticated(_)) => {
                return Ok(Hits {
                    rows: vec![],
                    exact: parsed.exact,
                });
            }
            Err(e) => return Err(friendly("Search", e)),
        };
        let rows = results::rows(&reply);
        // Keep earlier rows: GNOME asks for metas of ids from a previous
        // result set while KRunner may already have searched again.
        self.lock()
            .rows
            .extend(rows.iter().map(|r| (r.id.clone(), r.clone())));
        Ok(Hits {
            rows,
            exact: parsed.exact,
        })
    }

    fn cached(&self, id: &str) -> Option<Row> {
        self.lock().rows.get(id).cloned()
    }

    /// Open a picked result: `url:` ids go straight to the browser, issuables
    /// and epics are counted in the daemon first (same order as `forskap issue
    /// open`).
    async fn activate(&self, id: &str) -> Result<()> {
        match results::parse_id(id).ok_or_else(|| anyhow!("unknown result id {id:?}"))? {
            Target::Url(url) => self.open_url(&url),
            Target::Issuable {
                kind,
                project_id,
                iid,
            } => {
                let client = client::connect(&self.socket).await?;
                let url = match self.cached(id) {
                    Some(row) => row.url,
                    None => item::lookup(&client, kind, project_id, iid)
                        .await?
                        .web_url()
                        .to_string(),
                };
                client
                    .record_open(refspec::wire(kind), iid, Some(project_id), None)
                    .call()
                    .await
                    .map_err(|e| friendly("RecordOpen", e))?;
                self.open_url(&url)
            }
            Target::Epic { group_id, iid } => {
                let client = client::connect(&self.socket).await?;
                let url = match self.cached(id) {
                    Some(row) => row.url,
                    None => epic::lookup(&client, group_id, iid).await?.web_url,
                };
                client
                    .record_open(IssuableKind::work_item, iid, None, Some(group_id))
                    .call()
                    .await
                    .map_err(|e| friendly("RecordOpen", e))?;
                self.open_url(&url)
            }
        }
    }

    /// Open GitLab's own search page for `terms` (the instance start page
    /// when empty) on the host the daemon is logged in to.
    async fn launch_search(&self, terms: &str) -> Result<()> {
        let client = client::connect(&self.socket).await?;
        let me = client
            .who_am_i()
            .call()
            .await
            .map_err(|e| friendly("WhoAmI", e))?;
        let terms = terms.trim();
        let url = if terms.is_empty() {
            format!("https://{}/", me.host)
        } else {
            format!(
                "https://{}/search?search={}",
                me.host,
                query::percent_encode(terms)
            )
        };
        self.open_url(&url)
    }

    fn set_activation_token(&self, token: String) {
        self.lock().activation_token = Some(token);
    }

    /// Launch the browser detached, carrying a pending activation token.
    /// `open::that_detached` can't set env vars, and `std::env::set_var` is
    /// unsafe in a multi-threaded process, so this walks the same launcher
    /// list itself.
    fn open_url(&self, url: &str) -> Result<()> {
        use std::os::unix::process::CommandExt as _;

        let token = self.lock().activation_token.take();
        let mut last_err = None;
        for mut cmd in open::commands(url) {
            if let Some(token) = &token {
                cmd.env("XDG_ACTIVATION_TOKEN", token);
            }
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0);
            match cmd.spawn() {
                Ok(mut child) => {
                    // Reap it so no zombie lingers until we exit.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    return Ok(());
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(anyhow!(
            "couldn't open {url}: {}",
            last_err.map_or_else(|| "no launcher available".to_string(), |e| e.to_string())
        ))
    }
}

/// Map a provider failure to the generic D-Bus error the shells expect.
fn failed(e: anyhow::Error) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(format!("{e:#}"))
}

/// Box a plain value for an `a{sv}` dictionary.
fn ov<'a>(v: impl Into<Value<'a>>) -> OwnedValue {
    OwnedValue::try_from(v.into()).expect("only file descriptors fail to own")
}
