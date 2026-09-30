use crate::fsutil::write_atomic;
use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Manifest {
    plugins: Vec<PluginSpec>,
}

impl Manifest {
    pub fn load(path: &Path) -> Result<Self, ManifestError> {
        match fs::read_to_string(path) {
            Ok(input) => input.parse(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ManifestError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn plugins(&self) -> &[PluginSpec] {
        &self.plugins
    }

    pub fn upsert(&mut self, plugin: PluginSpec) {
        if let Some(existing) = self
            .plugins
            .iter_mut()
            .find(|existing| existing.name == plugin.name)
        {
            *existing = plugin;
        } else {
            self.plugins.push(plugin);
        }
    }

    pub fn save_atomic(&self, path: &Path) -> Result<(), ManifestError> {
        let mut document = KdlDocument::new();

        for plugin in &self.plugins {
            let mut node = KdlNode::new("plugin");
            node.entries_mut().push(KdlEntry::new(plugin.name.clone()));

            let children = node.ensure_children();
            push_string_node(children, "source", plugin.source.to_string());

            if let Some(version) = &plugin.version {
                push_string_node(children, "version", version.clone());
            }

            document.nodes_mut().push(node);
        }

        document.autoformat();
        write_atomic(path, document.to_string().as_bytes()).map_err(|source| {
            ManifestError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }
}

impl FromStr for Manifest {
    type Err = ManifestError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let document = input.parse::<KdlDocument>().map_err(ManifestError::Parse)?;

        let mut plugins = Vec::with_capacity(document.nodes().len());
        let mut names = BTreeSet::new();

        for node in document.nodes() {
            if node.name().value() != "plugin" {
                return Err(ManifestError::Schema(format!(
                    "unknown top-level node '{}'; expected 'plugin'",
                    node.name().value()
                )));
            }

            let plugin = parse_plugin(node)?;

            if !names.insert(plugin.name.clone()) {
                return Err(ManifestError::Schema(format!(
                    "plugin '{}' is declared more than once",
                    plugin.name
                )));
            }

            plugins.push(plugin);
        }

        Ok(Self { plugins })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSpec {
    pub name: String,
    pub source: PluginSource,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginSource {
    GitHub { owner: String, repo: String },
    Path(String),
}

impl fmt::Display for PluginSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitHub { owner, repo } => write!(formatter, "github:{owner}/{repo}"),
            Self::Path(path) => write!(formatter, "path:{path}"),
        }
    }
}

impl FromStr for PluginSource {
    type Err = String;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        if let Some(repository) = source.strip_prefix("github:") {
            let mut parts = repository.split('/');
            let owner = parts.next().unwrap_or_default();
            let repo = parts.next().unwrap_or_default();

            if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
                return Err(format!(
                    "invalid GitHub source '{source}'; expected github:owner/repo"
                ));
            }

            if owner.chars().any(char::is_whitespace) || repo.chars().any(char::is_whitespace) {
                return Err(format!(
                    "invalid GitHub source '{source}'; owner and repo cannot contain whitespace"
                ));
            }

            return Ok(Self::GitHub {
                owner: owner.to_owned(),
                repo: repo.to_owned(),
            });
        }

        if let Some(path) = source.strip_prefix("path:") {
            if path.trim().is_empty() {
                return Err("path source cannot be empty".to_owned());
            }

            return Ok(Self::Path(path.to_owned()));
        }

        Err(format!(
            "unsupported source '{source}'; expected github:owner/repo or path:<file>"
        ))
    }
}

#[derive(Debug)]
pub enum ManifestError {
    Read { path: PathBuf, source: io::Error },
    Write { path: PathBuf, source: io::Error },
    Parse(kdl::KdlError),
    Schema(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "could not write {}: {source}", path.display())
            }
            Self::Parse(source) => write!(formatter, "could not parse manifest: {source}"),
            Self::Schema(message) => write!(formatter, "invalid manifest: {message}"),
        }
    }
}

impl Error for ManifestError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Schema(_) => None,
        }
    }
}

