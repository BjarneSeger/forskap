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

use std::ffi::{OsStr, OsString};
use std::time::Duration;

use clap::{Arg, Command, CommandFactory};
use clap_complete::env::{Bash, Elvish, Fish, Powershell, Shells, Zsh};
use clap_complete::{ArgValueCompleter, CompleteEnv, CompletionCandidate};
use forskap_api::{
    Scope, Search_Reply, SearchKind, SearchOptions, VarlinkClient, VarlinkClientInterface, WorkItem,
};

use self::nushell::Nushell;
use crate::cli::Cli;
use crate::cmd::epic::group_of;
use crate::cmd::project;
use crate::item::{self, Item, project_of};
use crate::refspec::{self, RefKind};
use crate::state::{LastEpic, LastIssue};
use crate::{client, state};

mod nushell;

/// For everything one completion asks the daemon. Cache reads answer in a few
/// milliseconds; this only bounds the cases where nothing answers.
const BUDGET: Duration = Duration::from_millis(300);

/// How many numbers to offer once a project or group is named on the line:
/// every cached one of it then, not just the frequently opened ones.
const SCOPED_LIMIT: i64 = 200;

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
    let project = option_value(&line(), "project", 'p');
    candidates(&known(kind, project.as_deref()), "", current, "project")
}

/// `forskap epic <verb> <IID>`.
fn epics(current: &str) -> Vec<Candidate> {
    let group = option_value(&line(), "group", 'g');
    let mut rows = Vec::new();
    within_budget(fetch_epics(group.as_deref(), &mut rows));
    let last = state::load().ok().and_then(|st| st.last_epic);
    candidates(
        &ranked_epics(last, &rows, group.is_none()),
        "",
        current,
        "group",
    )
}

/// `forskap time log <REF>`.
fn references(current: &str) -> Vec<Candidate> {
    let line = line();
    let mr = line.iter().any(|arg| arg == "--mr");
    let project = option_value(&line, "project", 'p');
    let (kind, sigil, digits) = reference(current, mr);
    candidates(&known(kind, project.as_deref()), sigil, digits, "project")
}

/// The command line being completed. A completer sees only the word at the
/// cursor, but the shells pass the whole line as our arguments behind a
/// `--` (bash and zsh with the words after the cursor too; fish and nushell
/// cut at it).
fn line() -> Vec<OsString> {
    std::env::args_os()
        .skip_while(|arg| arg != "--")
        .skip(1)
        .collect()
}

/// The value given to `--long` or `-s` among `words`, in any spelling clap
/// takes: `--long v`, `--long=v`, `-s v`, `-sv`, `-s=v` — and `--long = v`,
/// which is how bash hands over `--long=v`. The quotes bash, zsh and nushell
/// leave on a word are stripped. A `--` ends the options; a value that is
/// missing or empty is none.
fn option_value(words: &[OsString], long: &str, short: char) -> Option<String> {
    let long_flag = format!("--{long}");
    let short_flag = format!("-{short}");
    let mut words = words.iter().filter_map(|word| word.to_str());
    while let Some(word) = words.next() {
        if word == "--" {
            return None;
        }
        // The value attached to the flag, if the word is the flag at all.
        let attached = if let Some(rest) = word.strip_prefix(&long_flag) {
            match rest.strip_prefix('=') {
                Some(value) => Some(value),
                None if rest.is_empty() => None,
                // `--longer`: another option.
                None => continue,
            }
        } else if let Some(rest) = word.strip_prefix(&short_flag) {
            match rest.strip_prefix('=') {
                Some(value) => Some(value),
                None if rest.is_empty() => None,
                None => Some(rest),
            }
        } else {
            continue;
        };
        let value = match attached {
            Some(value) => value,
            None => match words.next()? {
                "=" => words.next()?,
                next => next,
            },
        };
        return Some(unquoted(value)).filter(|value| !value.is_empty());
    }
    None
}

