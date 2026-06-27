use clap::{Parser, Subcommand};
use codex_task::{
    AppError, FinalizeOptions, InspectOptions, LogsOptions, ResetTaskOptions, ResumeOptions,
    ReviewOptions, RunTaskOptions, StartOptions, StatusResult, TaskPhase, VerifyOptions,
    WatchOptions, finalize_run, find_repo_root, format_doctor_text, format_inspect_text,
    format_logs_text, format_reset_text, format_status_text, home_dir, init_project, inspect_run,
    load_status, read_run_logs, reset_task, resume_run, review_task, run_doctor, run_one_task,
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
    /// Show run store, active/archive paths, and editable task/state files.
    Inspect {
        /// Run id under the repository run store. Archived runs are searched when no active run exists.
        #[arg(long)]
        run_id: Option<String>,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// List or print run logs from active or archived run artifacts.
    Logs {
        /// Run id under the repository run store. Archived runs are searched when no active run exists.
        #[arg(long)]
        run_id: Option<String>,
        /// Filter logs to one task id.
        #[arg(long)]
        task_id: Option<String>,
        /// Filter logs to one phase, e.g. analyze, implement, verify, review.
        #[arg(long)]
        phase: Option<String>,
        /// Only show the newest matching log file.
        #[arg(long)]
        latest: bool,
        /// Print the last N lines of matching logs instead of listing paths.
        #[arg(long)]
        tail: Option<usize>,
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
    /// Resume a run paused by requirement review after answers.md is filled.
    Resume {
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: String,
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
    /// Reset a non-done task back to a runnable phase after manual fixes.
    Reset {
        /// Task id to reset.
        task_id: String,
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
        /// Runnable phase to reset to. Defaults to implement.
        #[arg(long, value_enum)]
        phase: Option<FromPhase>,
        /// Clear normal attempts so maxAttempts no longer blocks the next run.
        #[arg(long)]
        clear_attempts: bool,
        /// Clear review attempts so maxReviewAttempts no longer blocks review.
        #[arg(long)]
        clear_review_attempts: bool,
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
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
        Commands::Inspect { run_id, json } => {
            let view = inspect_run(&cwd, InspectOptions { run_id })?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).map_err(|err| AppError::Runtime(
                        format!("failed to encode JSON: {err}")
                    ))?
                );
            } else {
                print!("{}", format_inspect_text(&view));
            }
            Ok(0)
        }
        Commands::Logs {
            run_id,
            task_id,
            phase,
            latest,
            tail,
            json,
        } => {
            let view = read_run_logs(
                &cwd,
                LogsOptions {
                    run_id,
                    task_id,
                    phase,
                    latest,
                    tail_lines: tail,
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&view).map_err(|err| AppError::Runtime(
                        format!("failed to encode JSON: {err}")
                    ))?
                );
            } else {
                print!("{}", format_logs_text(&view));
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
            println!("Visible run dir: {}", result.visible_run_dir.display());
            println!("Problem framing: {}", result.problem_status);
            if let Some(path) = result.decision_path {
                println!("Decision: {}", path.display());
            }
            if let Some(path) = result.resolved_problem_path {
                println!("Resolved problem: {}", path.display());
            }
            println!("Requirement review: {}", result.requirement_status);
            if let Some(path) = result.questions_path {
                println!("Questions: {}", path.display());
            }
            if let Some(path) = result.answers_path {
                println!("Answers: {}", path.display());
            }
            if let Some(path) = result.resolved_spec_path {
                println!("Resolved spec: {}", path.display());
            }
            println!("Tasks: {}", result.tasks_path.display());
            println!("State: {}", result.state_path.display());
            for warning in result.warnings {
                println!("warning: {warning}");
            }
            if result.problem_status == "needs_decision"
                || result.problem_status == "failed"
                || result.requirement_status == "needs_clarification"
                || result.requirement_status == "failed"
            {
                Ok(1)
            } else {
                Ok(0)
            }
        }
        Commands::Resume { run_id } => {
            let result = resume_run(
                &cwd,
                ResumeOptions {
                    run_id,
                    codex_bin: None,
                },
            )?;
            println!("Resumed run {}", result.run_id);
            println!("Branch: {}", result.branch);
            println!("Spec: {}", result.spec_file);
            println!("Run store: {}", result.run_dir.display());
            println!("Visible run dir: {}", result.visible_run_dir.display());
            println!("Problem framing: {}", result.problem_status);
            if let Some(path) = result.resolved_problem_path {
                println!("Resolved problem: {}", path.display());
            }
            println!("Requirement review: {}", result.requirement_status);
            if let Some(path) = result.resolved_spec_path {
                println!("Resolved spec: {}", path.display());
            }
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
        Commands::Reset {
            task_id,
            run_id,
            phase,
            clear_attempts,
            clear_review_attempts,
            json,
        } => {
            let result = reset_task(
                &cwd,
                ResetTaskOptions {
                    run_id,
                    task_id,
                    phase: phase.unwrap_or(FromPhase::Implement).into_task_phase(),
                    clear_attempts,
                    clear_review_attempts,
                },
            )?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).map_err(|err| AppError::Runtime(
                        format!("failed to encode JSON: {err}")
                    ))?
                );
            } else {
                print!("{}", format_reset_text(&result));
            }
            Ok(0)
        }
    }
}
