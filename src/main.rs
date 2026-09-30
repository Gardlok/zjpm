use clap::{Parser, Subcommand};
use std::error::Error;
use std::io;
use std::path::Path;

use zjpm::{
    GitHubClient, GitHubRepository, InstallError, Installer, LockedPlugin, Lockfile, Manifest,
    PluginHistory, PluginSource, PluginSpec, StorePaths,
};

#[derive(Parser)]
#[command(name = "zjpm", version, about = "A package manager for Zellij plugins")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install a local .wasm file or the latest GitHub release from owner/repo
    Install {
        /// Local plugin path or GitHub repository in owner/repo form
        source: String,

        /// Managed plugin name; defaults to the file or repository name
        #[arg(long)]
        name: Option<String>,

        /// Exact GitHub release asset to install when a release has several .wasm files
        #[arg(long)]
        asset: Option<String>,
    },
    /// List plugins managed by zjpm
    List,
    /// Update one managed GitHub plugin
    Update { plugin: String },
    /// Show recorded resolved states for a managed plugin
    History { plugin: String },
    /// Activate the previous recorded version of a managed plugin
    Rollback { plugin: String },
    /// Remove a managed plugin
    Remove { plugin: String },
    /// Check the local zjpm setup
    Doctor,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("zjpm: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match cli.command {
        Command::Install {
            source,
            name,
            asset,
        } => install_target(&source, name.as_deref(), asset.as_deref())?,
        Command::List => list_plugins()?,
        Command::Update { plugin } => update_plugin(&plugin)?,
        Command::History { plugin } => show_history(&plugin)?,
        Command::Rollback { plugin } => rollback_plugin(&plugin)?,
        Command::Remove { plugin } => {
            println!("remove is not implemented yet: {plugin}");
        }
        Command::Doctor => {
            println!("doctor is not implemented yet");
        }
    }

    Ok(())
}

fn install_target(
    source: &str,
    requested_name: Option<&str>,
    requested_asset: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let path = Path::new(source);

    if path.exists() || looks_like_local_path(source) {
        if requested_asset.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--asset only applies to GitHub installs",
            )
            .into());
        }
        install_local(path, requested_name)
    } else {
        let repository: GitHubRepository = source.parse()?;
        install_github(&repository, requested_name, requested_asset)
    }
}

fn install_local(source: &Path, requested_name: Option<&str>) -> Result<(), Box<dyn Error>> {
    let source = source.canonicalize()?;
    let source_text = source
        .to_str()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "plugin path is not UTF-8"))?
        .to_owned();

    let name = match requested_name {
        Some(name) => name.to_owned(),
        None => source
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "could not infer a plugin name; use --name",
                )
            })?
            .to_owned(),
    };

    let asset = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "plugin file name is not UTF-8")
        })?
        .to_owned();

    let paths = StorePaths::discover()?;
    let installer = Installer::new(paths.clone());
    let receipt = installer.install_path(&name, &source)?;
    let plugin_source = PluginSource::Path(source_text);

    persist_state(
        &paths,
        PluginSpec {
            name: name.clone(),
            source: plugin_source.clone(),
            version: None,
            asset: None,
        },
        LockedPlugin {
            name: name.clone(),
            source: plugin_source,
            version: None,
            asset,
            sha256: receipt.sha256.clone(),
            bytes: receipt.bytes,
        },
    )?;

    print_install(&name, None, &receipt.sha256, &receipt.current_path);

    Ok(())
}

fn install_github(
    repository: &GitHubRepository,
    requested_name: Option<&str>,
    requested_asset: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let name = requested_name.unwrap_or(repository.repo()).to_owned();
    let paths = StorePaths::discover()?;
    let installer = Installer::new(paths.clone());
    let client = GitHubClient::from_env();

    let installed = client.install_latest(repository, &installer, &name, requested_asset)?;
    let plugin_source = PluginSource::GitHub {
        owner: repository.owner().to_owned(),
        repo: repository.repo().to_owned(),
    };

    persist_state(
        &paths,
        PluginSpec {
            name: name.clone(),
            source: plugin_source.clone(),
            version: None,
            asset: requested_asset.map(str::to_owned),
        },
        LockedPlugin {
            name: name.clone(),
            source: plugin_source,
            version: Some(installed.tag_name.clone()),
            asset: installed.asset_name.clone(),
            sha256: installed.receipt.sha256.clone(),
            bytes: installed.receipt.bytes,
        },
    )?;

    print_install(
        &name,
        Some(&installed.tag_name),
        &installed.receipt.sha256,
        &installed.receipt.current_path,
    );

    Ok(())
}

