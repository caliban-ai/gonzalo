//! `gonzalo` — admin/ops CLI for the gonzalo persistence layer.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use gonzalo_cli::{get, list, migrate, status, sync_stores, ticket_sync};
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
    /// Read external ticket boards into the store, and inspect imported tickets.
    Ticket {
        #[command(subcommand)]
        command: TicketCommands,
    },
}

#[derive(Subcommand)]
enum TicketCommands {
    /// Sync all configured ticket connections into the store.
    Sync {
        /// Path to the tickets TOML config.
        #[arg(long, default_value = "tickets.toml")]
        config: PathBuf,
        /// Root directory of the fs store.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Author recorded on imported records.
        #[arg(long, default_value = "gonzalo-cli")]
        author: String,
    },
    /// List imported ticket record keys.
    List {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },
    /// Show one imported ticket record by uid (e.g. "caliban-ai/gonzalo#15").
    Get {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".")]
        root: PathBuf,
        /// Ticket uid (owner/repo#number).
        uid: String,
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
            let summary = sync_stores(&a, &b).await?;
            println!("copied_to_a: {}", summary.copied_to_a);
            println!("copied_to_b: {}", summary.copied_to_b);
            println!("merged:      {}", summary.merged);
            println!("conflicts:   {}", summary.conflicts);
        }

        Commands::Ticket { command } => match command {
            TicketCommands::Sync {
                config,
                root,
                author,
            } => {
                let reports = ticket_sync(&config, &root, &author).await?;
                if reports.is_empty() {
                    println!("(no connections configured)");
                }
                for r in reports {
                    println!(
                        "{}: imported {} updated {} unchanged {}",
                        r.connection, r.summary.imported, r.summary.updated, r.summary.unchanged
                    );
                }
            }
            TicketCommands::List { root } => {
                let keys = list(&root, Some("tickets".into()), None).await?;
                if keys.is_empty() {
                    println!("(no tickets)");
                } else {
                    for k in keys {
                        println!("{k}");
                    }
                }
            }
            TicketCommands::Get { root, uid } => {
                // Phase 1: github-projects is the only provider, so every ticket
                // record lives under collection "github" (see gonzalo_ticket::record_key).
                // `ticket list` (above) filters only the "tickets" namespace, so it
                // spans all providers; `get` needs the exact collection.
                match get(&root, "tickets", "github", &uid).await? {
                    Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
                    None => println!("not found"),
                }
            }
        },
    }

    Ok(())
}
