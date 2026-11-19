use deox_core::programs::{
    installed_programs, BloatwareList, Program, ProgramSource, RawRecord, WindowsRegistry,
};
use deox_core::report::{Category, Confidence};
use std::path::PathBuf;

struct Fake(Vec<RawRecord>);

impl ProgramSource for Fake {
    fn records(&self) -> Vec<RawRecord> {
        self.0.clone()
    }
}

fn record(name: &str, publisher: &str) -> RawRecord {
    RawRecord {
        display_name: Some(name.into()),
        publisher: Some(publisher.into()),
        ..RawRecord::default()
    }
}

#[test]
fn uninstall_records_are_parsed() {
    let mut app = record("Some App", "Some Vendor");
    app.estimated_size_kb = Some(2048);
    app.install_date = Some("20240131".into());
    app.install_location = Some(r"C:\Program Files\Some App".into());
    app.uninstall_string = Some(r#""C:\Program Files\Some App\unins000.exe""#.into());

    let programs = installed_programs(&Fake(vec![app]));

    assert_eq!(
        programs,
        vec![Program {
            name: "Some App".into(),
            publisher: Some("Some Vendor".into()),
            size: Some(2048 * 1024),
            install_date: Some("20240131".into()),
            install_location: Some(PathBuf::from(r"C:\Program Files\Some App")),
            uninstall: Some(r#""C:\Program Files\Some App\unins000.exe""#.into()),
        }]
    );
}

#[test]
fn entries_that_are_not_programs_are_skipped() {
    let mut system = record("Runtime Component", "Vendor");
    system.system_component = true;
    let mut patch = record("Security Update KB1", "Vendor");
    patch.has_parent = true;
    let nameless = RawRecord { publisher: Some("Vendor".into()), ..RawRecord::default() };
    let blank = record("   ", "Vendor");

    let programs = installed_programs(&Fake(vec![system, patch, nameless, blank, record("Real", "V")]));

    assert_eq!(programs.len(), 1);
    assert_eq!(programs[0].name, "Real");
}

#[test]
fn the_same_program_from_two_hives_is_listed_once() {
    let programs = installed_programs(&Fake(vec![record("Tool", "Vendor"), record("TOOL", "vendor")]));

    assert_eq!(programs.len(), 1);
}

#[test]
fn bloatware_by_name_or_publisher_is_an_info_finding() {
    let list = BloatwareList::from_toml(
        r#"
        [[rule]]
        name = "Trial antivirus"
        name_contains = ["mcafee"]
        reason = "Trial security suite."

        [[rule]]
        name = "Games bundle"
        publisher_contains = ["WildTangent"]
        reason = "Preinstalled games."
        "#,
    )
    .unwrap();
    let mut by_name = record("McAfee LiveSafe", "McAfee, LLC");
    by_name.estimated_size_kb = Some(100);
    by_name.uninstall_string = Some("uninstall.exe".into());
    let by_publisher = record("Fun Game", "WildTangent, Inc.");
    let clean = record("7-Zip", "Igor Pavlov");

    let programs = installed_programs(&Fake(vec![by_name, by_publisher, clean]));
    let mut findings = list.evaluate(&programs);
    findings.sort_by(|a, b| a.rule.cmp(&b.rule));

    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].rule, "Games bundle");
    assert_eq!(findings[1].rule, "Trial antivirus");
    assert_eq!(findings[1].size, 100 * 1024);
    assert_eq!(findings[1].uninstall.as_deref(), Some("uninstall.exe"));
    assert!(findings[1].reason.contains("McAfee LiveSafe") && findings[1].reason.contains("Trial security suite."));
    assert!(findings.iter().all(|f| f.category == Category::Bloatware && f.confidence == Confidence::Info));
}

#[test]
fn program_without_size_is_shown_without_panicking() {
    let list = BloatwareList::from_toml(
        "[[rule]]\nname = \"R\"\nname_contains = [\"x\"]\nreason = \"r\"\n",
    )
    .unwrap();
    let programs = installed_programs(&Fake(vec![record("Box", "V")]));

    let findings = list.evaluate(&programs);

    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].size, 0);
    assert!(findings[0].path.as_os_str().is_empty());
}

#[test]
fn builtin_list_loads_and_invalid_lists_are_rejected() {
    BloatwareList::builtin().unwrap();
    let err = |text: &str| format!("{:#}", BloatwareList::from_toml(text).err().expect("should fail"));

    assert!(err("nonsense").contains("invalid bloatware list"));
    // A rule with nothing to match would flag every program.
    assert!(err("[[rule]]\nname = \"R\"\nreason = \"r\"\n").contains("rule 'R'"));
    assert!(err("[[rule]]\nname = \"R\"\nname_contains = [\"\"]\nreason = \"r\"\n").contains("rule 'R'"));
}

#[test]
fn real_registry_can_be_read() {
    let programs = installed_programs(&WindowsRegistry);

    assert!(!programs.is_empty());
    assert!(programs.iter().all(|p| !p.name.trim().is_empty()));
}
