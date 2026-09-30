use sha2::{Digest, Sha256};
use std::env;
use std::error::Error;
use std::fmt;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use wasmparser::{Chunk, FuncValidatorAllocations, Parser, ValidPayload, Validator};

const WASM_MAGIC: [u8; 4] = [0x00, 0x61, 0x73, 0x6d];
const COPY_BUFFER_SIZE: usize = 64 * 1024;
static STAGING_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorePaths {
    config_dir: PathBuf,
    data_dir: PathBuf,
}

impl StorePaths {
    pub fn discover() -> Result<Self, StoreError> {
        let config_dir = env::var_os("ZJPM_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("XDG_CONFIG_HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join("zjpm"))
            })
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join(".config").join("zjpm"))
            })
            .ok_or(StoreError::HomeUnavailable)?;

        let data_dir = env::var_os("ZJPM_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join("zjpm"))
            })
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|path| path.join(".local").join("share").join("zjpm"))
            })
            .ok_or(StoreError::HomeUnavailable)?;

        Ok(Self {
            config_dir,
            data_dir,
        })
    }

    pub fn from_roots(config_dir: impl Into<PathBuf>, data_dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: config_dir.into(),
            data_dir: data_dir.into(),
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.config_dir.join("plugins.kdl")
    }

    pub fn lockfile_path(&self) -> PathBuf {
        self.config_dir.join("plugins.lock.kdl")
    }

    pub fn blobs_dir(&self) -> PathBuf {
        self.data_dir.join("blobs").join("sha256")
    }

    pub fn plugins_dir(&self) -> PathBuf {
        self.data_dir.join("plugins")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.data_dir.join(".staging")
    }

    pub fn plugin_dir(&self, name: &str) -> PathBuf {
        self.plugins_dir().join(name)
    }

    pub fn current_plugin_path(&self, name: &str) -> PathBuf {
        self.plugin_dir(name).join("current.wasm")
    }

    pub fn versions_dir(&self, name: &str) -> PathBuf {
        self.plugin_dir(name).join("versions")
    }

    pub fn version_blob_path(&self, name: &str, sha256: &str) -> Result<PathBuf, StoreError> {
        self.blob_path(sha256)?;

        Ok(self
            .versions_dir(name)
            .join(format!("{}.wasm", sha256.to_ascii_lowercase())))
    }

    pub fn blob_path(&self, sha256: &str) -> Result<PathBuf, StoreError> {
        if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StoreError::InvalidSha256);
        }

        let digest = sha256.to_ascii_lowercase();

        Ok(self
            .blobs_dir()
            .join(&digest[..2])
            .join(format!("{}.wasm", &digest[2..])))
    }

    pub fn install_state(&self, name: &str) -> io::Result<InstallState> {
        let current = self.current_plugin_path(name);

        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(InstallState::Missing);
            }
            Err(error) => return Err(error),
        };

        if metadata.file_type().is_symlink() {
            return match fs::metadata(&current) {
                Ok(target) if target.is_file() => Ok(InstallState::Installed),
                Ok(_) => Ok(InstallState::Broken),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(InstallState::Broken),
                Err(error) => Err(error),
            };
        }

        if metadata.is_file() {
            Ok(InstallState::Installed)
        } else {
            Ok(InstallState::Broken)
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContentStore {
    paths: StorePaths,
}

impl ContentStore {
    pub fn new(paths: StorePaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &StorePaths {
        &self.paths
    }

    pub fn ingest_wasm_path(&self, source: &Path) -> Result<BlobReceipt, ContentStoreError> {
        let file = File::open(source).map_err(|error| ContentStoreError::Io {
            operation: "open plugin",
            path: source.to_path_buf(),
            source: error,
        })?;

        self.ingest_wasm(file)
    }

    pub fn ingest_wasm<R: Read>(&self, mut reader: R) -> Result<BlobReceipt, ContentStoreError> {
        let mut magic = [0_u8; 4];

        match reader.read_exact(&mut magic) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(invalid_wasm(
                    "input ended before the WebAssembly magic header was complete",
                ));
            }
            Err(error) => {
                return Err(ContentStoreError::Io {
                    operation: "read plugin",
                    path: PathBuf::from("<stream>"),
                    source: error,
                });
            }
        }

        if magic != WASM_MAGIC {
            return Err(invalid_wasm("bad WebAssembly magic header"));
        }

        let staging_dir = self.paths.staging_dir();
        fs::create_dir_all(&staging_dir).map_err(|error| ContentStoreError::Io {
            operation: "create staging directory",
            path: staging_dir.clone(),
            source: error,
        })?;

        let (staging_path, mut staging_file) =
            create_staging_file(&staging_dir).map_err(|error| ContentStoreError::Io {
                operation: "create staging file",
                path: staging_dir.clone(),
                source: error,
            })?;

        let mut cleanup = StagingCleanup::new(staging_path.clone());
        let mut hasher = Sha256::new();
        let mut validator = StreamingWasmValidator::new();
        let mut bytes = 4_u64;

        validator.push(&magic)?;
        hasher.update(magic);
        staging_file
            .write_all(&magic)
            .map_err(|error| ContentStoreError::Io {
                operation: "write staging file",
                path: staging_path.clone(),
                source: error,
            })?;

        let mut buffer = [0_u8; COPY_BUFFER_SIZE];

        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(ContentStoreError::Io {
                        operation: "read plugin",
                        path: PathBuf::from("<stream>"),
                        source: error,
                    });
                }
            };

            validator.push(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            staging_file
                .write_all(&buffer[..read])
                .map_err(|error| ContentStoreError::Io {
                    operation: "write staging file",
                    path: staging_path.clone(),
                    source: error,
                })?;
            bytes += read as u64;
        }

        validator.finish()?;

        staging_file
            .flush()
            .map_err(|error| ContentStoreError::Io {
                operation: "flush staging file",
                path: staging_path.clone(),
                source: error,
            })?;

        let mut permissions = staging_file
            .metadata()
            .map_err(|error| ContentStoreError::Io {
                operation: "read staging metadata",
                path: staging_path.clone(),
                source: error,
            })?
            .permissions();
        permissions.set_readonly(true);
        staging_file
            .set_permissions(permissions)
            .map_err(|error| ContentStoreError::Io {
                operation: "make blob read-only",
                path: staging_path.clone(),
                source: error,
            })?;

        staging_file
            .sync_all()
            .map_err(|error| ContentStoreError::Io {
                operation: "sync staging file",
                path: staging_path.clone(),
                source: error,
            })?;
        drop(staging_file);

        let sha256 = digest_to_hex(hasher.finalize());
        let blob_path = self
            .paths
            .blob_path(&sha256)
            .map_err(ContentStoreError::Store)?;

        if fs::symlink_metadata(&blob_path).is_ok() {
            if self.verify_blob(&sha256)? {
                return Ok(BlobReceipt {
                    sha256,
                    bytes,
                    path: blob_path,
                    reused: true,
                });
            }

            return Err(ContentStoreError::CorruptExistingBlob { path: blob_path });
        }

        let blob_parent = blob_path
            .parent()
            .expect("blob paths always have a parent")
            .to_path_buf();

        fs::create_dir_all(&blob_parent).map_err(|error| ContentStoreError::Io {
            operation: "create blob directory",
            path: blob_parent.clone(),
            source: error,
        })?;

        match fs::rename(&staging_path, &blob_path) {
            Ok(()) => {
                cleanup.disarm();
                sync_directory(&blob_parent).map_err(|error| ContentStoreError::Io {
                    operation: "sync blob directory",
                    path: blob_parent,
                    source: error,
                })?;

                Ok(BlobReceipt {
                    sha256,
                    bytes,
                    path: blob_path,
                    reused: false,
                })
            }
            Err(_) if fs::symlink_metadata(&blob_path).is_ok() => {
                if self.verify_blob(&sha256)? {
                    Ok(BlobReceipt {
                        sha256,
                        bytes,
                        path: blob_path,
                        reused: true,
                    })
                } else {
                    Err(ContentStoreError::CorruptExistingBlob { path: blob_path })
                }
            }
            Err(error) => Err(ContentStoreError::Io {
                operation: "commit blob",
                path: blob_path,
                source: error,
            }),
        }
    }

    pub fn verify_blob(&self, sha256: &str) -> Result<bool, ContentStoreError> {
        let expected = sha256.to_ascii_lowercase();
        let path = self
            .paths
            .blob_path(&expected)
            .map_err(ContentStoreError::Store)?;

        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(ContentStoreError::Io {
                    operation: "read blob metadata",
                    path,
                    source: error,
                });
            }
        };

        if !metadata.file_type().is_file() {
            return Ok(false);
        }

        let mut file = File::open(&path).map_err(|error| ContentStoreError::Io {
            operation: "open blob",
            path: path.clone(),
            source: error,
        })?;

        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; COPY_BUFFER_SIZE];

        loop {
            let read = match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(ContentStoreError::Io {
                        operation: "read blob",
                        path,
                        source: error,
                    });
                }
            };

            hasher.update(&buffer[..read]);
        }

        Ok(digest_to_hex(hasher.finalize()) == expected)
    }
}

