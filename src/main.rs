use clap::{Parser, Subcommand};
use codex_task::{
    AppError, FinalizeOptions, ReviewOptions, RunTaskOptions, StartOptions, StatusResult,
    TaskPhase, VerifyOptions, WatchOptions, finalize_run, find_repo_root, format_doctor_text,
    format_status_text, home_dir, init_project, load_status, review_task, run_doctor, run_one_task,
    start_run, verify_tasks, watch_run,
};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "codex-task")]
#[command(about = "Local Codex task runner")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Initialize .codex/task-runner.toml in the current git repository.
    Init {
        /// Overwrite an existing project config.
        #[arg(long)]
        force: bool,
    },
    /// Check local tools, project config, prompt templates, and run store.
    Doctor {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a read-only status view for a run.
    Status {
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Create or resume a run from a feature specification.
    Start {
        /// Feature specification Markdown file.
        spec: PathBuf,
        /// Override the generated run id.
        #[arg(long)]
        run_id: Option<String>,
        /// Override the generated feature branch.
        #[arg(long)]
        branch: Option<String>,
        /// Require an existing run instead of decomposing a new one.
        #[arg(long)]
        resume: bool,
    },
    /// Run pending tasks serially through analyze and implement phases.
    Watch {
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
        /// Delay between scheduler iterations.
        #[arg(long, default_value_t = 0)]
        interval: u64,
        /// Stop after this many consecutive task failures.
        #[arg(long)]
        max_failures: Option<u64>,
    },
    /// Run one task through analyze and implement phases.
    Run {
        /// Task id to run.
        task_id: String,
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
        /// Start from a specific phase. Only analyze and implement execute in this milestone.
        #[arg(long, value_enum)]
        from: Option<FromPhase>,
    },
    /// Execute verification commands for one task or every task waiting at verify.
    Verify {
        /// Task id, or "all" for every task currently waiting at verify.
        target: String,
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Execute read-only review for one task.
    Review {
        /// Task id to review.
        task_id: String,
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Execute MVP final feature review.
    Finalize {
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
        /// Keep run artifacts after final review. Cleanup is not implemented in this milestone.
        #[arg(long)]
        no_cleanup: bool,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum FromPhase {
    Analyze,
    Implement,
    Verify,
    Review,
    Commit,
}

impl FromPhase {
    fn into_task_phase(self) -> TaskPhase {
        match self {
            FromPhase::Analyze => TaskPhase::Analyze,
            FromPhase::Implement => TaskPhase::Implement,
            FromPhase::Verify => TaskPhase::Verify,
            FromPhase::Review => TaskPhase::Review,
            FromPhase::Commit => TaskPhase::Commit,
        }
    }
}

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(err.exit_code());
        }
    }
}

fn run(cli: Cli) -> Result<i32, AppError> {
    let cwd = std::env::current_dir()
        .map_err(|err| AppError::Runtime(format!("failed to read current directory: {err}")))?;

    match cli.command {
        Commands::Init { force } => {
            let path = init_project(&cwd, force)?;
            println!("Initialized {}", path.display());
            Ok(0)
        }
        Commands::Doctor { json } => {
            let report = run_doctor(&cwd);
            if json {
                let encoded = serde_json::to_string_pretty(&report)
                    .map_err(|err| AppError::Runtime(format!("failed to encode JSON: {err}")))?;
                println!("{encoded}");
            } else {
                print!("{}", format_doctor_text(&report));
            }
            Ok(report.exit_code())
        }
        Commands::Status { run_id, json } => {
            let repo_root = find_repo_root(&cwd)?;
            let home = home_dir()?;
            match load_status(&repo_root, &home, run_id.as_deref())? {
                StatusResult::View(view) => {
                    if json {
                        let encoded = serde_json::to_string_pretty(&view).map_err(|err| {
                            AppError::Runtime(format!("failed to encode JSON: {err}"))
                        })?;
                        println!("{encoded}");
                    } else {
                        print!("{}", format_status_text(&view));
                    }
                }
                StatusResult::Message(message) => {
                    if json {
                        let encoded = serde_json::json!({ "message": message });
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&encoded).map_err(|err| {
                                AppError::Runtime(format!("failed to encode JSON: {err}"))
                            })?
                        );
                    } else {
                        println!("{message}");
                    }
                }
            }
            Ok(0)
        }
        Commands::Start {
            spec,
            run_id,
            branch,
            resume,
        } => {
            let result = start_run(
                &cwd,
                StartOptions {
                    spec_path: spec,
                    run_id,
                    branch,
                    resume,
                    codex_bin: None,
                },
            )?;
            println!(
                "{} run {}",
                if result.resumed { "Resumed" } else { "Started" },
                result.run_id
            );
            println!("Branch: {}", result.branch);
            println!("Spec: {}", result.spec_file);
            println!("Run store: {}", result.run_dir.display());
            println!("Tasks: {}", result.tasks_path.display());
            println!("State: {}", result.state_path.display());
            for warning in result.warnings {
                println!("warning: {warning}");
            }
            Ok(0)
        }
        Commands::Watch {
            run_id,
            interval,
            max_failures,
        } => {
            let result = watch_run(
                &cwd,
                WatchOptions {
                    run_id,
                    interval_seconds: interval,
                    max_failures,
                    codex_bin: None,
                },
            )?;
            println!("{}", result.message);
            Ok(result.exit_code)
        }
        Commands::Run {
            task_id,
            run_id,
            from,
        } => {
            let result = run_one_task(
                &cwd,
                RunTaskOptions {
                    run_id,
                    task_id,
                    from: from.map(FromPhase::into_task_phase),
                    codex_bin: None,
                },
            )?;
            println!("{}", result.message);
            Ok(result.exit_code)
        }
        Commands::Verify { target, run_id } => {
            let result = verify_tasks(&cwd, VerifyOptions { run_id, target })?;
            println!("{}", result.message);
            Ok(result.exit_code)
        }
        Commands::Review { task_id, run_id } => {
            let result = review_task(
                &cwd,
                ReviewOptions {
                    run_id,
                    task_id,
                    codex_bin: None,
                },
            )?;
            println!("{}", result.message);
            Ok(result.exit_code)
        }
        Commands::Finalize { run_id, no_cleanup } => {
            let result = finalize_run(
                &cwd,
                FinalizeOptions {
                    run_id,
                    no_cleanup,
                    codex_bin: None,
                },
            )?;
            println!("{}", result.message);
            Ok(result.exit_code)
        }
    }
}
