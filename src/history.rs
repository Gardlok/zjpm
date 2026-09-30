use crate::fsutil::write_atomic;
use crate::manifest::{PluginSource, validate_plugin_name};
use crate::lockfile::LockedPlugin;
use kdl::{KdlDocument, KdlEntry, KdlNode};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginHistory {
    entries: Vec<LockedPlugin>,
}

impl PluginHistory {
    pub fn load(path: &Path, plugin_name: &str) -> Result<Self, HistoryError> {
        validate_plugin_name(plugin_name).map_err(HistoryError::Schema)?;

        match fs::read_to_string(path) {
            Ok(input) => Self::parse(&input, plugin_name),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(HistoryError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    pub fn entries(&self) -> &[LockedPlugin] {
        &self.entries
    }

    pub fn record(&mut self, state: LockedPlugin) -> bool {
        if self.entries.last() == Some(&state) {
            return false;
        }

        self.entries.push(state);
        true
    }

    pub fn previous_distinct(&self, current_sha256: &str) -> Option<&LockedPlugin> {
        self.entries
            .iter()
            .rev()
            .find(|entry| !entry.sha256.eq_ignore_ascii_case(current_sha256))
    }

    pub fn save_atomic(&self, path: &Path) -> Result<(), HistoryError> {
        let mut document = KdlDocument::new();

        for entry in &self.entries {
            let mut node = KdlNode::new("state");
            node.entries_mut().push(KdlEntry::new(entry.name.clone()));
            let children = node.ensure_children();

            push_string_node(children, "source", entry.source.to_string());

            if let Some(version) = &entry.version {
                push_string_node(children, "version", version.clone());
            }

            push_string_node(children, "asset", entry.asset.clone());
            push_string_node(children, "sha256", entry.sha256.clone());
            push_integer_node(children, "bytes", entry.bytes as i128);

            document.nodes_mut().push(node);
        }

        document.autoformat();
        write_atomic(path, document.to_string().as_bytes()).map_err(|source| {
            HistoryError::Write {
                path: path.to_path_buf(),
                source,
            }
        })
    }

    fn parse(input: &str, plugin_name: &str) -> Result<Self, HistoryError> {
        let document = input.parse::<KdlDocument>().map_err(HistoryError::Parse)?;
        let mut entries = Vec::with_capacity(document.nodes().len());

        for node in document.nodes() {
            if node.name().value() != "state" {
                return Err(HistoryError::Schema(format!(
                    "unknown history node '{}'; expected 'state'",
                    node.name().value()
                )));
            }

            let entry = parse_state(node)?;

            if entry.name != plugin_name {
                return Err(HistoryError::Schema(format!(
                    "history for '{plugin_name}' contains state for '{}'",
                    entry.name
                )));
            }

            entries.push(entry);
        }

        Ok(Self { entries })
    }
}

#[derive(Debug)]
pub enum HistoryError {
    Read { path: PathBuf, source: io::Error },
    Write { path: PathBuf, source: io::Error },
    Parse(kdl::KdlError),
    Schema(String),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(formatter, "could not read {}: {source}", path.display())
            }
            Self::Write { path, source } => {
                write!(formatter, "could not write {}: {source}", path.display())
            }
            Self::Parse(source) => write!(formatter, "could not parse plugin history: {source}"),
            Self::Schema(message) => write!(formatter, "invalid plugin history: {message}"),
        }
    }
}

impl Error for HistoryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } | Self::Write { source, .. } => Some(source),
            Self::Parse(source) => Some(source),
            Self::Schema(_) => None,
        }
    }
}

