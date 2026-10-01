//! `forskapd` — GitLab time-tracking varlink daemon.
//!
//! Parse the arguments, build the real or the dry-run environment, run the
//! same daemon on it (see [`forskapd::daemon`]).

use tracing::error;
use tracing_subscriber::EnvFilter;

use forskapd::args::Args;
use forskapd::daemon::{self, Daemon, Environment, Scratch};
use forskapd::error::Result;

fn main() -> Result<()> {
    // Before anything else: `--version`, `--help` and a usage error exit
    // here, with nothing started, read or moved.
    let args = Args::parse_checked();
    init_logging(&args);

    // A dry run's directory. It goes last, after the runtime: the runtime's
    // tasks hold the stores inside it.
    let scratch = if args.dry_run {
        Some(Scratch::create()?)
    } else {
        None
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let served = runtime.block_on(async {
        let env = match &scratch {
            Some(scratch) => Environment::dry_run(scratch, &args)?,
            None => match Environment::real(&args).await {
                Ok(env) => env,
                Err(e) => {
                    error!(error = %e, "failed to load configuration");
                    std::process::exit(1);
                }
            },
        };
        let daemon = Daemon::start(env)?;
        daemon.serve_until(daemon::shutdown_signal()?).await
    });
    drop(runtime);
    drop(scratch);
    served
}

fn init_logging(args: &Args) {
    let logs = tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_env("FORSKAPD_LOG")
            .or_else(|_| EnvFilter::try_from_env("GITLAB_TRACKRD_LOG"))
            .unwrap_or_else(|_| EnvFilter::new("forskapd=info")),
    );
    if args.dry_run {
        // Stdout carries the socket's address and nothing else.
        logs.with_writer(std::io::stderr).init();
    } else {
        logs.init();
    }
}
