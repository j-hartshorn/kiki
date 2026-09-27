//! A bounded, immutable snapshot of project and user Agent Skills.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use cap_std::ambient_authority;
use cap_std::fs::Dir;
#[cfg(unix)]
use rustix::fs::{Mode, OFlags, openat};
use yaml_rust::parser::Parser;
use yaml_rust::{Event, Yaml, YamlLoader};

pub const MAX_ANCESTOR_DIRS: usize = 32;
pub const MAX_PROJECT_ENTRIES: usize = 128;
pub const MAX_USER_ENTRIES: usize = 128;
pub const MAX_SKILL_BYTES: usize = 64 * 1024;
pub const MAX_SKILL_LINES: usize = 700;
pub const MAX_NAME_BYTES: usize = 64;
pub const MAX_DESCRIPTION_BYTES: usize = 1_024;
pub const MAX_VISIBLE_SKILLS: usize = 32;
pub const MAX_DIRECT_ONLY_SKILLS: usize = 32;
pub const MAX_CATALOG_BYTES: usize = 16 * 1024;
const MAX_YAML_DEPTH: usize = 64;

const SKILL_FILE: &str = "SKILL.md";
const CATALOG_HEADER: &str = "## Available skills\n";
const CATALOG_OMISSION: &str =
    "Additional model-invocable skills were omitted because the catalog limit was reached.\n";

/// Project roots in precedence order. Discovering this scope reads no SKILL.md.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectScope {
    pub canonical_scope: PathBuf,
    pub roots: Vec<PathBuf>,
    pub git_root_found: bool,
}

impl ProjectScope {
    pub fn discover(launch_directory: &Path) -> io::Result<Self> {
        let launch = fs::canonicalize(launch_directory)?;
        if !launch.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "skill launch directory is not a directory",
            ));
        }
        let mut ancestors = Vec::new();
        let mut current = Some(launch.as_path());
        let mut git_root = None;
        while let Some(path) = current.take() {
            if ancestors.len() == MAX_ANCESTOR_DIRS {
                break;
            }
            ancestors.push(path.to_path_buf());
            let marker = path.join(".git");
            if fs::metadata(marker).is_ok_and(|metadata| metadata.is_dir() || metadata.is_file()) {
                git_root = Some(path.to_path_buf());
                break;
            }
            current = path.parent();
        }
        let git_root_found = git_root.is_some();
        let canonical_scope = git_root.unwrap_or_else(|| launch.clone());
        if !git_root_found {
            ancestors.truncate(1);
        }
        let roots = ancestors
            .into_iter()
            .map(|ancestor| ancestor.join(".agents/skills"))
            .collect();
        Ok(Self {
            canonical_scope,
            roots,
            git_root_found,
        })
    }

    pub fn has_skill_directory(&self) -> bool {
        self.roots
            .iter()
            .any(|root| fs::metadata(root).is_ok_and(|metadata| metadata.is_dir()))
    }
}

/// `has_active_project_skill_in_scope` is supplied by session ancestry once
/// activated skills are stored (Task 3). It lets resume prompt after a root is removed.
pub fn project_trust_prompt_required(
    scope: &ProjectScope,
    already_trusted: bool,
    has_active_project_skill_in_scope: bool,
) -> bool {
    !already_trusted && (scope.has_skill_directory() || has_active_project_skill_in_scope)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillOrigin {
    Project,
    User,
}

/// Captured instructions and a held directory capability for later resource reads.
pub struct Skill {
    name: String,
    description: String,
    source_text: Arc<str>,
    discovery_path: PathBuf,
    canonical_root: PathBuf,
    origin: SkillOrigin,
    trust_scope: Option<PathBuf>,
    disable_model_invocation: bool,
    directory: Arc<Dir>,
}

impl Skill {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn source_text(&self) -> &str {
        &self.source_text
    }

    pub fn canonical_root(&self) -> &Path {
        &self.canonical_root
    }

    pub fn discovery_path(&self) -> &Path {
        &self.discovery_path
    }

    pub fn origin(&self) -> SkillOrigin {
        self.origin
    }

    pub fn trust_scope(&self) -> Option<&Path> {
        self.trust_scope.as_deref()
    }

    pub fn disable_model_invocation(&self) -> bool {
        self.disable_model_invocation
    }

    pub fn directory(&self) -> &Dir {
        &self.directory
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub name: Option<String>,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SkillLookupError {
    Unknown(String),
    Ambiguous { name: String, paths: Vec<PathBuf> },
    Invalid { name: String, reason: String },
    Omitted { name: String, reason: String },
    DirectOnly(String),
}

impl fmt::Display for SkillLookupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(name) => write!(formatter, "unknown skill `{name}`"),
            Self::Ambiguous { name, paths } => {
                write!(formatter, "skill `{name}` is ambiguous in: ")?;
                for (index, path) in paths.iter().enumerate() {
                    if index != 0 {
                        write!(formatter, ", ")?;
                    }
                    write!(formatter, "{}", path.display())?;
                }
                Ok(())
            }
            Self::Invalid { name, reason } => {
                write!(formatter, "skill `{name}` is invalid: {reason}")
            }
            Self::Omitted { name, reason } => {
                write!(formatter, "skill `{name}` was omitted: {reason}")
            }
            Self::DirectOnly(name) => write!(
                formatter,
                "skill `{name}` requires direct /skill invocation"
            ),
        }
    }
}

impl std::error::Error for SkillLookupError {}

/// One process snapshot. Only selected entries are available for activation.
pub struct SkillCatalog {
    selected: BTreeMap<String, Arc<Skill>>,
    visible_names: Vec<String>,
    direct_only_names: Vec<String>,
    rejected: BTreeMap<String, SkillLookupError>,
    diagnostics: Vec<SkillDiagnostic>,
    rendered_catalog: Option<String>,
}

impl SkillCatalog {
    /// `guidance` is included in the 16 KiB prompt budget at snapshot creation.
    pub fn discover(
        project: &ProjectScope,
        home: &Path,
        include_project: bool,
        guidance: &str,
    ) -> Self {
        let mut scanner = Scanner::default();
        if include_project {
            for root in &project.roots {
                scanner.scan_root(SourceRoot {
                    path: root,
                    boundary: &project.canonical_scope,
                    origin: SkillOrigin::Project,
                    trust_scope: Some(&project.canonical_scope),
                });
            }
        }
        match fs::canonicalize(home) {
            Ok(canonical_home) if canonical_home.is_dir() => {
                scanner.scan_root(SourceRoot {
                    path: &home.join(".agents/skills"),
                    boundary: &canonical_home,
                    origin: SkillOrigin::User,
                    trust_scope: None,
                });
            }
            Ok(_) => scanner.diagnostic(home, None, "user home is not a directory"),
            Err(error) => {
                scanner.diagnostic(home, None, format!("cannot resolve user home: {error}"))
            }
        }
        scanner.finish(guidance)
    }

