use serde::Deserialize;
use std::path::PathBuf;

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
    pub category: Category,
    pub confidence: Confidence,
    pub rule: String,
    pub reason: String,
    /// For installed programs: the command that starts their own uninstaller.
    pub uninstall: Option<String>,
}
