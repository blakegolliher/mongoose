use mongoose::workdir_lock::WorkDirLock;
use std::fs;
use std::process::Command;
use tempfile::tempdir;

#[test]
fn copy_and_sync_fail_before_changing_state_when_another_process_owns_lock() {
    let temp = tempdir().unwrap();
    let work_dir = temp.path().join("job");
    fs::create_dir_all(&work_dir).unwrap();
    let checkpoint = work_dir.join("progress.json");
    fs::write(&checkpoint, "unchanged checkpoint\n").unwrap();
    let lock = WorkDirLock::acquire(&work_dir, "test lock holder").unwrap();

    for args in [
        vec![
            "copy",
            "--src",
            "nfs://source/export",
            "--dst",
            "nfs://destination/export",
            "--work-dir",
        ],
        vec!["sync", "--work-dir"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mongoose"))
            .args(args)
            .arg(&work_dir)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("already in use"), "{stderr}");
        assert!(stderr.contains("command=test lock holder"), "{stderr}");
        assert_eq!(
            fs::read_to_string(&checkpoint).unwrap(),
            "unchanged checkpoint\n"
        );
    }

    drop(lock);
}
