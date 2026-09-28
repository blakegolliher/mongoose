//! `mongoose` binary — parse the CLI and dispatch.
//!
//! Exit codes: 0 = success; 1 = fatal error, including failed cutover
//! verification; 2 = completed copy pass with per-file failures; 130 =
//! handled SIGINT; 143 = handled SIGTERM. Re-run interrupted work to resume.

use clap::Parser;
use mongoose::cli::{Cli, Command};
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExitOutcome {
    Success,
    CompletedWithFailures,
    Interrupted(mongoose::stop::StopReason),
    Fatal,
}

fn exit_code_for(outcome: ExitOutcome) -> ExitCode {
    match outcome {
        ExitOutcome::Success => ExitCode::SUCCESS,
        ExitOutcome::CompletedWithFailures => ExitCode::from(2),
        ExitOutcome::Interrupted(reason) => ExitCode::from(reason.exit_code()),
        ExitOutcome::Fatal => ExitCode::FAILURE,
    }
}

fn main() -> ExitCode {
    // Default the full reserved-port range on (libnfs checks only the
    // variable's *presence*): without it, /etc/services name
    // registrations throttle a host to ~55 context pairs, and every
    // invocation needed a LIBNFS_USE_ALL_RESERVED=1 prefix. Opt out
    // by setting it to "0" or "" — mongoose then unsets it so libnfs
    // sees it as absent. Must run before the tokio runtime exists:
    // env mutation is only sound while the process is single-threaded.
    match std::env::var("LIBNFS_USE_ALL_RESERVED") {
        Err(std::env::VarError::NotPresent) => std::env::set_var("LIBNFS_USE_ALL_RESERVED", "1"),
        Ok(v) if v == "0" || v.is_empty() => std::env::remove_var("LIBNFS_USE_ALL_RESERVED"),
        _ => {}
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(async_main())
}

async fn async_main() -> ExitCode {
    let cli = Cli::parse();
    if let Command::Licenses(args) = &cli.command {
        if let Err(error) = mongoose::licenses::write(args.component, std::io::stdout().lock()) {
            eprintln!("error: could not write license information: {error}");
            return exit_code_for(ExitOutcome::Fatal);
        }
        return exit_code_for(ExitOutcome::Success);
    }
    // Compact by default: engine libraries (walker, rewrite, shard
    // processor, mover) log at warn; mongoose's own stage lines and
    // progress ticks stay. -v = full info, -vv = debug. RUST_LOG wins
    // when set.
    let default_filter = match cli.verbose {
        0 => "warn,mongoose=info",
        1 => "info",
        _ => "debug",
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter)),
        )
        .init();
    let (work_dir, command_name) = match &cli.command {
        Command::Copy(args) => (&args.work_dir, "mongoose copy"),
        Command::Sync(args) => (&args.work_dir, "mongoose sync"),
        Command::Licenses(_) => unreachable!("licenses returned before work-dir dispatch"),
    };
    // Keep this guard in scope through dispatch and result reporting. In
    // particular, copy's prepare and copy stages share this one lock.
    let _work_dir_lock = match mongoose::workdir_lock::WorkDirLock::acquire(work_dir, command_name)
    {
        Ok(lock) => lock,
        Err(error) => {
            eprintln!("error: {error:#}");
            return exit_code_for(ExitOutcome::Fatal);
        }
    };
    let result = match &cli.command {
        Command::Copy(args) => match mongoose::prepare::run(args).await {
            Ok(_) => mongoose::copy::run(&args.work_dir, &args.tuning)
                .await
                .map(|summary| {
                    if summary.interrupted {
                        ExitOutcome::Interrupted(
                            summary.stop_reason.expect("handled stop has reason"),
                        )
                    } else if summary.files_failed > 0 {
                        ExitOutcome::CompletedWithFailures
                    } else {
                        ExitOutcome::Success
                    }
                }),
            Err(e) => Err(e),
        },
        Command::Sync(args) => mongoose::sync::run(args).await.map(|outcome| {
            if outcome.interrupted {
                ExitOutcome::Interrupted(outcome.stop_reason.expect("handled stop has reason"))
            } else if outcome.copy.is_some_and(|copy| copy.files_failed > 0) {
                ExitOutcome::CompletedWithFailures
            } else {
                ExitOutcome::Success
            }
        }),
        Command::Licenses(_) => unreachable!("licenses returned before work dispatch"),
    };

    match result {
        Ok(outcome) => exit_code_for(outcome),
        Err(e) => {
            eprintln!("error: {e:#}");
            exit_code_for(ExitOutcome::Fatal)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{exit_code_for, ExitOutcome};
    use mongoose::stop::StopReason;
    use std::process::ExitCode;

    #[test]
    fn exit_mapping_preserves_completed_and_interrupted_outcomes() {
        assert_eq!(exit_code_for(ExitOutcome::Success), ExitCode::SUCCESS);
        assert_eq!(
            exit_code_for(ExitOutcome::CompletedWithFailures),
            ExitCode::from(2)
        );
        assert_eq!(
            exit_code_for(ExitOutcome::Interrupted(StopReason::Sigint)),
            ExitCode::from(130)
        );
        assert_eq!(
            exit_code_for(ExitOutcome::Interrupted(StopReason::Sigterm)),
            ExitCode::from(143)
        );
        assert_eq!(exit_code_for(ExitOutcome::Fatal), ExitCode::FAILURE);
    }
}
