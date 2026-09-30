use crate::{
    Lockfile, Manifest, PluginHistory, PluginSource, StoreError, StorePaths,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

const HASH_BUFFER_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Doctor {
    paths: StorePaths,
}

impl Doctor {
    pub fn new(paths: StorePaths) -> Self {
        Self { paths }
    }

    pub fn audit(&self) -> Result<DoctorReport, DoctorError> {
        let manifest = Manifest::load(&self.paths.manifest_path()).map_err(DoctorError::Manifest)?;
        let lockfile = Lockfile::load(&self.paths.lockfile_path()).map_err(DoctorError::Lockfile)?;
        let mut report = DoctorReport::default();

        for plugin in manifest.plugins() {
            report.checked_plugins += 1;

            let Some(locked) = lockfile
                .plugins()
                .iter()
                .find(|locked| locked.name == plugin.name)
            else {
                report.error(
                    "missing-lock",
                    Some(&plugin.name),
                    "manifest entry has no resolved lock state",
                );
                continue;
            };

            if locked.source != plugin.source {
                report.error(
                    "source-mismatch",
                    Some(&plugin.name),
                    format!(
                        "manifest source '{}' does not match lock source '{}'",
                        plugin.source, locked.source
                    ),
                );
            }

            if let Some(asset) = plugin.asset.as_deref()
                && asset != locked.asset
            {
                report.error(
                    "asset-mismatch",
                    Some(&plugin.name),
                    format!(
                        "manifest asset '{asset}' does not match lock asset '{}'",
                        locked.asset
                    ),
                );
            }

            if let Some(version) = plugin.version.as_deref()
                && locked.version.as_deref() != Some(version)
            {
                report.error(
                    "version-mismatch",
                    Some(&plugin.name),
                    format!(
                        "manifest pins '{version}' but lock resolves '{}'",
                        locked.version.as_deref().unwrap_or("<none>")
                    ),
                );
            }

            self.audit_locked_state(&mut report, locked)?;
            self.audit_history(&mut report, locked)?;
        }

        for locked in lockfile.plugins() {
            if !manifest
                .plugins()
                .iter()
                .any(|plugin| plugin.name == locked.name)
            {
                report.error(
                    "orphan-lock",
                    Some(&locked.name),
                    "lock entry has no corresponding manifest entry",
                );
            }
        }

        Ok(report)
    }

    fn audit_locked_state(
        &self,
        report: &mut DoctorReport,
        locked: &crate::LockedPlugin,
    ) -> Result<(), DoctorError> {
        let blob = self.paths.blob_path(&locked.sha256).map_err(DoctorError::Store)?;
        let version = self
            .paths
            .version_blob_path(&locked.name, &locked.sha256)
            .map_err(DoctorError::Store)?;
        let current = self.paths.current_plugin_path(&locked.name);

        let blob_observation = audit_digest_file(
            report,
            Some(&locked.name),
            "blob",
            &blob,
            &locked.sha256,
        );

        if let Some(observation) = &blob_observation
            && observation.bytes != locked.bytes
        {
            report.error(
                "byte-count-mismatch",
                Some(&locked.name),
                format!(
                    "blob is {} bytes but lock records {} bytes",
                    observation.bytes, locked.bytes
                ),
            );
        }

        let version_observation = audit_digest_file(
            report,
            Some(&locked.name),
            "version",
            &version,
            &locked.sha256,
        );

        let current_observation = audit_digest_file(
            report,
            Some(&locked.name),
            "current",
            &current,
            &locked.sha256,
        );

        #[cfg(unix)]
        {
            if blob_observation.is_some() && version_observation.is_some() {
                audit_same_inode(report, &locked.name, "version", &blob, &version);
            }

            if blob_observation.is_some() && current_observation.is_some() {
                audit_same_inode(report, &locked.name, "current", &blob, &current);
            }
        }

        Ok(())
    }

    fn audit_history(
        &self,
        report: &mut DoctorReport,
        locked: &crate::LockedPlugin,
    ) -> Result<(), DoctorError> {
        let history_path = self.paths.history_path(&locked.name);

        match fs::symlink_metadata(&history_path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                report.error(
                    "history-not-file",
                    Some(&locked.name),
                    format!("history path is not a regular file: {}", history_path.display()),
                );
                return Ok(());
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                report.error(
                    "history-metadata",
                    Some(&locked.name),
                    format!("could not inspect history file: {error}"),
                );
                return Ok(());
            }
        }

        let history = match PluginHistory::load(&history_path, &locked.name) {
            Ok(history) => history,
            Err(error) => {
                report.error(
                    "history-invalid",
                    Some(&locked.name),
                    error.to_string(),
                );
                return Ok(());
            }
        };

        if let Some(head) = history.entries().last()
            && head != locked
        {
            report.error(
                "history-head-mismatch",
                Some(&locked.name),
                format!(
                    "history ends at {} but lock resolves {}",
                    short_sha(&head.sha256),
                    short_sha(&locked.sha256)
                ),
            );
        }

        let mut checked = BTreeSet::new();
        checked.insert(locked.sha256.to_ascii_lowercase());

        for entry in history.entries() {
            let digest = entry.sha256.to_ascii_lowercase();

            if !checked.insert(digest.clone()) {
                continue;
            }

            let blob = self.paths.blob_path(&digest).map_err(DoctorError::Store)?;
            let version = self
                .paths
                .version_blob_path(&locked.name, &digest)
                .map_err(DoctorError::Store)?;

            audit_digest_file(
                report,
                Some(&locked.name),
                "historical blob",
                &blob,
                &digest,
            );
            audit_digest_file(
                report,
                Some(&locked.name),
                "historical version",
                &version,
                &digest,
            );
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DoctorSeverity {
    Warning,
    Error,
}

impl DoctorSeverity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Warning => "WARN",
            Self::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorIssue {
    pub severity: DoctorSeverity,
    pub code: &'static str,
    pub plugin: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DoctorReport {
    checked_plugins: usize,
    issues: Vec<DoctorIssue>,
}

impl DoctorReport {
    pub fn checked_plugins(&self) -> usize {
        self.checked_plugins
    }

    pub fn issues(&self) -> &[DoctorIssue] {
        &self.issues
    }

    pub fn error_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity == DoctorSeverity::Error)
            .count()
    }

    pub fn warning_count(&self) -> usize {
        self.issues
            .iter()
            .filter(|issue| issue.severity == DoctorSeverity::Warning)
            .count()
    }

    pub fn is_healthy(&self) -> bool {
        self.issues.is_empty()
    }

    fn warning(
        &mut self,
        code: &'static str,
        plugin: Option<&str>,
        message: impl Into<String>,
    ) {
        self.issues.push(DoctorIssue {
            severity: DoctorSeverity::Warning,
            code,
            plugin: plugin.map(str::to_owned),
            message: message.into(),
        });
    }

    fn error(
        &mut self,
        code: &'static str,
        plugin: Option<&str>,
        message: impl Into<String>,
    ) {
        self.issues.push(DoctorIssue {
            severity: DoctorSeverity::Error,
            code,
            plugin: plugin.map(str::to_owned),
            message: message.into(),
        });
    }
}

#[derive(Debug)]
pub enum DoctorError {
    Manifest(crate::ManifestError),
    Lockfile(crate::LockfileError),
    Store(StoreError),
}

impl fmt::Display for DoctorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest(source) => source.fmt(formatter),
            Self::Lockfile(source) => source.fmt(formatter),
            Self::Store(source) => source.fmt(formatter),
        }
    }
}