    pub fn visible_names(&self) -> Vec<&str> {
        self.visible_names.iter().map(String::as_str).collect()
    }

    pub fn direct_only_names(&self) -> Vec<&str> {
        self.direct_only_names.iter().map(String::as_str).collect()
    }

    pub fn entries(&self) -> impl Iterator<Item = &Skill> {
        self.selected.values().map(Arc::as_ref)
    }

    pub fn visible_entries(&self) -> impl Iterator<Item = &Skill> {
        self.visible_names
            .iter()
            .filter_map(|name| self.selected.get(name).map(Arc::as_ref))
    }

    pub fn lookup(&self, name: &str) -> Result<&Skill, SkillLookupError> {
        self.selected.get(name).map(Arc::as_ref).ok_or_else(|| {
            self.rejected
                .get(name)
                .cloned()
                .unwrap_or_else(|| SkillLookupError::Unknown(name.to_owned()))
        })
    }

    pub fn lookup_model_invocable(&self, name: &str) -> Result<&Skill, SkillLookupError> {
        let skill = self.lookup(name)?;
        if skill.disable_model_invocation {
            Err(SkillLookupError::DirectOnly(name.to_owned()))
        } else {
            Ok(skill)
        }
    }

    pub fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    pub fn rendered_catalog(&self) -> Option<&str> {
        self.rendered_catalog.as_deref()
    }
}

struct SourceRoot<'a> {
    path: &'a Path,
    boundary: &'a Path,
    origin: SkillOrigin,
    trust_scope: Option<&'a Path>,
}

struct Scanner {
    resolved: BTreeMap<String, Result<Arc<Skill>, SkillLookupError>>,
    invalid: BTreeMap<String, SkillLookupError>,
    diagnostics: Vec<SkillDiagnostic>,
    project_remaining: usize,
    user_remaining: usize,
}

impl Default for Scanner {
    fn default() -> Self {
        Self {
            resolved: BTreeMap::new(),
            invalid: BTreeMap::new(),
            diagnostics: Vec::new(),
            project_remaining: MAX_PROJECT_ENTRIES,
            user_remaining: MAX_USER_ENTRIES,
        }
    }
}

impl Scanner {
    fn diagnostic(&mut self, path: &Path, name: Option<&str>, reason: impl Into<String>) {
        self.diagnostics.push(SkillDiagnostic {
            path: path.to_path_buf(),
            name: name.map(str::to_owned),
            reason: reason.into(),
        });
    }

