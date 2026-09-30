use crate::fsutil::write_atomic;
use crate::manifest::{validate_plugin_name, PluginSource};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::str::FromStr;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lockfile {
    plugins: Vec<LockedPlugin>,
}

impl Lockfile {
    pub fn load(path: &Path) -> Result<Self, LockfileError> {
        match fs::read_to_string(path) {
            Ok(input) => input.parse(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(LockfileError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn plugins(&self) -> &[LockedPlugin] {
        &self.plugins
    }

    pub fn upsert(&mut self, plugin: LockedPlugin) {
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

    pub fn save_atomic(&self, path: &Path) -> Result<(), LockfileError> {
        let mut document = KdlDocument::new();

        for plugin in &self.plugins {
            let mut node = KdlNode::new("plugin");
            node.entries_mut().push(KdlEntry::new(plugin.name.clone()));
            let children = node.ensure_children();

            push_string_node(children, "source", plugin.source.to_string());

            if let Some(version) = &plugin.version {
                push_string_node(children, "version", version.clone());
            }

            push_string_node(children, "asset", plugin.asset.clone());
            push_string_node(children, "sha256", plugin.sha256.clone());
            push_integer_node(children, "bytes", plugin.bytes as i128);

            document.nodes_mut().push(node);
        }

        document.autoformat();
        write_atomic(path, document.to_string().as_bytes()).map_err(|source| {
            LockfileError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }
}

impl FromStr for Lockfile {
    type Err = LockfileError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let document = input.parse::<KdlDocument>().map_err(LockfileError::Parse)?;
        let mut plugins = Vec::with_capacity(document.nodes().len());
        let mut names = BTreeSet::new();

        for node in document.nodes() {
            if node.name().value() != "plugin" {
                return Err(LockfileError::Schema(format!(
                    "unknown top-level node '{}'; expected 'plugin'",
                    node.name().value()
                )));
            }

            let plugin = parse_locked_plugin(node)?;

            if !names.insert(plugin.name.clone()) {
                return Err(LockfileError::Schema(format!(
                    "plugin '{}' is locked more than once",
                    plugin.name
                )));
            }

            plugins.push(plugin);
        }

        Ok(Self { plugins })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedPlugin {
    pub name: String,
    pub source: PluginSource,
    pub version: Option<String>,
    pub asset: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Debug)]
pub enum LockfileError {
    Read { path: PathBuf, source: io::Error },
    Write { path: PathBuf, source: io::Error },
    Parse(kdl::KdlError),
    Schema(String),
}

impl fmt::Display for LockfileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "could not write {}: {source}", path.display())
            }
            Self::Parse(source) => write!(formatter, "could not parse lockfile: {source}"),
            Self::Schema(message) => write!(formatter, "invalid lockfile: {message}"),
        }
    }
}

impl Error for LockfileError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Schema(_) => None,
        }
    }
}

fn parse_locked_plugin(node: &KdlNode) -> Result<LockedPlugin, LockfileError> {
    let name = positional_string(node, "plugin name")?.to_owned();
    validate_plugin_name(&name).map_err(LockfileError::Schema)?;

    let children = node.children().ok_or_else(|| {
        LockfileError::Schema(format!("plugin '{name}' is missing its lock block"))
    })?;

    let mut source = None;
    let mut version = None;
    let mut asset = None;
    let mut sha256 = None;
    let mut bytes = None;

    for child in children.nodes() {
        match child.name().value() {
            "source" => {
                set_once(&mut source, "source", &name)?;
                let value = scalar_string(child, "source")?;
                source = Some(
                    value
                        .parse()
                        .map_err(|error: String| LockfileError::Schema(format!("plugin '{name}': {error}")))?,
                );
            }
            "version" => {
                set_once(&mut version, "version", &name)?;
                let value = scalar_string(child, "version")?;
                if value.trim().is_empty() {
                    return Err(LockfileError::Schema(format!(
                        "plugin '{name}' has an empty version"
                    )));
                }
                version = Some(value.to_owned());
            }
            "asset" => {
                set_once(&mut asset, "asset", &name)?;
                asset = Some(scalar_string(child, "asset")?.to_owned());
            }
            "sha256" => {
                set_once(&mut sha256, "sha256", &name)?;
                let digest = scalar_string(child, "sha256")?.to_ascii_lowercase();
                if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(LockfileError::Schema(format!(
                        "plugin '{name}' has an invalid SHA-256"
                    )));
                }
                sha256 = Some(digest);
            }
            "bytes" => {
                set_once(&mut bytes, "bytes", &name)?;
                let value = scalar_integer(child, "bytes")?;
                if value < 0 || value > u64::MAX as i128 {
                    return Err(LockfileError::Schema(format!(
                        "plugin '{name}' has an invalid byte count"
                    )));
                }
                bytes = Some(value as u64);
            }
            other => {
                return Err(LockfileError::Schema(format!(
                    "plugin '{name}' has unknown lock field '{other}'"
                )));
            }
        }
    }

    Ok(LockedPlugin {
        name,
        source: source
            .ok_or_else(|| LockfileError::Schema("locked plugin is missing source".to_owned()))?,
        version,
        asset: asset
            .ok_or_else(|| LockfileError::Schema("locked plugin is missing asset".to_owned()))?,
        sha256: sha256
            .ok_or_else(|| LockfileError::Schema("locked plugin is missing sha256".to_owned()))?,
        bytes: bytes
            .ok_or_else(|| LockfileError::Schema("locked plugin is missing bytes".to_owned()))?,
    })
}