/// A word without the matching quotes around it.
fn unquoted(word: &str) -> String {
    let inner = word
        .strip_prefix('\'')
        .and_then(|w| w.strip_suffix('\''))
        .or_else(|| word.strip_prefix('"').and_then(|w| w.strip_suffix('"')));
    inner.unwrap_or(word).to_string()
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

/// Everything known of `kind`, most likely first — of one project where
/// `--project` names it on the line.
fn known(kind: RefKind, project: Option<&str>) -> Vec<Known> {
    let mut rows = Vec::new();
    within_budget(fetch_items(kind, project, &mut rows));
    let last = state::load().ok().and_then(|st| st.last_issue);
    ranked(kind, last.as_ref(), &rows, project.is_none())
}

/// Order `rows` (the assigned ones, then the opened ones by use) behind the
/// item time was last logged on, each item once. `anywhere`: the rows are
/// not kept to one project, so that item is offered even if it isn't among
/// them.
fn ranked(kind: RefKind, last: Option<&LastIssue>, rows: &[Item], anywhere: bool) -> Vec<Known> {
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
        put_first(
            &mut known,
            last.project_id,
            last.issue_iid,
            anywhere.then_some("project"),
        );
    }
    known
}

/// The epics opened before, most used first, behind the one opened last.
/// `anywhere` as for [`ranked`], with groups for projects.
fn ranked_epics(last: Option<LastEpic>, rows: &[WorkItem], anywhere: bool) -> Vec<Known> {
    let mut known: Vec<Known> = rows
        .iter()
        .map(|e| Known {
            iid: e.iid,
            project_id: e.group_id.unwrap_or_default(),
            help: match group_of(e) {
                Some(path) => format!("{} ({path})", e.title),
                None => e.title.clone(),
            },
        })
        .collect();
    if let Some(last) = last {
        put_first(
            &mut known,
            last.group_id,
            last.iid,
            anywhere.then_some("group"),
        );
    }
    known
}

