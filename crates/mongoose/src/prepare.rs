//! The first half of `mongoose copy`: build the local migration index.
//! Scan with the compiled-in nfs-walker library, rewrite the scan to
//! canonical parquet shards with the compiled-in mig-walker-rewrite
//! library, and write the local manifest. Every stage checkpoints
//! under the work dir and re-running resumes.

use crate::cli::CopyArgs;
use crate::endpoint;
use crate::manifest::{self, LocalManifest};
use crate::scan::{self, ScanParams};
use crate::util::{raise_fd_limit, utc_now};
use crate::workdir::{default_run_id, ensure_run_spec, RunSpec, WorkDir};
use anyhow::{Context, Result};
use migration_core::prepare_tools as tools;
use migration_core::records::{Endpoint, EndpointKind, MigrationOptions};

/// The whole URL is what both the scanner and the mover mount, and
/// everything under it is the migration root. (The engine also
/// supports a root *inside* the mount; mongoose does not expose it —
/// put the path in the URL instead.)
const ROOT: &str = "/";

pub async fn run(args: &CopyArgs) -> Result<LocalManifest> {
    // Root is required by the embedded scanner and the mover (reserved
    // ports for AUTH_SYS). SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        tracing::warn!("mongoose is not running as root; NFS access usually needs sudo");
    }
    raise_fd_limit();

    // Validate both URLs and refuse an overlapping pair before any
    // long stage — and before writing anything to the work dir.
    let src = endpoint::parse("--src", &args.src)?;
    let dst = endpoint::parse("--dst", &args.dst)?;
    endpoint::check_overlap(&src, &dst)?;
    let source = Endpoint {
        kind: EndpointKind::Nfs,
        url: src.url(),
        root: ROOT.to_string(),
    };
    let dest = Endpoint {
        kind: EndpointKind::Nfs,
        url: dst.url(),
        root: ROOT.to_string(),
    };
    migration_core::overlap::check(&source, &dest)?;

    let wd = WorkDir::new(&args.work_dir);
    std::fs::create_dir_all(wd.root())
        .with_context(|| format!("creating {}", wd.root().display()))?;
    let spec = ensure_run_spec(
        &wd.run_json(),
        RunSpec {
            run_id: default_run_id(),
            created_utc: utc_now(),
            source,
            dest,
            exclude: args.exclude.clone(),
        },
    )?;

    println!(
        "mongoose copy\n  job      {}\n  source   {}\n  dest     {}\n  exclude  {}\n  work     {}\n",
        spec.run_id,
        spec.source.url,
        spec.dest.url,
        if spec.exclude.is_empty() {
            "(none)".to_string()
        } else {
            spec.exclude.join(" ")
        },
        wd.root().display()
    );

    if let Some(existing) = manifest::load(&wd)? {
        println!(
            "index already built ({} shards, {} entries); skipping the scan\n",
            existing.shards.len(),
            existing.total_rows,
        );
        return Ok(existing);
    }

    // ---- 1. scan ---------------------------------------------------
    println!("[1/3] scan the source");
    let scan = scan::ensure_scan(
        &wd,
        &ScanParams {
            scan_url: tools::scan_url(&spec.source.url, &spec.source.root),
            workers: args.tuning.walker_workers(),
            exclude: spec.exclude.clone(),
        },
    )
    .await?;
    println!("  scanner  {}\n", scan.walker_version);

    // ---- 2. canonical rewrite --------------------------------------
    println!("[2/3] build the index");
    scan::ensure_canonical(&wd, &scan).await?;

    // ---- 3. verify shards and write the manifest -------------------
    println!("[3/3] verify the index");
    let m = manifest::build(
        &wd,
        &spec.run_id,
        spec.source.clone(),
        spec.dest.clone(),
        MigrationOptions::default(),
    )?;
    // The raw scan is a pure intermediate that doubles the index
    // footprint; the canonical shards and manifest are what copy and
    // sync read.
    scan::purge_scan_output(&wd);
    println!(
        "\nindex ready: {} shards, {} entries\n",
        m.shards.len(),
        m.total_rows,
    );
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{CopyArgs, Tuning};

    fn args(src: &str, dst: &str, work: &std::path::Path) -> CopyArgs {
        CopyArgs {
            src: src.into(),
            dst: dst.into(),
            work_dir: work.to_path_buf(),
            exclude: vec![],
            tuning: Tuning::default(),
        }
    }

    #[tokio::test]
    async fn overlapping_source_and_dest_are_refused_before_any_work() {
        let dir = tempfile::tempdir().unwrap();
        // Destination nested under the source on the same server: the
        // classic truncate-your-source misconfiguration, spelled the
        // way mongoose operators spell it (whole path in the URL).
        let a = args("nfs://h/export", "nfs://h/export/dst", dir.path());
        let err = run(&a).await.unwrap_err();
        assert!(format!("{err:#}").contains("overlap"), "{err:#}");
        assert!(
            !dir.path().join("run.json").exists(),
            "refused before writing anything"
        );
    }

    #[tokio::test]
    async fn malformed_urls_are_refused_before_any_work() {
        let dir = tempfile::tempdir().unwrap();
        let a = args("old-server:/export", "nfs://h/export", dir.path());
        let err = run(&a).await.unwrap_err();
        assert!(format!("{err:#}").contains("--src"), "{err:#}");
        assert!(!dir.path().join("run.json").exists());
    }
}
