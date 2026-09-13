//! curam — zero-trust coding agent harness (Rust).
//!
//! H1: CLI surface (clap) + config layering/validation + init + doctor.
//! Exit codes are stable (plan §6): 0 ok · 1 runtime · 2 usage/config ·
//! 3 provider · 4 budget · 5 denied · 130 SIGINT.
//!
//! Roadmap: docs/rust-harness-plan.md in the curam-harness repo.

use clap::{Parser, Subcommand, ValueEnum};

mod config;
mod doctor;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_PROVIDER: i32 = 3;
pub const EXIT_BUDGET: i32 = 4;
pub const EXIT_DENIED: i32 = 5;

#[derive(Parser)]
#[command(
    name = "curam",
    version,
    about = "zero-trust coding agent harness",
    long_about = None,
)]
struct Cli {
    /// Path to a config file (default: ./curam.toml, then ~/.config/curam/curam.toml)
    #[arg(short = 'C', long)]
    config: Option<String>,

    /// Journal database path override (default from config [journal] path)
    #[arg(long)]
    db: Option<String>,

    /// Verbose diagnostics on stderr
    #[arg(short, long)]
    verbose: bool,

    /// Suppress realtime output (scripts, CI)
    #[arg(short, long)]
    quiet: bool,

    /// Color output mode
    #[arg(long, value_enum, default_value_t = Color::Auto)]
    color: Color,

    /// What to do (default: run TASK as a one-shot task)
    #[command(subcommand)]
    command: Option<Commands>,

    /// One-shot task text
    task: Vec<String>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Color {
    /// Follow the terminal unless piped (default)
    Auto,
    /// Always emit ANSI colors
    Always,
    /// Never emit ANSI colors
    Never,
}

#[derive(Subcommand)]
enum Commands {
    /// Scaffold curam.toml + workdir + skills/ + evals/ in a directory
    Init {
        /// Directory to scaffold (default: current directory)
        dir: Option<String>,
    },

    /// Inspect or validate the effective configuration
    Config {
        /// Print the effective (merged) configuration as TOML
        #[arg(long)]
        show: bool,
        /// Validate configuration, print warnings, exit non-zero on errors
        #[arg(long)]
        validate: bool,
        /// Print the path of the config file that won (or "-" for defaults)
        #[arg(long)]
        path: bool,
        /// Set KEY=VALUE in the project config file (e.g. budget.cap_usd=0.05)
        #[arg(long)]
        set: Option<String>,
    },

    /// Preflight the environment: config, workdir, cage, lists, terminal
    Doctor,

    /// (H5) Full-screen TUI — not implemented yet
    Tui,

    /// (H5) Linear REPL — not implemented yet
    Repl,

    /// (H6) Journal SLO summary — not implemented yet
    Sli,

    /// (H6) Session performance report — not implemented yet
    Report {
        /// Only sessions from the last N (e.g. 2h, 30m, 1d)
        #[arg(long)]
        since: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },

    /// (H7) Run the offline eval corpus — not implemented yet
    Evals {
        /// Run a single task by name
        #[arg(long)]
        task: Option<String>,
        /// Emit JSON results
        #[arg(long)]
        json: bool,
    },

    /// (H7) List the eval tasks — not implemented yet
    Tasks,