struct StreamingWasmValidator {
    parser: Parser,
    validator: Validator,
    allocations: FuncValidatorAllocations,
    pending: Vec<u8>,
    consumed: usize,
    ended: bool,
}

impl StreamingWasmValidator {
    fn new() -> Self {
        Self {
            parser: Parser::new(0),
            validator: Validator::new(),
            allocations: FuncValidatorAllocations::default(),
            pending: Vec::with_capacity(COPY_BUFFER_SIZE),
            consumed: 0,
            ended: false,
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Result<(), ContentStoreError> {
        if self.ended {
            return Err(invalid_wasm("trailing bytes after the end of the module"));
        }

        self.compact();
        self.pending.extend_from_slice(bytes);
        self.process(false)
    }

    fn finish(&mut self) -> Result<(), ContentStoreError> {
        self.process(true)?;

        if self.ended {
            Ok(())
        } else {
            Err(invalid_wasm("unexpected end of WebAssembly module"))
        }
    }

    fn process(&mut self, eof: bool) -> Result<(), ContentStoreError> {
        loop {
            if self.ended {
                if self.consumed == self.pending.len() {
                    return Ok(());
                }
                return Err(invalid_wasm("trailing bytes after the end of the module"));
            }

            let data = &self.pending[self.consumed..];
            let chunk = self.parser.parse(data, eof).map_err(invalid_wasm)?;

            let Chunk::Parsed { consumed, payload } = chunk else {
                if eof {
                    return Err(invalid_wasm("unexpected end of WebAssembly module"));
                }
                return Ok(());
            };

            let validated = self.validator.payload(&payload).map_err(invalid_wasm)?;

            match validated {
                ValidPayload::Func(function, body) => {
                    let allocations = std::mem::take(&mut self.allocations);
                    let mut validator = function.into_validator(allocations);
                    validator.validate(&body).map_err(invalid_wasm)?;
                    self.allocations = validator.into_allocations();
                }
                ValidPayload::End(_) => {
                    self.ended = true;
                }
                ValidPayload::Parser(_) => {
                    return Err(invalid_wasm(
                        "WebAssembly components are not supported as Zellij plugins",
                    ));
                }
                ValidPayload::Ok => {}
            }

            self.consumed += consumed;

            if consumed == 0 && !self.ended {
                return Err(invalid_wasm("WebAssembly parser made no progress"));
            }
        }
    }

    fn compact(&mut self) {
        if self.consumed == 0 {
            return;
        }

        let remaining = self.pending.len() - self.consumed;
        self.pending.copy_within(self.consumed.., 0);
        self.pending.truncate(remaining);
        self.consumed = 0;
    }
}

fn invalid_wasm(reason: impl fmt::Display) -> ContentStoreError {
    ContentStoreError::InvalidWasm {
        reason: reason.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobReceipt {
    pub sha256: String,
    pub bytes: u64,
    pub path: PathBuf,
    pub reused: bool,
}

#[derive(Debug)]
pub enum ContentStoreError {
    Store(StoreError),
    InvalidWasm {
        reason: String,
    },
    CorruptExistingBlob {
        path: PathBuf,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl fmt::Display for ContentStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => error.fmt(formatter),
            Self::InvalidWasm { reason } => {
                write!(
                    formatter,
                    "plugin is not a valid WebAssembly module: {reason}"
                )
            }
            Self::CorruptExistingBlob { path } => write!(
                formatter,
                "content-addressed blob failed checksum verification: {}",
                path.display()
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(formatter, "{operation} {}: {source}", path.display()),
        }
    }
}

impl Error for ContentStoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Io { source, .. } => Some(source),
            Self::InvalidWasm { .. } | Self::CorruptExistingBlob { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallState {
    Installed,
    Missing,
    Broken,
}

impl InstallState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Missing => "missing",
            Self::Broken => "broken",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    HomeUnavailable,
    InvalidSha256,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeUnavailable => write!(
                formatter,
                "could not find a home directory; set ZJPM_CONFIG_DIR and ZJPM_DATA_DIR"
            ),
            Self::InvalidSha256 => write!(
                formatter,
                "SHA-256 digest must be 64 hexadecimal characters"
            ),
        }
    }
}

impl Error for StoreError {}

fn create_staging_file(directory: &Path) -> io::Result<(PathBuf, File)> {
    let process_id = std::process::id();

    for _ in 0..1024 {
        let sequence = STAGING_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!("blob-{process_id}-{sequence}.part"));

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique staging file",
    ))
}

