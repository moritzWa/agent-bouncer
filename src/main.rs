mod headroom;
mod predict;
mod run;
mod state;
mod stats;
mod status;
mod sysinfo;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent-bouncer",
    version,
    about = "A bouncer for your CPU and RAM: a first-come-first-served queue for heavy jobs, built for many parallel coding agents."
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command once a slot is free and the machine has room.
    /// Exits 75 when busy (or when the wait limit passes).
    Run {
        /// Wait in line instead of exiting 75 right away.
        #[arg(long)]
        wait: bool,
        /// Longest wait in seconds (implies --wait). 0 means no limit. Default 1800.
        #[arg(long)]
        max_wait: Option<u64>,
        /// Slots this job takes (a heavy job can take several).
        #[arg(long, default_value_t = 1)]
        weight: usize,
        /// Name shown in status and stats instead of the command line.
        #[arg(long)]
        label: Option<String>,
        /// Do not kill leftover processes of the job when it ends.
        #[arg(long)]
        no_reap: bool,
        #[arg(last = true, required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Show slots, machine load, running jobs and the line.
    Status {
        #[arg(long)]
        json: bool,
        /// Also sample the CPU (takes a second).
        #[arg(long)]
        cpu: bool,
    },
    /// Summarize waits, refusals and memory use from the event log.
    Stats {
        /// Window, like 90m, 12h, 7d.
        #[arg(long, default_value = "7d")]
        since: String,
    },
    /// Show what a command is expected to need, without running it.
    Predict {
        #[arg(last = true, required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Print the last events from the log as JSON lines.
    Log {
        #[arg(short, default_value_t = 20)]
        n: usize,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Cmd::Run { wait, max_wait, weight, label, no_reap, command } => run::run(run::RunOpts {
            command,
            wait: wait || max_wait.is_some(),
            max_wait_secs: max_wait.unwrap_or(1800),
            weight,
            label,
            reap: !no_reap,
        }),
        Cmd::Status { json, cpu } => {
            let root = state::root();
            let _ = state::ensure_dirs(&root);
            let report = status::collect(&root, &state::Settings::from_env(), cpu);
            if json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            } else {
                print!("{}", status::render(&report));
            }
            0
        }
        Cmd::Stats { since } => {
            let Some(window_ms) = stats::parse_duration(&since) else {
                eprintln!("agent-bouncer: --since must look like 90m, 12h or 7d");
                std::process::exit(run::EXIT_USAGE);
            };
            let events = state::read_events(&state::root());
            let from = state::now_ms().saturating_sub(window_ms);
            let result = stats::aggregate(&events, from, &state::local_day);
            print!("{}", stats::render(&result, &since));
            0
        }
        Cmd::Predict { command } => {
            let root = state::root();
            let _ = state::ensure_dirs(&root);
            let settings = state::Settings::from_env();
            let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
            let running = status::running_jobs(&root, settings.slots).0;
            let p = predict::predict(&root, &settings, None, &command.join(" "), &cwd, &running);
            println!("{} MB, {:.1} cores ({})", p.ram_mb, p.cores, p.source);
            0
        }
        Cmd::Log { n } => {
            let events = state::read_events(&state::root());
            let skip = events.len().saturating_sub(n);
            for event in &events[skip..] {
                if let Ok(line) = serde_json::to_string(event) {
                    println!("{line}");
                }
            }
            0
        }
    };
    std::process::exit(code);
}