fn parse_plugin(node: &KdlNode) -> Result<PluginSpec, ManifestError> {
    let name = positional_string(node, "plugin name")?.to_owned();
    validate_plugin_name(&name).map_err(ManifestError::Schema)?;

    let children = node.children().ok_or_else(|| {
        ManifestError::Schema(format!(
            "plugin '{name}' is missing its configuration block"
        ))
    })?;

    let mut source = None;
    let mut version = None;

    for child in children.nodes() {
        match child.name().value() {
            "source" => {
                if source.is_some() {
                    return Err(ManifestError::Schema(format!(
                        "plugin '{name}' has more than one source"
                    )));
                }

                let value = scalar_string(child, "source")?;
                source =
                    Some(value.parse::<PluginSource>().map_err(|error| {
                        ManifestError::Schema(format!("plugin '{name}': {error}"))
                    })?);
            }
            "version" => {
                if version.is_some() {
                    return Err(ManifestError::Schema(format!(
                        "plugin '{name}' has more than one version"
                    )));
                }

                let value = scalar_string(child, "version")?;
                if value.trim().is_empty() {
                    return Err(ManifestError::Schema(format!(
                        "plugin '{name}' has an empty version"
                    )));
                }
                version = Some(value.to_owned());
            }
            other => {
                return Err(ManifestError::Schema(format!(
                    "plugin '{name}' has unknown field '{other}'"
                )));
            }
        }
    }

    let source = source
        .ok_or_else(|| ManifestError::Schema(format!("plugin '{name}' is missing a source")))?;

    Ok(PluginSpec {
        name,
        source,
        version,
    })
}

fn push_string_node(document: &mut KdlDocument, name: &str, value: String) {
    let mut node = KdlNode::new(name);
    node.entries_mut().push(KdlEntry::new(value));
    document.nodes_mut().push(node);
}

fn positional_string<'a>(node: &'a KdlNode, label: &str) -> Result<&'a str, ManifestError> {
    if node.entries().len() != 1 || node.entries()[0].name().is_some() {
        return Err(ManifestError::Schema(format!(
            "{label} must be one quoted string"
        )));
    }

    node.entries()[0]
        .value()
        .as_string()
        .ok_or_else(|| ManifestError::Schema(format!("{label} must be a string")))
}

fn scalar_string<'a>(node: &'a KdlNode, label: &str) -> Result<&'a str, ManifestError> {
    if node.children().is_some() {
        return Err(ManifestError::Schema(format!(
            "{label} cannot have a child block"
        )));
    }

    positional_string(node, label)
}

pub(crate) fn validate_plugin_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(format!("invalid plugin name '{name}'"));
    }

    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(format!(
            "plugin name '{name}' may only contain letters, numbers, '.', '_' and '-'"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_github_path_and_pinned_plugins() {
        let manifest: Manifest = r#"
plugin "zjstatus" {
    source "github:dj95/zjstatus"
}

plugin "local-clock" {
    source "path:~/Dev/local-clock/plugin.wasm"
    version "dev"
}

plugin "pinned" {
    source "github:example/pinned"
    version "1.2.3"
}
"#
        .parse()
        .unwrap();

        assert_eq!(manifest.plugins.len(), 3);
        assert_eq!(manifest.plugins[0].name, "zjstatus");
        assert_eq!(manifest.plugins[0].version, None);
        assert_eq!(manifest.plugins[1].version.as_deref(), Some("dev"));
        assert_eq!(
            manifest.plugins[2].source,
            PluginSource::GitHub {
                owner: "example".to_owned(),
                repo: "pinned".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_duplicate_plugins() {
        let error = r#"
plugin "same" {
    source "github:one/same"
}
plugin "same" {
    source "github:two/same"
}
"#
        .parse::<Manifest>()
        .unwrap_err();

        assert!(error.to_string().contains("declared more than once"));
    }

    #[test]
    fn rejects_unsafe_plugin_names() {
        let error = r#"
plugin "../escape" {
    source "github:one/escape"
}
"#
        .parse::<Manifest>()
        .unwrap_err();

        assert!(error.to_string().contains("may only contain"));
    }

    #[test]
    fn rejects_unknown_plugin_fields() {
        let error = r#"
plugin "example" {
    source "github:one/example"
    surprise "nope"
}
"#
        .parse::<Manifest>()
        .unwrap_err();

        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn upsert_replaces_in_place_and_serializes_valid_kdl() {
        let mut manifest: Manifest = r#"
plugin "one" {
    source "github:old/one"
}
plugin "two" {
    source "github:two/two"
}
"#
        .parse()
        .unwrap();

        manifest.upsert(PluginSpec {
            name: "one".to_owned(),
            source: PluginSource::Path("/tmp/a weird \"plugin\".wasm".to_owned()),
            version: None,
        });

        let mut document = KdlDocument::new();
        for plugin in manifest.plugins() {
            let mut node = KdlNode::new("plugin");
            node.entries_mut().push(KdlEntry::new(plugin.name.clone()));
            let children = node.ensure_children();
            push_string_node(children, "source", plugin.source.to_string());
            document.nodes_mut().push(node);
        }
        document.autoformat();

        let reparsed: Manifest = document.to_string().parse().unwrap();
        assert_eq!(reparsed, manifest);
        assert_eq!(reparsed.plugins()[0].name, "one");
        assert_eq!(reparsed.plugins()[1].name, "two");
    }
}
