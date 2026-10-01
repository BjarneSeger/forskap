//! Dynamic shell completion: issue, MR and epic numbers, project and group
//! paths.
//!
//! The scripts `build.rs` writes (and `COMPLETE=<shell> forskap` prints) make
//! bash, zsh, fish and nushell call `forskap` itself on every Tab, with `COMPLETE` set
//! and the command line as arguments. clap_complete's engine answers for the
//! argument tree; the completers here add what only the daemon's cache knows.
//!
//! A completion runs while the user is typing, so it must never stall the
//! shell or write anything but candidates: the lookups share one short
//! [`BUDGET`], and every failure — no daemon, a broken config, a socket unit
//! whose daemon doesn't come up — just means fewer candidates.

use std::ffi::OsStr;
use std::time::Duration;

use clap::{Arg, Command, CommandFactory};
use clap_complete::env::{Bash, Elvish, Fish, Powershell, Shells, Zsh};
use clap_complete::{ArgValueCompleter, CompleteEnv, CompletionCandidate};
use forskap_api::{Epic, SearchKind, VarlinkClientInterface};

use self::nushell::Nushell;
use crate::cli::Cli;
use crate::cmd::epic::group_of;
use crate::item::{Item, project_of};
use crate::refspec::{self, RefKind};
use crate::state::{LastEpic, LastIssue};
use crate::{client, state};

mod nushell;

/// For everything one completion asks the daemon. Cache reads answer in a few
/// milliseconds; this only bounds the cases where nothing answers.
const BUDGET: Duration = Duration::from_millis(300);

/// The subcommands of `forskap issue` / `forskap mr` that take a number.
const TARGET_VERBS: [&str; 5] = ["view", "open", "close", "assign", "unassign"];

/// The subcommands of `forskap epic`, which all take a number.
const EPIC_VERBS: [&str; 2] = ["view", "open"];

/// Answer the shell and exit if this run is a completion request (`COMPLETE`
/// is set); return otherwise. Must run before anything can print.
pub fn run() {
    CompleteEnv::with_factory(command)
        .shells(Shells(&[
            &Bash,
            &Elvish,
            &Fish,
            &Powershell,
            &Zsh,
            &Nushell,
        ]))
        .complete();
}

/// The argument tree with the completers attached; `cli.rs` only declares it.
pub fn command() -> Command {
    let mut cmd = Cli::command();
    for (group, kind) in [("issue", RefKind::Issue), ("mr", RefKind::Mr)] {
        cmd = cmd.mut_subcommand(group, |mut group| {
            for verb in TARGET_VERBS {
                group = group.mut_subcommand(verb, |verb| {
                    verb.mut_args(|arg| match arg.get_id().as_str() {
                        "iid" => completing(arg, move |current| numbers(kind, current)),
                        "project" => completing(arg, projects),
                        _ => arg,
                    })
                });
            }
            group
        });
    }
    // Only issues are created, and `create` takes no number: kept out of
    // the loop above.
    cmd = cmd.mut_subcommand("issue", |issue| {
        issue.mut_subcommand("create", |create| {
            // Not `mut_arg`, which would move the argument behind the title.
            create.mut_args(|arg| match arg.get_id().as_str() {
                "project" => completing(arg, projects),
                "group" => completing(arg, groups),
                _ => arg,
            })
        })
    });
    cmd = cmd.mut_subcommand("epic", |mut epic| {
        for verb in EPIC_VERBS {
            epic = epic.mut_subcommand(verb, |verb| {
                verb.mut_args(|arg| match arg.get_id().as_str() {
                    "iid" => completing(arg, epics),
                    "group" => completing(arg, groups),
                    _ => arg,
                })
            });
        }
        epic
    });
    cmd = cmd.mut_subcommand("search", |search| {
        search.mut_args(|arg| match arg.get_id().as_str() {
            "project" => completing(arg, projects),
            "groups" => completing(arg, groups),
            _ => arg,
        })
    });
    cmd.mut_subcommand("time", |time| {
        time.mut_subcommand("log", |log| {
            // Not `mut_arg`: it moves the argument behind the others, which
            // would make `<REF>` the second positional.
            log.mut_args(|arg| match arg.get_id().as_str() {
                "reference" => completing(arg, references),
                "project" => completing(arg, projects),
                _ => arg,
            })
        })
    })
}

