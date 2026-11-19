use serde::Deserialize;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Category {
    Temp,
    AppCache,
    DevArtifact,
    Duplicate,
    OldLarge,
    Bloatware,
}

/// How much attention a finding needs from the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
pub enum Confidence {
    /// Regenerates on its own.
    Safe,
    /// The user must look before deleting.
    Review,
    /// Informational; removal only through the program's own uninstaller.
    Info,
}

#[derive(Debug, Clone)]
pub struct Finding {
    pub path: PathBuf,
    pub size: u64,
    /// Last change when scanned (files only); deletion is skipped if it differs.
    pub modified: Option<SystemTime>,
    pub category: Category,
    pub confidence: Confidence,
    pub rule: String,
    pub reason: String,
    /// For installed programs: the command or `ms-settings:` link that starts their removal.
    pub uninstall: Option<String>,
}

/// Findings of one category with their combined size.
pub struct CategoryGroup<'a> {
    pub category: Category,
    pub total_size: u64,
    /// Largest first.
    pub findings: Vec<&'a Finding>,
}

pub struct Report {
    pub findings: Vec<Finding>,
}

impl Report {
    pub fn new(findings: Vec<Finding>) -> Self {
        Report { findings }
    }

    pub fn total_size(&self) -> u64 {
        self.findings.iter().map(|f| f.size).sum()
    }

    /// Groups by category, the group with the most reclaimable space first.
    pub fn groups(&self) -> Vec<CategoryGroup<'_>> {
        let mut sorted: Vec<&Finding> = self.findings.iter().collect();
        sorted.sort_by_key(|f| std::cmp::Reverse(f.size));
        let mut groups: Vec<CategoryGroup> = Vec::new();
        for finding in sorted {
            match groups.iter_mut().find(|g| g.category == finding.category) {
                Some(group) => {
                    group.total_size += finding.size;
                    group.findings.push(finding);
                }
                None => groups.push(CategoryGroup {
                    category: finding.category,
                    total_size: finding.size,
                    findings: vec![finding],
                }),
            }
        }
        groups.sort_by_key(|g| std::cmp::Reverse(g.total_size));
        groups
    }
}