    fn scan_root(&mut self, source: SourceRoot<'_>) {
        let budget = match source.origin {
            SkillOrigin::Project => self.project_remaining,
            SkillOrigin::User => self.user_remaining,
        };
        let raw_metadata = match fs::symlink_metadata(source.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.diagnostic(
                    source.path,
                    None,
                    format!("cannot inspect skill root: {error}"),
                );
                return;
            }
        };
        if !raw_metadata.is_dir() && !raw_metadata.file_type().is_symlink() {
            self.diagnostic(source.path, None, "skill root is not a directory");
            return;
        }
        let canonical_root = match fs::canonicalize(source.path) {
            Ok(path) => path,
            Err(error) => {
                self.diagnostic(
                    source.path,
                    None,
                    format!("cannot resolve skill root: {error}"),
                );
                return;
            }
        };
        if !canonical_root.starts_with(source.boundary) {
            self.diagnostic(
                source.path,
                None,
                "skill root resolves outside its allowed boundary",
            );
            return;
        }
        if !canonical_root.is_dir() {
            self.diagnostic(source.path, None, "skill root is not a directory");
            return;
        }
        let directory = match open_directory_under_boundary(source.boundary, &canonical_root) {
            Ok(directory) => directory,
            Err(error) => {
                self.diagnostic(
                    source.path,
                    None,
                    format!("cannot open skill root safely: {error}"),
                );
                return;
            }
        };
        let mut entries = Vec::<OsString>::new();
        let reader = match directory.read_dir(".") {
            Ok(reader) => reader,
            Err(error) => {
                self.diagnostic(
                    source.path,
                    None,
                    format!("cannot read skill root: {error}"),
                );
                return;
            }
        };
        for entry in reader {
            match entry {
                Ok(entry) => entries.push(entry.file_name()),
                Err(error) => {
                    self.diagnostic(
                        source.path,
                        None,
                        format!("cannot enumerate skill root: {error}"),
                    );
                    return;
                }
            }
            if entries.len() > budget {
                self.diagnostic(source.path, None, format!("skill root exceeds its remaining {budget}-entry budget; entire root skipped"));
                return;
            }
        }
        match source.origin {
            SkillOrigin::Project => self.project_remaining -= entries.len(),
            SkillOrigin::User => self.user_remaining -= entries.len(),
        }
        entries.sort();
        let mut grouped: BTreeMap<String, Vec<Arc<Skill>>> = BTreeMap::new();
        for entry_name in entries {
            let path = canonical_root.join(entry_name);
            match self.read_candidate(&source, &path) {
                Candidate::Skill(skill) => {
                    grouped.entry(skill.name.clone()).or_default().push(skill)
                }
                Candidate::Invalid { name, reason } => {
                    self.diagnostic(&path, name.as_deref(), &reason);
                    if let Some(name) = name {
                        self.invalid
                            .entry(name.clone())
                            .or_insert(SkillLookupError::Invalid { name, reason });
                    }
                }
                Candidate::NotDirectory => {}
            }
        }
        for (name, skills) in grouped {
            if self.resolved.contains_key(&name) {
                continue;
            }
            if skills.len() == 1 {
                self.resolved
                    .insert(name, Ok(skills.into_iter().next().unwrap()));
            } else {
                let paths: Vec<_> = skills
                    .iter()
                    .map(|skill| skill.discovery_path.clone())
                    .collect();
                self.diagnostic(source.path, Some(&name), "duplicate skill name in one root");
                self.resolved.insert(
                    name.clone(),
                    Err(SkillLookupError::Ambiguous { name, paths }),
                );
            }
        }
    }

    fn read_candidate(&self, source: &SourceRoot<'_>, path: &Path) -> Candidate {
        let raw = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                return Candidate::Invalid {
                    name: valid_directory_name(path),
                    reason: format!("cannot inspect skill candidate: {error}"),
                };
            }
        };
        if !raw.is_dir() && !raw.file_type().is_symlink() {
            return Candidate::NotDirectory;
        }
        let canonical_root = match fs::canonicalize(path) {
            Ok(path) => path,
            Err(error) => {
                return Candidate::Invalid {
                    name: valid_directory_name(path),
                    reason: format!("cannot resolve skill candidate: {error}"),
                };
            }
        };
        if !canonical_root.starts_with(source.boundary) {
            return Candidate::Invalid {
                name: valid_directory_name(path),
                reason: "skill candidate resolves outside its allowed boundary".to_owned(),
            };
        }
        if !canonical_root.is_dir() {
            return Candidate::NotDirectory;
        }
        let directory = match open_directory_under_boundary(source.boundary, &canonical_root) {
            Ok(directory) => directory,
            Err(error) => {
                return Candidate::Invalid {
                    name: valid_directory_name(path),
                    reason: format!("cannot open skill directory: {error}"),
                };
            }
        };
        let source_text = match read_skill_file(&directory) {
            Ok(Some(text)) => text,
            Ok(None) => return Candidate::NotDirectory,
            Err(reason) => {
                return Candidate::Invalid {
                    name: valid_directory_name(path),
                    reason,
                };
            }
        };
        let metadata = match parse_metadata(&source_text, path) {
            Ok(metadata) => metadata,
            Err(reason) => {
                return Candidate::Invalid {
                    name: identified_name(&source_text, path),
                    reason,
                };
            }
        };
        Candidate::Skill(Arc::new(Skill {
            name: metadata.name,
            description: metadata.description,
            source_text: Arc::from(source_text),
            discovery_path: path.to_path_buf(),
            canonical_root,
            origin: source.origin,
            trust_scope: source.trust_scope.map(Path::to_path_buf),
            disable_model_invocation: metadata.disable_model_invocation,
            directory: Arc::new(directory),
        }))
    }

    fn finish(self, guidance: &str) -> SkillCatalog {
        let mut selected = BTreeMap::new();
        let mut rejected = BTreeMap::new();
        for (name, resolution) in self.resolved {
            match resolution {
                Ok(skill) => {
                    selected.insert(name, skill);
                }
                Err(error) => {
                    rejected.insert(name, error);
                }
            }
        }
        for (name, error) in self.invalid {
            if !selected.contains_key(&name) && !rejected.contains_key(&name) {
                rejected.insert(name, error);
            }
        }
        let mut diagnostics = self.diagnostics;
        let all_direct: Vec<String> = selected
            .iter()
            .filter(|(_, skill)| skill.disable_model_invocation)
            .map(|(name, _)| name.clone())
            .collect();
        let direct_only_names: Vec<String> = all_direct
            .iter()
            .take(MAX_DIRECT_ONLY_SKILLS)
            .cloned()
            .collect();
        for name in all_direct.iter().skip(MAX_DIRECT_ONLY_SKILLS) {
            let skill = selected.remove(name).unwrap();
            let reason = "direct-only skill limit reached".to_owned();
            diagnostics.push(SkillDiagnostic {
                path: skill.canonical_root.clone(),
                name: Some(name.clone()),
                reason: reason.clone(),
            });
            rejected.insert(
                name.clone(),
                SkillLookupError::Omitted {
                    name: name.clone(),
                    reason,
                },
            );
        }

        let all_visible: Vec<String> = selected
            .iter()
            .filter(|(_, skill)| !skill.disable_model_invocation)
            .map(|(name, _)| name.clone())
            .collect();
        let mut rendered = String::from(CATALOG_HEADER);
        if !guidance.is_empty() {
            rendered.push_str(guidance.trim_end_matches('\n'));
            rendered.push('\n');
        }
        let mut visible_names = Vec::new();
        for (index, name) in all_visible.iter().enumerate() {
            if visible_names.len() == MAX_VISIBLE_SKILLS {
                break;
            }
            let skill = selected.get(name).unwrap();
            let line = format!("- {}: {}\n", skill.name, skill.description);
            let has_more = index + 1 < all_visible.len();
            let reserve = if has_more { CATALOG_OMISSION.len() } else { 0 };
            if rendered.len() + line.len() + reserve > MAX_CATALOG_BYTES {
                break;
            }
            rendered.push_str(&line);
            visible_names.push(name.clone());
        }
        for name in all_visible.iter().skip(visible_names.len()) {
            let skill = selected.remove(name).unwrap();
            let reason = "model catalog entry or byte limit reached".to_owned();
            diagnostics.push(SkillDiagnostic {
                path: skill.canonical_root.clone(),
                name: Some(name.clone()),
                reason: reason.clone(),
            });
            rejected.insert(
                name.clone(),
                SkillLookupError::Omitted {
                    name: name.clone(),
                    reason,
                },
            );
        }
        let omitted_visible = visible_names.len() < all_visible.len();
        if omitted_visible && rendered.len() + CATALOG_OMISSION.len() <= MAX_CATALOG_BYTES {
            rendered.push_str(CATALOG_OMISSION);
        }
        let rendered_catalog = (!visible_names.is_empty()).then_some(rendered);
        SkillCatalog {
            selected,
            visible_names,
            direct_only_names,
            rejected,
            diagnostics,
            rendered_catalog,
        }
    }
}

enum Candidate {
    Skill(Arc<Skill>),
    Invalid {
        name: Option<String>,
        reason: String,
    },
    NotDirectory,
}

fn open_directory_under_boundary(boundary: &Path, target: &Path) -> io::Result<Dir> {
    let relative = target.strip_prefix(boundary).map_err(|_| {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory lies outside boundary",
        )
    })?;
    #[cfg(unix)]
    let mut directory = open_canonical_directory(boundary)?;
    #[cfg(not(unix))]
    let mut directory = Dir::open_ambient_dir(boundary, ambient_authority())?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resolved directory contains an invalid component",
            ));
        };
        directory = open_directory_component_no_follow(&directory, name)?;
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_canonical_directory(path: &Path) -> io::Result<Dir> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "canonical directory must be absolute",
        ));
    }
    // Anchor at the filesystem root so a renamed boundary cannot redirect the open.
    let mut directory = Dir::open_ambient_dir("/", ambient_authority())?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                directory = open_directory_component_no_follow(&directory, name)?;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "canonical directory contains an invalid component",
                ));
            }
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn open_directory_component_no_follow(directory: &Dir, name: &OsStr) -> io::Result<Dir> {
    openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(|descriptor| Dir::from_std_file(std::fs::File::from(descriptor)))
    .map_err(io::Error::from)
}

