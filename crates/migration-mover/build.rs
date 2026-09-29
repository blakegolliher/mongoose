// Find libnfs via pkg-config and emit linker flags.
//
// Release builds intentionally use the digest-pinned static archive so
// mongoose remains one executable. See docs/LGPL_COMPLIANCE.md: release
// packaging is blocked unless the corresponding source and relink kit pass.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=VAMOOSE_LIBNFS_DIR");

    // Release bundles set this to a staging directory containing an exact,
    // digest-pinned `libnfs.a`. This prevents a
    // release from being linked against whatever mutable pkg-config entry
    // happens to be installed on the build host. Ordinary developer builds
    // retain the convenient pkg-config path below.
    if let Some(dir) = std::env::var_os("VAMOOSE_LIBNFS_DIR") {
        let dir = std::path::PathBuf::from(dir);
        let linker_name = dir.join("libnfs.a");
        assert!(
            linker_name.is_file(),
            "VAMOOSE_LIBNFS_DIR does not contain libnfs.a: {}",
            dir.display()
        );
        println!("cargo:rustc-link-search=native={}", dir.display());
        println!("cargo:rustc-link-lib=static=nfs");
        return;
    }

    let lib = pkg_config::Config::new()
        .atleast_version("4.0.0")
        .probe("libnfs")
        .expect("libnfs not found via pkg-config; install libnfs-dev / libnfs-devel");

    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for name in &lib.libs {
        println!("cargo:rustc-link-lib=dylib={}", name);
    }
}