/// Move the number used last to the front. When it isn't cached (any more)
/// it is still the likeliest one, so it is offered all the same if `scope`
/// names what `project_id` is of; with `None` the rows are kept to a project
/// or group it may not be in, and a number outside them is no help.
fn put_first(known: &mut Vec<Known>, project_id: i64, iid: i64, scope: Option<&str>) {
    let is_last = |k: &Known| (k.project_id, k.iid) == (project_id, iid);
    let first = match (known.iter().position(is_last), scope) {
        (Some(at), _) => known.remove(at),
        (None, Some(scope)) => Known {
            iid,
            project_id,
            help: format!("{scope} {project_id}"),
        },
        (None, None) => return,
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
/// first (an empty `Search` lists just those) — or, in the one `project`,
/// the assigned ones and then every cached one, so naming it browses it.
async fn fetch_items(kind: RefKind, project: Option<&str>, rows: &mut Vec<Item>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    let scope = match project {
        // A path the cache doesn't know offers nothing: the command fails
        // with it as well, and numbers of other projects aren't what was
        // asked for.
        Some(project) => Some(Scope {
            projects: Some(vec![project::by_arg(&client, project).await.ok()?]),
            groups: None,
        }),
        None => None,
    };
    match kind {
        RefKind::Issue => {
            let reply = client
                .get_assigned_work_items(scope.clone())
                .call()
                .await
                .ok()?;
            rows.extend(reply.work_items.into_iter().map(Item::Issue));
        }
        RefKind::Mr => {
            let reply = client
                .get_assigned_merge_requests(scope.clone())
                .call()
                .await
                .ok()?;
            rows.extend(reply.merge_requests.into_iter().map(Item::Mr));
        }
    }
    let all = scope.is_some();
    let options = SearchOptions {
        kinds: Some(vec![refspec::search_kind(kind)]),
        exclude_types: refspec::excluded_types(kind),
        ..scoped(scope, all)
    };
    let reply = search_all_or_frequent(&client, options).await?;
    match kind {
        RefKind::Issue => rows.extend(item::issues(reply.work_items)),
        RefKind::Mr => rows.extend(reply.merge_requests.into_iter().map(Item::Mr)),
    }
    Some(())
}

/// The options of an empty `Search` in `scope`: every cached row (`all`,
/// once a project or group is named on the line) or the frequently opened
/// ones.
fn scoped(scope: Option<Scope>, all: bool) -> SearchOptions {
    SearchOptions {
        scope,
        match_all: all.then_some(true),
        limit: all.then_some(SCOPED_LIMIT),
        ..Default::default()
    }
}

/// An empty `Search`. A daemon older than `match_all` refuses the option;
/// its frequently opened rows are then what there is.
async fn search_all_or_frequent(
    client: &VarlinkClient,
    options: SearchOptions,
) -> Option<Search_Reply> {
    match client
        .search(String::new(), Some(options.clone()))
        .call()
        .await
    {
        Ok(reply) => Some(reply),
        Err(_) if options.match_all.is_some() => {
            let frequent = SearchOptions {
                match_all: None,
                limit: None,
                ..options
            };
            client
                .search(String::new(), Some(frequent))
                .call()
                .await
                .ok()
        }
        Err(_) => None,
    }
}

/// The projects of the assigned items, then the cached projects matching
/// what is typed.
async fn fetch_projects(current: &str, rows: &mut Vec<PathRow>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    let issues = client.get_assigned_work_items(None).call().await;
    let issues = issues.ok()?.work_items;
    let paths = issues
        .iter()
        .map(|i| (i.namespace_path.as_deref(), i.web_url.as_str()));
    let mrs = client.get_assigned_merge_requests(None).call().await.ok()?;
    let paths = paths.chain(
        mrs.merge_requests
            .iter()
            .map(|m| (m.project_path.as_deref(), m.web_url.as_str())),
    );
    rows.extend(
        paths
            .filter_map(|(path, web_url)| project_of(path, web_url))
            .map(|p| (p.to_string(), None)),
    );

    let query = current.trim_matches('/').to_string();
    let reply = client
        .search(query, Some(only(SearchKind::projects)))
        .call()
        .await
        .ok()?;
    rows.extend(
        reply
            .projects
            .into_iter()
            .map(|p| (p.full_path, Some(p.name))),
    );
    Some(())
}

/// The epics opened before, most used first (an empty `Search`) — or every
/// cached one of the `group`, named by its path or numeric ID.
async fn fetch_epics(group: Option<&str>, rows: &mut Vec<WorkItem>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    let id = group.and_then(|g| g.parse::<i64>().ok());
    let path = group.filter(|_| id.is_none()).map(|g| g.trim_matches('/'));
    // The daemon scopes by path, subgroups included; an ID is matched here,
    // and so is the path, exactly: `-g team` doesn't mean `team/backend`.
    let scope = path.map(|path| Scope {
        projects: None,
        groups: Some(vec![path.to_string()]),
    });
    let options = SearchOptions {
        kinds: Some(vec![SearchKind::work_items]),
        types: Some(vec!["epic".to_string()]),
        ..scoped(scope, group.is_some())
    };
    let reply = search_all_or_frequent(&client, options).await?;
    rows.extend(
        reply
            .work_items
            .into_iter()
            .filter(item::is_epic)
            .filter(|e| match (id, path) {
                (Some(id), _) => e.group_id == Some(id),
                (None, Some(path)) => group_of(e).is_some_and(|g| g.eq_ignore_ascii_case(path)),
                (None, None) => true,
            }),
    );
    Some(())
}

/// A `Search` for one kind of result.
fn only(kind: SearchKind) -> SearchOptions {
    SearchOptions {
        kinds: Some(vec![kind]),
        ..Default::default()
    }
}

/// The cached groups matching what is typed.
async fn fetch_groups(current: &str, rows: &mut Vec<PathRow>) -> Option<()> {
    let client = client::connect_default().await.ok()?;
    let query = current.trim_matches('/').to_string();
    let reply = client
        .search(query, Some(only(SearchKind::groups)))
        .call()
        .await
        .ok()?;
    rows.extend(
        reply
            .groups
            .into_iter()
            .map(|g| (g.full_path, Some(g.name))),
    );
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
        let known = ranked(RefKind::Issue, None, &rows(), true);
        let iids: Vec<i64> = known.iter().map(|k| k.iid).collect();
        assert_eq!(iids, [42, 7, 421]);
        assert_eq!(known[0].help, "Fix login (team/api)");
        assert_eq!(known[2].help, "No URL");
    }

    #[test]
    fn ranked_puts_the_last_logged_item_first() {
        let first = |last: &LastIssue| {
            let known = ranked(RefKind::Issue, Some(last), &rows(), true);
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
    fn a_project_on_the_line_keeps_the_last_logged_item_to_its_rows() {
        // The rows are those of team/web; the last logged item is among them.
        let rows = [item(RefKind::Issue, 2, "team/web", 7, "Dark mode")];
        let known = ranked(
            RefKind::Issue,
            Some(&last(RefKind::Issue, 2, 7)),
            &rows,
            false,
        );
        assert_eq!(values(&candidates(&known, "", "", "project")), ["7"]);
        // Logged on elsewhere, or not cached: nothing is made up.
        let known = ranked(
            RefKind::Issue,
            Some(&last(RefKind::Issue, 1, 42)),
            &rows,
            false,
        );
        assert_eq!(values(&candidates(&known, "", "", "project")), ["7"]);
        let known = ranked_epics(
            Some(LastEpic {
                group_id: 9,
                iid: 7,
            }),
            &[],
            false,
        );
        assert!(known.is_empty());
    }

    #[test]
    fn option_value_reads_every_spelling_clap_takes() {
        let words = |line: &str| -> Vec<OsString> { line.split(' ').map(OsString::from).collect() };
        let project = |line: &str| option_value(&words(line), "project", 'p');
        for line in [
            "forskap issue view --project team/api 4",
            "forskap issue view --project=team/api 4",
            "forskap issue view --project = team/api 4",
            "forskap issue view -p team/api 4",
            "forskap issue view -pteam/api 4",
            "forskap issue view -p=team/api 4",
            "forskap issue view 4 --project team/api",
            "forskap issue view --project 'team/api' 4",
            "forskap issue view --project \"team/api\" 4",
        ] {
            assert_eq!(project(line).as_deref(), Some("team/api"), "{line}");
        }
        for line in [
            "forskap issue view 4",
            "forskap issue view --project",
            "forskap issue view --project=",
            "forskap issue view --project ''",
            "forskap issue view --projects team/api 4",
            "forskap issue view -- --project team/api",
            "forskap issue view --mr 4",
        ] {
            assert_eq!(project(line), None, "{line}");
        }
        assert_eq!(
            option_value(&words("forskap epic view 5 -g team/backend"), "group", 'g').as_deref(),
            Some("team/backend")
        );
    }

    #[test]
    fn candidates_filter_on_the_typed_digits() {
        let known = ranked(RefKind::Issue, None, &rows(), true);
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
        let known = ranked(RefKind::Mr, None, &rows, true);
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
        let epic = |group_id, path: &str, iid, title: &str| WorkItem {
            title: title.to_string(),
            web_url: format!("https://gitlab.example.com/groups/{path}/-/epics/{iid}"),
            open_count: 1,
            ..crate::item::testing::epic(group_id, iid)
        };
        let rows = [
            epic(3, "team", 5, "Accounts"),
            epic(4, "team/backend", 5, "Billing"),
            epic(4, "team/backend", 12, "Search"),
        ];
        let last = |group_id, iid| Some(LastEpic { group_id, iid });

        let known = ranked_epics(last(4, 12), &rows, true);
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
        let known = ranked_epics(last(9, 7), &rows, true);
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
