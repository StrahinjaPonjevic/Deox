//! Hard-coded guard for paths that must never be offered or deleted.
//!
//! The list lives in code on purpose: a faulty rule file cannot weaken it.

use std::env;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// A path split into lowercase components. `*` in a zone matches any one component.
type Key = Vec<String>;

struct Zones {
    protected: Vec<Key>,
    /// Protected folders of the user's own files; scans still enter them so duplicates show.
    personal: Vec<Key>,
    /// Directories inside a protected zone whose *contents* may be offered.
    allowed: Vec<Key>,
}

/// Caches that live inside otherwise protected zones, relative to `%APPDATA%`.
const ELECTRON_APPS: [&str; 5] = ["Discord", "Slack", "Spotify", r"Microsoft\Teams", "Code"];
const ELECTRON_CACHES: [&str; 3] = ["Cache", "Code Cache", "GPUCache"];

/// Folder names in `%LOCALAPPDATA%` belonging to Windows, drivers, runtimes, or Deox itself.
/// Never offered as orphaned app data even if no matching uninstall entry exists.
const RESERVED_LOCAL_APPDATA: &[&str] = &[
    "amd",
    "application data",
    "cef",
    "comms",
    "connecteddevicesplatform",
    "crashdumps",
    "d3dscache",
    "deox",
    "diagnostics",
    "elevateddiagnostics",
    "fontcache",
    "intel",
    "low",
    "microsoft",
    "nvidia",
    "nvidia corporation",
    "packages",
    "peerdistrepub",
    "placeholdertilelogofolder",
    "programs",
    "publishers",
    "squirreltemp",
    "temp",
    "toastnotificationmanagercompat",
    "virtualstore",
];

/// Returns `true` if `name` is a reserved `%LOCALAPPDATA%` folder (Windows/driver/runtime/Deox)
/// or hidden/temporary (`.` or `~` prefix) that must never be offered as orphaned app data.
pub fn is_reserved_appdata_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.is_empty()
        || lower.starts_with('.')
        || lower.starts_with('~')
        || RESERVED_LOCAL_APPDATA.contains(&lower.as_str())
}

/// Returns `true` if the path must never be offered for deletion or deleted.
pub fn is_protected(path: &Path) -> bool {
    let key = key_of(&resolve(path));
    // A bare drive or share root is `[prefix, root]`.
    if key.len() <= 2 {
        return true;
    }
    let zones = zones();
    // An allowed directory exposes what is below it, never the directory itself.
    if zones.allowed.iter().any(|a| key.len() > a.len() && starts_with(&key, a)) {
        return false;
    }
    zones.protected.iter().chain(&zones.personal).any(|z| starts_with(&key, z))
}

/// Returns `true` if a scan can skip this directory: it is protected, holds no personal
/// files whose duplicates are shown, and has no allowed cache below it.
/// Cheaper than [`is_protected`] (no canonicalization) and errs towards `false`.
pub fn is_skippable_dir(path: &Path) -> bool {
    let key = key_of(path);
    if key.len() <= 2 {
        return false;
    }
    let zones = zones();
    zones.protected.iter().any(|z| starts_with(&key, z))
        && !zones.personal.iter().any(|z| starts_with(&key, z))
        // Related to an allowed zone, as a parent or as a subfolder; `*` matches one component.
        && !zones.allowed.iter().any(|a| a.iter().zip(&key).all(|(z, k)| z == "*" || z == k))
}

fn starts_with(key: &[String], zone: &[String]) -> bool {
    key.len() >= zone.len() && zone.iter().zip(key).all(|(z, k)| z == "*" || z == k)
}

fn zones() -> &'static Zones {
    static ZONES: OnceLock<Zones> = OnceLock::new();
    ZONES.get_or_init(|| {
        let var = |name: &str, default: &str| {
            env::var_os(name).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(default))
        };
        let windows = var("SystemRoot", r"C:\Windows");
        let users = var("USERPROFILE", r"C:\Users\default")
            .parent()
            .map_or_else(|| PathBuf::from(r"C:\Users"), Path::to_path_buf)
            .join("*");

        let mut protected = vec![
            key_of(&windows),
            key_of(&var("ProgramFiles", r"C:\Program Files")),
            key_of(&var("ProgramFiles(x86)", r"C:\Program Files (x86)")),
            key_of(&var("ProgramW6432", r"C:\Program Files")),
            key_of(&var("ProgramData", r"C:\ProgramData")),
            key_of(&users.join(r"AppData\Roaming")),
        ];
        let mut personal = Vec::new();
        for dir in ["Documents", "Pictures", "Desktop", "Videos"] {
            personal.push(key_of(&users.join(dir)));
        }
        // Credentials and the registry hive in the profile are never cleanup material.
        for item in [".ssh", ".gnupg", ".aws", "NTUSER.DAT"] {
            protected.push(key_of(&users.join(item)));
        }
        // Known folders may be redirected into OneDrive, so guard all of it.
        if let Some(onedrive) = env::var_os("OneDrive") {
            personal.push(key_of(Path::new(&onedrive)));
        }
        // Never offer or delete Deox's own local data folder (actions.log).
        protected.push(key_of(&users.join(r"AppData\Local\Deox")));
        if let Some(local) = env::var_os("LOCALAPPDATA") {
            protected.push(key_of(&PathBuf::from(local).join("Deox")));
        }

        let mut allowed = vec![
            key_of(&windows.join("Temp")),
            key_of(&windows.join(r"SoftwareDistribution\Download")),
            key_of(&windows.join(r"SoftwareDistribution\DeliveryOptimization")),
        ];
        let roaming = users.join(r"AppData\Roaming");
        for app in ELECTRON_APPS {
            for cache in ELECTRON_CACHES {
                allowed.push(key_of(&roaming.join(app).join(cache)));
            }
        }
        Zones { protected, personal, allowed }
    })
}

fn key_of(path: &Path) -> Key {
    path.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
}

/// Makes the path absolute, collapses `.`/`..` and follows links where it can.
/// Works for paths that do not exist (yet) by resolving the deepest existing ancestor.
pub(crate) fn resolve(path: &Path) -> PathBuf {
    if let Ok(real) = path.canonicalize() {
        return strip_verbatim(real);
    }
    let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut clean = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            // `..` at the root stays at the root, as the OS does.
            Component::ParentDir => {
                if clean.parent().is_some() {
                    clean.pop();
                }
            }
            other => clean.push(other),
        }
    }
    // shortcut: for missing paths `..` is collapsed before links are followed; delete only
    // acts on existing, canonicalized paths, so upgrade only if offered paths may not exist.
    let mut tail = Vec::new();
    let mut base = clean.as_path();
    while let Some(parent) = base.parent() {
        if let Ok(real) = base.canonicalize() {
            let mut out = strip_verbatim(real);
            out.extend(tail.iter().rev());
            return out;
        }
        if let Some(name) = base.file_name() {
            tail.push(name.to_owned());
        }
        base = parent;
    }
    clean
}

/// `\\?\C:\x` becomes `C:\x`; other verbatim forms (UNC) are left as they are.
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}
