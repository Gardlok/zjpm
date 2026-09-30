use clap::{Parser, Subcommand};

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
    Update {
        plugin: Option<String>,
    },
    /// Remove a managed plugin
    Remove {
        plugin: String,
    },
    /// Check the local zjpm setup
    Doctor,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Command::Install { plugin } => {
            println!("install is not implemented yet: {plugin}");
        }
        Command::List => {
            println!("list is not implemented yet");
        }
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
}