fn completing(arg: Arg, completer: impl Fn(&str) -> Vec<Candidate> + Send + Sync + 'static) -> Arg {
    arg.add(ArgValueCompleter::new(move |current: &OsStr| {
        let found = current.to_str().map(&completer).unwrap_or_default();
        found
            .into_iter()
            .enumerate()
            .map(|(rank, candidate)| {
                CompletionCandidate::new(candidate.value)
                    .help((!candidate.help.is_empty()).then(|| candidate.help.into()))
                    // The engine sorts by this; ours is the order to show.
                    .display_order(Some(rank))
            })
            .collect()
    }))
}

/// One value on offer, with the description fish and zsh show next to it
/// (bash shows none).
#[derive(Debug, PartialEq, Eq)]
struct Candidate {
    value: String,
    help: String,
}

/// `forskap issue|mr <verb> <IID>`.
fn numbers(kind: RefKind, current: &str) -> Vec<Candidate> {
    candidates(&known(kind), "", current, "project")
}

/// `forskap epic <verb> <IID>`.
fn epics(current: &str) -> Vec<Candidate> {
    let mut rows = Vec::new();
    within_budget(fetch_epics(&mut rows));
    let last = state::load().ok().and_then(|st| st.last_epic);
    candidates(&ranked_epics(last, &rows), "", current, "group")
}

/// `forskap time log <REF>`.
fn references(current: &str) -> Vec<Candidate> {
    // A completer sees only the word at the cursor; whether `--mr` is on the
    // line shows in our own arguments, which are that line.
    let mr = std::env::args_os().any(|arg| arg == "--mr");
    let (kind, sigil, digits) = reference(current, mr);
    candidates(&known(kind), sigil, digits, "project")
}

/// `-p/--project <PROJECT>`.
fn projects(current: &str) -> Vec<Candidate> {
    let mut found = Vec::new();
    within_budget(fetch_projects(current, &mut found));
    path_candidates(found, current)
}

/// `-g/--group <GROUP>`.
fn groups(current: &str) -> Vec<Candidate> {
    let mut found = Vec::new();
    within_budget(fetch_groups(current, &mut found));
    path_candidates(found, current)
}

/// Split a partial `time log` ref into its kind, the sigil to offer the
/// candidates with, and the digits typed so far. Mirrors [`refspec::parse`]:
/// a bare number is an issue unless `--mr` is given.
fn reference(current: &str, mr: bool) -> (RefKind, &'static str, &str) {
    if let Some(digits) = current.strip_prefix('#') {
        (RefKind::Issue, "#", digits)
    } else if let Some(digits) = current.strip_prefix('!') {
        (RefKind::Mr, "!", digits)
    } else if mr {
        (RefKind::Mr, "", current)
    } else {
        (RefKind::Issue, "", current)
    }
}

/// A number worth offering, and where it is from.
struct Known {
    iid: i64,
    /// Of an epic, its group.
    project_id: i64,
    /// Project path or title; whatever is known.
    help: String,
}

impl From<&Item> for Known {
    fn from(item: &Item) -> Self {
        let help = match item.project_path() {
            Some(path) => format!("{} ({path})", item.title()),
            None => item.title().to_string(),
        };
        Known {
            iid: item.iid(),
            project_id: item.project_id(),
            help,
        }
    }
}

/// Everything known of `kind`, most likely first.
fn known(kind: RefKind) -> Vec<Known> {
    let mut rows = Vec::new();
    within_budget(fetch_items(kind, &mut rows));
    let last = state::load().ok().and_then(|st| st.last_issue);
    ranked(kind, last.as_ref(), &rows)
}

/// Order `rows` (the assigned ones, then the opened ones by use) behind the
/// item time was last logged on, each item once.
fn ranked(kind: RefKind, last: Option<&LastIssue>, rows: &[Item]) -> Vec<Known> {
    let mut known: Vec<Known> = Vec::with_capacity(rows.len() + 1);
    for row in rows {
        if !known
            .iter()
            .any(|k| (k.project_id, k.iid) == (row.project_id(), row.iid()))
        {
            known.push(row.into());
        }
    }
    if let Some(last) = last.filter(|last| last.kind == kind) {
        put_first(&mut known, last.project_id, last.issue_iid, "project");
    }
    known
}

