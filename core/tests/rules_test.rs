use deox_core::report::{Category, Confidence};
use deox_core::rules::Rules;
use deox_core::scan::{Index, IndexBuilder};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const DAY: u64 = 86_400;

fn now() -> SystemTime {
    SystemTime::now()
}

fn days_ago(days: u64) -> Option<SystemTime> {
    Some(now() - Duration::from_secs(days * DAY))
}

/// Path, size and age in days.
type File = (PathBuf, u64, u64);

fn file(path: impl Into<PathBuf>, size: u64, age_days: u64) -> File {
    (path.into(), size, age_days)
}

fn index(files: Vec<File>) -> Index {
    let mut builder = IndexBuilder::new(r"C:\");
    for (path, size, age_days) in files {
        builder.file(&path, size, days_ago(age_days));
    }
    builder.finish()
}

fn env(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set")))
}

/// The scanner reports long paths, while `%TEMP%` may hold a short 8.3 name.
fn real_temp() -> PathBuf {
    let canonical = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    PathBuf::from(canonical.to_string_lossy().trim_start_matches(r"\\?\"))
}

#[test]
fn builtin_rules_load() {
    Rules::builtin().unwrap();
}

#[test]
fn old_temp_file_is_a_safe_temp_finding() {
    let rules = Rules::builtin().unwrap();
    let temp = real_temp();
    let idx = index(vec![file(temp.join("old.tmp"), 100, 30), file(temp.join("new.tmp"), 100, 1)]);

    let findings = rules.evaluate(&idx, now());

    assert_eq!(findings.len(), 1);
    assert!(findings[0].path.ends_with("old.tmp"));
    assert_eq!(findings[0].category, Category::Temp);
    assert_eq!(findings[0].confidence, Confidence::Safe);
    assert!(!findings[0].reason.is_empty());
}

#[test]
fn inactive_node_modules_is_one_review_finding() {
    let rules = Rules::builtin().unwrap();
    let idx = index(vec![
        file(r"C:\Dev\proj\src\main.js", 5, 200),
        file(r"C:\Dev\proj\node_modules\a\index.js", 100, 200),
        file(r"C:\Dev\proj\node_modules\a\node_modules\b\x.js", 50, 200),
    ]);

    let findings = rules.evaluate(&idx, now());

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].path, PathBuf::from(r"C:\Dev\proj\node_modules"));
    assert_eq!(findings[0].size, 150);
    assert_eq!(findings[0].category, Category::DevArtifact);
    assert_eq!(findings[0].confidence, Confidence::Review);
}

#[test]
fn scan_starting_at_or_inside_node_modules_offers_no_folder() {
    let rules = Rules::builtin().unwrap();
    let package = PathBuf::from(r"C:\Dev\proj\node_modules\a");
    for root in [package.parent().unwrap(), package.as_path()] {
        let mut builder = IndexBuilder::new(root);
        builder.file(&package.join("index.js"), 100, days_ago(200));
        builder.file(&package.join(r"node_modules\b\x.js"), 50, days_ago(200));

        assert!(rules.evaluate(&builder.finish(), now()).is_empty(), "{}", root.display());
    }
}

#[test]
fn active_project_keeps_its_node_modules() {
    let rules = Rules::builtin().unwrap();
    let idx = index(vec![
        file(r"C:\Dev\proj\src\main.js", 5, 2),
        file(r"C:\Dev\proj\node_modules\a\index.js", 100, 200),
    ]);

    assert!(rules.evaluate(&idx, now()).is_empty());
}

#[test]
fn protected_path_never_becomes_a_finding() {
    let rules = Rules::from_toml(
        r#"
        [[rule]]
        name = "Everything in Windows"
        category = "Temp"
        confidence = "Safe"
        path_glob = "C:\\Windows\\*"
        reason = "test"
        "#,
    )
    .unwrap();
    let idx = index(vec![file(r"C:\Windows\System32\x.dll", 1, 999)]);

    assert!(rules.evaluate(&idx, now()).is_empty());
}

#[test]
fn allowlisted_electron_cache_matches_but_user_data_does_not() {
    let rules = Rules::builtin().unwrap();
    let roaming = env("APPDATA");
    let idx = index(vec![
        file(roaming.join(r"Code\Code Cache\js\a"), 10, 30),
        file(roaming.join(r"code\gpucache\b"), 10, 30),
        file(roaming.join(r"Code\User\settings.json"), 10, 30),
        file(roaming.join(r"Discord\Local Storage\leveldb\c"), 10, 30),
    ]);

    let findings = rules.evaluate(&idx, now());

    assert_eq!(findings.len(), 2);
    assert!(findings.iter().all(|f| f.category == Category::AppCache && f.confidence == Confidence::Safe));
}

#[test]
fn invalid_rules_give_clear_errors() {
    let err = |text: &str| format!("{:#}", Rules::from_toml(text).err().expect("should fail"));

    assert!(err("this is not toml").contains("invalid rules file"));
    let rule = |glob: &str| {
        format!(
            "[[rule]]\nname = \"R\"\ncategory = \"Temp\"\nconfidence = \"Safe\"\npath_glob = '{glob}'\nreason = \"x\"\n"
        )
    };
    let msg = format!("{:#}", Rules::from_toml(&rule(r"%NO_SUCH_VAR_DEOX%\*")).err().unwrap());
    assert!(msg.contains("NO_SUCH_VAR_DEOX") && msg.contains("rule 'R'"), "{msg}");
    assert!(err(&rule(r"%TEMP\*")).contains("unterminated"));
    assert!(format!("{:#}", Rules::from_toml(&rule("[")).err().unwrap()).contains("rule 'R'"));
}
