use deox_core::dupes::{find_duplicates, Duplicates};
use deox_core::report::{Category, Confidence};
use deox_core::scan::{scan, Cancel};
use std::fs;
use std::path::Path;

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn find(dir: &Path) -> Duplicates {
    let cancel = Cancel::default();
    find_duplicates(&scan(dir, &cancel, |_| {}), &cancel)
}

#[test]
fn identical_files_are_grouped() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.txt"), b"same content");
    write(&dir.path().join("sub/b.txt"), b"same content");

    let dupes = find(dir.path());

    assert_eq!(dupes.groups.len(), 1);
    assert_eq!(dupes.groups[0].files.len(), 2);
    assert_eq!(dupes.groups[0].size, 12);
}

#[test]
fn same_name_or_size_with_different_content_is_not_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("x/report.txt"), b"content one");
    write(&dir.path().join("y/report.txt"), b"content two");

    assert!(find(dir.path()).groups.is_empty());
}

#[test]
fn files_differing_only_after_the_first_64_kib_are_not_grouped() {
    let dir = tempfile::tempdir().unwrap();
    let mut a = vec![7u8; 100_000];
    let mut b = a.clone();
    a[99_999] = 1;
    b[99_999] = 2;
    write(&dir.path().join("a.bin"), &a);
    write(&dir.path().join("b.bin"), &b);
    write(&dir.path().join("c.bin"), &a);

    let dupes = find(dir.path());

    assert_eq!(dupes.groups.len(), 1);
    assert_eq!(dupes.groups[0].files.len(), 2);
}

#[test]
fn empty_files_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a"), b"");
    write(&dir.path().join("b"), b"");

    assert!(find(dir.path()).groups.is_empty());
}

#[test]
fn hard_links_are_not_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a.txt"), b"linked");
    fs::hard_link(dir.path().join("a.txt"), dir.path().join("b.txt")).unwrap();
    assert!(find(dir.path()).groups.is_empty());

    write(&dir.path().join("c.txt"), b"linked");
    let dupes = find(dir.path());
    assert_eq!(dupes.groups.len(), 1);
    assert_eq!(dupes.groups[0].files.len(), 2);
}

#[test]
fn at_least_one_copy_always_remains() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a", "b", "c"] {
        write(&dir.path().join(name), b"three copies");
    }

    let dupes = find(dir.path());
    let group = &dupes.groups[0];
    let findings = group.findings();

    assert_eq!(findings.len(), 2);
    assert!(findings.iter().all(|f| f.path != group.files[0].path));
    assert!(findings.iter().all(|f| f.category == Category::Duplicate && f.confidence == Confidence::Review));
    assert!(findings.iter().all(|f| f.size == 12 && !f.reason.is_empty()));
}

#[test]
fn unreadable_file_is_reported_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a"), b"data");
    write(&dir.path().join("b"), b"data");
    write(&dir.path().join("c"), b"data");
    let cancel = Cancel::default();
    let index = scan(dir.path(), &cancel, |_| {});
    fs::remove_file(dir.path().join("c")).unwrap();

    let dupes = find_duplicates(&index, &cancel);

    assert_eq!(dupes.errors.len(), 1);
    assert_eq!(dupes.groups[0].files.len(), 2);
}

#[test]
fn cancelled_search_returns_no_groups() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("a"), b"data");
    write(&dir.path().join("b"), b"data");
    let index = scan(dir.path(), &Cancel::default(), |_| {});
    let cancel = Cancel::default();
    cancel.cancel();

    let dupes = find_duplicates(&index, &cancel);

    assert!(dupes.cancelled);
    assert!(dupes.groups.is_empty());
}
