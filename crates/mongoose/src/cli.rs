//! Command-line surface: `mongoose copy | sync`.
//!
//! Deliberately small. The engine underneath has many knobs (context
//! pairs, per-size-class inflight limits, raw-FH vs path-based copy,
//! commit mode, RPC timeouts, shard sizing …); mongoose fixes all of
//! them to the values that were always used in practice and exposes
//! one load knob, `--parallel`, for the case that matters to an
//! operator: the source or destination server is being crushed.

use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

/// Default `--parallel`: the engine's long-standing 32 context pairs.
pub const DEFAULT_PARALLEL: u32 = 32;

/// Upper bound for `--parallel`. Every libnfs context pair costs two
/// reserved ports and ~111 pairs is the observed per-host ceiling.
pub const MAX_PARALLEL: u32 = 100;

#[derive(Parser, Debug)]
#[command(
    name = "mongoose",
    version,
    about = "Copy one NFS path to another, then keep it in sync until cutover",
    long_about = "mongoose copies everything under one NFS path to another NFS path from a\n\
                  single host, speaking NFSv3 directly (nothing is mounted, nothing else is\n\
                  installed). `copy` does the initial full copy; `sync` copies whatever\n\
                  changed on the source since the last pass and, with --cutover, reads the\n\
                  destination back to verify that the two trees match.\n\n\
                  Run as root. Re-running a command with the same --work-dir resumes it."
)]
pub struct Cli {
    /// More logging: -v for engine detail, -vv for debug. RUST_LOG,
    /// when set, overrides this flag.
    #[arg(
        short = 'v',
        long = "verbose",
        action = clap::ArgAction::Count,
        global = true,
        // Sort after the subcommand's own flags in `copy --help`.
        display_order = 100
    )]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Full copy of --src to --dst. Safe to re-run: it picks up where
    /// it stopped.
    Copy(CopyArgs),
    /// Copy what changed on the source since the last copy or sync.
    /// Repeat while the source is live; finish with --cutover once
    /// source writers are stopped.
    Sync(SyncArgs),
}

#[derive(Args, Debug)]
pub struct CopyArgs {
    /// Where to copy from: nfs://server/export[/path]
    #[arg(long, value_name = "URL")]
    pub src: String,

    /// Where to copy to: nfs://server/export[/path]
    #[arg(long, value_name = "URL")]
    pub dst: String,

    /// Local directory for this job's index, checkpoints, and results.
    /// One job per directory; reuse it to resume and to sync.
    #[arg(long, value_name = "DIR")]
    pub work_dir: PathBuf,

    /// Directory name pattern to skip, e.g. .snapshot (repeatable).
    /// Remembered in the work dir, so later syncs skip it too.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    #[command(flatten)]
    pub tuning: Tuning,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Work dir of a completed `mongoose copy`.
    #[arg(long, value_name = "DIR")]
    pub work_dir: PathBuf,

    /// Final pass: source writers must be stopped. Copies nothing.
    /// Rescans both trees and reads every file back from both servers;
    /// fails if anything differs, so a clean exit means the trees match.
    #[arg(long)]
    pub cutover: bool,

    #[command(flatten)]
    pub tuning: Tuning,
}

/// The one load knob, shared by `copy` and `sync`. Not sticky: stop,
/// re-run with a different value, and the job resumes at the new
/// level.
#[derive(Args, Debug, Clone)]
pub struct Tuning {
    /// How much to do at once: NFS connections, with scan workers and
    /// in-flight files scaled to match. Lower it if the source or
    /// destination server is struggling; raise it if both are idle
    /// and the copy is slow.
    #[arg(
        long,
        value_name = "N",
        default_value_t = DEFAULT_PARALLEL,
        value_parser = clap::value_parser!(u32).range(1..=MAX_PARALLEL as i64)
    )]
    pub parallel: u32,
}

impl Tuning {
    /// Scan (GETATTR) workers for the embedded walker: one per
    /// context pair, the engine's long-standing pairing.
    pub fn walker_workers(&self) -> usize {
        self.parallel.max(1) as usize
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            parallel: DEFAULT_PARALLEL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn copy_parses_the_documented_invocation() {
        let cli = Cli::try_parse_from([
            "mongoose",
            "copy",
            "--src",
            "nfs://old.example.com/export",
            "--dst",
            "nfs://new.example.com/export",
            "--work-dir",
            "/var/lib/mongoose/job1",
            "--exclude",
            ".snapshot",
        ])
        .unwrap();
        let Command::Copy(args) = cli.command else {
            panic!("expected copy");
        };
        assert_eq!(args.src, "nfs://old.example.com/export");
        assert_eq!(args.dst, "nfs://new.example.com/export");
        assert_eq!(args.work_dir, PathBuf::from("/var/lib/mongoose/job1"));
        assert_eq!(args.exclude, vec![".snapshot".to_string()]);
        assert_eq!(args.tuning.parallel, DEFAULT_PARALLEL, "default load");
    }

    #[test]
    fn sync_parses_with_and_without_cutover() {
        let cli = Cli::try_parse_from(["mongoose", "sync", "--work-dir", "/w"]).unwrap();
        let Command::Sync(args) = cli.command else {
            panic!("expected sync");
        };
        assert!(!args.cutover);
        assert_eq!(args.tuning.parallel, DEFAULT_PARALLEL);

        let cli = Cli::try_parse_from([
            "mongoose",
            "sync",
            "--work-dir",
            "/w",
            "--cutover",
            "--parallel",
            "8",
        ])
        .unwrap();
        let Command::Sync(args) = cli.command else {
            panic!("expected sync");
        };
        assert!(args.cutover);
        assert_eq!(args.tuning.parallel, 8);
    }

    #[test]
    fn parallel_is_bounded() {
        let base = ["mongoose", "sync", "--work-dir", "/w", "--parallel"];
        let parse = |v: &str| Cli::try_parse_from(base.iter().copied().chain([v]));
        assert!(parse("1").is_ok());
        assert!(parse("100").is_ok());
        assert!(parse("0").is_err(), "zero would deadlock the pool");
        assert!(parse("101").is_err(), "beyond the reserved-port ceiling");
        assert!(parse("lots").is_err());
    }

    #[test]
    fn required_flags_are_enforced() {
        assert!(Cli::try_parse_from(["mongoose", "copy", "--src", "nfs://s/e"]).is_err());
        assert!(Cli::try_parse_from(["mongoose", "sync"]).is_err());
        // The old tuning surface is gone, not merely hidden.
        assert!(
            Cli::try_parse_from(["mongoose", "sync", "--work-dir", "/w", "--use-raw-fh"]).is_err()
        );
        assert!(Cli::try_parse_from(["mongoose", "prepare", "--work-dir", "/w"]).is_err());
    }

    #[test]
    fn verbosity_counts_and_is_global() {
        let cli = Cli::try_parse_from(["mongoose", "sync", "--work-dir", "/w", "-vv"]).unwrap();
        assert_eq!(cli.verbose, 2);
        let cli = Cli::try_parse_from(["mongoose", "sync", "--work-dir", "/w"]).unwrap();
        assert_eq!(cli.verbose, 0, "compact by default");
    }
}
