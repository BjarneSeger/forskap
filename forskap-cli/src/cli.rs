// Included verbatim by build.rs for the carapace spec: clap-only, no crate
// types. The dynamic completers are attached in complete.rs.
use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser)]
#[command(
    name = "forskap",
    about = "Cached GitLab CLI",
    version,
    max_term_width = 100
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Clone, Copy, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
}

/// Carried by the commands that print data.
#[derive(Args, Clone, Copy)]
pub struct OutputArgs {
    /// Output format.
    #[arg(
        long,
        short = 'o',
        value_enum,
        value_name = "FORMAT",
        default_value_t = OutputFormat::Text,
    )]
    pub output: OutputFormat,
}

#[derive(Args)]
pub struct ProjectArgs {
    /// Project, as numeric ID or full path (`group/project`).
    ///
    /// If omitted, it is resolved from the item you last logged time on, then
    /// your assigned items, then the search corpus. If the number exists in
    /// several projects, you are asked which one — outside a terminal that is
    /// an error.
    #[arg(short = 'p', long, value_name = "PROJECT")]
    pub project: Option<String>,
}

/// One issue or merge request; the command group says which.
#[derive(Args)]
pub struct TargetArgs {
    /// Number within the project: the `42` of `#42` / `!42`.
    #[arg(value_name = "IID", value_parser = clap::value_parser!(i64).range(1..))]
    pub iid: i64,
    #[command(flatten)]
    pub project: ProjectArgs,
}

/// One epic. Epics belong to a group, not a project.
#[derive(Args)]
pub struct EpicArgs {
    /// Number within the group: the `5` of `&5`.
    #[arg(value_name = "IID", value_parser = clap::value_parser!(i64).range(1..))]
    pub iid: i64,
    /// Group, as numeric ID or full path (`team/backend`). If omitted, it is
    /// the group of the epic you last opened under that number, else the one
    /// cached group with such an epic.
    #[arg(short = 'g', long, value_name = "GROUP")]
    pub group: Option<String>,
}

#[derive(Args, Clone, Copy)]
pub struct WindowArgs {
    /// How many days back to show, up to the daemon's retention.
    #[arg(long, default_value_t = 7)]
    pub days: u32,
}

