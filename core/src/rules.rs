use crate::protect::{resolve, ScanGuard};
use crate::report::{Category, Confidence, Finding};
use crate::scan::Index;
use anyhow::{anyhow, Context, Result};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
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
    min_age: Duration,
    reason: String,
}

/// Rules of one kind, matched against a path all at once. The first that matches wins.
struct RuleSet {
    rules: Vec<Rule>,
    /// The glob of each rule, in the same order.
    globs: GlobSet,
}

impl RuleSet {
    fn new(compiled: Vec<(Rule, Glob)>) -> Result<Self> {
        let mut globs = GlobSetBuilder::new();
        let rules = compiled
            .into_iter()
            .map(|(rule, glob)| {
                globs.add(glob);
                rule
            })
            .collect();
        Ok(RuleSet { rules, globs: globs.build().context("the rule globs cannot be combined")? })
    }

    /// `hits` is scratch space, so that matching many paths does not allocate.
    fn first_match(&self, path: &Path, hits: &mut Vec<usize>) -> Option<&Rule> {
        self.globs.matches_into(path, hits);
        hits.iter().min().map(|&i| &self.rules[i])
    }
}

pub struct Rules {
    files: RuleSet,
    dirs: RuleSet,
}

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
        let (mut files, mut dirs) = (Vec::new(), Vec::new());
        for rule in raw.rule {
            let kind = rule.kind;
            let compiled = compile(rule)?;
            match kind {
                Kind::File => files.push(compiled),
                Kind::Dir => dirs.push(compiled),
            }
        }
        Ok(Rules { files: RuleSet::new(files)?, dirs: RuleSet::new(dirs)? })
    }

    /// Turns an index into findings. `now` is a parameter so age checks are testable.
    /// Protected paths never produce a finding, whatever the rules say.
    pub fn evaluate(&self, index: &Index, now: SystemTime) -> Vec<Finding> {
        let old_enough = |modified: Option<SystemTime>, rule: &Rule| {
            modified.is_some_and(|m| now.duration_since(m).unwrap_or_default() >= rule.min_age)
        };
        let dirs = index.dir_paths();
        let guard = ScanGuard::new(&index.root);
        let mut findings = Vec::new();
        let (mut path, mut hits) = (PathBuf::new(), Vec::new());
        for file in &index.files {
            path.clone_from(&dirs[file.dir as usize]);
            path.push(&*file.name);
            if let Some(rule) = self.files.first_match(&path, &mut hits) {
                if old_enough(file.modified, rule) && !guard.protects(&path) {
                    findings.push(finding(rule, path.clone(), file.size, file.modified));
                }
            }
        }

        let enclosing = enclosing_artifacts(index, &dirs, &self.dirs);
        let outside = newest_outside(index, &enclosing);
        for (root, a) in find_artifacts(index, &enclosing) {
            // Active if the artifact itself or anything beside it changed recently.
            let beside = index.dirs[root].parent.and_then(|parent| outside[parent as usize]);
            if old_enough(beside.max(a.newest), a.rule) && !guard.protects(&dirs[root]) {
                findings.push(finding(a.rule, dirs[root].clone(), a.size, None));
            }
        }
        findings
    }
}

/// The position of a folder matched by a `dir` rule, and that rule.
type Match<'r> = Option<(usize, &'r Rule)>;

/// For each folder in the index, the outermost folder at or above it that a `dir` rule matches.
fn enclosing_artifacts<'r>(index: &Index, dirs: &[PathBuf], rules: &'r RuleSet) -> Vec<Match<'r>> {
    let mut hits = Vec::new();
    // A scan that starts at or inside a matched directory has not seen the project around it,
    // possibly not even the whole directory, so nothing in it can be judged inactive.
    if index.root.ancestors().any(|dir| rules.first_match(dir, &mut hits).is_some()) {
        return vec![None; dirs.len()];
    }
    let in_dot_folder = index.in_dot_folder();
    let mut enclosing: Vec<Match> = Vec::with_capacity(dirs.len());
    enclosing.push(None);
    for (id, (dir, path)) in index.dirs.iter().zip(dirs).enumerate().skip(1) {
        let parent = dir.parent.map_or(0, |parent| parent as usize);
        // A match inside a dot folder (an editor extension's dependencies, say) is part of a tool,
        // while a dot folder that matches by itself, such as `.venv`, is a project's own.
        let found = enclosing[parent].or_else(|| {
            let rule = rules.first_match(path, &mut hits).filter(|_| !in_dot_folder[parent]);
            rule.map(|rule| (id, rule))
        });
        enclosing.push(found);
    }
    enclosing
}

/// Sums the files under each matched folder.
fn find_artifacts<'r>(index: &Index, enclosing: &[Match<'r>]) -> HashMap<usize, Artifact<'r>> {
    let mut artifacts = HashMap::new();
    for file in &index.files {
        if let Some((root, rule)) = enclosing[file.dir as usize] {
            let a = artifacts.entry(root).or_insert(Artifact { rule, size: 0, newest: None });
            a.size += file.size;
            a.newest = a.newest.max(file.modified);
        }
    }
    artifacts
}

/// The newest change at or below each folder, leaving out what lies in matched folders.
fn newest_outside(index: &Index, enclosing: &[Match]) -> Vec<Option<SystemTime>> {
    let mut newest = vec![None; index.dirs.len()];
    for file in &index.files {
        let dir = file.dir as usize;
        if enclosing[dir].is_none() {
            newest[dir] = newest[dir].max(file.modified);
        }
    }
    // Parents come before their subfolders, so walking backwards carries each time all the way up.
    for (id, dir) in index.dirs.iter().enumerate().rev() {
        if let Some(parent) = dir.parent {
            newest[parent as usize] = newest[parent as usize].max(newest[id]);
        }
    }
    newest
}

fn finding(rule: &Rule, path: PathBuf, size: u64, modified: Option<SystemTime>) -> Finding {
    Finding {
        path,
        size,
        modified,
        category: rule.category,
        confidence: rule.confidence,
        rule: rule.name.clone(),
        reason: rule.reason.clone(),
        uninstall: None,
    }
}

fn compile(raw: RawRule) -> Result<(Rule, Glob)> {
    let pattern = expand(&raw.path_glob).with_context(|| format!("rule '{}'", raw.name))?;
    let glob = GlobBuilder::new(&pattern)
        .case_insensitive(true)
        .build()
        .with_context(|| format!("rule '{}': invalid path_glob '{}'", raw.name, raw.path_glob))?;
    let rule = Rule {
        name: raw.name,
        category: raw.category,
        confidence: raw.confidence,
        min_age: Duration::from_secs(raw.min_age_days * SECS_PER_DAY),
        reason: raw.reason,
    };
    Ok((rule, glob))
}

/// Globs use `/` as the separator. Paths being matched are converted by the glob library itself.
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