    /// (H7) List maintenance (delegates to curam-core) — not implemented yet
    Lists {
        /// Update a named list from its pinned snapshot
        #[arg(long)]
        update: Option<String>,
        /// Print installed list status
        #[arg(long)]
        status: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    std::process::exit(run(&cli));
}

fn run(cli: &Cli) -> i32 {
    let loader = config::Loader::new(cli.config.clone(), cli.db.clone());
    match &cli.command {
        None => {
            // No subcommand: either a one-shot task or nothing.
            if cli.task.is_empty() {
                println!(
                    "curam {} — zero-trust coding agent harness",
                    env!("CARGO_PKG_VERSION")
                );
                println!("No task given. Run  curam \"<describe the task>\"  or  curam --help");
                println!("(the TUI/REPL land in H5)");
                return EXIT_OK;
            }
            one_shot(cli.task.join(" "))
        }
        Some(Commands::Init { dir }) => cmd_init(&loader, dir),
        Some(Commands::Config { show, validate, path, set }) => {
            cmd_config(&loader, show, validate, path, set)
        }
        Some(Commands::Doctor) => cmd_doctor(&loader),
        Some(cmd) => not_yet(cmd),
    }
}

/// One-shot task: the provider + agent loop land in H3.
fn one_shot(task: String) -> i32 {
    eprintln!("curam: task not run — the agent loop (provider + tools) lands in H3 (task was: {task})");
    EXIT_ERROR
}

fn cmd_init(_loader: &config::Loader, dir: &Option<String>) -> i32 {
    let target = match dir {
        Some(d) => std::path::PathBuf::from(d.as_str()),
        None => std::path::PathBuf::from("."),
    };
    match config::scaffold(&target) {
        Ok(report) => {
            for line in report {
                println!("{line}");
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("curam init: {e}");
            EXIT_ERROR
        }
    }
}

fn cmd_config(
    loader: &config::Loader,
    show: &bool,
    validate: &bool,
    path: &bool,
    set: &Option<String>,
) -> i32 {
    // --set writes to the project file and is independent of loading.
    if let Some(kv) = set {
        let (key, value) = split_key_value(kv.as_str());
        let project = std::path::PathBuf::from("curam.toml");
        match config::set_key(&project, key, value) {
            Ok(_) => println!("curam config: set {} = {} (curam.toml)", key, value),
            Err(e) => {
                eprintln!("curam config: {}", e.render());
                return EXIT_USAGE;
            }
        }
    }
    let cfg = loader.load();
    match cfg {
        Err(e) => {
            eprintln!("{}", e.render());
            return EXIT_USAGE;
        }
        Ok(c) => {
            for w in &c.warnings {
                eprintln!("[curam] config warning: {w}");
            }
            if *path {
                match &c.config_path {
                    Some(p) => println!("{}", p.to_str().unwrap_or_default()),
                    None => println!("-"),
                }
            }
            if *show {
                println!("{}", c.effective_toml);
            }
            if *validate {
                if c.has_errors() {
                    return EXIT_USAGE;
                }
                println!("curam config: ok");
            }
        }
    }
    EXIT_OK
}

/// Split "key.path=value" on the first '='.
fn split_key_value(kv: &str) -> (&str, &str) {
    match kv.find('=') {
        Some(i) => (&kv[0..i], &kv[i + 1..]),
        None => (kv, ""),
    }
}

fn cmd_doctor(loader: &config::Loader) -> i32 {
    let loaded = loader.load();
    match loaded {
        Err(e) => {
            eprintln!("{}", e.render());
            return EXIT_USAGE;
        }
        Ok(cfg) => {
            for w in &cfg.warnings {
                eprintln!("[curam] config warning: {w}");
            }
            println!("curam doctor");
            let checks = doctor::run(&cfg);
            let mut all_ok = true;
            for c in checks {
                all_ok &= c.ok;
                let mark = if c.ok { "✓" } else { "✗" };
                println!("  [{mark}] {} {}", c.name, c.detail);
            }
            if !all_ok {
                eprintln!("curam doctor: some checks failed — fix above, then retry");
                return EXIT_ERROR;
            }
            println!("curam doctor: all checks passed");
        }
    }
    EXIT_OK
}

fn not_yet(cmd: &Commands) -> i32 {
    eprintln!(
        "curam: '{}' is registered but not implemented yet (see docs/rust-harness-plan.md build phases)",
        cmd_name(cmd)
    );
    EXIT_ERROR
}

fn cmd_name(cmd: &Commands) -> &str {
    match cmd {
        Commands::Init { .. } => "init",
        Commands::Config { .. } => "config",
        Commands::Doctor => "doctor",
        Commands::Tui => "tui",
        Commands::Repl => "repl",
        Commands::Sli => "sli",
        Commands::Report { .. } => "report",
        Commands::Evals { .. } => "evals",
        Commands::Tasks => "tasks",
        Commands::Lists { .. } => "lists",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_key_value_splits_on_first_equals() {
        assert_eq!(split_key_value("budget.cap_usd=0.05"), ("budget.cap_usd", "0.05"));
        assert_eq!(split_key_value("ui.theme"), ("ui.theme", ""));
    }
}