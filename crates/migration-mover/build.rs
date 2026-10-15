// Link the digest-pinned static libnfs archive so mongoose remains one
// executable. See docs/LGPL_COMPLIANCE.md: release packaging is blocked unless
// the corresponding source and relink kit pass.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=VAMOOSE_LIBNFS_DIR");

    // A distro libnfs can satisfy a version check while omitting the raw
    // NFSv3 task symbols mongoose uses. Require the verified fork for every
    // build instead of discovering that ABI mismatch at the final link.
    let dir = std::env::var_os("VAMOOSE_LIBNFS_DIR").unwrap_or_else(|| {
        panic!(
            "VAMOOSE_LIBNFS_DIR is required; build the pinned archive with \
             `make libnfs-stage LIBNFS_SOURCE=/path/to/libnfs`, then run `make`"
        )
    });
    let dir = std::path::PathBuf::from(dir);
    let linker_name = dir.join("libnfs.a");
    assert!(
        linker_name.is_file(),
        "VAMOOSE_LIBNFS_DIR does not contain libnfs.a: {}",
        dir.display()
    );
    println!("cargo:rustc-link-search=native={}", dir.display());
    println!("cargo:rustc-link-lib=static=nfs");
}