#[cfg(not(unix))]
fn open_directory_component_no_follow(directory: &Dir, name: &OsStr) -> io::Result<Dir> {
    if directory.symlink_metadata(name)?.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory component became a symlink",
        ));
    }
    directory.open_dir(name)
}

struct ParsedMetadata {
    name: String,
    description: String,
    disable_model_invocation: bool,
}

fn read_skill_file(directory: &Dir) -> Result<Option<String>, String> {
    let metadata = match directory.symlink_metadata(SKILL_FILE) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect SKILL.md: {error}")),
    };
    if metadata.file_type().is_symlink() {
        return Err("SKILL.md may not be a symlink".to_owned());
    }
    if !metadata.is_file() {
        return Err("SKILL.md is not a regular file".to_owned());
    }
    if metadata.len() > MAX_SKILL_BYTES as u64 {
        return Err(format!("SKILL.md exceeds {MAX_SKILL_BYTES} bytes"));
    }
    #[cfg(unix)]
    let mut file = std::fs::File::from(
        openat(
            directory,
            SKILL_FILE,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| format!("cannot open SKILL.md without following symlinks: {error}"))?,
    );
    #[cfg(not(unix))]
    let mut file = directory
        .open(SKILL_FILE)
        .map_err(|error| format!("cannot open SKILL.md: {error}"))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take((MAX_SKILL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read SKILL.md: {error}"))?;
    if bytes.len() > MAX_SKILL_BYTES {
        return Err(format!("SKILL.md exceeds {MAX_SKILL_BYTES} bytes"));
    }
    let text = String::from_utf8(bytes).map_err(|_| "SKILL.md is not UTF-8".to_owned())?;
    if line_count(&text) > MAX_SKILL_LINES {
        return Err(format!("SKILL.md exceeds {MAX_SKILL_LINES} lines"));
    }
    Ok(Some(text))
}

pub(crate) fn line_count(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut count = 0;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'\n' || (*byte == b'\r' && bytes.get(index + 1) != Some(&b'\n')) {
            count += 1;
        }
    }
    if !bytes.is_empty() && !matches!(bytes.last(), Some(b'\n' | b'\r')) {
        count += 1;
    }
    count
}

fn parse_metadata(source: &str, path: &Path) -> Result<ParsedMetadata, String> {
    let (frontmatter, body) = split_frontmatter(source)?;
    if body.trim().is_empty() {
        return Err("SKILL.md body is empty".to_owned());
    }
    validate_yaml_frontmatter(frontmatter)?;
    let documents = YamlLoader::load_from_str(frontmatter)
        .map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
    let [Yaml::Hash(fields)] = documents.as_slice() else {
        return Err("frontmatter must be one YAML mapping".to_owned());
    };
    let description = match fields.get(&Yaml::String("description".to_owned())) {
        Some(Yaml::String(value)) => value.split_whitespace().collect::<Vec<_>>().join(" "),
        Some(_) => return Err("description must be a string".to_owned()),
        None => return Err("description is required".to_owned()),
    };
    if description.is_empty() {
        return Err("description must not be empty".to_owned());
    }
    if description.len() > MAX_DESCRIPTION_BYTES {
        return Err(format!(
            "description exceeds {MAX_DESCRIPTION_BYTES} UTF-8 bytes"
        ));
    }
    let name = match fields.get(&Yaml::String("name".to_owned())) {
        Some(Yaml::String(name)) => name.clone(),
        Some(_) => return Err("name must be a string".to_owned()),
        None => path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "directory name is not UTF-8; provide a name".to_owned())?
            .to_owned(),
    };
    if !valid_name(&name) {
        return Err("name must be 1–64 bytes of lowercase ASCII letters, digits, or hyphens, starting and ending with a letter or digit".to_owned());
    }
    let disable_model_invocation =
        match fields.get(&Yaml::String("disable-model-invocation".to_owned())) {
            Some(Yaml::Boolean(value)) => *value,
            Some(_) => return Err("disable-model-invocation must be a boolean".to_owned()),
            None => false,
        };
    Ok(ParsedMetadata {
        name,
        description,
        disable_model_invocation,
    })
}

fn validate_yaml_frontmatter(frontmatter: &str) -> Result<(), String> {
    // YamlLoader clones aliases and recursively builds collections. Check both bounds
    // with the iterative event parser before constructing a YAML value tree.
    let mut parser = Parser::new(frontmatter.chars());
    let mut depth = 0usize;
    let max_events = frontmatter.len().saturating_mul(4).saturating_add(16);
    for _ in 0..max_events {
        let (event, _) = parser
            .next()
            .map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
        match event {
            Event::Alias(_) => return Err("YAML anchors and aliases are not supported".to_owned()),
            Event::Scalar(_, _, anchor, _) if anchor != 0 => {
                return Err("YAML anchors and aliases are not supported".to_owned());
            }
            Event::SequenceStart(anchor) | Event::MappingStart(anchor) => {
                if anchor != 0 {
                    return Err("YAML anchors and aliases are not supported".to_owned());
                }
                depth += 1;
                if depth > MAX_YAML_DEPTH {
                    return Err(format!("YAML nesting exceeds {MAX_YAML_DEPTH} levels"));
                }
            }
            Event::SequenceEnd | Event::MappingEnd => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    "invalid YAML frontmatter: unmatched collection end".to_owned()
                })?;
            }
            Event::StreamEnd => return Ok(()),
            _ => {}
        }
    }
    Err("YAML frontmatter exceeds its event budget".to_owned())
}

fn split_frontmatter(source: &str) -> Result<(&str, &str), String> {
    let mut offset = 0;
    let mut opening = false;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if !opening {
            if trimmed != "---" {
                return Err("SKILL.md must begin with YAML frontmatter".to_owned());
            }
            opening = true;
            offset += line.len();
            break;
        }
    }
    if !opening {
        return Err("SKILL.md must begin with YAML frontmatter".to_owned());
    }
    let frontmatter_start = offset;
    for line in source[frontmatter_start..].split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            return Ok((
                &source[frontmatter_start..offset],
                &source[offset + line.len()..],
            ));
        }
        offset += line.len();
    }
    Err("YAML frontmatter has no closing --- delimiter".to_owned())
}

fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_NAME_BYTES
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn valid_directory_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| valid_name(name))
        .map(str::to_owned)
}

fn identified_name(source: &str, path: &Path) -> Option<String> {
    if let Ok((frontmatter, _)) = split_frontmatter(source)
        && validate_yaml_frontmatter(frontmatter).is_ok()
        && let Ok(documents) = YamlLoader::load_from_str(frontmatter)
        && let [Yaml::Hash(fields)] = documents.as_slice()
        && let Some(Yaml::String(name)) = fields.get(&Yaml::String("name".to_owned()))
        && valid_name(name)
    {
        return Some(name.clone());
    }
    valid_directory_name(path)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::fs;

    struct Fixture {
        temporary: tempfile::TempDir,
        repo: PathBuf,
        launch: PathBuf,
        home: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temporary = tempfile::tempdir().unwrap();
            let base = temporary.path().canonicalize().unwrap();
            let repo = base.join("repo");
            let launch = repo.join("nested/work");
            let home = base.join("home");
            fs::create_dir_all(&launch).unwrap();
            fs::create_dir_all(&home).unwrap();
            fs::create_dir(repo.join(".git")).unwrap();
            Self {
                temporary,
                repo,
                launch,
                home,
            }
        }

        fn project_root(&self, directory: &Path) -> PathBuf {
            directory.join(".agents/skills")
        }

        fn user_root(&self) -> PathBuf {
            self.home.join(".agents/skills")
        }

        fn scope(&self) -> ProjectScope {
            ProjectScope::discover(&self.launch).unwrap()
        }

        fn catalog(&self) -> SkillCatalog {
            SkillCatalog::discover(
                &self.scope(),
                &self.home,
                true,
                "Use the appropriate skill.",
            )
        }

        fn skill(root: &Path, directory: &str, frontmatter: &str, body: &str) -> PathBuf {
            let path = root.join(directory);
            fs::create_dir_all(&path).unwrap();
            fs::write(
                path.join(SKILL_FILE),
                format!("---\n{frontmatter}---\n{body}"),
            )
            .unwrap();
            path
        }
    }

    #[test]
    fn missing_roots_yield_empty_catalog() {
        let temporary = tempfile::tempdir().unwrap();
        let launch = temporary.path().join("project");
        let home = temporary.path().join("home");
        fs::create_dir_all(&launch).unwrap();
        fs::create_dir_all(&home).unwrap();
        let project = ProjectScope::discover(&launch).unwrap();
        assert!(!project.has_skill_directory());
        let catalog = SkillCatalog::discover(&project, &home, true, "guidance");
        assert!(catalog.visible_names().is_empty());
    }

    #[test]
    fn trust_prompt_requires_directory_or_active_in_scope_and_no_saved_trust() {
        let fixture = Fixture::new();
        let scope = fixture.scope();
        assert!(!project_trust_prompt_required(&scope, false, false));
        assert!(project_trust_prompt_required(&scope, false, true));
        assert!(!project_trust_prompt_required(&scope, true, true));
        fs::create_dir_all(fixture.project_root(&fixture.launch)).unwrap();
        assert!(project_trust_prompt_required(&scope, false, false));
        assert!(!project_trust_prompt_required(&scope, true, false));
    }

    #[test]
    fn git_file_stops_ancestor_discovery() {
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path().join("repo");
        let launch = repo.join("nested");
        fs::create_dir_all(&launch).unwrap();
        fs::write(repo.join(".git"), "gitdir: elsewhere").unwrap();
        let project = ProjectScope::discover(&launch).unwrap();
        assert!(project.git_root_found);
        assert_eq!(project.canonical_scope, repo.canonicalize().unwrap());
        assert_eq!(project.roots.len(), 2);
    }

    #[test]
    fn git_directory_and_no_git_scope_use_the_documented_roots() {
        let fixture = Fixture::new();
        let project = fixture.scope();
        assert!(project.git_root_found);
        assert_eq!(
            project.canonical_scope,
            fixture.repo.canonicalize().unwrap()
        );
        assert_eq!(project.roots.len(), 3);
        assert_eq!(project.roots[0], fixture.project_root(&fixture.launch));
        assert_eq!(project.roots[2], fixture.project_root(&fixture.repo));

        fs::remove_dir(fixture.repo.join(".git")).unwrap();
        let without_git = fixture.scope();
        assert!(!without_git.git_root_found);
        assert_eq!(
            without_git.canonical_scope,
            fixture.launch.canonicalize().unwrap()
        );
        assert_eq!(
            without_git.roots,
            vec![fixture.project_root(&fixture.launch)]
        );
    }

    #[test]
    fn git_marker_beyond_32_directory_bound_is_ignored() {
        let temporary = tempfile::tempdir().unwrap();
        let mut launch = temporary.path().join("repo");
        fs::create_dir_all(launch.join(".git")).unwrap();
        for _ in 0..MAX_ANCESTOR_DIRS - 1 {
            launch.push("nested");
        }
        fs::create_dir_all(&launch).unwrap();
        let at_boundary = ProjectScope::discover(&launch).unwrap();
        assert!(at_boundary.git_root_found);
        assert_eq!(at_boundary.roots.len(), MAX_ANCESTOR_DIRS);
        launch.push("nested");
        fs::create_dir_all(&launch).unwrap();
        let project = ProjectScope::discover(&launch).unwrap();
        assert!(!project.git_root_found);
        assert_eq!(
            project.roots,
            vec![launch.canonicalize().unwrap().join(".agents/skills")]
        );
    }

    #[test]
    fn nearest_project_root_wins_then_project_wins_over_user() {
        let fixture = Fixture::new();
        Fixture::skill(
            &fixture.user_root(),
            "shared",
            "description: user\n",
            "user body",
        );
        Fixture::skill(
            &fixture.project_root(&fixture.repo),
            "shared",
            "description: repo\n",
            "repo body",
        );
        Fixture::skill(
            &fixture.project_root(&fixture.launch),
            "shared",
            "description: near\n",
            "near body",
        );
        let catalog = fixture.catalog();
        let skill = catalog.lookup("shared").unwrap();
        assert_eq!(skill.description(), "near");
        assert!(skill.source_text().contains("near body"));
        assert_eq!(skill.origin(), SkillOrigin::Project);
        assert_eq!(skill.trust_scope(), Some(fixture.repo.as_path()));

        let user_only = SkillCatalog::discover(&fixture.scope(), &fixture.home, false, "");
        assert_eq!(user_only.lookup("shared").unwrap().description(), "user");
        assert_eq!(
            user_only.lookup("shared").unwrap().origin(),
            SkillOrigin::User
        );
    }

    #[test]
    fn duplicate_name_at_highest_root_is_ambiguous_even_with_lower_fallback() {
        let fixture = Fixture::new();
        let near = fixture.project_root(&fixture.launch);
        Fixture::skill(&near, "first", "name: shared\ndescription: first\n", "body");
        Fixture::skill(
            &near,
            "second",
            "name: shared\ndescription: second\n",
            "body",
        );
        Fixture::skill(
            &fixture.user_root(),
            "shared",
            "description: user\n",
            "body",
        );
        let catalog = fixture.catalog();
        match catalog.lookup("shared").err().unwrap() {
            SkillLookupError::Ambiguous { paths, .. } => assert_eq!(paths.len(), 2),
            error => panic!("unexpected error: {error}"),
        }
        assert!(catalog.visible_names().is_empty());
        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.reason.contains("duplicate"))
        );
    }

    #[test]
    fn duplicate_name_in_lower_root_does_not_invalidate_nearer_skill() {
        let fixture = Fixture::new();
        Fixture::skill(
            &fixture.project_root(&fixture.launch),
            "shared",
            "description: near\n",
            "near",
        );
        let farther = fixture.project_root(&fixture.repo);
        Fixture::skill(&farther, "one", "name: shared\ndescription: one\n", "one");
        Fixture::skill(&farther, "two", "name: shared\ndescription: two\n", "two");
        let catalog = fixture.catalog();
        assert_eq!(catalog.lookup("shared").unwrap().description(), "near");
    }

    #[test]
    fn frontmatter_requires_valid_description_name_boolean_and_body() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        Fixture::skill(
            &root,
            "default-name",
            "description: >\n  multi\n  line\n",
            "body",
        );
        Fixture::skill(
            &root,
            "direct",
            "description: direct\ndisable-model-invocation: true\n",
            "body",
        );
        Fixture::skill(
            &root,
            "missing-description",
            "name: missing-description\n",
            "body",
        );
        Fixture::skill(&root, "wrong-description", "description: 123\n", "body");
        Fixture::skill(
            &root,
            "wrong-boolean",
            "description: okay\ndisable-model-invocation: 'true'\n",
            "body",
        );
        Fixture::skill(
            &root,
            "bad-name",
            "name: Bad_Name\ndescription: okay\n",
            "body",
        );
        Fixture::skill(&root, "empty-body", "description: okay\n", " \n");
        fs::create_dir_all(root.join("no-frontmatter")).unwrap();
        fs::write(
            root.join("no-frontmatter/SKILL.md"),
            "description: nope\nbody",
        )
        .unwrap();
        let catalog = fixture.catalog();
        assert_eq!(
            catalog.lookup("default-name").unwrap().description(),
            "multi line"
        );
        assert_eq!(catalog.direct_only_names(), vec!["direct"]);
        assert!(matches!(
            catalog.lookup_model_invocable("direct"),
            Err(SkillLookupError::DirectOnly(_))
        ));
        for name in [
            "missing-description",
            "wrong-description",
            "wrong-boolean",
            "bad-name",
            "empty-body",
            "no-frontmatter",
        ] {
            assert!(
                matches!(catalog.lookup(name), Err(SkillLookupError::Invalid { .. })),
                "{name}"
            );
        }
    }

    #[test]
    fn yaml_aliases_and_anchors_are_rejected_without_rejecting_quoted_markers() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        Fixture::skill(
            &root,
            "alias-bomb",
            "description: safe\nchain: &a [x, x]\nmore: &b [*a, *a]\n",
            "body",
        );
        Fixture::skill(
            &root,
            "literal-markers",
            "description: 'Use *literal and &literal'\n",
            "body",
        );
        let catalog = fixture.catalog();
        assert!(format!("{}", catalog.lookup("alias-bomb").err().unwrap()).contains("anchor"));
        assert_eq!(
            catalog.lookup("literal-markers").unwrap().description(),
            "Use *literal and &literal"
        );
    }

    #[test]
    fn yaml_nesting_over_depth_limit_is_rejected_before_loading() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        let nested = format!(
            "nested: {}x{}\n",
            "[".repeat(MAX_YAML_DEPTH + 1),
            "]".repeat(MAX_YAML_DEPTH + 1)
        );
        Fixture::skill(
            &root,
            "too-deep",
            &format!("description: safe\n{nested}"),
            "body",
        );
        let catalog = fixture.catalog();
        assert!(format!("{}", catalog.lookup("too-deep").err().unwrap()).contains("nesting"));
    }

    #[test]
    fn plain_child_directory_without_skill_file_is_ignored() {
        let fixture = Fixture::new();
        let plain = fixture.user_root().join("notes");
        fs::create_dir_all(&plain).unwrap();
        let catalog = fixture.catalog();
        assert!(matches!(
            catalog.lookup("notes"),
            Err(SkillLookupError::Unknown(_))
        ));
        assert!(
            catalog
                .diagnostics()
                .iter()
                .all(|diagnostic| diagnostic.path != plain)
        );
    }

    #[test]
    fn invalid_utf8_and_symlinked_skill_file_are_rejected() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        let invalid = root.join("invalid-utf8");
        fs::create_dir_all(&invalid).unwrap();
        fs::write(invalid.join(SKILL_FILE), [0xff, 0xfe]).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let linked = root.join("linked-file");
            fs::create_dir_all(&linked).unwrap();
            let target = fixture.home.join("target.md");
            fs::write(&target, "---\ndescription: valid\n---\nbody").unwrap();
            symlink(target, linked.join(SKILL_FILE)).unwrap();
        }
        let catalog = fixture.catalog();
        assert!(format!("{}", catalog.lookup("invalid-utf8").err().unwrap()).contains("UTF-8"));
        #[cfg(unix)]
        assert!(format!("{}", catalog.lookup("linked-file").err().unwrap()).contains("symlink"));
    }

    #[test]
    fn skill_size_and_line_limits_accept_exact_boundary_and_reject_one_more() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        let header = "---\ndescription: size\n---\n";
        for (name, total) in [
            ("exact-size", MAX_SKILL_BYTES),
            ("over-size", MAX_SKILL_BYTES + 1),
        ] {
            let directory = root.join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join(SKILL_FILE),
                format!("{header}{}", "x".repeat(total - header.len())),
            )
            .unwrap();
        }
        let header = "---\ndescription: lines\n---\n";
        for (name, body_lines) in [
            ("exact-lines", MAX_SKILL_LINES - 3),
            ("over-lines", MAX_SKILL_LINES - 2),
        ] {
            let directory = root.join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join(SKILL_FILE),
                format!("{header}{}", "x\n".repeat(body_lines)),
            )
            .unwrap();
        }
        for (name, body_lines) in [
            ("exact-cr-lines", MAX_SKILL_LINES - 3),
            ("over-cr-lines", MAX_SKILL_LINES - 2),
        ] {
            let directory = root.join(name);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join(SKILL_FILE),
                format!("{header}{}", "x\r".repeat(body_lines)),
            )
            .unwrap();
        }
        let catalog = fixture.catalog();
        assert!(catalog.lookup("exact-size").is_ok());
        assert!(catalog.lookup("exact-lines").is_ok());
        assert!(catalog.lookup("exact-cr-lines").is_ok());
        assert!(format!("{}", catalog.lookup("over-size").err().unwrap()).contains("bytes"));
        assert!(format!("{}", catalog.lookup("over-lines").err().unwrap()).contains("lines"));
        assert!(format!("{}", catalog.lookup("over-cr-lines").err().unwrap()).contains("lines"));
    }

    #[test]
    fn name_and_description_byte_limits_use_exact_utf8_lengths() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        let exact_name = "a".repeat(MAX_NAME_BYTES);
        let over_name = "b".repeat(MAX_NAME_BYTES + 1);
        Fixture::skill(&root, &exact_name, "description: okay\n", "body");
        Fixture::skill(&root, &over_name, "description: okay\n", "body");
        Fixture::skill(
            &root,
            "exact-description",
            &format!("description: {}\n", "é".repeat(MAX_DESCRIPTION_BYTES / 2)),
            "body",
        );
        Fixture::skill(
            &root,
            "over-description",
            &format!(
                "description: {}\n",
                "é".repeat(MAX_DESCRIPTION_BYTES / 2 + 1)
            ),
            "body",
        );
        let catalog = fixture.catalog();
        assert!(catalog.lookup(&exact_name).is_ok());
        assert_eq!(
            catalog
                .lookup("exact-description")
                .unwrap()
                .description()
                .len(),
            MAX_DESCRIPTION_BYTES
        );
        assert!(
            catalog
                .visible_names()
                .iter()
                .all(|name| *name != over_name)
        );
        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.path.ends_with(&over_name)
                    && diagnostic.reason.contains("name"))
        );
        assert!(
            format!("{}", catalog.lookup("over-description").err().unwrap())
                .contains("UTF-8 bytes")
        );
    }

    #[test]
    fn visible_and_direct_only_limits_are_separate_and_do_not_leak_direct_names() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        for index in (0..=MAX_VISIBLE_SKILLS).rev() {
            Fixture::skill(
                &root,
                &format!("visible-{index:02}"),
                "description: visible\n",
                "body",
            );
            Fixture::skill(
                &root,
                &format!("secret-{index:02}"),
                "description: secret\ndisable-model-invocation: true\n",
                "body",
            );
        }
        let catalog = fixture.catalog();
        assert_eq!(catalog.visible_names().len(), MAX_VISIBLE_SKILLS);
        assert_eq!(catalog.direct_only_names().len(), MAX_DIRECT_ONLY_SKILLS);
        assert!(catalog.lookup("visible-31").is_ok());
        assert!(catalog.lookup("secret-31").is_ok());
        assert!(matches!(
            catalog.lookup("visible-32"),
            Err(SkillLookupError::Omitted { .. })
        ));
        assert!(matches!(
            catalog.lookup("secret-32"),
            Err(SkillLookupError::Omitted { .. })
        ));
        let rendered = catalog.rendered_catalog().unwrap();
        assert!(!rendered.contains("secret"));
        assert!(rendered.contains("omitted"));
    }

    #[test]
    fn over_budget_root_is_skipped_whole_and_user_root_has_own_budget() {
        let fixture = Fixture::new();
        let project = fixture.project_root(&fixture.launch);
        for index in (0..=MAX_PROJECT_ENTRIES).rev() {
            Fixture::skill(
                &project,
                &format!("project-{index:03}"),
                "description: project\n",
                "body",
            );
        }
        Fixture::skill(
            &fixture.user_root(),
            "user-one",
            "description: user\n",
            "body",
        );
        let catalog = fixture.catalog();
        assert_eq!(catalog.visible_names(), vec!["user-one"]);
        assert!(matches!(
            catalog.lookup("project-000"),
            Err(SkillLookupError::Unknown(_))
        ));
        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.path == project
                    && diagnostic.reason.contains("entire root skipped"))
        );
    }

    #[test]
    fn project_and_user_roots_accept_128_entries_then_skip_129() {
        let fixture = Fixture::new();
        let project = fixture.project_root(&fixture.launch);
        let user = fixture.user_root();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&user).unwrap();
        for index in 0..MAX_PROJECT_ENTRIES - 1 {
            fs::write(project.join(format!("filler-{index:03}")), "not a skill").unwrap();
        }
        for index in 0..MAX_USER_ENTRIES - 1 {
            fs::write(user.join(format!("filler-{index:03}")), "not a skill").unwrap();
        }
        Fixture::skill(&project, "project-one", "description: project\n", "body");
        Fixture::skill(&user, "user-one", "description: user\n", "body");
        let exact = fixture.catalog();
        assert_eq!(exact.visible_names(), vec!["project-one", "user-one"]);
        fs::write(project.join("extra"), "not a skill").unwrap();
        fs::write(user.join("extra"), "not a skill").unwrap();
        let over = fixture.catalog();
        assert!(over.visible_names().is_empty());
        for root in [&project, &user] {
            assert!(over.diagnostics().iter().any(|diagnostic| {
                diagnostic.path == *root && diagnostic.reason.contains("entire root skipped")
            }));
        }
    }

    #[test]
    fn project_budget_is_shared_across_ancestors() {
        let fixture = Fixture::new();
        let near = fixture.project_root(&fixture.launch);
        fs::create_dir_all(&near).unwrap();
        for index in 0..100 {
            fs::write(near.join(format!("filler-{index:03}")), "not a skill").unwrap();
        }
        let far = fixture.project_root(&fixture.repo);
        for index in 0..28 {
            Fixture::skill(
                &far,
                &format!("far-{index:03}"),
                "description: far\n",
                "body",
            );
        }
        let exact = fixture.catalog();
        assert_eq!(exact.visible_names().len(), 28);
        Fixture::skill(&far, "far-028", "description: far\n", "body");
        let catalog = fixture.catalog();
        assert!(catalog.visible_names().is_empty());
        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.path == far
                    && diagnostic.reason.contains("28-entry budget"))
        );
    }

    #[test]
    fn catalog_byte_cap_accepts_exact_boundary_and_omits_over_boundary() {
        let fixture = Fixture::new();
        Fixture::skill(&fixture.user_root(), "one", "description: one\n", "body");
        let line = "- one: one\n";
        let exact_guidance = "g".repeat(MAX_CATALOG_BYTES - CATALOG_HEADER.len() - line.len() - 1);
        let project = fixture.scope();
        let exact = SkillCatalog::discover(&project, &fixture.home, false, &exact_guidance);
        assert_eq!(exact.rendered_catalog().unwrap().len(), MAX_CATALOG_BYTES);
        assert_eq!(exact.visible_names(), vec!["one"]);
        let over = SkillCatalog::discover(
            &project,
            &fixture.home,
            false,
            &format!("{exact_guidance}g"),
        );
        assert!(over.visible_names().is_empty());
        assert!(matches!(
            over.lookup("one"),
            Err(SkillLookupError::Omitted { .. })
        ));
    }

    #[test]
    fn catalog_omission_is_sorted_and_deterministic() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        for index in (0..25).rev() {
            Fixture::skill(
                &root,
                &format!("entry-{index:02}"),
                &format!("description: {}\n", "d".repeat(1_000)),
                "body",
            );
        }
        let first = fixture.catalog();
        let second = fixture.catalog();
        assert_eq!(first.visible_names(), second.visible_names());
        assert_eq!(first.rendered_catalog(), second.rendered_catalog());
        assert!(first.visible_names().len() < 25);
        assert!(
            first
                .visible_names()
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert!(first.rendered_catalog().unwrap().len() <= MAX_CATALOG_BYTES);
        assert!(first.rendered_catalog().unwrap().contains("omitted"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_roots_and_candidates_inside_boundary_are_accepted_outside_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        let internal_root = fixture.repo.join("actual-skills");
        Fixture::skill(
            &internal_root,
            "inside-root",
            "description: inside root\n",
            "body",
        );
        let linked_root = fixture.project_root(&fixture.launch);
        fs::create_dir_all(linked_root.parent().unwrap()).unwrap();
        symlink(&internal_root, &linked_root).unwrap();
        let target = Fixture::skill(
            &fixture.repo.join("other"),
            "inside-candidate",
            "description: inside candidate\n",
            "body",
        );
        symlink(&target, internal_root.join("inside-candidate")).unwrap();
        let external = Fixture::skill(
            &fixture.temporary.path().join("outside"),
            "outside-candidate",
            "description: outside\n",
            "body",
        );
        symlink(&external, internal_root.join("outside-candidate")).unwrap();
        let catalog = fixture.catalog();
        assert!(catalog.lookup("inside-root").is_ok());
        assert!(catalog.lookup("inside-candidate").is_ok());
        assert!(
            format!("{}", catalog.lookup("outside-candidate").err().unwrap()).contains("outside")
        );

        fs::remove_file(&linked_root).unwrap();
        symlink(fixture.temporary.path().join("outside"), &linked_root).unwrap();
        let outside_root = fixture.catalog();
        assert!(outside_root.visible_names().is_empty());
        assert!(
            outside_root
                .diagnostics()
                .iter()
                .any(|diagnostic| diagnostic.path == linked_root
                    && diagnostic.reason.contains("outside"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_symlink_targets_must_remain_inside_canonical_home() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let actual_root = fixture.home.join("actual-skills");
        Fixture::skill(&actual_root, "home-skill", "description: home\n", "body");
        let linked_root = fixture.user_root();
        fs::create_dir_all(linked_root.parent().unwrap()).unwrap();
        symlink(&actual_root, &linked_root).unwrap();
        let inside = Fixture::skill(
            &fixture.home.join("other"),
            "inside-home",
            "description: inside\n",
            "body",
        );
        symlink(&inside, actual_root.join("inside-home")).unwrap();
        let outside = Fixture::skill(
            &fixture.temporary.path().join("outside"),
            "outside-home",
            "description: outside\n",
            "body",
        );
        symlink(&outside, actual_root.join("outside-home")).unwrap();

        let catalog = fixture.catalog();
        assert!(catalog.lookup("home-skill").is_ok());
        assert!(catalog.lookup("inside-home").is_ok());
        assert!(matches!(
            catalog.lookup("outside-home"),
            Err(SkillLookupError::Invalid { .. })
        ));
        fs::remove_file(&linked_root).unwrap();
        symlink(fixture.temporary.path().join("outside"), &linked_root).unwrap();
        let rejected_root = fixture.catalog();
        assert!(rejected_root.visible_names().is_empty());
        assert!(rejected_root.diagnostics().iter().any(|diagnostic| {
            diagnostic.path == linked_root && diagnostic.reason.contains("outside")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn replaced_canonical_boundary_cannot_redirect_directory_open() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().canonicalize().unwrap();
        let boundary = base.join("boundary");
        let target = boundary.join("skill");
        let outside = base.join("outside");
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(outside.join("skill")).unwrap();
        fs::rename(&boundary, base.join("moved-boundary")).unwrap();
        symlink(&outside, &boundary).unwrap();

        assert!(open_directory_under_boundary(&boundary, &target).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn held_directory_handle_stays_on_accepted_candidate_after_replacement() {
        let fixture = Fixture::new();
        let root = fixture.user_root();
        let original = Fixture::skill(&root, "held", "description: held\n", "body");
        fs::write(original.join("resource.txt"), "original").unwrap();
        let catalog = fixture.catalog();
        fs::rename(&original, root.join("moved")).unwrap();
        fs::create_dir(&original).unwrap();
        fs::write(original.join("resource.txt"), "replacement").unwrap();
        let mut resource = String::new();
        catalog
            .lookup("held")
            .unwrap()
            .directory()
            .open("resource.txt")
            .unwrap()
            .read_to_string(&mut resource)
            .unwrap();
        assert_eq!(resource, "original");
        fs::write(
            root.join("moved/SKILL.md"),
            "---\ndescription: changed\n---\nchanged",
        )
        .unwrap();
        assert!(
            catalog
                .lookup("held")
                .unwrap()
                .source_text()
                .contains("body")
        );
    }
}
