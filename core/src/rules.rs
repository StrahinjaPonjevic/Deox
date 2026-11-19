use crate::protect::{is_protected, resolve};
use crate::report::{Category, Confidence, Finding};
use crate::scan::Index;
use anyhow::{anyhow, Context, Result};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const BUILTIN: &str = include_str!("../rules/categories.toml");
const SECS_PER_DAY: u64 = 86_400;

#[derive(Deserialize)]
struct RawFile {
    rule: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    category: Category,
    confidence: Confidence,
    path_glob: String,
    #[serde(default)]
    kind: Kind,
    #[serde(default)]
    min_age_days: u64,
    reason: String,
}

/// What a rule matches: single files, or whole directories reported as one finding.
#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Kind {
    #[default]
    File,
    Dir,
}

struct Rule {
    name: String,
    category: Category,
    confidence: Confidence,
    kind: Kind,
    matcher: GlobMatcher,
    min_age: Duration,
    reason: String,
}

pub struct Rules(Vec<Rule>);

/// A directory matched by a `dir` rule, with what lies inside it.
struct Artifact<'a> {
    rule: &'a Rule,
    size: u64,
    newest: Option<SystemTime>,
}

impl Rules {
    /// The rule set shipped with Deox.
    pub fn builtin() -> Result<Self> {
        Self::from_toml(BUILTIN)
    }

    pub fn from_toml(text: &str) -> Result<Self> {
        let raw: RawFile = toml::from_str(text).context("invalid rules file")?;
        raw.rule.into_iter().map(compile).collect::<Result<_>>().map(Rules)
    }

    /// Turns an index into findings. `now` is a parameter so age checks are testable.
    /// Protected paths never produce a finding, whatever the rules say.
    pub fn evaluate(&self, index: &Index, now: SystemTime) -> Vec<Finding> {
        let old_enough = |modified: Option<SystemTime>, rule: &Rule| {
            modified.is_some_and(|m| now.duration_since(m).unwrap_or_default() >= rule.min_age)
        };
        let file_rules: Vec<&Rule> = self.0.iter().filter(|r| r.kind == Kind::File).collect();
        let dir_rules: Vec<&Rule> = self.0.iter().filter(|r| r.kind == Kind::Dir).collect();

        let mut findings = Vec::new();
        for file in &index.files {
            let path = glob_str(&file.path);
            if let Some(rule) = file_rules.iter().find(|r| r.matcher.is_match(&path)) {
                if old_enough(file.modified, rule) && !is_protected(&file.path) {
                    findings.push(finding(rule, file.path.clone(), file.size));
                }
            }
        }

        let artifacts = find_artifacts(index, &dir_rules);
        let activity = project_activity(index, &artifacts);
        for (root, a) in artifacts {
            // Active if the artifact itself or anything beside it changed recently.
            let beside = root.parent().and_then(|p| activity.get(p).copied().flatten());
            if old_enough(beside.max(a.newest), a.rule) && !is_protected(&root) {
                findings.push(finding(a.rule, root, a.size));
            }
        }
        findings
    }
}

/// Groups files under the outermost directory matched by a `dir` rule.
fn find_artifacts<'a>(index: &Index, dir_rules: &[&'a Rule]) -> HashMap<PathBuf, Artifact<'a>> {
    let mut artifacts = HashMap::new();
    if dir_rules.is_empty() {
        return artifacts;
    }
    for file in &index.files {
        let ancestors: Vec<&Path> = file.path.ancestors().skip(1).collect();
        let hit = ancestors.iter().rev().find_map(|dir| {
            let s = glob_str(dir);
            dir_rules.iter().find(|r| r.matcher.is_match(&s)).map(|r| (*dir, *r))
        });
        if let Some((root, rule)) = hit {
            let a = artifacts.entry(root.to_path_buf()).or_insert(Artifact { rule, size: 0, newest: None });
            a.size += file.size;
            a.newest = a.newest.max(file.modified);
        }
    }
    artifacts
}

/// Newest change outside the artifacts, per directory that contains an artifact.
fn project_activity(index: &Index, artifacts: &HashMap<PathBuf, Artifact>) -> HashMap<PathBuf, Option<SystemTime>> {
    let mut activity: HashMap<PathBuf, Option<SystemTime>> =
        artifacts.keys().filter_map(|root| root.parent().map(|p| (p.to_path_buf(), None))).collect();
    if activity.is_empty() {
        return activity;
    }
    for file in &index.files {
        if file.path.ancestors().any(|a| artifacts.contains_key(a)) {
            continue;
        }
        for dir in file.path.ancestors().skip(1) {
            if let Some(latest) = activity.get_mut(dir) {
                *latest = (*latest).max(file.modified);
            }
        }
    }
    activity
}

fn finding(rule: &Rule, path: PathBuf, size: u64) -> Finding {
    Finding {
        path,
        size,
        category: rule.category,
        confidence: rule.confidence,
        rule: rule.name.clone(),
        reason: rule.reason.clone(),
        uninstall: None,
    }
}

fn compile(raw: RawRule) -> Result<Rule> {
    let pattern = expand(&raw.path_glob).with_context(|| format!("rule '{}'", raw.name))?;
    let matcher = GlobBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("rule '{}': invalid path_glob '{}'", raw.name, raw.path_glob))?
        .compile_matcher();
    Ok(Rule {
        name: raw.name,
        category: raw.category,
        confidence: raw.confidence,
        kind: raw.kind,
        matcher,
        min_age: Duration::from_secs(raw.min_age_days * SECS_PER_DAY),
        reason: raw.reason,
    })
}

/// Globs are matched against `/`-separated paths, so `\` in rules and paths is converted.
fn glob_str(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Expands `%VAR%` to the resolved path of the variable. Resolving matters because
/// variables such as `%TEMP%` may hold a short 8.3 name that the scanner never reports.
fn expand(glob: &str) -> Result<String> {
    let mut out = String::new();
    let mut rest = glob;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start].replace('\\', "/"));
        let after = &rest[start + 1..];
        let end = after.find('%').ok_or_else(|| anyhow!("unterminated '%' in '{glob}'"))?;
        let var = &after[..end];
        let value = env::var_os(var).ok_or_else(|| anyhow!("environment variable %{var}% is not set"))?;
        out.push_str(&escape(&glob_str(&resolve(Path::new(&value)))));
        rest = &after[end + 1..];
    }
    out.push_str(&rest.replace('\\', "/"));
    Ok(out)
}

/// Makes glob metacharacters in a literal path (e.g. a user name with `[`) match themselves.
fn escape(literal: &str) -> String {
    literal.chars().map(|c| if "[{}*?,".contains(c) { format!("[{c}]") } else { c.to_string() }).collect()
}
