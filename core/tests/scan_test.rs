use deox_core::scan::{scan, walk, Cancel};
use std::fs;
use std::path::Path;
use std::process::Command;

fn write(path: &Path, len: usize) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, vec![b'x'; len]).unwrap();
}

#[test]
fn counts_files_and_sizes_exactly() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.txt"), 3);
    write(&dir.path().join("sub/b.txt"), 10);
    write(&dir.path().join("sub/deep/c.txt"), 0);

    let index = scan(dir.path(), &Cancel::default(), |_, _, _| {});

    assert_eq!(index.files.len(), 3);
    assert_eq!(index.files.iter().map(|f| f.size).sum::<u64>(), 13);
    assert!(index.errors.is_empty());
    assert!(!index.cancelled);
    assert!(index.files.iter().all(|f| f.modified.is_some()));
}

#[test]
fn does_not_follow_junctions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("root");
    let outside = dir.path().join("outside");
    write(&root.join("own.txt"), 1);
    write(&outside.join("secret.txt"), 1);
    let status = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(root.join("link"))
        .arg(&outside)
        .output()
        .unwrap()
        .status;
    assert!(status.success(), "junction could not be created");

    let index = scan(&root, &Cancel::default(), |_, _, _| {});

    assert_eq!(index.files.len(), 1);
    assert!(index.files[0].path.ends_with("own.txt"));
}

/// Denies listing a directory for the lifetime of the guard.
struct Denied<'a>(&'a Path);

impl<'a> Denied<'a> {
    fn new(path: &'a Path) -> Self {
        let out = Command::new("icacls").arg(path).args(["/deny", "*S-1-1-0:(R)"]).output().unwrap();
        assert!(out.status.success(), "icacls failed");
        Denied(path)
    }
}

impl Drop for Denied<'_> {
    fn drop(&mut self) {
        let _ = Command::new("icacls").arg(self.0).args(["/remove:d", "*S-1-1-0"]).output();
    }
}

#[test]
fn inaccessible_folder_is_recorded_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("ok.txt"), 1);
    let locked = dir.path().join("locked");
    write(&locked.join("hidden.txt"), 1);
    let _guard = Denied::new(&locked);

    let index = scan(dir.path(), &Cancel::default(), |_, _, _| {});

    assert_eq!(index.files.len(), 1);
    assert_eq!(index.errors.len(), 1);
    assert!(index.errors[0].path.as_deref().is_some_and(|p| p.ends_with("locked")));
}

#[test]
fn missing_root_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();

    let index = scan(&dir.path().join("nope"), &Cancel::default(), |_, _, _| {});

    assert!(index.files.is_empty());
    assert_eq!(index.errors.len(), 1);
}

#[test]
fn cancel_stops_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..200 {
        write(&dir.path().join(format!("d{}/f{i}.txt", i % 10)), 1);
    }
    let cancel = Cancel::default();

    let index = scan(dir.path(), &cancel, |seen, _, _| {
        if seen == 5 {
            cancel.cancel();
        }
    });

    assert!(index.cancelled);
    assert!(index.files.len() < 200);
}

#[test]
fn skipped_folders_are_not_entered_but_the_root_is_always_scanned() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("keep.txt"), 1);
    write(&dir.path().join("skipme/inner/a.txt"), 1);
    write(&dir.path().join("other/b.txt"), 1);
    let skip_named = |p: &Path| p.file_name().is_some_and(|n| n == "skipme");

    let index = walk(dir.path(), &Cancel::default(), |_, _, _| {}, |_| {}, skip_named);
    assert_eq!(index.files.len(), 2);
    assert!(index.files.iter().all(|f| !f.path.to_string_lossy().contains("skipme")));

    let index = walk(&dir.path().join("skipme"), &Cancel::default(), |_, _, _| {}, |_| {}, skip_named);
    assert_eq!(index.files.len(), 1);
}

#[test]
fn progress_reports_every_file_and_its_size() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..4 {
        write(&dir.path().join(format!("f{i}.txt")), 10);
    }
    let mut last = (0, 0);

    scan(dir.path(), &Cancel::default(), |seen, bytes, _| last = (seen, bytes));

    assert_eq!(last, (4, 40));
}
