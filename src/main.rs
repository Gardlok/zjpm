use clap::{Parser, Subcommand};
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use zjpm::{
    Installer, LockedPlugin, Lockfile, Manifest, PluginSource, PluginSpec, StorePaths,
};

#[derive(Parser)]
#[command(name = "zjpm", version, about = "A package manager for Zellij plugins")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install a local .wasm plugin
    Install {
        /// Path to a WebAssembly plugin
        source: PathBuf,

        /// Managed plugin name; defaults to the source file name
        #[arg(long)]
        name: Option<String>,
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
        Command::Install { source, name } => install_local(&source, name.as_deref())?,
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
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "plugin file name is not UTF-8"))?
        .to_owned();

    let paths = StorePaths::discover()?;
    let installer = Installer::new(paths.clone());
    let receipt = installer.install_path(&name, &source)?;
    let plugin_source = PluginSource::Path(source_text);

    let mut lockfile = Lockfile::load(&paths.lockfile_path())?;
    lockfile.upsert(LockedPlugin {
        name: name.clone(),
        source: plugin_source.clone(),
        version: None,
        asset,
        sha256: receipt.sha256.clone(),
        bytes: receipt.bytes,
    });
    lockfile.save_atomic(&paths.lockfile_path())?;

    let mut manifest = Manifest::load(&paths.manifest_path())?;
    manifest.upsert(PluginSpec {
        name: name.clone(),
        source: plugin_source,
        version: None,
    });
    manifest.save_atomic(&paths.manifest_path())?;

    println!("Installed {name}");
    println!("  sha256: {}", receipt.sha256);
    println!("  current: {}", receipt.current_path.display());

    Ok(())
}

fn list_plugins() -> Result<(), Box<dyn Error>> {
    let paths = StorePaths::discover()?;
    let manifest = Manifest::load(&paths.manifest_path())?;

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
        .map(|plugin| display_version(plugin).len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<name_width$}  {:<version_width$}  {:<9}  SOURCE",
        "NAME", "VERSION", "STATUS"
    );

    for plugin in manifest.plugins() {
        let version = display_version(plugin);
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

fn display_version(plugin: &PluginSpec) -> &str {
    plugin.version.as_deref().unwrap_or(match plugin.source {
        PluginSource::Path(_) => "local",
        PluginSource::GitHub { .. } => "latest",
    })
}
