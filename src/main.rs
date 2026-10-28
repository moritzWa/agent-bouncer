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

/// What you, the one starting the job, expect it to take. You know what the job
/// does better than any history, so the model gets this as one more clue.
#[derive(clap::Args, Clone, Default)]
struct Expect {
    /// Your guess of the job's peak RAM, like 800mb or 4gb.
    #[arg(long, value_parser = parse_mb_arg)]
    expect_ram: Option<u64>,
    /// Your guess of the CPU cores it will use at its busiest.
    #[arg(long)]
    expect_cores: Option<f64>,
    /// Your guess of how long it runs, like 90s, 3m or 2h.
    #[arg(long, value_parser = parse_secs_arg)]
    expect_secs: Option<u64>,
}

fn parse_mb_arg(text: &str) -> Result<u64, String> {
    predict::parse_mb(text).ok_or_else(|| "use a size like 800mb or 4gb".to_string())
}

fn parse_secs_arg(text: &str) -> Result<u64, String> {
    predict::parse_secs(text).ok_or_else(|| "use a time like 90s, 3m or 2h".to_string())
}

impl Expect {
    fn hint(&self) -> predict::Hint {
        predict::Hint {
            ram_mb: self.expect_ram,
            cores: self.expect_cores.filter(|c| *c > 0.0),
            secs: self.expect_secs,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a command once the machine has room for it.
    /// Exits 75 when busy (or when the wait limit passes).
    Run {
        /// Wait in line instead of exiting 75 right away.
        #[arg(long)]
        wait: bool,
        /// Longest wait in seconds (implies --wait). 0 means no limit. Default 1800.
        #[arg(long)]
        max_wait: Option<u64>,
        /// Counts as this many jobs against the optional AGENT_BOUNCER_SLOTS cap.
        #[arg(long, default_value_t = 1)]
        weight: usize,
        /// Name shown in status and stats instead of the command line.
        #[arg(long)]
        label: Option<String>,
        /// Do not kill leftover processes of the job when it ends.
        #[arg(long)]
        no_reap: bool,
        #[command(flatten)]
        expect: Expect,
        #[arg(last = true, required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Show machine load, running jobs and the line.
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
        /// Print exactly what the model would be sent, then stop.
        #[arg(long)]
        show: bool,
        #[command(flatten)]
        expect: Expect,
        #[arg(last = true, required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Check that the model and its key work, with a real test call.
    Doctor,
    /// Print the last events from the log as JSON lines.
    Log {
        #[arg(short, default_value_t = 20)]
        n: usize,
    },
}

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Cmd::Run {
            wait,
            max_wait,
            weight,
            label,
            no_reap,
            expect,
            command,
        } => run::run(run::RunOpts {
            command,
            wait: wait || max_wait.is_some(),
            max_wait_secs: max_wait.unwrap_or(1800),
            weight,
            label,
            reap: !no_reap,
            hint: expect.hint(),
        }),
        Cmd::Status { json, cpu } => {
            let root = state::root();
            let _ = state::ensure_dirs(&root);
            let report = status::collect(&root, &state::Settings::from_env(), cpu);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).unwrap_or_default()
                );
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
        Cmd::Doctor => {
            let (ok, text) = predict::doctor(&state::Settings::from_env());
            println!("{text}");
            if ok {
                0
            } else {
                1
            }
        }
        Cmd::Predict {
            show,
            expect,
            command,
        } => {
            let root = state::root();
            let _ = state::ensure_dirs(&root);
            let settings = state::Settings::from_env();
            let cwd = std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let running = status::running_jobs(&root, settings.slots).0;
            let (cmd, hint) = (command.join(" "), expect.hint());
            if show {
                print!(
                    "{}",
                    predict::show_prompt(&root, None, &cmd, &cwd, &running, &hint)
                );
            } else {
                let p = predict::predict(&root, &settings, None, &cmd, &cwd, &running, &hint);
                println!("{} MB, {:.1} cores ({})", p.ram_mb, p.cores, p.source);
            }
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
