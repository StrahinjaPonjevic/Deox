use deox_core::delete::{delete, launch_uninstaller, Outcome};
use deox_core::report::{Category, Confidence, Finding, Report};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn write(path: &Path, content: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

/// A finding that matches the file as it is right now.
fn finding(path: &Path) -> Finding {
    let meta = fs::metadata(path).unwrap();
    Finding {
        path: path.to_path_buf(),
        size: meta.len(),
        modified: meta.modified().ok(),
        category: Category::Temp,
        confidence: Confidence::Safe,
        rule: "test".into(),
        reason: "test".into(),
        uninstall: None,
    }
}

fn run(findings: &[Finding], dry_run: bool, log: &Path) -> Vec<Outcome> {
    delete(findings, dry_run, log).unwrap().entries.into_iter().map(|e| e.outcome).collect()
}

/// The Recycle Bin reports long paths, while a temp folder may be given by its short 8.3 name.
fn long(path: &Path) -> PathBuf {
    let canonical = fs::canonicalize(path).unwrap();
    PathBuf::from(canonical.to_string_lossy().trim_start_matches(r"\\?\"))
}

/// Removes this test's files from the Recycle Bin again, and nothing else.
struct BinCleanup<'a>(&'a Path);

impl Drop for BinCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(items) = trash::os_limited::list() {
            let dir = long(self.0);
            let ours: Vec<_> = items.into_iter().filter(|i| i.original_parent.starts_with(&dir)).collect();
            if let Err(e) = trash::os_limited::purge_all(ours) {
                eprintln!("could not clean the Recycle Bin: {e}");
            }
        }
    }
}

fn in_recycle_bin(dir: &Path, name: &str) -> bool {
    trash::os_limited::list()
        .unwrap()
        .iter()
        .any(|i| i.original_parent.starts_with(long(dir)) && i.name.to_string_lossy() == name)
}

#[test]
fn file_goes_to_the_recycle_bin() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = BinCleanup(dir.path());
    let file = dir.path().join("old.tmp");
    write(&file, b"data");
    let findings = [finding(&file)];

    let outcomes = run(&findings, false, &dir.path().join("actions.log"));

    assert_eq!(outcomes, vec![Outcome::Deleted]);
    assert!(!file.exists());
    assert!(in_recycle_bin(dir.path(), "old.tmp"));
}

#[test]
fn folder_goes_to_the_recycle_bin() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = BinCleanup(dir.path());
    let folder = dir.path().join("node_modules");
    write(&folder.join("a/x.js"), b"12345");
    write(&folder.join("y.js"), b"123");
    let mut f = finding(&folder);
    f.size = 8;
    f.modified = None;
    f.category = Category::DevArtifact;

    let outcomes = run(&[f], false, &dir.path().join("actions.log"));

    assert_eq!(outcomes, vec![Outcome::Deleted]);
    assert!(!folder.exists());
}

#[test]
fn folder_that_changed_since_the_scan_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("node_modules");
    write(&folder.join("x.js"), b"12345");
    let mut f = finding(&folder);
    f.size = 5;
    f.modified = None;
    write(&folder.join("new.js"), b"added later");

    assert_eq!(run(&[f], false, &dir.path().join("actions.log")), vec![Outcome::Changed]);
    assert!(folder.join("x.js").exists());
}

#[test]
fn protected_path_is_refused_even_when_listed() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("placeholder"), b"x");
    let mut f = finding(&dir.path().join("placeholder"));
    f.path = PathBuf::from(r"C:\Windows\System32\deox-test-nonexistent.dll");

    assert_eq!(run(&[f], false, &dir.path().join("actions.log")), vec![Outcome::Protected]);
}

#[test]
fn file_changed_after_the_scan_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.tmp");
    write(&file, b"before");
    let f = finding(&file);
    write(&file, b"changed after the scan");

    assert_eq!(run(&[f], false, &dir.path().join("actions.log")), vec![Outcome::Changed]);
    assert!(file.exists());
}

#[test]
fn missing_file_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("gone.tmp");
    write(&file, b"x");
    let f = finding(&file);
    fs::remove_file(&file).unwrap();

    assert_eq!(run(&[f], false, &dir.path().join("actions.log")), vec![Outcome::Missing]);
}

