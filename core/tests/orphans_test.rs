use deox_core::delete::delete;
use deox_core::orphans::{find_orphaned_appdata_in, running_processes, RunningProcesses, MIN_ORPHAN_SIZE};
use deox_core::programs::Program;
use deox_core::report::{Category, Confidence, Finding};
use deox_core::scan::{FileEntry, Index};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const DAY: u64 = 86_400;

fn now() -> SystemTime {
    SystemTime::now()
}

fn days_ago(days: u64) -> Option<SystemTime> {
    Some(now() - Duration::from_secs(days * DAY))
}

fn file(path: impl Into<PathBuf>, size: u64, age_days: u64) -> FileEntry {
    FileEntry { path: path.into(), size, modified: days_ago(age_days), accessed: None }
}

fn program(name: &str) -> Program {
    Program {
        name: name.into(),
        publisher: None,
        size: None,
        install_date: None,
        install_location: None,
        uninstall: None,
    }
}

#[test]
fn unmatched_old_appdata_folder_is_a_review_finding() {
    let local = PathBuf::from(r"C:\Users\Alice\AppData\Local");
    let idx = Index {
        files: vec![
            file(local.join(r"OldTool\cache.bin"), MIN_ORPHAN_SIZE, 120),
            // Bare file directly in LocalAppData is not a folder.
            file(local.join("loose.dat"), MIN_ORPHAN_SIZE * 2, 120),
        ],
        ..Index::default()
    };

    let findings = find_orphaned_appdata_in(&local, &idx, &local, &[], &RunningProcesses::default(), &[], &[], now());

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].path, local.join("OldTool"));
    assert_eq!(findings[0].size, MIN_ORPHAN_SIZE);
    assert_eq!(findings[0].category, Category::OrphanedAppData);
    assert_eq!(findings[0].confidence, Confidence::Review);
    assert!(findings[0].modified.is_none());
    assert!(findings[0].reason.contains("120 days"));
    assert!(findings[0].reason.contains("saves or settings"));
}

#[test]
fn folders_matching_installed_programs_or_install_dirs_are_skipped() {
    let local = PathBuf::from(r"C:\Users\Alice\AppData\Local");
    let pf = tempfile::tempdir().unwrap();
    fs::create_dir(pf.path().join("CoolApp")).unwrap();

    let mut by_pub = program("IntelliJ IDEA 2024.3");
    by_pub.publisher = Some("JetBrains s.r.o.".into());

    let mut by_loc = program("Some Product");
    by_loc.install_location = Some(PathBuf::from(r"C:\Apps\My Vendor Dir"));

    let mut by_uninstall = program("My Space App");
    by_uninstall.uninstall = Some(r#""C:\Program Files\Space Tool\unins000.exe" /SILENT"#.into());

    let programs = vec![
        program("7-Zip"),
        program("Microsoft Visual Studio Code"),
        by_pub,
        by_loc,
        by_uninstall,
    ];
    let idx = Index {
        files: vec![
            file(local.join(r"7zip\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"Code\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"JetBrains\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"My Vendor Dir\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"Space Tool\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"CoolApp\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"TrulyOrphaned\data.bin"), MIN_ORPHAN_SIZE, 200),
        ],
        ..Index::default()
    };

    let findings = find_orphaned_appdata_in(
        Path::new(r"C:\"),
        &idx,
        &local,
        &programs,
        &RunningProcesses::default(),
        &[pf.path().to_path_buf()],
        &[],
        now(),
    );

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].path, local.join("TrulyOrphaned"));
}

#[test]
fn active_process_recent_change_small_reserved_and_covered_folders_are_skipped() {
    let local = PathBuf::from(r"C:\Users\Alice\AppData\Local");
    let processes = RunningProcesses {
        names: HashSet::from(["portabletool".into()]),
        paths: vec![local.join(r"CustomDir\bin\worker.exe")],
    };
    let covered = Finding {
        path: local.join(r"AlreadyFlagged\cache.bin"),
        size: MIN_ORPHAN_SIZE,
        modified: days_ago(200),
        category: Category::AppCache,
        confidence: Confidence::Safe,
        rule: "Test".into(),
        reason: "test".into(),
        uninstall: None,
    };
    let idx = Index {
        files: vec![
            file(local.join(r"PortableTool\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"CustomDir\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"RecentApp\data.bin"), MIN_ORPHAN_SIZE, 10),
            file(local.join(r"TinyApp\data.bin"), MIN_ORPHAN_SIZE - 1, 200),
            file(local.join(r"Microsoft\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"Packages\data.bin"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"Deox\actions.log"), MIN_ORPHAN_SIZE, 200),
            file(local.join(r"AlreadyFlagged\cache.bin"), MIN_ORPHAN_SIZE, 200),
            // Case difference in scanned path still works.
            file(PathBuf::from(r"c:\users\alice\appdata\local\Leftover\old.bin"), MIN_ORPHAN_SIZE, 150),
        ],
        ..Index::default()
    };

    let findings = find_orphaned_appdata_in(&local, &idx, &local, &[], &processes, &[], &[covered], now());

    assert_eq!(findings.len(), 1);
    assert!(findings[0].path.ends_with("Leftover"));
}

#[test]
fn scanning_a_subfolder_inside_local_appdata_returns_no_orphan_findings() {
    let local = PathBuf::from(r"C:\Users\Alice\AppData\Local");
    let idx = Index {
        files: vec![file(local.join(r"OldTool\cache\blob.bin"), MIN_ORPHAN_SIZE, 200)],
        ..Index::default()
    };

    // Parent scan (including different casing) reports the orphan; subfolder scans do not.
    assert_eq!(
        find_orphaned_appdata_in(Path::new(r"c:\users\alice"), &idx, &local, &[], &RunningProcesses::default(), &[], &[], now()).len(),
        1
    );
    assert!(find_orphaned_appdata_in(&local.join("OldTool"), &idx, &local, &[], &RunningProcesses::default(), &[], &[], now()).is_empty());
    assert!(find_orphaned_appdata_in(&local.join(r"OldTool\cache"), &idx, &local, &[], &RunningProcesses::default(), &[], &[], now()).is_empty());
}

#[test]
fn real_process_list_can_be_read() {
    let running = running_processes();
    assert!(!running.names.is_empty());
}

#[test]
fn orphaned_folder_finding_passes_delete_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("Local");
    let orphan = local.join("DeadApp");
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("blob.bin"), vec![b'a'; MIN_ORPHAN_SIZE as usize]).unwrap();

    let idx = Index {
        files: vec![file(orphan.join("blob.bin"), MIN_ORPHAN_SIZE, 180)],
        ..Index::default()
    };
    let findings = find_orphaned_appdata_in(&local, &idx, &local, &[], &RunningProcesses::default(), &[], &[], now());
    assert_eq!(findings.len(), 1);

    let report = delete(&findings, true, &dir.path().join("actions.log")).unwrap();
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].outcome, deox_core::delete::Outcome::WouldDelete);
    assert!(Path::new(&orphan).exists());
}
