//! License information embedded in every mongoose executable.

use crate::cli::LicenseComponent;
use serde::Deserialize;
use std::io::{self, Write};

const LIBNFS_LOCK: &str = include_str!("../../../packaging/libnfs.lock.json");
const LGPL_2_1: &str = include_str!("../../../packaging/licenses/LGPL-2.1.txt");

#[derive(Debug, Deserialize)]
struct LibnfsLock {
    source_url: String,
    source_git_sha: String,
    license: String,
    release_linkage: String,
}

fn libnfs_lock() -> LibnfsLock {
    serde_json::from_str(LIBNFS_LOCK).expect("checked-in libnfs lock must be valid JSON")
}

pub fn write(component: LicenseComponent, mut output: impl Write) -> io::Result<()> {
    match component {
        LicenseComponent::Libnfs => write_libnfs(&mut output),
    }
}

fn write_libnfs(output: &mut impl Write) -> io::Result<()> {
    let lock = libnfs_lock();
    let short_sha = &lock.source_git_sha[..12];
    let version = env!("CARGO_PKG_VERSION");
    let release_url = format!("https://github.com/blakegolliher/mongoose/releases/tag/v{version}");

    writeln!(output, "Component: libnfs")?;
    writeln!(output, "License: {}", lock.license)?;
    writeln!(output, "Source repository: {}", lock.source_url)?;
    writeln!(output, "Source revision: {}", lock.source_git_sha)?;
    writeln!(
        output,
        "Linkage: {} (libnfs is statically linked into this executable)",
        lock.release_linkage
    )?;
    writeln!(
        output,
        "Corresponding source and relink materials: {release_url}"
    )?;
    writeln!(
        output,
        "  libnfs-{short_sha}-source.tar.gz\n  mongoose-{version}-source.tar.gz\n  mongoose-{version}-relink-kit.tar.gz"
    )?;
    writeln!(output)?;
    output.write_all(LGPL_2_1.as_bytes())?;
    if !LGPL_2_1.ends_with('\n') {
        writeln!(output)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn libnfs_output_contains_complete_release_identity_and_license() {
        let mut output = Vec::new();
        write(LicenseComponent::Libnfs, &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        let lock = libnfs_lock();

        assert!(output.contains("LGPL-2.1-or-later"));
        assert!(output.contains(&lock.source_git_sha));
        assert!(output.contains("statically linked"));
        assert!(output.contains("mongoose-0.2.0-relink-kit.tar.gz"));
        assert!(output.contains("GNU LESSER GENERAL PUBLIC LICENSE"));
        assert!(output.contains("END OF TERMS AND CONDITIONS"));
    }
}
