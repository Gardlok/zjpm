use std::env;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
