//! The argument tree itself: what parses, and what must not.

use clap::{CommandFactory, Parser};
use clap_complete::ArgValueCompleter;

use crate::cli::{
    Cli, Command, EpicCommand, ItemCommand, OutputFormat, QueueCommand, RefreshScope, SyncCommand,
    TickMode, TimeCommand,
};

fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(std::iter::once("forskap").chain(args.iter().copied()))
}

fn ok(args: &[&str]) -> Command {
    match parse(args) {
        Ok(cli) => cli.command,
        Err(e) => panic!("{args:?} should parse: {e}"),
    }
}

#[test]
fn tree_is_well_formed() {
    Cli::command().debug_assert();
}

/// The completers are attached by name outside `cli.rs`: a verb added to
/// `issue`/`mr`/`epic`, or an argument renamed, must not silently lose its
/// completion.
#[test]
fn every_number_and_project_argument_completes() {
    fn check(cmd: &clap::Command, path: &str, seen: &mut usize) {
        for arg in cmd.get_arguments() {
            if ["iid", "reference", "project", "group"].contains(&arg.get_id().as_str()) {
                assert!(
                    arg.get::<ArgValueCompleter>().is_some(),
                    "`{path}` has no completer for `{}`",
                    arg.get_id()
                );
                *seen += 1;
            }
        }
        for sub in cmd.get_subcommands() {
            check(sub, &format!("{path} {}", sub.get_name()), seen);
        }
    }
    let mut cmd = crate::complete::command();
    cmd.clone().debug_assert();
    cmd.build();
    let mut seen = 0;
    check(&cmd, "forskap", &mut seen);
    // Number and project of five verbs in two groups and of `time log`,
    // number and group of the two epic verbs.
    assert_eq!(seen, 2 * 5 * 2 + 2 + 2 * 2);

    // Attaching them must not reorder the positionals.
    let log = cmd.find_subcommand("time").unwrap();
    let log = log.find_subcommand("log").unwrap();
    let positionals: Vec<&str> = log.get_positionals().map(|p| p.get_id().as_str()).collect();
    assert_eq!(positionals, ["reference", "duration"]);
    assert_eq!(log.get_positionals().next().unwrap().get_index(), Some(1));
}

#[test]
fn issue_and_mr_share_their_verbs() {
    for group in ["issue", "mr"] {
        for verb in ["view", "open", "close", "assign", "unassign"] {
            ok(&[group, verb, "42"]);
            ok(&[group, verb, "42", "-p", "team/api"]);
        }
        ok(&[group, "list", "--group", "a", "--group", "b", "-o", "json"]);
    }
    let Command::Mr {
        command: ItemCommand::Open { target, no_browser },
    } = ok(&["mr", "open", "7", "--project", "12", "--no-browser"])
    else {
        panic!("not `mr open`");
    };
    assert_eq!(target.iid, 7);
    assert_eq!(target.project.project.as_deref(), Some("12"));
    assert!(no_browser);
}

#[test]
fn epics_are_viewed_and_opened_by_group() {
    ok(&["epic", "view", "5", "-o", "json"]);
    ok(&["epic", "open", "5"]);
    let Command::Epic {
        command: EpicCommand::Open { target, no_browser },
    } = ok(&[
        "epic",
        "open",
        "5",
        "--group",
        "team/backend",
        "--no-browser",
    ])
    else {
        panic!("not `epic open`");
    };
    assert_eq!(target.iid, 5);
    assert_eq!(target.group.as_deref(), Some("team/backend"));
    assert!(no_browser);
    ok(&["epic", "view", "5", "-g", "12"]);

    // Epics live in groups, and are read-only here.
    assert!(parse(&["epic", "open", "5", "-p", "team/api"]).is_err());
    for bad in ["0", "&5", "abc"] {
        assert!(parse(&["epic", "view", bad]).is_err(), "{bad:?}");
    }
    for verb in ["list", "close", "assign", "unassign"] {
        assert!(parse(&["epic", verb, "5"]).is_err(), "{verb}");
    }
    ok(&["search", "--kind", "epics", "roadmap"]);
}

#[test]
fn iid_is_a_positive_number() {
    for bad in ["0", "-3", "#42", "!42", "abc"] {
        assert!(parse(&["issue", "close", bad]).is_err(), "{bad:?}");
    }
}

