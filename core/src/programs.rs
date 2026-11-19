use crate::report::{Category, Confidence, Finding};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
use winreg::RegKey;

const BUILTIN: &str = include_str!("../rules/bloatware.toml");

const UNINSTALL: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
const UNINSTALL_32BIT: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";

const APPX_PACKAGES: &str =
    r"Software\Classes\Local Settings\Software\Microsoft\Windows\CurrentVersion\AppModel\Repository\Packages";
/// AppX packages have no uninstaller of their own; this opens the Apps page in Settings.
const SETTINGS_APPS: &str = "ms-settings:appsfeatures";

/// One uninstall entry exactly as stored, before any filtering.
#[derive(Debug, Clone, Default)]
pub struct RawRecord {
    pub display_name: Option<String>,
    pub publisher: Option<String>,
    pub estimated_size_kb: Option<u32>,
    pub install_date: Option<String>,
    pub install_location: Option<String>,
    pub uninstall_string: Option<String>,
    pub system_component: bool,
    pub has_parent: bool,
}

/// Where installed-program records come from: the registry in production, a fake in tests.
pub trait ProgramSource {
    fn records(&self) -> Vec<RawRecord>;
}

pub struct WindowsRegistry;

impl ProgramSource for WindowsRegistry {
    fn records(&self) -> Vec<RawRecord> {
        let mut records = Vec::new();
        for (hive, path) in [
            (HKEY_LOCAL_MACHINE, UNINSTALL),
            (HKEY_LOCAL_MACHINE, UNINSTALL_32BIT),
            (HKEY_CURRENT_USER, UNINSTALL),
        ] {
            // A missing hive is normal (no 32-bit programs, no per-user installs).
            let Ok(root) = RegKey::predef(hive).open_subkey(path) else { continue };
            // An unreadable single entry is skipped rather than failing the whole list.
            records.extend(root.enum_keys().flatten().filter_map(|name| root.open_subkey(name).ok()).map(read_record));
        }
        records
    }
}

/// Store (UWP/AppX) packages of the current user.
pub struct AppxRegistry;

impl ProgramSource for AppxRegistry {
    fn records(&self) -> Vec<RawRecord> {
        let Ok(root) = RegKey::predef(HKEY_CURRENT_USER).open_subkey(APPX_PACKAGES) else {
            return Vec::new();
        };
        root.enum_keys()
            .flatten()
            .map(|name| {
                let folder = root.open_subkey(&name).ok().and_then(|k| k.get_value::<String, _>("PackageRootFolder").ok());
                appx_record(&name, folder.as_deref())
            })
            .collect()
    }
}