fn update_plugin(name: &str) -> Result<(), Box<dyn Error>> {
    let paths = StorePaths::discover()?;
    let mut manifest = Manifest::load(&paths.manifest_path())?;
    let mut lockfile = Lockfile::load(&paths.lockfile_path())?;

    let mut plugin = manifest
        .plugins()
        .iter()
        .find(|plugin| plugin.name == name)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("plugin '{name}' is not managed by zjpm"),
            )
        })?;

    if let Some(version) = &plugin.version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "plugin '{name}' is pinned to {version}; remove the manifest version pin before updating"
            ),
        )
        .into());
    }

    let PluginSource::GitHub { owner, repo } = &plugin.source else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("plugin '{name}' uses a local path; reinstall that path to refresh it"),
        )
        .into());
    };

    let repository: GitHubRepository = format!("{owner}/{repo}").parse()?;
    let locked = locked_plugin(&lockfile, name);
    let conventional_asset = format!("{repo}.wasm");

    if plugin.asset.is_none()
        && let Some(locked) = locked
        && !locked.asset.eq_ignore_ascii_case(&conventional_asset)
    {
        plugin.asset = Some(locked.asset.clone());
    }

    let client = GitHubClient::from_env();
    let resolved = client.resolve_latest(&repository, plugin.asset.as_deref())?;

    if let Some(locked) = locked
        && resolved.matches_locked(locked)
    {
        let installer = Installer::new(paths.clone());

        match installer.activate_blob(name, &locked.sha256) {
            Ok(current) => {
                if manifest
                    .plugins()
                    .iter()
                    .find(|existing| existing.name == name)
                    .is_some_and(|existing| existing.asset != plugin.asset)
                {
                    manifest.upsert(plugin);
                    manifest.save_atomic(&paths.manifest_path())?;
                }

                println!(
                    "{name} is already current at {} ({})",
                    resolved.tag_name, resolved.asset_name
                );
                println!("  current: {}", current.display());
                return Ok(());
            }
            Err(InstallError::BlobUnavailable { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }

    let installer = Installer::new(paths.clone());
    let installed = client.install_resolved(&resolved, &installer, name)?;

    persist_locked_state(
        &paths,
        &mut lockfile,
        LockedPlugin {
            name: name.to_owned(),
            source: plugin.source.clone(),
            version: Some(installed.tag_name.clone()),
            asset: installed.asset_name.clone(),
            sha256: installed.receipt.sha256.clone(),
            bytes: installed.receipt.bytes,
        },
    )?;

    if manifest
        .plugins()
        .iter()
        .find(|existing| existing.name == name)
        .is_some_and(|existing| existing.asset != plugin.asset)
    {
        manifest.upsert(plugin);
        manifest.save_atomic(&paths.manifest_path())?;
    }

    println!("Updated {name} to {}", installed.tag_name);
    println!("  asset: {}", installed.asset_name);
    println!("  sha256: {}", installed.receipt.sha256);
    println!("  current: {}", installed.receipt.current_path.display());

    Ok(())
}

fn persist_state(
    paths: &StorePaths,
    plugin: PluginSpec,
    locked: LockedPlugin,
) -> Result<(), Box<dyn Error>> {
    let mut lockfile = Lockfile::load(&paths.lockfile_path())?;
    persist_locked_state(paths, &mut lockfile, locked)?;

    let mut manifest = Manifest::load(&paths.manifest_path())?;
    manifest.upsert(plugin);
    manifest.save_atomic(&paths.manifest_path())?;

    Ok(())
}

fn persist_locked_state(
    paths: &StorePaths,
    lockfile: &mut Lockfile,
    locked: LockedPlugin,
) -> Result<(), Box<dyn Error>> {
    let history_path = paths.history_path(&locked.name);
    let mut history = PluginHistory::load(&history_path, &locked.name)?;

    if let Some(previous) = locked_plugin(lockfile, &locked.name).cloned()
        && history.record(previous)
    {
        history.save_atomic(&history_path)?;
    }

    lockfile.upsert(locked.clone());
    lockfile.save_atomic(&paths.lockfile_path())?;

    if history.record(locked) {
        history.save_atomic(&history_path)?;
    }

    Ok(())
}

fn show_history(name: &str) -> Result<(), Box<dyn Error>> {
    let paths = StorePaths::discover()?;
    let manifest = Manifest::load(&paths.manifest_path())?;
    let lockfile = Lockfile::load(&paths.lockfile_path())?;

    manifest
        .plugins()
        .iter()
        .find(|plugin| plugin.name == name)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("plugin '{name}' is not managed by zjpm"),
            )
        })?;

    let current = locked_plugin(&lockfile, name).cloned();
    let mut entries = PluginHistory::load(&paths.history_path(name), name)?
        .entries()
        .to_vec();

    if let Some(current) = &current
        && entries.last() != Some(current)
    {
        entries.push(current.clone());
    }

    if entries.is_empty() {
        println!("No resolved history recorded for {name}.");
        return Ok(());
    }

    println!("STATE    VERSION       ASSET                     SHA256        BYTES");

    for (index, entry) in entries.iter().rev().enumerate() {
        let state = if index == 0 && current.as_ref() == Some(entry) {
            "current"
        } else {
            ""
        };
        let version = entry.version.as_deref().unwrap_or("local");
        let short_sha = &entry.sha256[..12];

        println!(
            "{state:<8} {version:<13} {:<25} {short_sha:<12} {}",
            entry.asset, entry.bytes
        );
    }

    Ok(())
}

