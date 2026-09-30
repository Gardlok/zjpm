use crate::manifest::validate_plugin_name;
use crate::{BlobReceipt, ContentStore, ContentStoreError, StorePaths};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static ACTIVATION_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct Installer {
    store: ContentStore,
}

impl Installer {
    pub fn new(paths: StorePaths) -> Self {
        Self {
            store: ContentStore::new(paths),
        }
    }

    pub fn store(&self) -> &ContentStore {
        &self.store
    }

    pub fn install_path(&self, name: &str, source: &Path) -> Result<InstallReceipt, InstallError> {
        validate_plugin_name(name).map_err(InstallError::InvalidPluginName)?;
        let blob = self.store.ingest_wasm_path(source)?;
        self.activate_ingested(name, blob)
    }

    pub fn install_reader<R: Read>(
        &self,
        name: &str,
        reader: R,
    ) -> Result<InstallReceipt, InstallError> {
        validate_plugin_name(name).map_err(InstallError::InvalidPluginName)?;
        let blob = self.store.ingest_wasm(reader)?;
        self.activate_ingested(name, blob)
    }

    pub fn activate_ingested(
        &self,
        name: &str,
        blob: BlobReceipt,
    ) -> Result<InstallReceipt, InstallError> {
        validate_plugin_name(name).map_err(InstallError::InvalidPluginName)?;
        let version_path = self
            .store
            .paths()
            .version_blob_path(name, &blob.sha256)?;
        let current_path = self.activate_verified_blob(name, &blob.sha256)?;

        Ok(InstallReceipt {
            name: name.to_owned(),
            sha256: blob.sha256,
            bytes: blob.bytes,
            blob_reused: blob.reused,
            version_path,
            current_path,
        })
    }

    pub fn activate_blob(&self, name: &str, sha256: &str) -> Result<PathBuf, InstallError> {
        validate_plugin_name(name).map_err(InstallError::InvalidPluginName)?;

        if !self.store.verify_blob(sha256)? {
            return Err(InstallError::BlobUnavailable {
                sha256: sha256.to_ascii_lowercase(),
            });
        }

        self.activate_verified_blob(name, sha256)
    }

    fn activate_verified_blob(&self, name: &str, sha256: &str) -> Result<PathBuf, InstallError> {
        let paths = self.store.paths();
        let blob_path = paths.blob_path(sha256)?;
        let version_path = paths.version_blob_path(name, sha256)?;
        let current_path = paths.current_plugin_path(name);

        let versions_dir = paths.versions_dir(name);
        let plugin_dir = paths.plugin_dir(name);

        fs::create_dir_all(&versions_dir).map_err(|source| InstallError::Io {
            operation: "create plugin versions directory",
            path: versions_dir.clone(),
            source,
        })?;

        replace_hard_link(&blob_path, &version_path)?;
        sync_directory(&versions_dir).map_err(|source| InstallError::Io {
            operation: "sync plugin versions directory",
            path: versions_dir,
            source,
        })?;

        replace_hard_link(&blob_path, &current_path)?;
        sync_directory(&plugin_dir).map_err(|source| InstallError::Io {
            operation: "sync plugin directory",
            path: plugin_dir,
            source,
        })?;

        Ok(current_path)
    }

}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReceipt {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
    pub blob_reused: bool,
    pub version_path: PathBuf,
    pub current_path: PathBuf,
}

#[derive(Debug)]
pub enum InstallError {
    InvalidPluginName(String),
    BlobUnavailable {
        sha256: String,
    },
    Store(ContentStoreError),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPluginName(message) => write!(formatter, "{message}"),
            Self::BlobUnavailable { sha256 } => {
                write!(formatter, "verified plugin blob is unavailable: {sha256}")
            }
            Self::Store(error) => error.fmt(formatter),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
        }
    }
}

impl Error for InstallError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::InvalidPluginName(_) | Self::BlobUnavailable { .. } => None,
        }
    }
}

