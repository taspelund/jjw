use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Create a generic workspace and print its path.
    #[command(visible_alias = "c")]
    Create {
        /// Use this workspace name instead of the next available ws-N.
        name: Option<String>,
        /// Symlink a relative path from the default workspace.
        #[arg(long = "share", value_name = "PATH")]
        shared_paths: Vec<PathBuf>,
    },
    /// Work with GitHub pull requests.
    Pr {
        #[command(subcommand)]
        command: PrCommands,
    },
    /// List all workspaces.
    Ls,
    /// Remove workspaces interactively, returning to the root when needed.
    Rm {
        /// Delete files that jj does not track.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum PrCommands {
    /// Fetch a PR, create its workspace, and print its path.
    Checkout {
        /// GitHub PR number to check out.
        #[arg(value_name = "PR_NUMBER")]
        number: u64,
        /// Symlink a relative path from the default workspace.
        #[arg(long = "share", value_name = "PATH")]
        shared_paths: Vec<PathBuf>,
    },
    /// Show the full diff.
    Diff {
        /// GitHub PR number; omit inside its exact pr-N workspace.
        #[arg(value_name = "PR_NUMBER")]
        number: Option<u64>,
    },
    /// List changed files.
    Files {
        /// GitHub PR number; omit inside its exact pr-N workspace.
        #[arg(value_name = "PR_NUMBER")]
        number: Option<u64>,
    },
    /// Show commits.
    Log {
        /// GitHub PR number; omit inside its exact pr-N workspace.
        #[arg(value_name = "PR_NUMBER")]
        number: Option<u64>,
    },
    /// Show PR metadata.
    Summary {
        /// GitHub PR number; omit inside its exact pr-N workspace.
        #[arg(value_name = "PR_NUMBER")]
        number: Option<u64>,
    },
    /// Show CI status.
    Checks {
        /// GitHub PR number; omit inside its exact pr-N workspace.
        #[arg(value_name = "PR_NUMBER")]
        number: Option<u64>,
    },
    /// Open the PR in a browser.
    Open {
        /// GitHub PR number; omit inside its exact pr-N workspace.
        #[arg(value_name = "PR_NUMBER")]
        number: Option<u64>,
    },
    /// Remove a PR workspace, printing the root when removing the current one.
    Cleanup {
        /// GitHub PR number; omit inside its exact pr-N workspace.
        #[arg(value_name = "PR_NUMBER")]
        number: Option<u64>,
        /// Delete files that jj does not track.
        #[arg(long)]
        force: bool,
        /// Skip the deletion confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
}
