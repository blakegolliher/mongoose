//! `--exclude` patterns: the contract, and its validation.
//!
//! ## Contract
//!
//! A pattern is a **glob matched against a directory's own name**:
//! `*` matches any run of characters, `?` exactly one, `[...]` a
//! character class (`[!...]` negated), `\x` the literal `x`; anything
//! else is literal. It is never matched against a path, so it cannot
//! contain `/`, and it never matches a file. A directory whose name
//! matches is left out together with everything under it: its own
//! row is not emitted and it is not descended into.
//!
//! The set is job identity (`run.json`) and is applied unchanged to
//! every scan of the job: the initial copy, every sync, and the
//! cutover's destination scan, so an excluded tree is absent from
//! both indexes and never reported as missing or extra.
//!
//! The matcher is the embedded walker's own (`--exclude-dir`;
//! [`nfs_walker::config::compile_dir_glob`]), so what mongoose
//! validates here is exactly what the scan applies. An invalid pattern
//! is refused before anything is mounted or written.

use anyhow::Result;

/// Refuse any pattern the walker would refuse, naming the flag, the
/// pattern, and the reason.
pub fn validate(flag: &str, patterns: &[String]) -> Result<()> {
    for p in patterns {
        if let Err(e) = nfs_walker::config::compile_dir_glob(p) {
            anyhow::bail!(
                "{flag} {p:?} is not a valid directory-name glob: {}\n  \
                 Patterns match a directory's name only (`*`, `?`, `[...]`), never its path: \
                 for example `--exclude .snapshot` or `--exclude '*.tmp'`.",
                reason_of(&e)
            );
        }
    }
    Ok(())
}

/// Does `pattern` exclude a directory called `name`? For tests and
/// documentation; the scan itself matches inside the walker.
pub fn matches(pattern: &str, name: &str) -> Result<bool> {
    let re = nfs_walker::config::compile_dir_glob(pattern)
        .map_err(|e| anyhow::anyhow!("{pattern:?}: {}", reason_of(&e)))?;
    Ok(re.is_match(name))
}

fn reason_of(e: &nfs_walker::error::ConfigError) -> String {
    match e {
        nfs_walker::error::ConfigError::InvalidExcludePattern { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_patterns_are_accepted() {
        let ok: Vec<String> = [".snapshot", ".zfs", "~snapshot", "*.tmp", "cache?", "[!.]*"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        validate("--exclude", &ok).unwrap();
        validate("--exclude", &[]).unwrap();
    }

    #[test]
    fn invalid_patterns_are_refused_naming_flag_and_pattern() {
        for bad in ["[", "", "a/b", "x\\"] {
            let err = validate("--exclude", &[bad.to_string()]).unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("--exclude"), "{bad:?}: {msg}");
            assert!(msg.contains(&format!("{bad:?}")), "{bad:?}: {msg}");
            assert!(msg.contains("directory-name glob"), "{bad:?}: {msg}");
        }
        // One bad pattern in a set fails the set.
        let err = validate("--exclude", &[".snapshot".into(), "[".into()]).unwrap_err();
        assert!(format!("{err:#}").contains("\"[\""));
    }

    #[test]
    fn matching_is_by_name_as_a_glob_not_a_regex() {
        assert!(matches(".snapshot", ".snapshot").unwrap());
        assert!(
            !matches(".snapshot", "mysnapshot").unwrap(),
            "'.' is literal"
        );
        assert!(!matches(".snapshot", ".snapshots").unwrap(), "whole name");
        assert!(matches("*.tmp", "build.tmp").unwrap());
        assert!(!matches("*.tmp", "build.tmp.keep").unwrap());
        assert!(matches(".*", ".zfs").unwrap());
        assert!(!matches(".*", "zfs").unwrap());
        assert!(matches("[!.]*", "data").unwrap());
        assert!(!matches("[!.]*", ".data").unwrap());
        assert!(matches("a[]]b", "a]b").unwrap());
    }
}