fn digest_to_hex(digest: impl AsRef<[u8]>) -> String {
    let bytes = digest.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);

    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }

    output
}

struct StagingCleanup {
    path: PathBuf,
    armed: bool,
}

impl StagingCleanup {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagingCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new() -> Self {
            let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path =
                env::temp_dir().join(format!("zjpm-store-test-{}-{sequence}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_store() -> (TestDir, ContentStore) {
        let root = TestDir::new();
        let paths = StorePaths::from_roots(root.path.join("config"), root.path.join("data"));
        let store = ContentStore::new(paths);
        (root, store)
    }

    fn minimal_wasm(payload: &[u8]) -> Vec<u8> {
        let mut wasm = WASM_MAGIC.to_vec();
        wasm.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);

        if !payload.is_empty() {
            let section_size = payload.len() + 2;
            assert!(
                section_size < 128,
                "test helper only supports small payloads"
            );
            wasm.extend_from_slice(&[0x00, section_size as u8, 0x01, b'z']);
            wasm.extend_from_slice(payload);
        }

        wasm
    }

    #[test]
    fn uses_a_stable_current_plugin_path() {
        let paths = StorePaths::from_roots("config", "data");

        assert_eq!(
            paths.current_plugin_path("zjstatus"),
            PathBuf::from("data")
                .join("plugins")
                .join("zjstatus")
                .join("current.wasm")
        );
    }

