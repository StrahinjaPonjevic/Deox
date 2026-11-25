use deox_core::protect::{is_protected, is_reserved_appdata_dir, is_skippable_dir};
use std::path::{Path, PathBuf};

fn env(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set")))
}

fn p(s: &str) -> bool {
    is_protected(Path::new(s))
}

#[test]
fn system_and_program_dirs_are_protected() {
    assert!(p(r"C:\Windows\System32\x.dll"));
    assert!(p(r"C:\Windows"));
    assert!(p(r"C:\Program Files\App"));
    assert!(p(r"C:\Program Files (x86)\App\a.dll"));
    assert!(p(r"C:\ProgramData\Vendor\data.bin"));
}

#[test]
fn drive_root_is_protected() {
    assert!(p(r"C:\"));
    assert!(p(r"D:\"));
}

#[test]
fn user_data_folders_are_protected() {
    let home = env("USERPROFILE");
    for dir in ["Documents", "Pictures", "Desktop", "Videos"] {
        assert!(is_protected(&home.join(dir).join("a.docx")), "{dir}");
        assert!(is_protected(&home.join(dir)), "{dir}");
    }
    for item in [r".ssh\id_rsa", r".gnupg\key", r".aws\credentials", "NTUSER.DAT"] {
        assert!(is_protected(&home.join(item)), "{item}");
    }
}

#[test]
fn temp_files_are_not_protected() {
    assert!(!is_protected(&std::env::temp_dir().join("foo.tmp")));
    assert!(!p(r"C:\Windows\Temp\x"));
}

#[test]
fn allowlisted_cache_dirs_expose_only_their_contents() {
    assert!(!p(r"C:\Windows\SoftwareDistribution\Download\a.cab"));
    assert!(!p(r"C:\Windows\SoftwareDistribution\DeliveryOptimization\a"));
    assert!(p(r"C:\Windows\Temp"));
    assert!(p(r"C:\Windows\SoftwareDistribution"));
    assert!(p(r"C:\Windows\SoftwareDistribution\DataStore\x.edb"));
}

#[test]
fn scan_skips_protected_dirs_without_allowed_caches_or_personal_files() {
    let roaming = env("APPDATA");
    let home = env("USERPROFILE");
    for dir in [
        PathBuf::from(r"C:\Windows\System32"),
        PathBuf::from(r"C:\Program Files\App"),
        PathBuf::from(r"C:\ProgramData\Vendor"),
        roaming.join("SomeGame"),
        roaming.join(r"Discord\Local Storage"),
        home.join(".ssh"),
    ] {
        assert!(is_skippable_dir(&dir), "{dir:?}");
    }
    for dir in [
        PathBuf::from(r"C:\Windows"),
        PathBuf::from(r"C:\Windows\Temp"),
        PathBuf::from(r"C:\Windows\SoftwareDistribution"),
        roaming.clone(),
        roaming.join("Discord"),
        roaming.join(r"Discord\Cache"),
        roaming.join(r"Discord\Cache\Cache_Data"),
        roaming.join(r"Code\Code Cache\js"),
        PathBuf::from(r"C:\Windows\Temp\sub"),
        PathBuf::from(r"C:\Windows\SoftwareDistribution\Download\a\b"),
        home.join("Documents"),
        home.join(r"Pictures\Trip"),
        std::env::temp_dir().join("work"),
    ] {
        assert!(!is_skippable_dir(&dir), "{dir:?}");
    }
}

#[test]
fn roaming_is_protected_except_electron_caches() {
    let roaming = env("APPDATA");
    assert!(!is_protected(&roaming.join(r"Discord\Cache\f_000001")));
    assert!(!is_protected(&roaming.join(r"Code\GPUCache\data_0")));
    assert!(!is_protected(&roaming.join(r"Microsoft\Teams\Code Cache\js\a")));
    assert!(is_protected(&roaming.join(r"Discord\Local Storage\leveldb\a")));
    assert!(is_protected(&roaming.join(r"Code\User\settings.json")));
    assert!(is_protected(&roaming.join("SomeGame").join("save.dat")));
}

#[test]
fn paths_are_normalized_before_the_check() {
    assert!(p(r"C:\Windows\Temp\..\System32\x.dll"));
    assert!(p(r"c:\WINDOWS\system32\X.DLL"));
    assert!(p(r"C:\Windows\.\System32"));
    assert!(p(r"C:\Windows\Temp\..\.."));
}

#[test]
fn deox_data_and_reserved_appdata_dirs_are_guarded() {
    let local = env("LOCALAPPDATA");
    assert!(is_protected(&local.join(r"Deox\actions.log")));
    for name in ["Microsoft", "Packages", "Programs", "Temp", "Deox", ".hidden", "~tmp"] {
        assert!(is_reserved_appdata_dir(name), "{name}");
    }
    assert!(!is_reserved_appdata_dir("OldUninstalledApp"));
}
