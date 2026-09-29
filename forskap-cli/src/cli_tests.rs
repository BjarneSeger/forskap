//! The argument tree itself: what parses, and what must not.

use clap::{CommandFactory, Parser};

use crate::cli::{
    Cli, Command, ItemCommand, OutputFormat, QueueCommand, RefreshScope, SyncCommand, TickMode,
    TimeCommand,
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
        &["search"],
        &["time", "history"],
        &["auth", "status"],
        &["queue", "list"],
    ] {
        let json = [args, &["-o", "json"]].concat();
        ok(&json);
    }
    for args in [
        &["issue", "close", "1"][..],
        &["issue", "open", "1"],
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
    for group in ["issue", "mr", "time", "auth", "sync", "queue", "config"] {
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