fn rollback_plugin(name: &str) -> Result<(), Box<dyn Error>> {
    let paths = StorePaths::discover()?;
    let manifest = Manifest::load(&paths.manifest_path())?;
    let mut lockfile = Lockfile::load(&paths.lockfile_path())?;

    manifest
        .plugins()
        .iter()
        .find(|plugin| plugin.name == name)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("plugin '{name}' is not managed by zjpm"),
            )
        })?;

    let current = locked_plugin(&lockfile, name).cloned().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("plugin '{name}' has no resolved lock state"),
        )
    })?;

    let history = PluginHistory::load(&paths.history_path(name), name)?;
    let target = history
        .previous_distinct(&current.sha256)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("plugin '{name}' has no previous recorded version to roll back to"),
            )
        })?;

    let installer = Installer::new(paths.clone());
    let current_path = installer.activate_blob(name, &target.sha256)?;

    persist_locked_state(&paths, &mut lockfile, target.clone())?;

    println!("Rolled back {name}");
    println!(
        "  version: {}",
        target.version.as_deref().unwrap_or("local")
    );
    println!("  asset: {}", target.asset);
    println!("  sha256: {}", target.sha256);
    println!("  current: {}", current_path.display());

    Ok(())
}

fn print_install(name: &str, version: Option<&str>, sha256: &str, current: &Path) {
    match version {
        Some(version) => println!("Installed {name} {version}"),
        None => println!("Installed {name}"),
    }
    println!("  sha256: {sha256}");
    println!("  current: {}", current.display());
}

fn looks_like_local_path(source: &str) -> bool {
    source.starts_with('.')
        || source.starts_with('/')
        || source.ends_with(".wasm")
        || source.contains('\\')
}

fn list_plugins() -> Result<(), Box<dyn Error>> {
    let paths = StorePaths::discover()?;
    let manifest = Manifest::load(&paths.manifest_path())?;
    let lockfile = Lockfile::load(&paths.lockfile_path())?;

    if manifest.plugins().is_empty() {
        println!("No plugins managed by zjpm.");
        return Ok(());
    }

    let name_width = manifest
        .plugins()
        .iter()
        .map(|plugin| plugin.name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    let version_width = manifest
        .plugins()
        .iter()
        .map(|plugin| display_version(plugin, locked_plugin(&lockfile, &plugin.name)).len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<name_width$}  {:<version_width$}  {:<9}  SOURCE",
        "NAME", "VERSION", "STATUS"
    );

    for plugin in manifest.plugins() {
        let version = display_version(plugin, locked_plugin(&lockfile, &plugin.name));
        let state = paths.install_state(&plugin.name)?;

        println!(
            "{:<name_width$}  {:<version_width$}  {:<9}  {}",
            plugin.name,
            version,
            state.label(),
            plugin.source
        );
    }

    Ok(())
}

fn locked_plugin<'a>(lockfile: &'a Lockfile, name: &str) -> Option<&'a LockedPlugin> {
    lockfile.plugins().iter().find(|plugin| plugin.name == name)
}

fn display_version<'a>(plugin: &'a PluginSpec, locked: Option<&'a LockedPlugin>) -> &'a str {
    match &plugin.source {
        PluginSource::Path(_) => "local",
        PluginSource::GitHub { .. } => locked
            .and_then(|locked| locked.version.as_deref())
            .unwrap_or("unresolved"),
    }
}