/// The epics opened before, most used first, behind the one opened last.
fn ranked_epics(last: Option<LastEpic>, rows: &[Epic]) -> Vec<Known> {
    let mut known: Vec<Known> = rows
        .iter()
        .map(|e| Known {
            iid: e.iid,
            project_id: e.group_id,
            help: match group_of(e) {
                Some(path) => format!("{} ({path})", e.title),
                None => e.title.clone(),
            },
        })
        .collect();
    if let Some(last) = last {
        put_first(&mut known, last.group_id, last.iid, "group");
    }
    known
}

/// Move the number used last to the front. `scope` names what `project_id`
/// is of when the number isn't cached (any more): still the likeliest one.
fn put_first(known: &mut Vec<Known>, project_id: i64, iid: i64, scope: &str) {
    let is_last = |k: &Known| (k.project_id, k.iid) == (project_id, iid);
    let first = match known.iter().position(is_last) {
        Some(at) => known.remove(at),
        None => Known {
            iid,
            project_id,
            help: format!("{scope} {project_id}"),
        },
    };
    known.insert(0, first);
}

/// The numbers starting with `digits`, written behind `sigil`. A number used
/// in several projects (or whatever `scope` names) is offered once, described
/// by its best-ranked item.
fn candidates(known: &[Known], sigil: &str, digits: &str, scope: &str) -> Vec<Candidate> {
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Vec::new();
    }
    let mut found: Vec<(i64, Candidate, usize)> = Vec::new();
    for k in known {
        let number = k.iid.to_string();
        if !number.starts_with(digits) {
            continue;
        }
        match found.iter_mut().find(|(iid, ..)| *iid == k.iid) {
            Some((.., others)) => *others += 1,
            None => found.push((
                k.iid,
                Candidate {
                    value: format!("{sigil}{number}"),
                    help: k.help.clone(),
                },
                0,
            )),
        }
    }
    found
        .into_iter()
        .map(|(_, mut candidate, others)| {
            match others {
                0 => {}
                1 => candidate
                    .help
                    .push_str(&format!(" — and in 1 more {scope}")),
                n => candidate
                    .help
                    .push_str(&format!(" — and in {n} more {scope}s")),
            }
            candidate
        })
        .collect()
}

/// A project or group path and, if the daemon told, its name.
type PathRow = (String, Option<String>);

/// The paths starting with `current`, each once. GitLab paths are
/// case-insensitive.
fn path_candidates(rows: Vec<PathRow>, current: &str) -> Vec<Candidate> {
    let prefix = current.trim_start_matches('/').to_ascii_lowercase();
    let mut found: Vec<PathRow> = Vec::new();
    for (path, name) in rows {
        if !path.to_ascii_lowercase().starts_with(&prefix) {
            continue;
        }
        match found
            .iter_mut()
            .find(|(p, _)| p.eq_ignore_ascii_case(&path))
        {
            Some((_, known)) => *known = known.take().or(name),
            None => found.push((path, name)),
        }
    }
    found
        .into_iter()
        .map(|(path, name)| Candidate {
            value: path,
            help: name.unwrap_or_default(),
        })
        .collect()
}

/// Drive a lookup on a runtime of its own (completers are synchronous and run
/// before `main` builds one), dropping it when the [`BUDGET`] is spent. The
/// lookups write into a buffer of the caller's, so what arrived in time
/// counts.
fn within_budget(lookup: impl Future<Output = Option<()>>) {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        let _ = tokio::time::timeout(BUDGET, lookup).await;
    });
}

