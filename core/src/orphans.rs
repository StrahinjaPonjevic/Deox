//! Conservative detection of leftover `%LOCALAPPDATA%` folders from uninstalled programs.
//!
//! Because folder names often differ from registry `DisplayName` values and portable apps have
//! no registry entry at all, findings are always emitted as [`Confidence::Review`] (never
//! preselected) and pass multiple exclusion checks before being reported.

use crate::programs::Program;
use crate::protect::{is_protected, is_reserved_appdata_dir, resolve};
use crate::report::{Category, Confidence, Finding};
use crate::scan::Index;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Minimum folder size worth showing to the user (1 MiB).
pub const MIN_ORPHAN_SIZE: u64 = 1024 * 1024;

/// Nothing inside the folder may have changed within this many days.
pub const MIN_ORPHAN_AGE_DAYS: u64 = 90;

const SECS_PER_DAY: u64 = 86_400;

/// Words too generic on their own to prove a folder belongs to a multi-word program name.
const GENERIC_WORDS: &[&str] = &[
    "app", "apps", "client", "co", "corp", "corporation", "data", "desktop", "edition", "files",
    "for", "gmbh", "helper", "inc", "installer", "labs", "llc", "ltd", "microsoft", "pro",
    "runtime", "setup", "software", "sro", "studio", "systems", "technologies", "the", "tool",
    "tools", "update", "updater", "windows", "x64", "x86",
];

/// Currently running executables, used to avoid flagging active portable or background apps.
#[derive(Default)]
pub struct RunningProcesses {
    /// Lowercased executable stems without `.exe` (e.g. `"code"`, `"obsidian"`).
    pub names: HashSet<String>,
    /// Full executable paths when readable from the OS.
    pub paths: Vec<PathBuf>,
}

/// Returns the executables currently running on the host.
pub fn running_processes() -> RunningProcesses {
    #[cfg(windows)]
    {
        win_proc::running()
    }
    #[cfg(not(windows))]
    {
        RunningProcesses::default()
    }
}

/// Finds likely orphaned `%LOCALAPPDATA%` folders within the scanned `root` and `index`.
/// Always returns [`Confidence::Review`] findings and never crosses outside `index`.
pub fn find_orphaned_appdata(
    root: &Path,
    index: &Index,
    programs: &[Program],
    existing_findings: &[Finding],
    now: SystemTime,
) -> Vec<Finding> {
    let Some(local) = env::var_os("LOCALAPPDATA").map(PathBuf::from) else {
        return Vec::new();
    };
    let install_roots: Vec<PathBuf> = [
        env::var_os("ProgramFiles").map(PathBuf::from),
        env::var_os("ProgramFiles(x86)").map(PathBuf::from),
        env::var_os("ProgramW6432").map(PathBuf::from),
        Some(local.join("Programs")),
    ]
    .into_iter()
    .flatten()
    .collect();
    find_orphaned_appdata_in(
        &resolve(root),
        index,
        &resolve(&local),
        programs,
        &running_processes(),
        &install_roots,
        existing_findings,
        now,
    )
}

