//! Times each stage of an analysis on a real folder. Reads only; nothing is changed.
//!
//! cargo run --release -p deox-core --example bench_scan -- <path> [runs] [--dupes]
//!
//! `--dupes` also times the duplicate search, which reads file contents (and may download
//! cloud-only files).

use deox_core::dupes::find_duplicates;
use deox_core::orphans::find_orphaned_appdata;
use deox_core::programs::{installed_programs, AppxRegistry, WindowsRegistry};
use deox_core::rules::Rules;
use deox_core::scan::{scan, Cancel, FileEntry, Index};
use deox_core::treemap::{FolderTree, LiveTree};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

/// The same lower limit the application uses.
const MIN_DUPLICATE_SIZE: u64 = 64 * 1024;

fn timed<T>(work: impl FnOnce() -> T) -> (T, f64) {
    let start = Instant::now();
    let value = work();
    (value, start.elapsed().as_secs_f64())
}

fn total_size(index: &Index) -> u64 {
    index.files.iter().map(|f| f.size).sum()
}

/// Scans the way the application does: every file also updates the shared live tree.
fn scan_live(root: &Path) -> (Index, f64) {
    let live = Mutex::new(LiveTree::new(root));
    let (index, seconds) = timed(|| {
        scan(root, &Cancel::default(), |_, _, file| live.lock().unwrap().add(&file.path, file.size))
    });
    let live = live.into_inner().unwrap();
    assert_eq!(live.tree().node(FolderTree::ROOT).size, total_size(&index), "live tree lost files");
    (index, seconds)
}

/// Heap held by the file list; error entries are not counted.
fn index_bytes(index: &Index) -> usize {
    index.files.capacity() * size_of::<FileEntry>() + index.files.iter().map(|f| f.path.capacity()).sum::<usize>()
}

fn main() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let dupes = args.iter().any(|a| a == "--dupes");
    args.retain(|a| a != "--dupes");
    let Some(root) = args.first().map(PathBuf::from) else {
        anyhow::bail!("usage: bench_scan <path> [runs] [--dupes]");
    };
    anyhow::ensure!(root.is_dir(), "{} is not a folder", root.display());
    let runs: u32 = args.get(1).map(|n| n.parse()).transpose()?.unwrap_or(3);
    if cfg!(debug_assertions) {
        eprintln!("warning: debug build; use --release for numbers worth comparing");
    }

    // Reported apart from the runs: the disk cache is usually cold here.
    let (index, seconds) = scan_live(&root);
    println!("first scan: {seconds:.2} s");
    let folders: HashSet<&Path> =
        index.files.iter().flat_map(|f| f.path.ancestors().skip(1).take_while(|a| a.starts_with(&root))).collect();
    println!(
        "{} files in {} folders (folders holding no files are not counted), {:.2} GiB, {} unreadable",
        index.files.len(),
        folders.len(),
        total_size(&index) as f64 / (1u64 << 30) as f64,
        index.errors.len(),
    );
    println!(
        "index memory: about {:.1} MiB ({} B per file)",
        index_bytes(&index) as f64 / (1u64 << 20) as f64,
        index_bytes(&index) / index.files.len().max(1),
    );
    drop(folders);
    drop(index);

    let rules = Rules::builtin()?;
    for run in 1..=runs {
        println!("run {run}");
        let (index, plain) = timed(|| scan(&root, &Cancel::default(), |_, _, _| {}));
        println!("  scan, no live tree    {plain:8.2} s");
        drop(index);
        let (index, live) = scan_live(&root);
        println!("  scan, as in the app   {live:8.2} s");
        let (mut findings, seconds) = timed(|| rules.evaluate(&index, SystemTime::now()));
        println!("  cleanup rules         {seconds:8.2} s");
        if dupes {
            let (found, seconds) =
                timed(|| find_duplicates(&index, &Cancel::default(), MIN_DUPLICATE_SIZE, &|_, _, _| {}));
            println!("  duplicates            {seconds:8.2} s");
            findings.extend(found.groups.iter().flat_map(|g| g.findings()));
        }
        let (programs, seconds) = timed(|| {
            let mut programs = installed_programs(&WindowsRegistry);
            programs.extend(installed_programs(&AppxRegistry));
            programs
        });
        println!("  installed programs    {seconds:8.2} s");
        let (orphans, seconds) =
            timed(|| find_orphaned_appdata(&root, &index, &programs, &findings, SystemTime::now()));
        println!("  orphaned app data     {seconds:8.2} s");
        findings.extend(orphans);
        let (_tree, seconds) = timed(|| FolderTree::build(&root, &index, &findings));
        println!("  folder tree           {seconds:8.2} s");
    }
    Ok(())
}