impl From<ContentStoreError> for InstallError {
    fn from(error: ContentStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<crate::StoreError> for InstallError {
    fn from(error: crate::StoreError) -> Self {
        Self::Store(ContentStoreError::Store(error))
    }
}

fn replace_hard_link(source: &Path, target: &Path) -> Result<(), InstallError> {
    let directory = target
        .parent()
        .expect("activation paths always have a parent");
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("plugin");

    for _ in 0..1024 {
        let sequence = ACTIVATION_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temporary = directory.join(format!(
            ".{file_name}.{}-{sequence}.part",
            std::process::id()
        ));

        match fs::hard_link(source, &temporary) {
            Ok(()) => match fs::rename(&temporary, target) {
                Ok(()) => return Ok(()),
                Err(source) => {
                    let _ = fs::remove_file(&temporary);
                    return Err(InstallError::Io {
                        operation: "activate plugin",
                        path: target.to_path_buf(),
                        source,
                    });
                }
            },
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(InstallError::Io {
                    operation: "create plugin hard link",
                    path: temporary,
                    source,
                });
            }
        }
    }

    Err(InstallError::Io {
        operation: "allocate activation path",
        path: target.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique activation path",
        ),
    })
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "zjpm-install-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_installer() -> (TestDir, Installer) {
        let root = TestDir::new();
        let paths = StorePaths::from_roots(root.path.join("config"), root.path.join("data"));
        let installer = Installer::new(paths);
        (root, installer)
    }

    fn empty_wasm() -> Vec<u8> {
        vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
    }

    fn tagged_wasm(tag: u8) -> Vec<u8> {
        vec![
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x00, 0x03, 0x01, b'z', tag,
        ]
    }

    #[test]
    fn installs_a_verified_blob_and_activates_it() {
        let (_root, installer) = test_installer();
        let wasm = tagged_wasm(1);

        let receipt = installer.install_reader("clock", wasm.as_slice()).unwrap();

        assert!(receipt.version_path.is_file());
        assert!(receipt.current_path.is_file());
        assert_eq!(fs::read(&receipt.current_path).unwrap(), wasm);
        assert_eq!(
            fs::read(&receipt.version_path).unwrap(),
            fs::read(&receipt.current_path).unwrap()
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;

            let blob = installer
                .store()
                .paths()
                .blob_path(&receipt.sha256)
                .unwrap();
            let blob_meta = fs::metadata(blob).unwrap();
            let version_meta = fs::metadata(&receipt.version_path).unwrap();
            let current_meta = fs::metadata(&receipt.current_path).unwrap();

            assert_eq!(blob_meta.dev(), version_meta.dev());
            assert_eq!(blob_meta.ino(), version_meta.ino());
            assert_eq!(blob_meta.dev(), current_meta.dev());
            assert_eq!(blob_meta.ino(), current_meta.ino());
        }
    }

    #[test]
    fn reinstalling_the_same_bytes_reuses_the_blob() {
        let (_root, installer) = test_installer();
        let wasm = empty_wasm();

        let first = installer.install_reader("empty", wasm.as_slice()).unwrap();
        let second = installer.install_reader("empty", wasm.as_slice()).unwrap();

        assert!(!first.blob_reused);
        assert!(second.blob_reused);
        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.current_path, second.current_path);
    }

    #[test]
    fn activating_new_bytes_keeps_the_old_version_available() {
        let (_root, installer) = test_installer();

        let first = installer
            .install_reader("switcher", tagged_wasm(1).as_slice())
            .unwrap();
        let first_bytes = fs::read(&first.version_path).unwrap();

        let second = installer
            .install_reader("switcher", tagged_wasm(2).as_slice())
            .unwrap();

        assert_ne!(first.sha256, second.sha256);
        assert_eq!(fs::read(&first.version_path).unwrap(), first_bytes);
        assert_eq!(
            fs::read(&second.current_path).unwrap(),
            fs::read(&second.version_path).unwrap()
        );
        assert_ne!(
            fs::read(&first.version_path).unwrap(),
            fs::read(&second.current_path).unwrap()
        );
    }

    #[test]
    fn invalid_name_is_rejected_before_blob_ingestion() {
        let (_root, installer) = test_installer();

        let error = installer
            .install_reader("../escape", empty_wasm().as_slice())
            .unwrap_err();

        assert!(matches!(error, InstallError::InvalidPluginName(_)));
        assert!(!installer.store().paths().blobs_dir().exists());
    }

    #[test]
    fn invalid_wasm_never_creates_a_plugin_activation() {
        let (_root, installer) = test_installer();

        let error = installer
            .install_reader("bad", b"not wasm".as_slice())
            .unwrap_err();

        assert!(matches!(error, InstallError::Store(_)));
        assert!(!installer.store().paths().plugin_dir("bad").exists());
    }

    #[test]
    fn missing_blob_cannot_be_activated() {
        let (_root, installer) = test_installer();
        let digest = "0".repeat(64);

        let error = installer.activate_blob("missing", &digest).unwrap_err();

        assert!(matches!(error, InstallError::BlobUnavailable { .. }));
        assert!(!installer.store().paths().plugin_dir("missing").exists());
    }
}