/// Testable core of [`find_orphaned_appdata`] with explicit environment inputs.
#[allow(clippy::too_many_arguments)]
pub fn find_orphaned_appdata_in(
    root: &Path,
    index: &Index,
    local_appdata: &Path,
    programs: &[Program],
    processes: &RunningProcesses,
    install_roots: &[PathBuf],
    existing_findings: &[Finding],
    now: SystemTime,
) -> Vec<Finding> {
    let base_parts = key_parts(local_appdata);
    // Only run when the scan covers all of `%LOCALAPPDATA%`; a scan of a subfolder inside it
    // would otherwise compute size and age from a partial slice of the parent folder.
    if base_parts.is_empty() || !base_parts.starts_with(&key_parts(root)) {
        return Vec::new();
    }

    // Resolved once per folder, since all of its files share the answer. The joined `_` stands in
    // for a file name: `top_folder_under` takes the path of a file.
    let tops: Vec<Option<(String, PathBuf)>> = index
        .dir_paths()
        .iter()
        .map(|dir| top_folder_under(&dir.join("_"), &base_parts).map(|(name, path)| (name.to_lowercase(), path)))
        .collect();
    let mut folders: HashMap<&str, (&Path, u64, Option<SystemTime>)> = HashMap::new();
    for file in &index.files {
        let Some((key, path)) = &tops[file.dir as usize] else { continue };
        let entry = folders.entry(key).or_insert((path, 0, None));
        entry.1 += file.size;
        entry.2 = entry.2.max(file.modified);
    }
    if folders.is_empty() {
        return Vec::new();
    }

    let covered: HashSet<String> = existing_findings
        .iter()
        .filter_map(|f| top_folder_under(&f.path, &base_parts).map(|(n, _)| n.to_lowercase()))
        .collect();
    let installed_dirs = collect_install_dirs(install_roots);
    let min_age = Duration::from_secs(MIN_ORPHAN_AGE_DAYS * SECS_PER_DAY);

    let mut findings = Vec::new();
    for (key, (path, size, newest)) in folders {
        if size < MIN_ORPHAN_SIZE || covered.contains(key) {
            continue;
        }
        let Some(age) = newest.and_then(|m| now.duration_since(m).ok()) else { continue };
        if age < min_age {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if is_reserved_appdata_dir(name) || is_protected(path) {
            continue;
        }
        if matches_installed(name, programs, &installed_dirs) || matches_running(name, path, processes) {
            continue;
        }

        let days = age.as_secs() / SECS_PER_DAY;
        findings.push(Finding {
            path: path.to_path_buf(),
            size,
            modified: None,
            category: Category::OrphanedAppData,
            confidence: Confidence::Review,
            rule: "Orphaned app data".to_string(),
            reason: format!(
                "No matching installed program or running process found; nothing inside changed in {days} days. Check before removing — it may hold saves or settings, and portable apps may still use it."
            ),
            uninstall: None,
        });
    }
    findings
}

fn key_parts(path: &Path) -> Vec<String> {
    path.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
}

fn eq_part(comp: Component, lower_part: &str) -> bool {
    match comp.as_os_str().to_str() {
        Some(s) if s.is_ascii() => s.eq_ignore_ascii_case(lower_part),
        _ => comp.as_os_str().to_string_lossy().to_lowercase() == lower_part,
    }
}

/// If `path` lies inside `<base>/<child>/...`, returns `(child_name, <base_as_in_path>/<child>)`.
/// Returns `None` for files placed directly in `<base>`.
fn top_folder_under(path: &Path, base_parts: &[String]) -> Option<(String, PathBuf)> {
    let mut comps = path.components();
    let mut folder_path = PathBuf::new();
    for base in base_parts {
        let comp = comps.next()?;
        if !eq_part(comp, base) {
            return None;
        }
        folder_path.push(comp);
    }
    let child = match comps.next()? {
        Component::Normal(name) => name,
        _ => return None,
    };
    // Require at least one component below `<child>` so a bare file in `<base>` is ignored.
    comps.next()?;
    folder_path.push(child);
    Some((child.to_string_lossy().into_owned(), folder_path))
}

fn collect_install_dirs(roots: &[PathBuf]) -> HashSet<String> {
    let mut dirs = HashSet::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                let n = norm(&entry.file_name().to_string_lossy());
                if !n.is_empty() {
                    dirs.insert(n);
                }
            }
        }
    }
    dirs
}

fn matches_installed(folder: &str, programs: &[Program], installed_dirs: &HashSet<String>) -> bool {
    let folder_norm = norm(folder);
    if folder_norm.is_empty() {
        return true;
    }
    if installed_dirs.contains(&folder_norm) {
        return true;
    }
    let folder_lower = folder.to_lowercase();
    programs.iter().any(|p| program_matches(folder_lower.as_str(), &folder_norm, p))
}

fn program_matches(folder_lower: &str, folder_norm: &str, p: &Program) -> bool {
    if text_matches(folder_lower, folder_norm, &p.name) {
        return true;
    }
    if p.publisher.as_deref().is_some_and(|pub_name| text_matches(folder_lower, folder_norm, pub_name)) {
        return true;
    }
    if let Some(loc) = &p.install_location {
        if loc.components().any(|c| matches!(c, Component::Normal(s) if norm(&s.to_string_lossy()) == folder_norm)) {
            return true;
        }
    }
    if let Some(uninstall) = &p.uninstall {
        if uninstall.split(['\\', '/', '"', '\'']).any(|seg| !seg.trim().is_empty() && norm(seg) == folder_norm) {
            return true;
        }
    }
    false
}

