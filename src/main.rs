use clap::{Parser, Subcommand};
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use zjpm::{
    GitHubClient, GitHubRepository, Installer, LockedPlugin, Lockfile, Manifest, PluginSource,
    PluginSpec, StorePaths,
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
    /// Update one plugin, or all managed plugins
    Update { plugin: Option<String> },
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
        Command::Update { plugin } => match plugin {
            Some(plugin) => println!("update is not implemented yet: {plugin}"),
            None => println!("update is not implemented yet"),
        },
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

    let installed =
        client.install_latest(repository, &installer, &name, requested_asset)?;
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

fn persist_state(
    paths: &StorePaths,
    plugin: PluginSpec,
    locked: LockedPlugin,
) -> Result<(), Box<dyn Error>> {
    let mut lockfile = Lockfile::load(&paths.lockfile_path())?;
    lockfile.upsert(locked);
    lockfile.save_atomic(&paths.lockfile_path())?;

    let mut manifest = Manifest::load(&paths.manifest_path())?;
    manifest.upsert(plugin);
    manifest.save_atomic(&paths.manifest_path())?;

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