    #[test]
    fn shards_content_addressed_blobs() {
        let paths = StorePaths::from_roots("config", "data");
        let digest = format!("ab{}", "c".repeat(62));

        assert_eq!(
            paths.blob_path(&digest).unwrap(),
            PathBuf::from("data")
                .join("blobs")
                .join("sha256")
                .join("ab")
                .join(format!("{}.wasm", "c".repeat(62)))
        );
    }

    #[test]
    fn rejects_bad_digests() {
        let paths = StorePaths::from_roots("config", "data");

        assert_eq!(
            paths.blob_path("not-a-sha256").unwrap_err(),
            StoreError::InvalidSha256
        );
    }

    #[test]
    fn streams_wasm_into_a_content_addressed_blob() {
        let (_root, store) = test_store();
        let wasm = minimal_wasm(b"stream-me");

        let receipt = store.ingest_wasm(wasm.as_slice()).unwrap();

        assert_eq!(receipt.bytes, wasm.len() as u64);
        assert!(!receipt.reused);
        assert!(receipt.path.is_file());
        assert!(store.verify_blob(&receipt.sha256).unwrap());
        assert_eq!(fs::read(&receipt.path).unwrap(), wasm);
    }

    #[test]
    fn reuses_an_existing_verified_blob() {
        let (_root, store) = test_store();
        let wasm = minimal_wasm(b"same bytes");

        let first = store.ingest_wasm(wasm.as_slice()).unwrap();
        let second = store.ingest_wasm(wasm.as_slice()).unwrap();

        assert_eq!(first.sha256, second.sha256);
        assert_eq!(first.path, second.path);
        assert!(!first.reused);
        assert!(second.reused);
    }

    #[test]
    fn rejects_non_wasm_without_leaving_staging_files() {
        let (_root, store) = test_store();

        let error = store.ingest_wasm(b"not wasm".as_slice()).unwrap_err();

        assert!(matches!(error, ContentStoreError::InvalidWasm { .. }));
        assert!(!store.paths().staging_dir().exists());
    }

    #[test]
    fn rejects_truncated_wasm_after_a_valid_header() {
        let (_root, store) = test_store();
        let mut wasm = minimal_wasm(b"");
        wasm.extend_from_slice(&[0x00, 0x05, 0x00]);

        let error = store.ingest_wasm(wasm.as_slice()).unwrap_err();

        assert!(matches!(error, ContentStoreError::InvalidWasm { .. }));
        assert!(!store.paths().blobs_dir().exists());

        let staging = store.paths().staging_dir();
        if staging.exists() {
            assert_eq!(fs::read_dir(staging).unwrap().count(), 0);
        }
    }

    #[test]
    fn accepts_a_valid_function_body() {
        let (_root, store) = test_store();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            0x03, 0x02, 0x01, 0x00, 0x0a, 0x04, 0x01, 0x02, 0x00, 0x0b,
        ];

        let receipt = store.ingest_wasm(wasm.as_slice()).unwrap();

        assert!(receipt.path.is_file());
        assert!(store.verify_blob(&receipt.sha256).unwrap());
    }

    #[test]
    fn rejects_an_invalid_function_body() {
        let (_root, store) = test_store();
        let wasm = [
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00,
            0x03, 0x02, 0x01, 0x00, 0x0a, 0x04, 0x01, 0x02, 0x00, 0xff,
        ];

        let error = store.ingest_wasm(wasm.as_slice()).unwrap_err();

        assert!(matches!(error, ContentStoreError::InvalidWasm { .. }));
        assert!(!store.paths().blobs_dir().exists());
    }

    #[test]
    fn checksum_verification_detects_tampering() {
        let (_root, store) = test_store();
        let wasm = minimal_wasm(b"trusted bytes");
        let receipt = store.ingest_wasm(wasm.as_slice()).unwrap();

        make_writable(&receipt.path);
        fs::write(&receipt.path, minimal_wasm(b"changed bytes")).unwrap();

        assert!(!store.verify_blob(&receipt.sha256).unwrap());
    }

    #[cfg(unix)]
    fn make_writable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(not(unix))]
    fn make_writable(path: &Path) {
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions).unwrap();
    }
}
