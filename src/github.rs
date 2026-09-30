use crate::{InstallError, InstallReceipt, Installer, LockedPlugin};
use serde::Deserialize;
use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Read};
use std::str::FromStr;

const API_BASE: &str = "https://api.github.com";
const API_VERSION: &str = "2026-03-10";
const RELEASE_METADATA_LIMIT: u64 = 4 * 1024 * 1024;
const MAX_PLUGIN_ASSET_BYTES: u64 = 128 * 1024 * 1024;
const REQUEST_TIMEOUT_SECONDS: u64 = 300;
const MAX_HEADERS_BYTES: usize = 64 * 1024;
const MAX_STATUS_LINE_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubRepository {
    owner: String,
    repo: String,
}

impl GitHubRepository {
    pub fn owner(&self) -> &str {
        &self.owner
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }
}

impl fmt::Display for GitHubRepository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.owner, self.repo)
    }
}

impl FromStr for GitHubRepository {
    type Err = GitHubError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (owner, repo) = value
            .split_once('/')
            .ok_or_else(|| GitHubError::InvalidRepository(value.to_owned()))?;

        if repo.contains('/')
            || !valid_repository_component(owner)
            || !valid_repository_component(repo)
        {
            return Err(GitHubError::InvalidRepository(value.to_owned()));
        }

