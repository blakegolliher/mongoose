//! The local manifest — mongoose's run plan.
//!
//! Deliberately its own format rather than a reuse of
//! `migration_core::records::Manifest`: that shape carries S3
//! assumptions (per-shard ETags, S3 keys) that have no local meaning.
//! Endpoints and copy options are reused from `migration_core` so the
//! mover sees the exact types it already understands.
//!
//! Shard `path`s are **work-dir-relative local paths**
//! (`canonical/part-0000.parquet`), never S3 keys.

use crate::util::{read_json_opt, sha256_file, utc_now, write_json_atomic};
use crate::workdir::{RunSpec, WorkDir};
use anyhow::{Context, Result};
use migration_core::records::{Endpoint, MigrationOptions};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

/// Bump on breaking changes to the local manifest shape.
pub const MANIFEST_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalManifest {
    pub format_version: u32,
    pub run_id: String,
    pub created_utc: String,
    pub source: Endpoint,
    pub dest: Endpoint,
    pub options: MigrationOptions,
    /// Sorted by `path`; `copy` processes them in this order.
    pub shards: Vec<LocalShard>,
    pub total_rows: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalShard {
    /// Work-dir-relative path, e.g. `canonical/part-0000.parquet`.
    pub path: String,
    pub rows: u64,
    pub bytes: u64,
    pub sha256: String,
}

/// Job identity used when loading a pass, destination, or delta manifest.
#[derive(Debug, Clone)]
pub struct ExpectedIdentity {
    pub run_id: String,
    pub source: Endpoint,
    pub dest: Endpoint,
}

impl From<&LocalManifest> for ExpectedIdentity {
    fn from(m: &LocalManifest) -> Self {
        Self {
            run_id: m.run_id.clone(),
            source: m.source.clone(),
            dest: m.dest.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestKind {
    Canonical,
    Delta,
}

impl LocalShard {
    /// `part-0000.parquet` — the name stamped on failure/downgrade
    /// records and used for the per-shard JSONL files.
    pub fn file_name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// `part-0000` — stem for `failures/<stem>.jsonl`.
    pub fn stem(&self) -> &str {
        let name = self.file_name();
        name.strip_suffix(".parquet").unwrap_or(name)
    }
}

/// The subset of `mig-walker-rewrite`'s `--report` JSON mongoose
/// consumes (schema version 1; same contract `vamoose prepare` uses).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RewriteReport {
    pub schema_version: u32,
    pub output_dir: String,
    pub complete: bool,
    pub shards: Vec<RewriteShard>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RewriteShard {
    pub output_name: String,
    pub output_bytes: u64,
    pub output_sha256: String,
    pub rows: u64,
}

/// Build `manifest.json` from a completed rewrite report: every shard
/// is re-verified on disk (size and SHA256) so the manifest never
/// trusts a checkpoint over the bytes. Shards are sorted by path.
pub fn build(
    workdir: &WorkDir,
    run_id: &str,
    source: Endpoint,
    dest: Endpoint,
    options: MigrationOptions,
) -> Result<LocalManifest> {
    let report_path = workdir.rewrite_json();
    let report: RewriteReport = read_json_opt(&report_path)?
        .ok_or_else(|| anyhow::anyhow!("rewrite report {} is missing", report_path.display()))?;
    if report.schema_version != 1 || !report.complete {
        anyhow::bail!(
            "rewrite report {} is not a completed schema-version-1 checkpoint",
            report_path.display()
        );
    }
    if report.shards.is_empty() {
        anyhow::bail!(
            "rewrite report {} contains no shards",
            report_path.display()
        );
    }

    let canonical = workdir.canonical_dir();
    let mut shards = Vec::with_capacity(report.shards.len());
    for shard in &report.shards {
        validate_shard_path(
            &format!("canonical/{}", shard.output_name),
            ManifestKind::Canonical,
        )
        .with_context(|| format!("invalid rewrite shard name {}", shard.output_name))?;
        let path = canonical.join(&shard.output_name);
        shards.push(verify_shard_on_disk(&path, shard)?);
    }
    shards.sort_by(|a, b| a.path.cmp(&b.path));
    let (total_rows, total_bytes) = checked_totals(&shards)?;

    let manifest = LocalManifest {
        format_version: MANIFEST_FORMAT_VERSION,
        run_id: run_id.to_string(),
        created_utc: utc_now(),
        source,
        dest,
        options,
        total_rows,
        total_bytes,
        shards,
    };
    validate(workdir, &manifest, None, ManifestKind::Canonical)?;
    write_json_atomic(&workdir.manifest_json(), &manifest)?;
    Ok(manifest)
}

pub(crate) fn checked_totals(shards: &[LocalShard]) -> Result<(u64, u64)> {
    shards
        .iter()
        .try_fold((0u64, 0u64), |(rows, bytes), shard| {
            Ok((
                rows.checked_add(shard.rows).ok_or_else(|| {
                    anyhow::anyhow!("manifest total_rows overflow at shard {}", shard.path)
                })?,
                bytes.checked_add(shard.bytes).ok_or_else(|| {
                    anyhow::anyhow!("manifest total_bytes overflow at shard {}", shard.path)
                })?,
            ))
        })
}

fn verify_shard_on_disk(path: &Path, shard: &RewriteShard) -> Result<LocalShard> {
    let size = std::fs::metadata(path)
        .map(|m| m.len())
        .with_context(|| format!("canonical shard {} is missing", path.display()))?;
    if size != shard.output_bytes {
        anyhow::bail!(
            "canonical shard {} is {size} bytes; rewrite checkpoint says {}",
            path.display(),
            shard.output_bytes
        );
    }
    let digest = sha256_file(path)?;
    if digest != shard.output_sha256 {
        anyhow::bail!(
            "canonical shard {} SHA256 mismatch: expected {}, actual {}",
            path.display(),
            shard.output_sha256,
            digest
        );
    }
    Ok(LocalShard {
        path: format!("canonical/{}", shard.output_name),
        rows: shard.rows,
        bytes: size,
        sha256: digest,
    })
}

/// Load a previously built manifest; `Ok(None)` when prepare has not
/// finished.
pub fn load(workdir: &WorkDir) -> Result<Option<LocalManifest>> {
    load_validated(
        workdir,
        &workdir.manifest_json(),
        None,
        ManifestKind::Canonical,
    )
}

/// [`load`] for an arbitrary manifest file (the resync delta manifest
/// shares the format under a different name).
pub fn load_file(path: &Path) -> Result<Option<LocalManifest>> {
    let wd = WorkDir::new(path.parent().unwrap_or_else(|| Path::new(".")));
    let kind = if path.file_name().is_some_and(|n| n == "delta-manifest.json") {
        ManifestKind::Delta
    } else {
        ManifestKind::Canonical
    };
    load_validated(&wd, path, None, kind)
}

/// The single manifest trust boundary. Structure, identity, relative paths,
/// file type, recorded size, and digest are checked before any consumer gets
/// a manifest. All shards are checked, including completed copy shards: a
/// resume must not accept a damaged work directory as a valid completed run.
pub fn load_validated(
    workdir: &WorkDir,
    path: &Path,
    expected: Option<&ExpectedIdentity>,
    kind: ManifestKind,
) -> Result<Option<LocalManifest>> {
    let Some(m) = read_json_opt::<LocalManifest>(path)? else {
        return Ok(None);
    };
    let spec = read_json_opt::<RunSpec>(&workdir.run_json())?;
    let spec_identity = spec.as_ref().map(|s| ExpectedIdentity {
        run_id: s.run_id.clone(),
        source: s.source.clone(),
        dest: s.dest.clone(),
    });
    if let (Some(a), Some(b)) = (expected, spec_identity.as_ref()) {
        anyhow::ensure!(
            a.run_id == b.run_id && a.source == b.source && a.dest == b.dest,
            "manifest validation context disagrees with work directory run.json"
        );
    }
    validate(workdir, &m, expected.or(spec_identity.as_ref()), kind)?;
    Ok(Some(m))
}

/// Validate an already-loaded manifest through the same trust boundary.
pub fn validate(
    workdir: &WorkDir,
    m: &LocalManifest,
    expected: Option<&ExpectedIdentity>,
    kind: ManifestKind,
) -> Result<()> {
    if m.format_version != MANIFEST_FORMAT_VERSION {
        anyhow::bail!(
            "manifest format_version {} does not match this mongoose ({})",
            m.format_version,
            MANIFEST_FORMAT_VERSION,
        );
    }
    if let Some(expected) = expected {
        anyhow::ensure!(
            m.run_id == expected.run_id,
            "manifest run identity mismatch: expected {}, actual {}",
            expected.run_id,
            m.run_id
        );
        anyhow::ensure!(
            m.source == expected.source,
            "manifest source endpoint mismatch for run {}",
            m.run_id
        );
        anyhow::ensure!(
            m.dest == expected.dest,
            "manifest destination endpoint mismatch for run {}",
            m.run_id
        );
    }
    anyhow::ensure!(
        !m.shards.is_empty(),
        "manifest contains no canonical or delta shards"
    );

    let mut paths = HashSet::with_capacity(m.shards.len());
    let mut rows = 0u64;
    let mut bytes = 0u64;
    for shard in &m.shards {
        validate_shard_path(&shard.path, kind)
            .with_context(|| format!("invalid manifest shard {}", shard.path))?;
        anyhow::ensure!(
            paths.insert(shard.path.as_str()),
            "duplicate manifest shard entry {}",
            shard.path
        );
        anyhow::ensure!(
            shard.rows > 0,
            "manifest shard {} has invalid row count 0",
            shard.path
        );
        anyhow::ensure!(
            shard.bytes > 0,
            "manifest shard {} has invalid byte count 0",
            shard.path
        );
        anyhow::ensure!(
            shard.sha256.len() == 64 && shard.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
            "manifest shard {} has invalid expected SHA256 {}",
            shard.path,
            shard.sha256
        );
        rows = rows.checked_add(shard.rows).ok_or_else(|| {
            anyhow::anyhow!("manifest total_rows overflow at shard {}", shard.path)
        })?;
        bytes = bytes.checked_add(shard.bytes).ok_or_else(|| {
            anyhow::anyhow!("manifest total_bytes overflow at shard {}", shard.path)
        })?;

        verify_shard_integrity(workdir, shard)?;
    }
    anyhow::ensure!(
        rows == m.total_rows,
        "manifest total_rows mismatch: expected {}, actual {}",
        m.total_rows,
        rows
    );
    anyhow::ensure!(
        bytes == m.total_bytes,
        "manifest total_bytes mismatch: expected {}, actual {}",
        m.total_bytes,
        bytes
    );
    Ok(())
}

/// Recheck one already-validated shard immediately before a consumer reads it.
pub fn verify_shard_integrity(workdir: &WorkDir, shard: &LocalShard) -> Result<()> {
    // Infer the only two supported layouts from the path itself, then apply
    // the same strict path parser used by whole-manifest validation.
    let kind = if shard.path.starts_with("delta/") {
        ManifestKind::Delta
    } else {
        ManifestKind::Canonical
    };
    validate_shard_path(&shard.path, kind)
        .with_context(|| format!("invalid manifest shard {}", shard.path))?;
    let root = std::fs::canonicalize(workdir.root())
        .with_context(|| format!("resolving work directory {}", workdir.root().display()))?;
    let resolved = std::fs::canonicalize(workdir.shard_path(&shard.path)).with_context(|| {
        format!(
            "manifest shard {} is missing or cannot be resolved",
            shard.path
        )
    })?;
    anyhow::ensure!(
        resolved.starts_with(&root),
        "manifest shard {} resolves outside work directory {}",
        shard.path,
        root.display()
    );
    let metadata = std::fs::metadata(&resolved)
        .with_context(|| format!("reading metadata for manifest shard {}", shard.path))?;
    anyhow::ensure!(
        metadata.is_file(),
        "manifest shard {} is not a regular file",
        shard.path
    );
    let actual_bytes = metadata.len();
    anyhow::ensure!(
        actual_bytes == shard.bytes,
        "manifest shard {} size mismatch: expected {}, actual {}",
        shard.path,
        shard.bytes,
        actual_bytes
    );
    let actual_sha =
        sha256_file(&resolved).with_context(|| format!("hashing manifest shard {}", shard.path))?;
    anyhow::ensure!(
        actual_sha.eq_ignore_ascii_case(&shard.sha256),
        "manifest shard {} SHA256 mismatch: expected {}, actual {}",
        shard.path,
        shard.sha256,
        actual_sha
    );
    let parquet = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        std::fs::File::open(&resolved)
            .with_context(|| format!("opening manifest shard {}", shard.path))?,
    )
    .with_context(|| format!("reading Parquet footer for manifest shard {}", shard.path))?;
    let actual_rows = parquet.metadata().file_metadata().num_rows() as u64;
    anyhow::ensure!(
        actual_rows == shard.rows,
        "manifest shard {} row count mismatch: expected {}, actual {}",
        shard.path,
        shard.rows,
        actual_rows
    );
    Ok(())
}

fn validate_shard_path(path: &str, kind: ManifestKind) -> Result<PathBuf> {
    anyhow::ensure!(!path.is_empty(), "path is empty");
    anyhow::ensure!(
        !path.contains('\\'),
        "backslash path separators are not allowed"
    );
    anyhow::ensure!(
        !path.bytes().any(|b| b == 0 || b < 0x20),
        "control bytes are not allowed"
    );
    let native = Path::new(path);
    anyhow::ensure!(!native.is_absolute(), "absolute paths are not allowed");
    for component in native.components() {
        match component {
            Component::Normal(_) => {}
            _ => anyhow::bail!("unsafe path component {component:?}"),
        }
    }
    let components: Vec<_> = path.split('/').collect();
    let expected_dir = match kind {
        ManifestKind::Canonical => "canonical",
        ManifestKind::Delta => "delta",
    };
    anyhow::ensure!(
        components.len() == 2 && components[0] == expected_dir,
        "expected {expected_dir}/<shard>.parquet path"
    );
    let name = components[1];
    let stem = name.strip_suffix(".parquet").unwrap_or("");
    anyhow::ensure!(
        stem.starts_with("part-")
            && stem.len() > 5
            && stem[5..]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "expected a safe part-*.parquet shard filename"
    );
    Ok(native.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::{ArrayRef, UInt8Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use arrow::record_batch::RecordBatch;
    use migration_core::records::EndpointKind;
    use std::sync::Arc;

    fn endpoint(url: &str) -> Endpoint {
        Endpoint {
            kind: EndpointKind::Nfs,
            url: url.into(),
            root: "/".into(),
        }
    }

    fn write_test_parquet(path: &Path, rows: usize) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new(
            "value",
            DataType::UInt8,
            false,
        )]));
        let values: ArrayRef = Arc::new(UInt8Array::from(vec![1; rows]));
        let batch = RecordBatch::try_new(schema.clone(), vec![values]).unwrap();
        let mut writer = parquet::arrow::ArrowWriter::try_new(
            std::fs::File::create(path).unwrap(),
            schema,
            None,
        )
        .unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    /// A work dir with a completed rewrite report and matching shard
    /// files on disk. Shards are reported out of order on purpose.
    fn fixture(dir: &Path) -> WorkDir {
        let wd = WorkDir::new(dir);
        std::fs::create_dir_all(wd.canonical_dir()).unwrap();
        let mut shards = Vec::new();
        for (name, rows) in [("part-0001.parquet", 7u64), ("part-0000.parquet", 5u64)] {
            let path = wd.canonical_dir().join(name);
            write_test_parquet(&path, rows as usize);
            shards.push(RewriteShard {
                output_name: name.into(),
                output_bytes: std::fs::metadata(&path).unwrap().len(),
                output_sha256: sha256_file(&path).unwrap(),
                rows,
            });
        }
        let report = RewriteReport {
            schema_version: 1,
            output_dir: wd.canonical_dir().to_string_lossy().into_owned(),
            complete: true,
            shards,
        };
        write_json_atomic(&wd.rewrite_json(), &report).unwrap();
        wd
    }

    #[test]
    fn build_verifies_sorts_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let wd = fixture(dir.path());
        let m = build(
            &wd,
            "run-t",
            endpoint("nfs://s/e"),
            endpoint("nfs://d/e"),
            MigrationOptions::default(),
        )
        .unwrap();

        // Shard ordering: sorted by path even though the report listed
        // part-0001 first.
        let paths: Vec<&str> = m.shards.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["canonical/part-0000.parquet", "canonical/part-0001.parquet"]
        );
        assert_eq!(m.total_rows, 12);
        assert_eq!(m.total_bytes, m.shards.iter().map(|s| s.bytes).sum::<u64>());
        assert_eq!(m.shards[0].stem(), "part-0000");
        assert_eq!(m.shards[0].file_name(), "part-0000.parquet");

        let back = load(&wd).unwrap().expect("manifest.json written");
        assert_eq!(back.run_id, "run-t");
        assert_eq!(back.shards, m.shards);
    }

    #[test]
    fn build_refuses_missing_or_tampered_shards() {
        let dir = tempfile::tempdir().unwrap();
        let wd = fixture(dir.path());

        // Tamper with one shard: same length, different bytes.
        let path = wd.canonical_dir().join("part-0000.parquet");
        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let err = build(
            &wd,
            "r",
            endpoint("nfs://s/e"),
            endpoint("nfs://d/e"),
            MigrationOptions::default(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("SHA256"), "{err:#}");

        // Remove it entirely.
        std::fs::remove_file(wd.canonical_dir().join("part-0000.parquet")).unwrap();
        let err = build(
            &wd,
            "r",
            endpoint("nfs://s/e"),
            endpoint("nfs://d/e"),
            MigrationOptions::default(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("missing"), "{err:#}");
    }

    #[test]
    fn build_refuses_an_incomplete_report() {
        let dir = tempfile::tempdir().unwrap();
        let wd = fixture(dir.path());
        let mut report: RewriteReport = read_json_opt(&wd.rewrite_json()).unwrap().unwrap();
        report.complete = false;
        write_json_atomic(&wd.rewrite_json(), &report).unwrap();
        let err = build(
            &wd,
            "r",
            endpoint("nfs://s/e"),
            endpoint("nfs://d/e"),
            MigrationOptions::default(),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("not a completed"), "{err:#}");
    }

    fn built_fixture(dir: &Path) -> (WorkDir, LocalManifest) {
        let wd = fixture(dir);
        let m = build(
            &wd,
            "run-t",
            endpoint("nfs://s/e"),
            endpoint("nfs://d/e"),
            MigrationOptions::default(),
        )
        .unwrap();
        (wd, m)
    }

    #[test]
    fn validator_rejects_same_length_mutation_with_expected_and_actual_digest() {
        let dir = tempfile::tempdir().unwrap();
        let (wd, m) = built_fixture(dir.path());
        let path = wd.shard_path(&m.shards[0].path);
        let mut bytes = std::fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 1;
        std::fs::write(&path, bytes).unwrap();
        let err = validate(&wd, &m, None, ManifestKind::Canonical).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains(&m.shards[0].path), "{message}");
        assert!(message.contains(&m.shards[0].sha256), "{message}");
        assert!(message.contains("actual"), "{message}");
    }

    #[test]
    fn validator_rejects_missing_duplicate_absolute_and_traversal_shards() {
        let dir = tempfile::tempdir().unwrap();
        let (wd, m) = built_fixture(dir.path());

        std::fs::remove_file(wd.shard_path(&m.shards[0].path)).unwrap();
        let err = validate(&wd, &m, None, ManifestKind::Canonical).unwrap_err();
        assert!(format!("{err:#}").contains(&m.shards[0].path));
        write_test_parquet(&wd.shard_path(&m.shards[0].path), m.shards[0].rows as usize);

        let mut duplicate = m.clone();
        duplicate.shards.push(duplicate.shards[0].clone());
        let err = validate(&wd, &duplicate, None, ManifestKind::Canonical).unwrap_err();
        assert!(format!("{err:#}").contains("duplicate manifest shard"));

        for path in ["/tmp/part-0000.parquet", "canonical/../part-0000.parquet"] {
            let mut invalid = m.clone();
            invalid.shards[0].path = path.into();
            let err = validate(&wd, &invalid, None, ManifestKind::Canonical).unwrap_err();
            assert!(format!("{err:#}").contains("invalid manifest shard"));
        }
    }

    #[cfg(unix)]
    #[test]
    fn validator_rejects_a_shard_symlink_that_escapes_the_work_directory() {
        let parent = tempfile::tempdir().unwrap();
        let work_root = parent.path().join("work");
        std::fs::create_dir_all(&work_root).unwrap();
        let (wd, m) = built_fixture(&work_root);
        let outside = parent.path().join("outside.parquet");
        write_test_parquet(&outside, m.shards[0].rows as usize);
        let shard = wd.shard_path(&m.shards[0].path);
        std::fs::remove_file(&shard).unwrap();
        std::os::unix::fs::symlink(&outside, &shard).unwrap();
        let err = validate(&wd, &m, None, ManifestKind::Canonical).unwrap_err();
        assert!(format!("{err:#}").contains("resolves outside work directory"));
    }

    #[test]
    fn validator_rejects_wrong_totals_run_and_endpoints() {
        let dir = tempfile::tempdir().unwrap();
        let (wd, m) = built_fixture(dir.path());
        let mut wrong_total = m.clone();
        wrong_total.total_rows += 1;
        assert!(format!(
            "{:#}",
            validate(&wd, &wrong_total, None, ManifestKind::Canonical).unwrap_err()
        )
        .contains("total_rows mismatch"));
        let mut wrong_version = m.clone();
        wrong_version.format_version += 1;
        assert!(format!(
            "{:#}",
            validate(&wd, &wrong_version, None, ManifestKind::Canonical).unwrap_err()
        )
        .contains("format_version"));
        let mut wrong_shard_rows = m.clone();
        wrong_shard_rows.shards[0].rows += 1;
        wrong_shard_rows.total_rows += 1;
        assert!(format!(
            "{:#}",
            validate(&wd, &wrong_shard_rows, None, ManifestKind::Canonical).unwrap_err()
        )
        .contains("row count mismatch"));

        let mut expected = ExpectedIdentity::from(&m);
        expected.run_id = "another-run".into();
        assert!(format!(
            "{:#}",
            validate(&wd, &m, Some(&expected), ManifestKind::Canonical).unwrap_err()
        )
        .contains("run identity mismatch"));
        expected = ExpectedIdentity::from(&m);
        expected.source = endpoint("nfs://other/e");
        assert!(format!(
            "{:#}",
            validate(&wd, &m, Some(&expected), ManifestKind::Canonical).unwrap_err()
        )
        .contains("source endpoint mismatch"));
        expected = ExpectedIdentity::from(&m);
        expected.dest = endpoint("nfs://other/e");
        assert!(format!(
            "{:#}",
            validate(&wd, &m, Some(&expected), ManifestKind::Canonical).unwrap_err()
        )
        .contains("destination endpoint mismatch"));
    }

    #[test]
    fn canonical_and_delta_manifests_share_validation_and_valid_resume_loads() {
        let dir = tempfile::tempdir().unwrap();
        let (wd, m) = built_fixture(dir.path());
        let expected = ExpectedIdentity::from(&m);
        assert!(load_validated(
            &wd,
            &wd.manifest_json(),
            Some(&expected),
            ManifestKind::Canonical
        )
        .unwrap()
        .is_some());

        std::fs::create_dir_all(wd.delta_dir()).unwrap();
        let delta_file = wd.delta_dir().join("part-0000.parquet");
        write_test_parquet(&delta_file, 1);
        let shard = LocalShard {
            path: "delta/part-0000.parquet".into(),
            rows: 1,
            bytes: std::fs::metadata(&delta_file).unwrap().len(),
            sha256: sha256_file(&delta_file).unwrap(),
        };
        let delta = LocalManifest {
            format_version: MANIFEST_FORMAT_VERSION,
            run_id: m.run_id.clone(),
            created_utc: m.created_utc.clone(),
            source: m.source.clone(),
            dest: m.dest.clone(),
            options: m.options.clone(),
            shards: vec![shard],
            total_rows: 1,
            total_bytes: std::fs::metadata(&delta_file).unwrap().len(),
        };
        write_json_atomic(&wd.delta_manifest_json(), &delta).unwrap();
        assert!(load_validated(
            &wd,
            &wd.delta_manifest_json(),
            Some(&expected),
            ManifestKind::Delta
        )
        .unwrap()
        .is_some());
        let mut bytes = std::fs::read(&delta_file).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&delta_file, bytes).unwrap();
        let err = load_validated(
            &wd,
            &wd.delta_manifest_json(),
            Some(&expected),
            ManifestKind::Delta,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("delta/part-0000.parquet"));
    }

    #[test]
    fn load_absent_manifest_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(&WorkDir::new(dir.path())).unwrap().is_none());
    }
}