/// The assigned items of `kind`, then the ones opened before, most used
/// first (an empty `Search` lists just those).
async fn fetch_items(kind: RefKind, rows: &mut Vec<Item>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    match kind {
        RefKind::Issue => {
            let reply = client.get_assigned_issues(None).call().await.ok()?;
            rows.extend(reply.issues.into_iter().map(Item::Issue));
        }
        RefKind::Mr => {
            let reply = client.get_assigned_merge_requests(None).call().await.ok()?;
            rows.extend(reply.merge_requests.into_iter().map(Item::Mr));
        }
    }
    let kinds = vec![refspec::search_kind(kind)];
    let reply = client
        .search(String::new(), Some(kinds), None, None)
        .call()
        .await
        .ok()?;
    match kind {
        RefKind::Issue => rows.extend(reply.issues.into_iter().map(Item::Issue)),
        RefKind::Mr => rows.extend(reply.merge_requests.into_iter().map(Item::Mr)),
    }
    Some(())
}

/// The projects of the assigned items, then the cached projects matching
/// what is typed.
async fn fetch_projects(current: &str, rows: &mut Vec<PathRow>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    let issues = client.get_assigned_issues(None).call().await.ok()?.issues;
    let paths = issues.iter().map(|i| (&i.project_path, &i.web_url));
    let mrs = client.get_assigned_merge_requests(None).call().await.ok()?;
    let paths = paths.chain(
        mrs.merge_requests
            .iter()
            .map(|m| (&m.project_path, &m.web_url)),
    );
    rows.extend(
        paths
            .filter_map(|(path, web_url)| project_of(path, web_url))
            .map(|p| (p.to_string(), None)),
    );

    let query = current.trim_matches('/').to_string();
    let kinds = vec![SearchKind::projects];
    let reply = client
        .search(query, Some(kinds), None, None)
        .call()
        .await
        .ok()?;
    rows.extend(reply.projects.into_iter().map(|p| (p.path, Some(p.name))));
    Some(())
}

/// The epics opened before, most used first (an empty `Search`).
async fn fetch_epics(rows: &mut Vec<Epic>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    let kinds = vec![SearchKind::epics];
    let reply = client
        .search(String::new(), Some(kinds), None, None)
        .call()
        .await
        .ok()?;
    rows.extend(reply.epics);
    Some(())
}

