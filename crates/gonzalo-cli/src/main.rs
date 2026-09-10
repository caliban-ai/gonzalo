//! `gonzalo` — admin/ops CLI for the gonzalo persistence layer.

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use gonzalo_cli::{
    IndexFilter, WatchConfig, gc, get, index_with_gc_filtered, list, migrate, resolve_parse_worker,
    status, sync_stores, ticket_move, ticket_sync, watch,
};
use gonzalo_core::RecordKind;
use gonzalo_store_fs::expand_tilde;
use std::path::PathBuf;
use std::time::Duration;

/// Admin/ops CLI for the gonzalo persistence layer.
/// clap parser for a store-root argument: expands a leading `~`.
///
/// argv arrives verbatim when nothing shell-like launched the process — a
/// systemd unit, a container `command:`, a scheduler — so `--root ~/.gonzalo`
/// has exactly the same trap as `GONZALO_ROOT` did (#211). Applied at parse
/// time so every subcommand's root is expanded the same way.
fn store_root(raw: &str) -> Result<PathBuf, std::convert::Infallible> {
    Ok(expand_tilde(raw))
}

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
        #[arg(long, default_value = ".", value_parser = store_root)]
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
        #[arg(long, default_value = ".", value_parser = store_root)]
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
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
    },
    /// Recursively import files from a directory into the store.
    Migrate {
        /// Root directory of the fs store (destination).
        #[arg(long, default_value = ".", value_parser = store_root)]
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
    /// Index a source tree into a code-graph view (parse `.rs` files into
    /// content-addressed slices and reconcile the view's manifest).
    Index {
        /// Root directory of the fs store (destination).
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
        /// Source directory to index.
        src: PathBuf,
        /// Repository identifier for the view, e.g. `acme/widgets`.
        #[arg(long)]
        repo: String,
        /// View id, e.g. `main`.
        #[arg(long, default_value = "main")]
        view: String,
        /// After indexing, sweep orphaned slices across all live views (opt-in;
        /// always a whole-store GC, never a per-view subset).
        #[arg(long)]
        gc: bool,
        /// Keep running: re-index on filesystem changes (debounced) with a
        /// periodic full reconcile, until Ctrl-C.
        #[arg(long)]
        watch: bool,
        /// In `--watch`, quiet period after the last change before re-indexing.
        #[arg(long, default_value_t = 500)]
        debounce_ms: u64,
        /// In `--watch`, seconds between self-healing full reconciles.
        #[arg(long, default_value_t = 300)]
        reconcile_secs: u64,
        /// Index this repo-relative path even though a built-in rule would skip
        /// it (vendored code you do want in the graph). Repeatable; matched on
        /// whole path components. Cannot override `.gitignore` — a view must
        /// stay reproducible from the commit alone.
        #[arg(long = "include", value_name = "PATH")]
        include: Vec<String>,
        /// Fail instead of falling back to in-process parsing when no
        /// `gonzalo-parse-worker` can be found. Crash isolation is otherwise a
        /// silent best-effort, which is not something CI or a container build
        /// should have to take on trust (#212).
        #[arg(long)]
        require_parse_worker: bool,
    },
    /// Garbage-collect orphaned code-graph slices, marking against every live
    /// view's manifest across all repos.
    Gc {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
    },
    /// Sync two filesystem stores.
    Sync {
        /// Root directory of store A.
        #[arg(value_parser = store_root)]
        a: PathBuf,
        /// Root directory of store B.
        #[arg(value_parser = store_root)]
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
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
        /// Author recorded on imported records.
        #[arg(long, default_value = "gonzalo-cli")]
        author: String,
    },
    /// List imported ticket record keys.
    List {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
    },
    /// Show one imported ticket record by uid (e.g. "caliban-ai/gonzalo#15").
    Get {
        /// Root directory of the fs store.
        #[arg(long, default_value = ".", value_parser = store_root)]
        root: PathBuf,
        /// Connection name the ticket was synced under. Required to find records
        /// synced from a board, whose keys are scoped by connection (#159).
        #[arg(long)]
        connection: Option<String>,
        /// Ticket uid (owner/repo#number).
        uid: String,
    },
    /// Move a board card to the column for a normalized state category.
    Move {
        /// Path to the tickets TOML config.
        #[arg(long, default_value = "tickets.toml")]
        config: PathBuf,
        /// Connection name (optional when only one is configured).
        #[arg(long)]
        connection: Option<String>,
        /// Ticket uid (owner/repo#number).
        uid: String,
        /// Target category: triage|backlog|open|in_progress|pending|done|canceled.
        category: String,
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
            // Automation-driven CLI: an absent record is an error, not a success.
            // Report to stderr (stdout stays empty for clean piping) and let main
            // map the `Err` to a non-zero exit so callers can tell absent apart
            // from present.
            None => anyhow::bail!("record not found: {namespace}/{collection}/{id}"),
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

        Commands::Index {
            root,
            src,
            repo,
            view,
            gc,
            watch: watch_mode,
            debounce_ms,
            reconcile_secs,
            include,
            require_parse_worker,
        } => {
            let filter = IndexFilter::new(&include);

            // Say which parse mode is active before doing any work. The two
            // modes produce identical graphs, so this line is the only way to
            // know whether a grammar crash will skip one file or kill the run.
            let parse_mode = resolve_parse_worker();
            if require_parse_worker && !parse_mode.is_isolated() {
                anyhow::bail!(
                    "--require-parse-worker: no {} found.\n{}",
                    "gonzalo-parse-worker",
                    parse_mode.warning().unwrap_or_default()
                );
            }
            println!("parse:    {}", parse_mode.summary());
            if let Some(warning) = parse_mode.warning() {
                eprintln!("{warning}");
            }
            if watch_mode {
                let config = WatchConfig {
                    debounce: Duration::from_millis(debounce_ms),
                    full_reconcile: Duration::from_secs(reconcile_secs),
                };
                // Thread `--gc` into the watch loop so each reconcile sweeps when
                // requested, rather than silently dropping the flag (#157).
                watch(&root, &src, &repo, &view, config, gc).await?;
                return Ok(());
            }
            let (summary, swept) =
                index_with_gc_filtered(&root, &src, &repo, &view, gc, &filter).await?;
            println!(
                "driver:   {}",
                if summary.incremental {
                    "incremental (git diff)"
                } else {
                    "full walk"
                }
            );
            println!("files:    {}", summary.files);
            println!("added:    {}", summary.added);
            println!("modified: {}", summary.modified);
            println!("deleted:  {}", summary.deleted);
            println!("skipped:  {}", summary.skipped);
            println!(
                "ignored:  {} files, {} dirs not descended",
                summary.ignored.files, summary.ignored.dirs
            );
            if summary.unindexed.files > 0 {
                println!(
                    "unindexed: {} files, no grammar{}",
                    summary.unindexed.files,
                    named_extensions(&summary.unindexed.extensions())
                );
            }
            // The case a user is most likely to misread: a view that indexed
            // nothing looks exactly like an empty repository unless we say
            // otherwise (#259).
            if summary.files == 0 && summary.unindexed.files > 0 {
                println!(
                    "note:     nothing was indexed — gonzalo parses none of the files in this tree"
                );
            }
            if let Some(swept) = swept {
                println!("gc.freed:    {}", swept.freed);
                println!("gc.retained: {}", swept.retained);
            }
        }

        Commands::Gc { root } => {
            let summary = gc(&root).await?;
            println!("manifests: {}", summary.manifests);
            println!("freed:     {}", summary.freed);
            println!("retained:  {}", summary.retained);
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
            TicketCommands::Get {
                root,
                connection,
                uid,
            } => {
                // Phase 1: github-projects is the only provider, so every ticket
                // record lives under collection "github" (see gonzalo_ticket::record_key).
                // `ticket list` (above) filters only the "tickets" namespace, so it
                // spans all providers; `get` needs the exact collection + id.
                // Board records key their id as "<connection>/<uid>" (#159), so
                // pass --connection to reconstruct the exact id; without it we look
                // up the bare uid (plain/unscoped records).
                let id = gonzalo_ticket::scoped_uid(&uid, connection.as_deref());
                match get(&root, "tickets", "github", &id).await? {
                    Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
                    // Absent ticket → stderr message + non-zero exit (stdout empty)
                    // so automation can distinguish missing from present.
                    None => anyhow::bail!("ticket not found: {uid}"),
                }
            }
            TicketCommands::Move {
                config,
                connection,
                uid,
                category,
            } => {
                ticket_move(&config, connection.as_deref(), &uid, &category).await?;
                println!("moved {uid} → {category}");
            }
        },
    }

    Ok(())
}

/// ` (.ipynb, .org)` for a report, or empty when nothing has an extension to
/// name. Bounded, so a tree full of odd files does not print a wall of text.
fn named_extensions(extensions: &[(String, usize)]) -> String {
    const MOST: usize = 5;
    if extensions.is_empty() {
        return String::new();
    }
    let named: Vec<String> = extensions
        .iter()
        .take(MOST)
        .map(|(extension, _)| format!(".{extension}"))
        .collect();
    let more = extensions.len().saturating_sub(MOST);
    let tail = if more > 0 {
        format!(", and {more} more")
    } else {
        String::new()
    };
    format!(" for {}{}", named.join(", "), tail)
}