#[test]
fn junction_is_never_followed_or_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    write(&target.join("keep.txt"), b"x");
    let link = dir.path().join("link");
    let ok = Command::new("cmd").args(["/C", "mklink", "/J"]).arg(&link).arg(&target).output().unwrap().status;
    assert!(ok.success());
    let mut f = finding(&target);
    f.path = link;

    assert_eq!(run(&[f], false, &dir.path().join("actions.log")), vec![Outcome::Changed]);
    assert!(target.join("keep.txt").exists());
}

#[test]
fn dry_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.tmp");
    write(&file, b"data");
    let log = dir.path().join("actions.log");

    let outcomes = run(&[finding(&file)], true, &log);

    assert_eq!(outcomes, vec![Outcome::WouldDelete]);
    assert!(file.exists());
    assert!(!log.exists());
}

#[test]
fn installed_programs_are_never_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("program.exe");
    write(&file, b"x");
    let mut f = finding(&file);
    f.category = Category::Bloatware;
    f.confidence = Confidence::Info;

    assert_eq!(run(&[f], false, &dir.path().join("actions.log")), vec![Outcome::NotDeletable]);
    assert!(file.exists());
}

#[test]
fn every_action_is_logged_with_path_size_and_time() {
    let dir = tempfile::tempdir().unwrap();
    let _cleanup = BinCleanup(dir.path());
    let file = dir.path().join("logged.tmp");
    write(&file, b"12345");
    let missing = dir.path().join("missing.tmp");
    write(&missing, b"x");
    let findings = [finding(&file), finding(&missing)];
    fs::remove_file(&missing).unwrap();
    let log = dir.path().join("logs/actions.log");

    delete(&findings, false, &log).unwrap();
    let text = fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = text.lines().collect();

    assert_eq!(lines.len(), 2);
    let fields: Vec<&str> = lines[0].split('\t').collect();
    assert_eq!(fields.len(), 4);
    assert!(fields[0].contains('T') && fields[0].ends_with('Z'), "{}", fields[0]);
    assert_eq!(fields[1], "DELETED");
    assert_eq!(fields[2], "5");
    assert_eq!(fields[3], file.to_string_lossy());
    assert!(lines[1].contains("SKIPPED") && lines[1].contains("missing.tmp"));
}

#[test]
fn nothing_is_deleted_if_the_log_cannot_be_written() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.tmp");
    write(&file, b"data");
    let blocker = dir.path().join("blocker");
    write(&blocker, b"a file, not a folder");

    let result = delete(&[finding(&file)], false, &blocker.join("actions.log"));

    assert!(result.is_err());
    assert!(file.exists());
}

#[test]
fn only_programs_can_launch_an_uninstaller() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a.tmp");
    write(&file, b"x");
    let mut plain = finding(&file);
    plain.uninstall = Some("cmd /C exit".into());
    let mut no_command = finding(&file);
    no_command.category = Category::Bloatware;
    no_command.confidence = Confidence::Info;

    assert!(launch_uninstaller(&plain).is_err());
    assert!(launch_uninstaller(&no_command).is_err());
}

#[test]
fn report_groups_by_category_and_sums_sizes() {
    let make = |category, size| Finding { category, size, ..finding_stub() };
    let report = Report::new(vec![
        make(Category::Temp, 100),
        make(Category::AppCache, 5000),
        make(Category::Temp, 300),
        make(Category::DevArtifact, 20),
    ]);

    let groups = report.groups();

    assert_eq!(report.total_size(), 5420);
    assert_eq!(groups.len(), 3);
    assert_eq!(groups[0].category, Category::AppCache);
    assert_eq!(groups[1].category, Category::Temp);
    assert_eq!(groups[1].total_size, 400);
    assert_eq!(groups[1].findings.iter().map(|f| f.size).collect::<Vec<_>>(), vec![300, 100]);
}

fn finding_stub() -> Finding {
    Finding {
        path: PathBuf::from("x"),
        size: 0,
        modified: None,
        category: Category::Temp,
        confidence: Confidence::Safe,
        rule: "r".into(),
        reason: "r".into(),
        uninstall: None,
    }
}