fn set_once<T>(slot: &mut Option<T>, field: &str, plugin: &str) -> Result<(), LockfileError> {
    if slot.is_some() {
        return Err(LockfileError::Schema(format!(
            "plugin '{plugin}' has more than one {field}"
        )));
    }
    Ok(())
}

fn push_string_node(document: &mut KdlDocument, name: &str, value: String) {
    let mut node = KdlNode::new(name);
    node.entries_mut().push(KdlEntry::new(value));
    document.nodes_mut().push(node);
}

fn push_integer_node(document: &mut KdlDocument, name: &str, value: i128) {
    let mut node = KdlNode::new(name);
    node.entries_mut().push(KdlEntry::new(value));
    document.nodes_mut().push(node);
}

fn positional_string<'a>(node: &'a KdlNode, label: &str) -> Result<&'a str, LockfileError> {
    if node.entries().len() != 1 || node.entries()[0].name().is_some() {
        return Err(LockfileError::Schema(format!(
            "{label} must be one quoted string"
        )));
    }

    node.entries()[0]
        .value()
        .as_string()
        .ok_or_else(|| LockfileError::Schema(format!("{label} must be a string")))
}

fn scalar_string<'a>(node: &'a KdlNode, label: &str) -> Result<&'a str, LockfileError> {
    if node.children().is_some() {
        return Err(LockfileError::Schema(format!(
            "{label} cannot have a child block"
        )));
    }
    positional_string(node, label)
}

fn scalar_integer(node: &KdlNode, label: &str) -> Result<i128, LockfileError> {
    if node.children().is_some()
        || node.entries().len() != 1
        || node.entries()[0].name().is_some()
    {
        return Err(LockfileError::Schema(format!(
            "{label} must be one integer"
        )));
    }

    node.entries()[0]
        .value()
        .as_integer()
        .ok_or_else(|| LockfileError::Schema(format!("{label} must be an integer")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_local_plugin_lock_state() {
        let plugin = LockedPlugin {
            name: "clock".to_owned(),
            source: PluginSource::Path("/tmp/my clock.wasm".to_owned()),
            version: None,
            asset: "my clock.wasm".to_owned(),
            sha256: "a".repeat(64),
            bytes: 42,
        };
        let mut lockfile = Lockfile::default();
        lockfile.upsert(plugin.clone());

        let mut document = KdlDocument::new();
        let mut node = KdlNode::new("plugin");
        node.entries_mut().push(KdlEntry::new(plugin.name.clone()));
        let children = node.ensure_children();
        push_string_node(children, "source", plugin.source.to_string());
        push_string_node(children, "asset", plugin.asset.clone());
        push_string_node(children, "sha256", plugin.sha256.clone());
        push_integer_node(children, "bytes", plugin.bytes as i128);
        document.nodes_mut().push(node);
        document.autoformat();

        let reparsed: Lockfile = document.to_string().parse().unwrap();
        assert_eq!(reparsed, lockfile);
    }

    #[test]
    fn upsert_replaces_existing_lock_entry() {
        let old: Lockfile = format!(
            "plugin \"clock\" {{\n source \"path:/old\"\n asset \"old.wasm\"\n sha256 \"{}\"\n bytes 1\n}}\n",
            "a".repeat(64)
        )
        .parse()
        .unwrap();

        let mut lockfile = old;
        lockfile.upsert(LockedPlugin {
            name: "clock".to_owned(),
            source: PluginSource::Path("/new".to_owned()),
            version: None,
            asset: "new.wasm".to_owned(),
            sha256: "b".repeat(64),
            bytes: 2,
        });

        assert_eq!(lockfile.plugins().len(), 1);
        assert_eq!(lockfile.plugins()[0].sha256, "b".repeat(64));
    }
}