fn text_matches(folder_lower: &str, folder_norm: &str, text: &str) -> bool {
    let text_norm = norm(text);
    if text_norm == folder_norm {
        return true;
    }
    if folder_norm.len() >= 4 && text_norm.len() >= 4 && (text_norm.contains(folder_norm) || folder_norm.contains(&text_norm)) {
        return true;
    }
    // Single word match for cases like `Code` vs `Microsoft Visual Studio Code`.
    folder_lower.len() >= 3
        && text
            .split(|c: char| !c.is_alphanumeric())
            .any(|w| {
                let w = w.to_lowercase();
                w.len() >= 3 && !GENERIC_WORDS.contains(&w.as_str()) && w == folder_lower
            })
}

fn matches_running(folder: &str, folder_path: &Path, processes: &RunningProcesses) -> bool {
    let folder_norm = norm(folder);
    if folder_norm.len() >= 3 && processes.names.iter().any(|n| norm(n) == folder_norm) {
        return true;
    }
    let folder_parts = key_parts(folder_path);
    processes.paths.iter().any(|p| {
        let p_parts = key_parts(p);
        p_parts.len() > folder_parts.len() && p_parts[..folder_parts.len()] == folder_parts[..]
    })
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

#[cfg(windows)]
mod win_proc {
    use super::RunningProcesses;
    use crate::protect::resolve;
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const MAX_PATH: usize = 260;

    #[repr(C)]
    struct ProcessEntry32W {
        dw_size: u32,
        cnt_usage: u32,
        th32_process_id: u32,
        th32_default_heap_id: usize,
        th32_module_id: u32,
        cnt_threads: u32,
        th32_parent_process_id: u32,
        pc_pri_class_base: i32,
        dw_flags: u32,
        sz_exe_file: [u16; MAX_PATH],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(dwFlags: u32, th32ProcessID: u32) -> isize;
        fn Process32FirstW(hSnapshot: isize, lppe: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(hSnapshot: isize, lppe: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> isize;
        fn QueryFullProcessImageNameW(hProcess: isize, dwFlags: u32, lpExeName: *mut u16, lpdwSize: *mut u32) -> i32;
        fn CloseHandle(hObject: isize) -> i32;
    }

    pub fn running() -> RunningProcesses {
        let mut names = HashSet::new();
        let mut paths = Vec::new();
        // SAFETY: standard Win32 snapshot handle checked against invalid values and closed below.
        let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snap == -1 || snap == 0 {
            return RunningProcesses { names, paths };
        }

        let mut entry = ProcessEntry32W {
            dw_size: std::mem::size_of::<ProcessEntry32W>() as u32,
            cnt_usage: 0,
            th32_process_id: 0,
            th32_default_heap_id: 0,
            th32_module_id: 0,
            cnt_threads: 0,
            th32_parent_process_id: 0,
            pc_pri_class_base: 0,
            dw_flags: 0,
            sz_exe_file: [0; MAX_PATH],
        };
        // SAFETY: `entry.dw_size` is set to the struct's exact size before calling `Process32FirstW`.
        let mut ok = unsafe { Process32FirstW(snap, &mut entry) };
        while ok != 0 {
            let end = entry.sz_exe_file.iter().position(|&c| c == 0).unwrap_or(MAX_PATH);
            let exe = OsString::from_wide(&entry.sz_exe_file[..end]);
            if let Some(stem) = Path::new(&exe).file_stem().and_then(|s| s.to_str()) {
                names.insert(stem.to_lowercase());
            }
            if entry.th32_process_id != 0 {
                if let Some(full) = process_path(entry.th32_process_id) {
                    paths.push(full);
                }
            }
            // SAFETY: `snap` and `entry` remain valid for the duration of enumeration.
            ok = unsafe { Process32NextW(snap, &mut entry) };
        }
        // SAFETY: `snap` is a valid snapshot handle opened above.
        unsafe { CloseHandle(snap) };
        RunningProcesses { names, paths }
    }

    fn process_path(pid: u32) -> Option<PathBuf> {
        // SAFETY: `OpenProcess` returns 0 on failure; valid handles are closed before returning.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle == 0 {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        // SAFETY: `buf` is writable for `len` UTF-16 code units and `handle` is open.
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buf.as_mut_ptr(), &mut len) };
        unsafe { CloseHandle(handle) };
        if ok == 0 || len == 0 {
            return None;
        }
        let os = OsString::from_wide(&buf[..len as usize]);
        Some(resolve(Path::new(&os)))
    }
}