#[derive(Subcommand)]
pub enum Command {
    /// Issues: list, view, open, close, assign.
    Issue {
        #[command(subcommand)]
        command: ItemCommand,
    },
    /// Merge requests: list, view, open, close, assign.
    Mr {
        #[command(subcommand)]
        command: ItemCommand,
    },
    /// Epics: view, open.
    ///
    /// Only on GitLab instances that have epics (Premium and up); synced for
    /// the groups above the projects in the search corpus.
    Epic {
        #[command(subcommand)]
        command: EpicCommand,
    },
    /// Search the cached issues, merge requests, epics, projects and groups.
    ///
    /// Matches titles, labels and project/group paths case-insensitively;
    /// `#123` finds issues/MRs by number, `&5` epics. Items you open often
    /// rank first; with no query it lists just those.
    Search {
        /// Search text. Omit it to list the frequently opened issues, MRs and
        /// epics.
        #[arg(value_name = "QUERY")]
        query: Vec<String>,
        /// Restrict to one or more result kinds. Repeat the flag to combine.
        #[arg(long = "kind", value_enum, value_name = "KIND")]
        kinds: Vec<SearchKind>,
        /// Maximum results per kind.
        #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
        limit: Option<i64>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Show what you did on GitLab recently: pushes, comments, and the
    /// issues and merge requests you opened, closed or merged.
    ///
    /// These are your contribution events as the daemon syncs them; how far
    /// back they reach is its `search.tracked_retention_hours`.
    Activity {
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Time tracking: log time, review it, and the shell reminder.
    Time {
        #[command(subcommand)]
        command: TimeCommand,
    },
    /// The daemon's GitLab login.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// The daemon's background sync with GitLab.
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
    /// Writes the daemon queued because GitLab was unreachable.
    ///
    /// One that GitLab then rejects, or that outlives the retry window, is
    /// kept here as failed.
    Queue {
        #[command(subcommand)]
        command: QueueCommand,
    },
    /// Inspect or scaffold the user configuration file.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Desktop integrations.
    #[cfg(target_os = "linux")]
    Integration {
        #[command(subcommand)]
        command: IntegrationCommand,
    },
    // Installed shell hooks call these two spellings.
    #[command(hide = true)]
    Tick {
        #[arg(long, value_enum, default_value_t = TickMode::Inline)]
        mode: TickMode,
    },
    #[command(hide = true)]
    Prompt,
}

/// The verbs shared by `forskap issue` and `forskap mr`.
#[derive(Subcommand)]
pub enum ItemCommand {
    /// List the open ones assigned to you.
    List {
        /// Restrict to the given GitLab group. Repeat the flag for several.
        #[arg(long = "group", value_name = "GROUP")]
        groups: Vec<String>,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Show what the cache knows about one.
    View {
        #[command(flatten)]
        target: TargetArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Open one in the browser.
    ///
    /// The open is counted, so it ranks higher in `forskap search` and the
    /// launchers built on it.
    Open {
        #[command(flatten)]
        target: TargetArgs,
        /// Only count the open and print the URL.
        #[arg(long)]
        no_browser: bool,
    },
    /// Close one.
    Close {
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Add yourself to the assignees, keeping the existing ones.
    Assign {
        #[command(flatten)]
        target: TargetArgs,
    },
    /// Remove yourself from the assignees.
    Unassign {
        #[command(flatten)]
        target: TargetArgs,
    },
}

#[derive(Subcommand)]
pub enum EpicCommand {
    /// Show what the cache knows about one.
    View {
        #[command(flatten)]
        target: EpicArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Open one in the browser.
    ///
    /// The open is counted, so it ranks higher in `forskap search` and the
    /// launchers built on it.
    Open {
        #[command(flatten)]
        target: EpicArgs,
        /// Only count the open and print the URL.
        #[arg(long)]
        no_browser: bool,
    },
}

#[derive(Subcommand)]
pub enum TimeCommand {
    /// Log time on an issue or merge request.
    Log {
        /// `42` or `#42` for an issue, `!42` for a merge request. The sigils
        /// need quoting in bash/zsh; `42 --mr` does not.
        #[arg(value_name = "REF")]
        reference: String,
        /// Duration in GitLab syntax (e.g. `30m`, `1h15m`).
        duration: String,
        /// Treat a bare number as a merge request.
        #[arg(long)]
        mr: bool,
        #[command(flatten)]
        project: ProjectArgs,
        /// Summary note.
        #[arg(short = 's', long)]
        summary: Option<String>,
    },
    /// Interactively pick an assigned issue or merge request and log time.
    Prompt,
    /// Show the time you logged recently.
    History {
        #[command(flatten)]
        window: WindowArgs,
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Print the snippet that makes your shell remind you to log time.
    Hook {
        /// Shell to print the snippet for.
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Subcommand)]
pub enum AuthCommand {
    /// Authenticate against GitLab.
    ///
    /// Asks for a personal access token and stores it in the OS keychain
    /// (Keychain on macOS, secret-service on Linux).
    Login {
        /// GitLab host (e.g. `gitlab.com` or `gitlab.mycorp.com`).
        #[arg(long, default_value = "gitlab.com")]
        host: String,
    },
    /// Clear the stored credentials and disconnect the daemon from GitLab.
    Logout,
    /// Print the host and user the daemon is logged in as.
    Status {
        #[command(flatten)]
        output: OutputArgs,
    },
}

#[derive(Subcommand)]
pub enum SyncCommand {
    /// Drop cached data and fetch it again.
    ///
    /// Without `--scope` that is everything synced; open counts only go with
    /// an explicit `--scope usage`. Waits for the assigned lists and the time
    /// history; the search corpus refills in the background.
    Refresh {
        /// What to drop. Repeat the flag to combine.
        #[arg(long = "scope", value_enum, value_name = "SCOPE")]
        scopes: Vec<RefreshScope>,
    },
    /// Show the sync jobs: what runs now, what comes next, what failed.
    ///
    /// The daemon runs one job at a time; they are listed in the order it
    /// runs them.
    Jobs {
        #[command(flatten)]
        output: OutputArgs,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RefreshScope {
    /// The assigned issue and merge request lists, and the board columns.
    Assigned,
    /// The search corpus: issues, merge requests, epics, projects, groups.
    Search,
    /// The logged time.
    History,
    /// The open counts behind the search ranking. User data, not a cache.
    Usage,
}

#[derive(Subcommand)]
pub enum QueueCommand {
    /// List the failed writes.
    List {
        #[command(flatten)]
        output: OutputArgs,
    },
    /// Queue a failed write for another attempt.
    Retry {
        /// Id as shown by `forskap queue list`.
        #[arg(value_parser = clap::value_parser!(i64).range(0..))]
        id: i64,
    },
    /// Drop a failed write.
    Dismiss {
        /// Id as shown by `forskap queue list`.
        #[arg(value_parser = clap::value_parser!(i64).range(0..))]
        id: i64,
    },
    /// Drop every failed write.
    Clear,
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Print an annotated TOML template with the current defaults.
    ///
    /// Pipe it into the file `forskap config path` names.
    Template,
    /// Print the path of the user config file.
    Path,
}

#[cfg(target_os = "linux")]
#[derive(Subcommand)]
pub enum IntegrationCommand {
    /// `forskap search` for GNOME Shell and KRunner.
    SearchProvider {
        #[command(subcommand)]
        command: SearchProviderCommand,
    },
}

#[cfg(target_os = "linux")]
#[derive(Subcommand)]
pub enum SearchProviderCommand {
    /// Serve searches over D-Bus until idle.
    ///
    /// The session bus starts this on demand once the files from `install`
    /// are in place.
    Serve,
    /// Write the GNOME Shell, KRunner and D-Bus activation files.
    ///
    /// They go under PREFIX with this binary's path filled in. GNOME Shell
    /// only reads providers from `$XDG_DATA_DIRS`, so the default prefix needs
    /// root; KRunner and D-Bus activation also work from `~/.local/share`.
    Install {
        /// Data directory to install into (default: /usr/local/share).
        #[arg(long, value_name = "DIR")]
        prefix: Option<std::path::PathBuf>,
    },
    /// Open your GitLab instance in the browser (the desktop entry's action).
    #[command(hide = true)]
    Launch,
}

/// `inline` is for shells whose prompt hooks run in a cooked terminal
/// (bash, zsh, fish). Nushell's reedline stays in raw mode across its hooks, so
/// a picker launched from one corrupts the line editor: `remind` only prints.
#[derive(Clone, Copy, Default, ValueEnum)]
pub enum TickMode {
    /// Run the interactive prompt once the interval has elapsed.
    #[default]
    Inline,
    /// Print a one-line reminder once the interval has elapsed.
    Remind,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum Shell {
    Fish,
    Zsh,
    Bash,
    Nu,
}

/// `mrs` maps to the wire value `merge_requests`.
#[derive(Clone, Copy, ValueEnum)]
pub enum SearchKind {
    Issues,
    Mrs,
    Projects,
    Groups,
    Epics,
}
