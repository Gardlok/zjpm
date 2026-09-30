use clap::{Parser, Subcommand};
use std::error::Error;

use zjpm::{Manifest, StorePaths};

#[derive(Parser)]
#[command(name = "zjpm", version, about = "A package manager for Zellij plugins")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Install a plugin
    Install {
        /// GitHub repository, for example owner/repo
        plugin: String,
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
        Command::Install { plugin } => {
            println!("install is not implemented yet: {plugin}");
        }
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
        .map(|plugin| plugin.version.as_deref().unwrap_or("latest").len())
        .max()
        .unwrap_or(7)
        .max(7);

    println!(
        "{:<name_width$}  {:<version_width$}  {:<9}  SOURCE",
        "NAME", "VERSION", "STATUS"
    );

    for plugin in manifest.plugins() {
        let version = plugin.version.as_deref().unwrap_or("latest");
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