/// The cached groups matching what is typed.
async fn fetch_groups(current: &str, rows: &mut Vec<PathRow>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    let query = current.trim_matches('/').to_string();
    let kinds = vec![SearchKind::groups];
    let reply = client
        .search(query, Some(kinds), None, None)
        .call()
        .await
        .ok()?;
    rows.extend(reply.groups.into_iter().map(|g| (g.path, Some(g.name))));
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::testing::item;

    fn values(candidates: &[Candidate]) -> Vec<&str> {
        candidates.iter().map(|c| c.value.as_str()).collect()
    }

    fn last(kind: RefKind, project_id: i64, issue_iid: i64) -> LastIssue {
        LastIssue {
            project_id,
            issue_iid,
            kind,
        }
    }

    /// Two assigned issues, then two opened ones — one of them assigned too.
    fn rows() -> Vec<Item> {
        vec![
            item(RefKind::Issue, 1, "team/api", 42, "Fix login"),
            item(RefKind::Issue, 2, "team/web", 7, "Dark mode"),
            item(RefKind::Issue, 1, "team/api", 42, "Fix login"),
            item(RefKind::Issue, 3, "", 421, "No URL"),
        ]
    }

    #[test]
    fn ranked_lists_each_item_once_in_order() {
        let known = ranked(RefKind::Issue, None, &rows());
        let iids: Vec<i64> = known.iter().map(|k| k.iid).collect();
        assert_eq!(iids, [42, 7, 421]);
        assert_eq!(known[0].help, "Fix login (team/api)");
        assert_eq!(known[2].help, "No URL");
    }

    #[test]
    fn ranked_puts_the_last_logged_item_first() {
        let first = |last: &LastIssue| {
            let known = ranked(RefKind::Issue, Some(last), &rows());
            (known[0].iid, known[0].help.clone(), known.len())
        };
        assert_eq!(
            first(&last(RefKind::Issue, 2, 7)),
            (7, "Dark mode (team/web)".to_string(), 3)
        );
        // Not in the cache: offered all the same.
        assert_eq!(
            first(&last(RefKind::Issue, 9, 5)),
            (5, "project 9".to_string(), 4)
        );
        // An MR's number says nothing about issues.
        assert_eq!(first(&last(RefKind::Mr, 2, 7)).0, 42);
    }

    #[test]
    fn candidates_filter_on_the_typed_digits() {
        let known = ranked(RefKind::Issue, None, &rows());
        assert_eq!(
            values(&candidates(&known, "", "", "project")),
            ["42", "7", "421"]
        );
        assert_eq!(
            values(&candidates(&known, "", "4", "project")),
            ["42", "421"]
        );
        assert_eq!(
            values(&candidates(&known, "#", "42", "project")),
            ["#42", "#421"]
        );
        assert!(candidates(&known, "", "9", "project").is_empty());
        assert!(candidates(&known, "", "4x", "project").is_empty());
        assert!(candidates(&known, "", "-", "project").is_empty());
    }

    #[test]
    fn a_number_in_several_projects_is_offered_once() {
        let rows = [
            item(RefKind::Mr, 1, "team/api", 3, "Bump deps"),
            item(RefKind::Mr, 2, "team/web", 3, "Fix CI"),
            item(RefKind::Mr, 3, "team/docs", 3, "Typo"),
            item(RefKind::Mr, 3, "team/docs", 30, "Index"),
        ];
        let known = ranked(RefKind::Mr, None, &rows);
        assert_eq!(
            candidates(&known, "!", "3", "project"),
            [
                Candidate {
                    value: "!3".to_string(),
                    help: "Bump deps (team/api) — and in 2 more projects".to_string(),
                },
                Candidate {
                    value: "!30".to_string(),
                    help: "Index (team/docs)".to_string(),
                },
            ]
        );
    }

    #[test]
    fn epics_rank_behind_the_one_opened_last() {
        let epic = |group_id, path: &str, iid, title: &str| Epic {
            id: group_id * 100 + iid,
            iid,
            group_id,
            title: title.to_string(),
            web_url: format!("https://gitlab.example.com/groups/{path}/-/epics/{iid}"),
            state: "opened".to_string(),
            open_count: 1,
            group_path: String::new(),
            updated_at: 0,
        };
        let rows = [
            epic(3, "team", 5, "Accounts"),
            epic(4, "team/backend", 5, "Billing"),
            epic(4, "team/backend", 12, "Search"),
        ];
        let last = |group_id, iid| Some(LastEpic { group_id, iid });

        let known = ranked_epics(last(4, 12), &rows);
        assert_eq!(
            candidates(&known, "", "", "group"),
            [
                Candidate {
                    value: "12".to_string(),
                    help: "Search (team/backend)".to_string(),
                },
                Candidate {
                    value: "5".to_string(),
                    help: "Accounts (team) — and in 1 more group".to_string(),
                },
            ]
        );
        // Not in the cache: offered all the same.
        let known = ranked_epics(last(9, 7), &rows);
        assert_eq!(known[0].help, "group 9");
        assert_eq!(
            values(&candidates(&known, "", "", "group")),
            ["7", "5", "12"]
        );
    }

    #[test]
    fn reference_follows_the_sigil_then_the_flag() {
        assert_eq!(reference("", false), (RefKind::Issue, "", ""));
        assert_eq!(reference("4", false), (RefKind::Issue, "", "4"));
        assert_eq!(reference("4", true), (RefKind::Mr, "", "4"));
        assert_eq!(reference("#4", false), (RefKind::Issue, "#", "4"));
        assert_eq!(reference("!", false), (RefKind::Mr, "!", ""));
        assert_eq!(reference("!4", true), (RefKind::Mr, "!", "4"));
    }

    #[test]
    fn path_candidates_match_the_path_prefix() {
        let rows = || {
            vec![
                ("team/api".to_string(), None),
                ("Team/Web".to_string(), None),
                ("team/api".to_string(), Some("API".to_string())),
                ("other/team/api".to_string(), Some("Other".to_string())),
            ]
        };
        assert_eq!(
            path_candidates(rows(), "/team/"),
            [
                Candidate {
                    value: "team/api".to_string(),
                    help: "API".to_string(),
                },
                Candidate {
                    value: "Team/Web".to_string(),
                    help: String::new(),
                },
            ]
        );
        assert_eq!(path_candidates(rows(), "").len(), 3);
        assert!(path_candidates(rows(), "api").is_empty());
    }
}
