//! `gonzalo` — admin/ops CLI for the gonzalo persistence layer.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use gonzalo_cli::{get, list, migrate, parse_kind, status, sync_stores};
use gonzalo_core::RecordKind;
use std::path::PathBuf;

/// Admin/ops CLI for the gonzalo persistence layer.
#[derive(Parser)]
#[command(name = "gonzalo", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List record keys in the store.
    List {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Filter by namespace.
        #[arg(long)]
        namespace: Option<String>,
        /// Filter by collection.
        #[arg(long)]
        collection: Option<String>,
    },
    /// Fetch a single record.
    Get {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Namespace of the record.
        namespace: String,
        /// Collection of the record.
        collection: String,
        /// ID of the record.
        id: String,
    },
    /// Show record counts grouped by namespace/collection.
    Status {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Recursively import files from a directory into the store.
    Migrate {
        /// Root directory of the fs store (destination).
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Source directory to import from.
        src: PathBuf,
        /// Namespace for the imported records.
        #[arg(long)]
        namespace: String,
        /// Collection for the imported records.
        #[arg(long)]
        collection: String,
        /// Record kind.
        #[arg(long, default_value = "topic")]
        kind: KindArg,
    },
    /// Sync two filesystem stores.
    Sync {
        /// Root directory of store A.
        a: PathBuf,
        /// Root directory of store B.
        b: PathBuf,
    },
}

/// Record kind accepted by the CLI.
#[derive(Clone, ValueEnum)]
enum KindArg {
    Topic,
    MemoryTier,
    Session,
    Checkpoint,
}

impl From<KindArg> for RecordKind {
    fn from(k: KindArg) -> Self {
        match k {
            KindArg::Topic => RecordKind::Topic,
            KindArg::MemoryTier => RecordKind::MemoryTier,
            KindArg::Session => RecordKind::Session,
            KindArg::Checkpoint => RecordKind::Checkpoint,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::List {
            root,
            namespace,
            collection,
        } => {
            let keys = list(&root, namespace, collection).await?;
            if keys.is_empty() {
                println!("(no records)");
            } else {
                for k in keys {
                    println!("{k}");
                }
            }
        }

        Commands::Get {
            root,
            namespace,
            collection,
            id,
        } => match get(&root, &namespace, &collection, &id).await? {
            Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
            None => println!("not found"),
        },

        Commands::Status { root } => {
            let map = status(&root).await?;
            if map.is_empty() {
                println!("(empty store)");
            } else {
                for (path, count) in &map {
                    println!("{path}\t{count}");
                }
            }
        }

        Commands::Migrate {
            root,
            src,
            namespace,
            collection,
            kind,
        } => {
            let summary = migrate(&root, &src, &namespace, &collection, kind.into()).await?;
            println!("imported: {}", summary.imported);
            println!("skipped:  {}", summary.skipped);
        }

        Commands::Sync { a, b } => {
            // Use parse_kind indirectly; it lives in lib for tests.
            let _ = parse_kind("topic"); // ensure symbol is reachable
            let summary = sync_stores(&a, &b).await?;
            println!("copied_to_a: {}", summary.copied_to_a);
            println!("copied_to_b: {}", summary.copied_to_b);
            println!("merged:      {}", summary.merged);
            println!("conflicts:   {}", summary.conflicts);
        }
    }

    Ok(())
}