fn parse_state(node: &KdlNode) -> Result<LockedPlugin, HistoryError> {
    let name = positional_string(node, "history plugin name")?.to_owned();
    validate_plugin_name(&name).map_err(HistoryError::Schema)?;

    let children = node.children().ok_or_else(|| {
        HistoryError::Schema(format!("history state for '{name}' is missing its state block"))
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
                source = Some(value.parse::<PluginSource>().map_err(|error| {
                    HistoryError::Schema(format!("history state for '{name}': {error}"))
                })?);
            }
            "version" => {
                set_once(&mut version, "version", &name)?;
                let value = scalar_string(child, "version")?;

                if value.trim().is_empty() {
                    return Err(HistoryError::Schema(format!(
                        "history state for '{name}' has an empty version"
                    )));
                }

                version = Some(value.to_owned());
            }
            "asset" => {
                set_once(&mut asset, "asset", &name)?;
                let value = scalar_string(child, "asset")?;

                if value.trim().is_empty() {
                    return Err(HistoryError::Schema(format!(
                        "history state for '{name}' has an empty asset"
                    )));
                }

                asset = Some(value.to_owned());
            }
            "sha256" => {
                set_once(&mut sha256, "sha256", &name)?;
                let digest = scalar_string(child, "sha256")?.to_ascii_lowercase();

                if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(HistoryError::Schema(format!(
                        "history state for '{name}' has an invalid SHA-256"
                    )));
                }

                sha256 = Some(digest);
            }
            "bytes" => {
                set_once(&mut bytes, "bytes", &name)?;
                let value = scalar_integer(child, "bytes")?;

                if value < 0 || value > u64::MAX as i128 {
                    return Err(HistoryError::Schema(format!(
                        "history state for '{name}' has an invalid byte count"
                    )));
                }

                bytes = Some(value as u64);
            }
            other => {
                return Err(HistoryError::Schema(format!(
                    "history state for '{name}' has unknown field '{other}'"
                )));
            }
        }
    }

    Ok(LockedPlugin {
        name,
        source: source
            .ok_or_else(|| HistoryError::Schema("history state is missing source".to_owned()))?,
        version,
        asset: asset
            .ok_or_else(|| HistoryError::Schema("history state is missing asset".to_owned()))?,
        sha256: sha256
            .ok_or_else(|| HistoryError::Schema("history state is missing sha256".to_owned()))?,
        bytes: bytes
            .ok_or_else(|| HistoryError::Schema("history state is missing bytes".to_owned()))?,
    })
}

fn set_once<T>(slot: &mut Option<T>, field: &str, plugin: &str) -> Result<(), HistoryError> {
    if slot.is_some() {
        return Err(HistoryError::Schema(format!(
            "history state for '{plugin}' has more than one {field}"
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

fn positional_string<'a>(node: &'a KdlNode, label: &str) -> Result<&'a str, HistoryError> {
    if node.entries().len() != 1 || node.entries()[0].name().is_some() {
        return Err(HistoryError::Schema(format!(
            "{label} must be one string"
        )));
    }

    node.entries()[0]
        .value()
        .as_string()
        .ok_or_else(|| HistoryError::Schema(format!("{label} must be a string")))
}

fn scalar_string<'a>(node: &'a KdlNode, label: &str) -> Result<&'a str, HistoryError> {
    if node.children().is_some() {
        return Err(HistoryError::Schema(format!(
            "{label} cannot have a child block"
        )));
    }

    positional_string(node, label)
}

fn scalar_integer(node: &KdlNode, label: &str) -> Result<i128, HistoryError> {
    if node.children().is_some() || node.entries().len() != 1 || node.entries()[0].name().is_some()
    {
        return Err(HistoryError::Schema(format!(
            "{label} must be one integer"
        )));
    }

    node.entries()[0]
        .value()
        .as_integer()
        .ok_or_else(|| HistoryError::Schema(format!("{label} must be an integer")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn state(version: &str, digest: char) -> LockedPlugin {
        LockedPlugin {
            name: "clock".to_owned(),
            source: PluginSource::GitHub {
                owner: "owner".to_owned(),
                repo: "clock".to_owned(),
            },
            version: Some(version.to_owned()),
            asset: "clock.wasm".to_owned(),
            sha256: digest.to_string().repeat(64),
            bytes: 42,
        }
    }

    #[test]
    fn suppresses_consecutive_duplicate_states() {
        let mut history = PluginHistory::default();
        let first = state("v1", 'a');

        assert!(history.record(first.clone()));
        assert!(!history.record(first));
        assert_eq!(history.entries().len(), 1);
    }

    #[test]
    fn finds_the_previous_distinct_digest() {
        let mut history = PluginHistory::default();
        history.record(state("v1", 'a'));
        history.record(state("v2", 'b'));
        history.record(state("v2-republished", 'b'));

        let previous = history.previous_distinct(&"b".repeat(64)).unwrap();

        assert_eq!(previous.sha256, "a".repeat(64));
    }

    #[test]
    fn preserves_real_rollback_transitions() {
        let mut history = PluginHistory::default();
        history.record(state("v1", 'a'));
        history.record(state("v2", 'b'));
        history.record(state("v1", 'a'));

        assert_eq!(history.entries().len(), 3);
        assert_eq!(
            history.previous_distinct(&"a".repeat(64)).unwrap().sha256,
            "b".repeat(64)
        );
    }

    #[test]
    fn roundtrips_history_atomically() {
        let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "zjpm-history-test-{}-{sequence}",
            std::process::id()
        ));
        let path = root.join("clock").join("history.kdl");

        let mut history = PluginHistory::default();
        history.record(state("v1", 'a'));
        history.record(state("v2", 'b'));
        history.save_atomic(&path).unwrap();

        let loaded = PluginHistory::load(&path, "clock").unwrap();

        assert_eq!(loaded, history);

        let _ = fs::remove_dir_all(root);
    }
}