        Ok(Self {
            owner: owner.to_owned(),
            repo: repo.to_owned(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GitHubClient {
    token: Option<String>,
}

impl GitHubClient {
    pub fn from_env() -> Self {
        let token = env::var("GITHUB_TOKEN")
            .ok()
            .map(|token| token.trim().to_owned())
            .filter(|token| !token.is_empty());

        Self { token }
    }

    pub fn resolve_latest(
        &self,
        repository: &GitHubRepository,
        requested_asset: Option<&str>,
    ) -> Result<GitHubResolvedRelease, GitHubError> {
        let release = self.latest_release(repository)?;
        let conventional_asset = format!("{}.wasm", repository.repo());
        let asset =
            select_wasm_asset(&release, requested_asset, Some(conventional_asset.as_str()))?;

        if asset.size > MAX_PLUGIN_ASSET_BYTES {
            return Err(GitHubError::AssetTooLarge {
                name: asset.name.clone(),
                bytes: asset.size,
                limit: MAX_PLUGIN_ASSET_BYTES,
            });
        }

        Ok(GitHubResolvedRelease {
            tag_name: release.tag_name,
            asset_name: asset.name,
            asset_size: asset.size,
            sha256: parse_sha256_digest(asset.digest.as_deref())?,
            asset_url: asset.url,
        })
    }

    pub fn install_latest(
        &self,
        repository: &GitHubRepository,
        installer: &Installer,
        name: &str,
        requested_asset: Option<&str>,
    ) -> Result<GitHubInstallReceipt, GitHubError> {
        let resolved = self.resolve_latest(repository, requested_asset)?;
        self.install_resolved(&resolved, installer, name)
    }

    pub fn install_resolved(
        &self,
        resolved: &GitHubResolvedRelease,
        installer: &Installer,
        name: &str,
    ) -> Result<GitHubInstallReceipt, GitHubError> {
        let response = self.download_asset(&resolved.asset_url)?;
        let reader = response.take(resolved.asset_size.saturating_add(1));
        let blob = installer.store().ingest_wasm(reader)?;

        if blob.bytes != resolved.asset_size {
            return Err(GitHubError::AssetSizeMismatch {
                name: resolved.asset_name.clone(),
                expected: resolved.asset_size,
                actual: blob.bytes,
            });
        }

        if let Some(expected) = resolved.sha256.as_deref()
            && blob.sha256 != expected
        {
            return Err(GitHubError::AssetDigestMismatch {
                name: resolved.asset_name.clone(),
                expected: expected.to_owned(),
                actual: blob.sha256,
            });
        }

        let receipt = installer.activate_ingested(name, blob)?;

        Ok(GitHubInstallReceipt {
            tag_name: resolved.tag_name.clone(),
            asset_name: resolved.asset_name.clone(),
            receipt,
        })
    }

    fn latest_release(
        &self,
        repository: &GitHubRepository,
    ) -> Result<ReleaseResponse, GitHubError> {
        let url = format!(
            "{API_BASE}/repos/{}/{}/releases/latest",
            repository.owner, repository.repo
        );
        let mut response = self.github_get(&url, "application/vnd.github+json", false)?;

        ensure_status(&response, 200)?;

        let mut body = Vec::new();
        response
            .by_ref()
            .take(RELEASE_METADATA_LIMIT + 1)
            .read_to_end(&mut body)
            .map_err(|source| GitHubError::Read {
                operation: "read latest release metadata",
                source,
            })?;

        if body.len() as u64 > RELEASE_METADATA_LIMIT {
            return Err(GitHubError::MetadataTooLarge {
                limit: RELEASE_METADATA_LIMIT,
            });
        }

        serde_json::from_slice(&body).map_err(GitHubError::Json)
    }

    fn download_asset(&self, url: &str) -> Result<minreq::ResponseLazy, GitHubError> {
        let response = self.github_get(url, "application/octet-stream", false)?;

        match response.status_code {
            200 => Ok(response),
            301 | 302 | 303 | 307 | 308 => {
                let location = response_header(&response, "Location")
                    .ok_or(GitHubError::RedirectMissingLocation)?
                    .to_owned();

                if !location.starts_with("https://") {
                    return Err(GitHubError::UnsafeRedirect(location));
                }

                let response = minreq::get(location)
                    .with_header("Accept", "application/octet-stream")
                    .with_header("User-Agent", user_agent())
                    .with_timeout(REQUEST_TIMEOUT_SECONDS)
                    .with_max_redirects(5)
                    .with_max_headers_size(MAX_HEADERS_BYTES)
                    .with_max_status_line_length(MAX_STATUS_LINE_BYTES)
                    .send_lazy()
                    .map_err(|source| GitHubError::Http {
                        operation: "download GitHub asset",
                        source,
                    })?;

                ensure_status(&response, 200)?;
                Ok(response)
            }
            _ => Err(status_error(&response)),
        }
    }

    fn github_get(
        &self,
        url: &str,
        accept: &'static str,
        follow_redirects: bool,
    ) -> Result<minreq::ResponseLazy, GitHubError> {
        let request = minreq::get(url)
            .with_header("Accept", accept)
            .with_header("User-Agent", user_agent())
            .with_header("X-GitHub-Api-Version", API_VERSION)
            .with_timeout(REQUEST_TIMEOUT_SECONDS)
            .with_follow_redirects(follow_redirects)
            .with_max_redirects(5)
            .with_max_headers_size(MAX_HEADERS_BYTES)
            .with_max_status_line_length(MAX_STATUS_LINE_BYTES);

        let response = match self.token.as_deref() {
            Some(token) => request
                .with_header("Authorization", format!("Bearer {token}"))
                .send_lazy(),
            None => request.send_lazy(),
        };

        response.map_err(|source| GitHubError::Http {
            operation: "request GitHub",
            source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubResolvedRelease {
    pub tag_name: String,
    pub asset_name: String,
    pub asset_size: u64,
    pub sha256: Option<String>,
    asset_url: String,
}

impl GitHubResolvedRelease {
    pub fn matches_locked(&self, locked: &LockedPlugin) -> bool {
        self.sha256.as_deref().is_some_and(|sha256| {
            locked.version.as_deref() == Some(self.tag_name.as_str())
                && locked.asset == self.asset_name
                && locked.bytes == self.asset_size
                && locked.sha256 == sha256
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubInstallReceipt {
    pub tag_name: String,
    pub asset_name: String,
    pub receipt: InstallReceipt,
}

#[derive(Debug)]
pub enum GitHubError {
    InvalidRepository(String),
    NoWasmAsset {
        repository_release: String,
    },
    AssetNotFound {
        repository_release: String,
        requested: String,
        names: Vec<String>,
    },
    MultipleWasmAssets {
        repository_release: String,
        names: Vec<String>,
    },
    AssetTooLarge {
        name: String,
        bytes: u64,
        limit: u64,
    },
    InvalidAssetDigest(String),
    AssetSizeMismatch {
        name: String,
        expected: u64,
        actual: u64,
    },
    AssetDigestMismatch {
        name: String,
        expected: String,
        actual: String,
    },
    MetadataTooLarge {
        limit: u64,
    },
    RedirectMissingLocation,
    UnsafeRedirect(String),
    HttpStatus {
        status: u16,
        reason: String,
        url: String,
    },
    Http {
        operation: &'static str,
        source: minreq::Error,
    },
    Read {
        operation: &'static str,
        source: io::Error,
    },
    Json(serde_json::Error),
    Install(InstallError),
    Store(crate::ContentStoreError),
}

impl fmt::Display for GitHubError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRepository(value) => write!(
                formatter,
                "invalid GitHub repository '{value}'; expected owner/repo"
            ),
            Self::NoWasmAsset { repository_release } => write!(
                formatter,
                "GitHub release {repository_release} has no uploaded .wasm asset"
            ),
            Self::AssetNotFound {
                repository_release,
                requested,
                names,
            } => write!(
                formatter,
                "GitHub release {repository_release} has no uploaded .wasm asset named '{requested}'; available: {}",
                names.join(", ")
            ),
            Self::MultipleWasmAssets {
                repository_release,
                names,
            } => write!(
                formatter,
                "GitHub release {repository_release} has multiple .wasm assets: {}; use --asset to choose",
                names.join(", ")
            ),
            Self::AssetTooLarge { name, bytes, limit } => write!(
                formatter,
                "GitHub asset '{name}' is {bytes} bytes; zjpm currently limits plugins to {limit} bytes"
            ),
            Self::InvalidAssetDigest(value) => {
                write!(
                    formatter,
                    "GitHub returned an invalid asset digest '{value}'"
                )
            }
            Self::AssetSizeMismatch {
                name,
                expected,
                actual,
            } => write!(
                formatter,
                "GitHub asset '{name}' size mismatch: expected {expected} bytes, received {actual}"
            ),
            Self::AssetDigestMismatch {
                name,
                expected,
                actual,
            } => write!(
                formatter,
                "GitHub asset '{name}' SHA-256 mismatch: expected {expected}, received {actual}"
            ),
            Self::MetadataTooLarge { limit } => write!(
                formatter,
                "GitHub release metadata exceeded the {limit}-byte safety limit"
            ),
            Self::RedirectMissingLocation => {
                write!(
                    formatter,
                    "GitHub asset redirect did not include a Location header"
                )
            }
            Self::UnsafeRedirect(url) => {
                write!(formatter, "GitHub asset redirect was not HTTPS: {url}")
            }
            Self::HttpStatus {
                status,
                reason,
                url,
            } => write!(
                formatter,
                "GitHub request returned HTTP {status} {reason}: {url}"
            ),
            Self::Http { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Read { operation, source } => write!(formatter, "{operation}: {source}"),
            Self::Json(source) => write!(formatter, "parse GitHub release metadata: {source}"),
            Self::Install(source) => source.fmt(formatter),
            Self::Store(source) => source.fmt(formatter),
        }
    }
}

impl Error for GitHubError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Http { source, .. } => Some(source),
            Self::Read { source, .. } => Some(source),
            Self::Json(source) => Some(source),
            Self::Install(source) => Some(source),
            Self::Store(source) => Some(source),
            _ => None,
        }
    }
}

impl From<InstallError> for GitHubError {
    fn from(source: InstallError) -> Self {
        Self::Install(source)
    }
}

impl From<crate::ContentStoreError> for GitHubError {
    fn from(source: crate::ContentStoreError) -> Self {
        Self::Store(source)
    }
}

#[derive(Debug, Deserialize)]
struct ReleaseResponse {
    tag_name: String,
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleaseAsset {
    name: String,
    url: String,
    state: String,
    size: u64,
    digest: Option<String>,
}

fn select_wasm_asset(
    release: &ReleaseResponse,
    requested: Option<&str>,
    conventional: Option<&str>,
) -> Result<ReleaseAsset, GitHubError> {
    let mut assets: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.state == "uploaded")
        .filter(|asset| asset.name.to_ascii_lowercase().ends_with(".wasm"))
        .cloned()
        .collect();

    assets.sort_by(|left, right| left.name.cmp(&right.name));

    if assets.is_empty() {
        return Err(GitHubError::NoWasmAsset {
            repository_release: release.tag_name.clone(),
        });
    }

    if let Some(requested) = requested {
        return assets
            .iter()
            .find(|asset| asset.name == requested)
            .cloned()
            .ok_or_else(|| GitHubError::AssetNotFound {
                repository_release: release.tag_name.clone(),
                requested: requested.to_owned(),
                names: assets.iter().map(|asset| asset.name.clone()).collect(),
            });
    }

    if assets.len() == 1 {
        return Ok(assets.remove(0));
    }

    if let Some(conventional) = conventional {
        let matching: Vec<_> = assets
            .iter()
            .filter(|asset| asset.name.eq_ignore_ascii_case(conventional))
            .cloned()
            .collect();

        if matching.len() == 1 {
            return Ok(matching.into_iter().next().expect("one matching asset"));
        }
    }

    Err(GitHubError::MultipleWasmAssets {
        repository_release: release.tag_name.clone(),
        names: assets.into_iter().map(|asset| asset.name).collect(),
    })
}

fn parse_sha256_digest(digest: Option<&str>) -> Result<Option<String>, GitHubError> {
    let Some(digest) = digest else {
        return Ok(None);
    };

    let Some(value) = digest.strip_prefix("sha256:") else {
        return Err(GitHubError::InvalidAssetDigest(digest.to_owned()));
    };

    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitHubError::InvalidAssetDigest(digest.to_owned()));
    }

    Ok(Some(value.to_ascii_lowercase()))
}

fn response_header<'a>(response: &'a minreq::ResponseLazy, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn ensure_status(response: &minreq::ResponseLazy, expected: u16) -> Result<(), GitHubError> {
    if response.status_code == expected {
        Ok(())
    } else {
        Err(status_error(response))
    }
}

fn status_error(response: &minreq::ResponseLazy) -> GitHubError {
    GitHubError::HttpStatus {
        status: response.status_code,
        reason: response.reason_phrase.clone(),
        url: response.url.clone(),
    }
}

fn user_agent() -> &'static str {
    concat!("zjpm/", env!("CARGO_PKG_VERSION"))
}

fn valid_repository_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(name: &str, state: &str) -> ReleaseAsset {
        ReleaseAsset {
            name: name.to_owned(),
            url: format!("https://api.github.com/assets/{name}"),
            state: state.to_owned(),
            size: 8,
            digest: None,
        }
    }

    #[test]
    fn parses_repository_locator() {
        let repository: GitHubRepository = "dj95/zjstatus".parse().unwrap();

        assert_eq!(repository.owner(), "dj95");
        assert_eq!(repository.repo(), "zjstatus");
        assert_eq!(repository.to_string(), "dj95/zjstatus");
    }

    #[test]
    fn rejects_ambiguous_repository_locator() {
        assert!("owner/repo/extra".parse::<GitHubRepository>().is_err());
        assert!("../repo".parse::<GitHubRepository>().is_err());
        assert!("owner".parse::<GitHubRepository>().is_err());
    }

    #[test]
    fn selects_the_only_uploaded_wasm_asset() {
        let release = ReleaseResponse {
            tag_name: "v1.2.3".to_owned(),
            assets: vec![
                asset("notes.txt", "uploaded"),
                asset("plugin.wasm", "uploaded"),
                asset("unfinished.wasm", "starter"),
            ],
        };

        let selected = select_wasm_asset(&release, None, None).unwrap();

        assert_eq!(selected.name, "plugin.wasm");
    }

    #[test]
    fn refuses_to_guess_between_multiple_wasm_assets() {
        let release = ReleaseResponse {
            tag_name: "v1.2.3".to_owned(),
            assets: vec![asset("b.wasm", "uploaded"), asset("a.wasm", "uploaded")],
        };

        let error = select_wasm_asset(&release, None, None).unwrap_err();

        match error {
            GitHubError::MultipleWasmAssets { names, .. } => {
                assert_eq!(names, vec!["a.wasm", "b.wasm"]);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn prefers_the_repository_named_wasm_when_release_has_multiple() {
        let release = ReleaseResponse {
            tag_name: "v1.2.3".to_owned(),
            assets: vec![
                asset("zjframes.wasm", "uploaded"),
                asset("zjstatus.wasm", "uploaded"),
            ],
        };

        let selected = select_wasm_asset(&release, None, Some("zjstatus.wasm")).unwrap();

        assert_eq!(selected.name, "zjstatus.wasm");
    }

    #[test]
    fn explicit_asset_overrides_conventional_name() {
        let release = ReleaseResponse {
            tag_name: "v1.2.3".to_owned(),
            assets: vec![
                asset("zjframes.wasm", "uploaded"),
                asset("zjstatus.wasm", "uploaded"),
            ],
        };

        let selected =
            select_wasm_asset(&release, Some("zjframes.wasm"), Some("zjstatus.wasm")).unwrap();

        assert_eq!(selected.name, "zjframes.wasm");
    }

    #[test]
    fn resolved_release_matches_lock_only_with_exact_digest_state() {
        let resolved = GitHubResolvedRelease {
            tag_name: "v1.2.3".to_owned(),
            asset_name: "plugin.wasm".to_owned(),
            asset_size: 42,
            sha256: Some("a".repeat(64)),
            asset_url: "https://api.github.com/assets/1".to_owned(),
        };
        let locked = LockedPlugin {
            name: "plugin".to_owned(),
            source: crate::PluginSource::GitHub {
                owner: "owner".to_owned(),
                repo: "plugin".to_owned(),
            },
            version: Some("v1.2.3".to_owned()),
            asset: "plugin.wasm".to_owned(),
            sha256: "a".repeat(64),
            bytes: 42,
        };

        assert!(resolved.matches_locked(&locked));

        let mut changed = locked.clone();
        changed.sha256 = "b".repeat(64);
        assert!(!resolved.matches_locked(&changed));

        let unresolved_digest = GitHubResolvedRelease {
            sha256: None,
            ..resolved
        };
        assert!(!unresolved_digest.matches_locked(&locked));
    }

    #[test]
    fn parses_github_sha256_digest() {
        let digest = format!("sha256:{}", "A".repeat(64));

        assert_eq!(
            parse_sha256_digest(Some(&digest)).unwrap(),
            Some("a".repeat(64))
        );
        assert!(parse_sha256_digest(Some("sha512:abc")).is_err());
        assert_eq!(parse_sha256_digest(None).unwrap(), None);
    }
}