impl Error for DoctorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Manifest(source) => Some(source),
            Self::Lockfile(source) => Some(source),
            Self::Store(source) => Some(source),
        }
    }
}

#[derive(Debug)]
struct FileObservation {
    bytes: u64,
}

fn audit_digest_file(
    report: &mut DoctorReport,
    plugin: Option<&str>,
    role: &str,
    path: &Path,
    expected_sha256: &str,
) -> Option<FileObservation> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            report.error(
                "file-missing",
                plugin,
                format!("{role} file is missing: {}", path.display()),
            );
            return None;
        }
        Err(error) => {
            report.error(
                "file-metadata",
                plugin,
                format!("could not inspect {role} file {}: {error}", path.display()),
            );
            return None;
        }
    };

    if !metadata.file_type().is_file() {
        report.error(
            "file-not-regular",
            plugin,
            format!("{role} path is not a regular file: {}", path.display()),
        );
        return None;
    }

    let actual = match sha256_file(path) {
        Ok(actual) => actual,
        Err(error) => {
            report.error(
                "file-read",
                plugin,
                format!("could not hash {role} file {}: {error}", path.display()),
            );
            return None;
        }
    };

    if !actual.eq_ignore_ascii_case(expected_sha256) {
        report.error(
            "checksum-mismatch",
            plugin,
            format!(
                "{role} file {} hashes to {} but expected {}",
                path.display(),
                short_sha(&actual),
                short_sha(expected_sha256)
            ),
        );
    }

    Some(FileObservation {
        bytes: metadata.len(),
    })
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];

    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };

        hasher.update(&buffer[..read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(unix)]
fn audit_same_inode(
    report: &mut DoctorReport,
    plugin: &str,
    role: &str,
    blob: &Path,
    other: &Path,
) {
    use std::os::unix::fs::MetadataExt;

    let Ok(blob_metadata) = fs::metadata(blob) else {
        return;
    };
    let Ok(other_metadata) = fs::metadata(other) else {
        return;
    };

    if blob_metadata.dev() != other_metadata.dev() || blob_metadata.ino() != other_metadata.ino() {
        report.warning(
            "not-hardlinked",
            Some(plugin),
            format!(
                "{role} file has verified bytes but is not hard-linked to the content blob"
            ),
        );
    }
}

fn short_sha(sha256: &str) -> &str {
    sha256.get(..12).unwrap_or(sha256)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Installer, LockedPlugin, PluginSpec};
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn test_paths() -> (std::path::PathBuf, StorePaths) {
        let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!(
            "zjpm-doctor-test-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let paths = StorePaths::from_roots(root.join("config"), root.join("data"));
        (root, paths)
    }

    fn minimal_wasm(marker: &[u8]) -> Vec<u8> {
        let mut wasm = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

        if !marker.is_empty() {
            wasm.push(0x00);
            wasm.push((marker.len() + 1) as u8);
            wasm.push(marker.len() as u8);
            wasm.extend_from_slice(marker);
        }

        wasm
    }

    fn install_state(paths: &StorePaths, name: &str) -> LockedPlugin {
        let installer = Installer::new(paths.clone());
        let receipt = installer
            .install_reader(name, minimal_wasm(name.as_bytes()).as_slice())
            .unwrap();
        let source = PluginSource::Path(format!("/tmp/{name}.wasm"));

        let manifest = Manifest::load(&paths.manifest_path()).unwrap();
        let mut manifest = manifest;
        manifest.upsert(PluginSpec {
            name: name.to_owned(),
            source: source.clone(),
            version: None,
            asset: None,
        });
        manifest.save_atomic(&paths.manifest_path()).unwrap();

        let locked = LockedPlugin {
            name: name.to_owned(),
            source,
            version: None,
            asset: format!("{name}.wasm"),
            sha256: receipt.sha256,
            bytes: receipt.bytes,
        };

        let mut lockfile = Lockfile::load(&paths.lockfile_path()).unwrap();
        lockfile.upsert(locked.clone());
        lockfile.save_atomic(&paths.lockfile_path()).unwrap();

        locked
    }

    fn has_code(report: &DoctorReport, code: &str) -> bool {
        report.issues().iter().any(|issue| issue.code == code)
    }

    #[test]
    fn healthy_managed_plugin_passes() {
        let (root, paths) = test_paths();
        install_state(&paths, "clock");

        let report = Doctor::new(paths).audit().unwrap();

        assert!(report.is_healthy());
        assert_eq!(report.checked_plugins(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detects_missing_lock_state() {
        let (root, paths) = test_paths();
        let mut manifest = Manifest::default();
        manifest.upsert(PluginSpec {
            name: "clock".to_owned(),
            source: PluginSource::Path("/tmp/clock.wasm".to_owned()),
            version: None,
            asset: None,
        });
        manifest.save_atomic(&paths.manifest_path()).unwrap();

        let report = Doctor::new(paths).audit().unwrap();

        assert!(has_code(&report, "missing-lock"));
        assert_eq!(report.error_count(), 1);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detects_current_file_drift() {
        let (root, paths) = test_paths();
        let locked = install_state(&paths, "clock");
        let current = paths.current_plugin_path("clock");

        fs::remove_file(&current).unwrap();
        fs::write(&current, minimal_wasm(b"different")).unwrap();

        let report = Doctor::new(paths).audit().unwrap();

        assert!(has_code(&report, "checksum-mismatch"));
        assert!(report.error_count() >= 1);

        assert_eq!(
            sha256_file(&paths.blob_path(&locked.sha256).unwrap()).unwrap(),
            locked.sha256
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detects_orphan_lock_entry() {
        let (root, paths) = test_paths();
        let source = PluginSource::Path("/tmp/orphan.wasm".to_owned());
        let mut lockfile = Lockfile::default();
        lockfile.upsert(LockedPlugin {
            name: "orphan".to_owned(),
            source,
            version: None,
            asset: "orphan.wasm".to_owned(),
            sha256: "a".repeat(64),
            bytes: 8,
        });
        lockfile.save_atomic(&paths.lockfile_path()).unwrap();

        let report = Doctor::new(paths).audit().unwrap();

        assert!(has_code(&report, "orphan-lock"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn detects_history_head_mismatch() {
        let (root, paths) = test_paths();
        let current = install_state(&paths, "clock");
        let mut history = PluginHistory::default();
        let mut stale = current.clone();
        stale.sha256 = "b".repeat(64);
        history.record(stale);
        history.save_atomic(&paths.history_path("clock")).unwrap();

        let report = Doctor::new(paths).audit().unwrap();

        assert!(has_code(&report, "history-head-mismatch"));

        let _ = fs::remove_dir_all(root);
    }
}
