pub mod manifest;
pub mod store;

pub use manifest::{Manifest, ManifestError, PluginSource, PluginSpec};
pub use store::{InstallState, StoreError, StorePaths};
