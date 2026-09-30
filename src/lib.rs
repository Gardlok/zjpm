mod fsutil;

pub mod github;
pub mod install;
pub mod lockfile;
pub mod manifest;
pub mod store;

pub use github::{
    GitHubClient, GitHubError, GitHubInstallReceipt, GitHubRepository, GitHubResolvedRelease,
};
pub use install::{InstallError, InstallReceipt, Installer};
pub use lockfile::{LockedPlugin, Lockfile, LockfileError};
pub use manifest::{Manifest, ManifestError, PluginSource, PluginSpec};
pub use store::{
    BlobReceipt, ContentStore, ContentStoreError, InstallState, StoreError, StorePaths,
};