/// Builds a record from a package registry key. `full_name` is
/// `Name_Version_Architecture_ResourceId_PublisherId`; the stored display name is a
/// resource reference that cannot be read without Windows APIs, so the package name is used.
pub fn appx_record(full_name: &str, root_folder: Option<&str>) -> RawRecord {
    let mut parts = full_name.split('_');
    let name = parts.next().filter(|n| !n.is_empty());
    let resource_id = parts.nth(2).unwrap_or_default();
    RawRecord {
        display_name: name.map(str::to_string),
        install_location: root_folder.map(str::to_string),
        uninstall_string: Some(SETTINGS_APPS.to_string()),
        // Apps that ship with Windows cannot be removed by the user.
        system_component: root_folder.is_some_and(|f| f.to_lowercase().contains(r"\systemapps\")),
        // Language and scale packs belong to another package.
        has_parent: !resource_id.is_empty(),
        ..RawRecord::default()
    }
}

fn read_record(key: RegKey) -> RawRecord {
    let text = |name: &str| key.get_value::<String, _>(name).ok();
    RawRecord {
        display_name: text("DisplayName"),
        publisher: text("Publisher"),
        estimated_size_kb: key.get_value::<u32, _>("EstimatedSize").ok(),
        install_date: text("InstallDate"),
        install_location: text("InstallLocation"),
        uninstall_string: text("UninstallString"),
        system_component: key.get_value::<u32, _>("SystemComponent").is_ok_and(|v| v == 1),
        has_parent: text("ParentKeyName").is_some(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub name: String,
    pub publisher: Option<String>,
    /// Bytes; `None` when the program does not report a size.
    pub size: Option<u64>,
    /// `YYYYMMDD` as stored by the installer.
    pub install_date: Option<String>,
    pub install_location: Option<PathBuf>,
    pub uninstall: Option<String>,
}

/// Programs a user would recognize: no hidden components, no updates or patches,
/// and each program once even if it is registered in both the 32- and 64-bit views.
pub fn installed_programs(source: &impl ProgramSource) -> Vec<Program> {
    let mut seen = HashSet::new();
    let non_empty = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    source
        .records()
        .into_iter()
        .filter(|r| !r.system_component && !r.has_parent)
        .filter_map(|r| {
            let name = non_empty(r.display_name)?;
            let publisher = non_empty(r.publisher);
            let key = (name.to_lowercase(), publisher.as_ref().map(|p| p.to_lowercase()));
            seen.insert(key).then(|| Program {
                name,
                publisher,
                size: r.estimated_size_kb.map(|kb| u64::from(kb) * 1024),
                install_date: non_empty(r.install_date).filter(|d| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit())),
                install_location: non_empty(r.install_location).map(PathBuf::from),
                uninstall: non_empty(r.uninstall_string),
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct RawFile {
    rule: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    #[serde(default)]
    name_contains: Vec<String>,
    #[serde(default)]
    publisher_contains: Vec<String>,
    reason: String,
}

struct BloatRule {
    name: String,
    name_contains: Vec<String>,
    publisher_contains: Vec<String>,
    reason: String,
}

/// Programs worth a second look, matched by name or publisher substring (case-insensitive).
pub struct BloatwareList(Vec<BloatRule>);

impl BloatwareList {
    /// The list shipped with Deox.
    pub fn builtin() -> Result<Self> {
        Self::from_toml(BUILTIN)
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let raw: RawFile = toml::from_str(text).context("invalid bloatware list")?;
        raw.rule.into_iter().map(compile).collect::<Result<_>>().map(BloatwareList)
    }

    /// Findings are informational: a program is only ever removed by its own uninstaller.
    pub fn evaluate(&self, programs: &[Program]) -> Vec<Finding> {
        programs
            .iter()
            .filter_map(|p| {
                let name = p.name.to_lowercase();
                let publisher = p.publisher.as_deref().unwrap_or_default().to_lowercase();
                let rule = self.0.iter().find(|r| {
                    r.name_contains.iter().any(|s| name.contains(s))
                        || r.publisher_contains.iter().any(|s| publisher.contains(s))
                })?;
                let who = match &p.publisher {
                    Some(publisher) => format!("{} ({publisher})", p.name),
                    None => p.name.clone(),
                };
                Some(Finding {
                    path: p.install_location.clone().unwrap_or_default(),
                    size: p.size.unwrap_or(0),
                    category: Category::Bloatware,
                    confidence: Confidence::Info,
                    rule: rule.name.clone(),
                    reason: format!("{who}: {}", rule.reason),
                    uninstall: p.uninstall.clone(),
                })
            })
            .collect()
    }
}

fn compile(raw: RawRule) -> Result<BloatRule> {
    let lowered = |list: Vec<String>| list.into_iter().map(|s| s.to_lowercase()).collect::<Vec<_>>();
    let rule = BloatRule {
        name_contains: lowered(raw.name_contains),
        publisher_contains: lowered(raw.publisher_contains),
        name: raw.name,
        reason: raw.reason,
    };
    let patterns = rule.name_contains.iter().chain(&rule.publisher_contains);
    // An empty pattern matches every program.
    if patterns.clone().next().is_none() || patterns.into_iter().any(|s| s.trim().is_empty()) {
        bail!("rule '{}': needs at least one non-empty name_contains or publisher_contains", rule.name);
    }
    Ok(rule)
}
