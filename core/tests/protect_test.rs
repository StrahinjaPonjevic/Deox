use deox_core::protect::is_protected;
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