#[test]
fn output_only_where_data_is_printed() {
    for args in [
        &["issue", "list"][..],
        &["mr", "view", "1"],
        &["epic", "view", "1"],
        &["search"],
        &["activity"],
        &["time", "history"],
        &["auth", "status"],
        &["queue", "list"],
        &["sync", "jobs"],
    ] {
        for format in ["text", "json", "yaml"] {
            ok(&[args, &["-o", format]].concat());
        }
    }
    for args in [
        &["issue", "close", "1"][..],
        &["issue", "open", "1"],
        &["epic", "open", "1"],
        &["time", "log", "1", "30m"],
        &["sync", "refresh"],
        &["queue", "clear"],
        &["auth", "logout"],
    ] {
        let json = [args, &["-o", "json"]].concat();
        assert!(parse(&json).is_err(), "{json:?}");
    }
    // Not global: it belongs behind the command.
    assert!(parse(&["-o", "json", "issue", "list"]).is_err());
}

#[test]
fn search_joins_words_and_bounds_the_limit() {
    let Command::Search {
        query,
        kinds,
        limit,
        output,
    } = ok(&[
        "search", "--output", "json", "--limit", "15", "--kind", "mrs", "foo", "bar",
    ])
    else {
        panic!("not `search`");
    };
    assert_eq!(query, ["foo", "bar"]);
    assert_eq!(kinds.len(), 1);
    assert_eq!(limit, Some(15));
    assert!(matches!(output.output, OutputFormat::Json));

    assert!(parse(&["search", "--limit", "0"]).is_err());
    assert!(parse(&["search", "--limit", "-1"]).is_err());
}

#[test]
fn time_log_takes_a_ref() {
    let Command::Time {
        command:
            TimeCommand::Log {
                reference,
                duration,
                mr,
                summary,
                ..
            },
    } = ok(&["time", "log", "!42", "1h30m", "-s", "review"])
    else {
        panic!("not `time log`");
    };
    assert_eq!((reference.as_str(), duration.as_str()), ("!42", "1h30m"));
    assert!(!mr);
    assert_eq!(summary.as_deref(), Some("review"));
    ok(&["time", "log", "42", "1h", "--mr", "-p", "team/api"]);
}

#[test]
fn refresh_scopes_combine() {
    let Command::Sync {
        command: SyncCommand::Refresh { scopes },
    } = ok(&[
        "sync", "refresh", "--scope", "assigned", "--scope", "history",
    ])
    else {
        panic!("not `sync refresh`");
    };
    assert!(scopes == [RefreshScope::Assigned, RefreshScope::History]);
    assert!(parse(&["sync", "refresh", "--scope", "quick"]).is_err());
    assert!(matches!(
        ok(&["sync", "jobs"]),
        Command::Sync {
            command: SyncCommand::Jobs { .. }
        }
    ));
}

#[test]
fn queue_ids_are_not_negative() {
    assert!(matches!(
        ok(&["queue", "retry", "3"]),
        Command::Queue {
            command: QueueCommand::Retry { id: 3 }
        }
    ));
    assert!(parse(&["queue", "dismiss", "-1"]).is_err());
}

#[test]
fn groups_need_a_subcommand() {
    for group in [
        "issue", "mr", "epic", "time", "auth", "sync", "queue", "config",
    ] {
        assert!(parse(&[group]).is_err(), "{group}");
    }
    #[cfg(target_os = "linux")]
    {
        assert!(parse(&["integration", "search-provider"]).is_err());
        ok(&["integration", "search-provider", "serve"]);
        ok(&["integration", "search-provider", "launch"]);
    }
}

/// Shell hooks already installed in rc files call these.
#[test]
fn installed_hook_spellings_still_parse() {
    assert!(matches!(
        ok(&["tick"]),
        Command::Tick {
            mode: TickMode::Inline
        }
    ));
    assert!(matches!(
        ok(&["tick", "--mode", "remind"]),
        Command::Tick {
            mode: TickMode::Remind
        }
    ));
    assert!(matches!(ok(&["prompt"]), Command::Prompt));
}

#[test]
fn old_spellings_are_gone() {
    for args in [
        &["list"][..],
        &["open", "42"],
        &["close", "42"],
        &["log", "42", "1h"],
        &["history"],
        &["refresh"],
        &["login"],
        &["whoami"],
        &["hook", "bash"],
        &["search-provider"],
    ] {
        assert!(parse(args).is_err(), "{args:?}");
    }
}
