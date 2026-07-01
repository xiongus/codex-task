use clap::{Parser, Subcommand};
use codex_task::{
    AppError, FinalizeOptions, InspectOptions, LogsOptions, PendingUserInput, PendingUserInputKind,
    ResetTaskOptions, ResumeOptions, ReviewOptions, RunTaskOptions, SkipPhaseOptions, StartOptions,
    StartResult, StatusResult, TaskPhase, VerifyOptions, WatchOptions, finalize_run,
    find_repo_root, format_doctor_text, format_inspect_text, format_logs_text, format_reset_text,
    format_skip_phase_text, format_status_text, home_dir, init_project, inspect_run, load_status,
    pending_user_input, read_run_logs, reset_task, resume_run, review_task, run_doctor,
    run_one_task, skip_phase, start_run, verify_tasks, watch_run,
};
use std::fs;
use std::io::{self, Write};
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
        /// Feature specification Markdown file(s) or directory, in phase order.
        spec: Vec<PathBuf>,
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
    /// Resume a run after decision/answers are filled, then continue scheduling.
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
        /// Run only tasks in this decomposition group.
        #[arg(long)]
        group: Option<String>,
        /// Run only tasks in this roadmap phase.
        #[arg(long)]
        phase: Option<String>,
        /// Run tasks through this roadmap phase, using phase order from tasks.json.
        #[arg(long)]
        until_phase: Option<String>,
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
    /// Skip a roadmap phase and ignore its unfinished generated tasks.
    SkipPhase {
        /// Phase id, e.g. 01-foundation-data-model.
        phase_id: String,
        /// Run id under the repository run store.
        #[arg(long)]
        run_id: Option<String>,
        /// Reason recorded in metadata/state.
        #[arg(long)]
        reason: Option<String>,
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
            let mut specs = spec;
            let spec_path = specs.first().cloned().ok_or_else(|| {
                AppError::Config("start requires at least one spec file".to_string())
            })?;
            let extra_spec_paths = specs.split_off(1);
            let result = start_run(
                &cwd,
                StartOptions {
                    spec_path,
                    spec_paths: extra_spec_paths,
                    run_id,
                    branch,
                    resume,
                    codex_bin: None,
                },
            )?;
            drive_run_interactively(&cwd, result)
        }
        Commands::Resume { run_id } => {
            let result = resume_run(
                &cwd,
                ResumeOptions {
                    run_id,
                    codex_bin: None,
                },
            )?;
            drive_run_interactively(&cwd, result)
        }
        Commands::Watch {
            run_id,
            interval,
            max_failures,
            group,
            phase,
            until_phase,
        } => {
            let mut options = WatchOptions {
                run_id,
                interval_seconds: interval,
                max_failures,
                group,
                phase,
                until_phase,
                codex_bin: None,
            };
            let result = watch_run(&cwd, options.clone())?;
            println!("{}", result.message);
            if result.message == "Run is waiting for phase input" {
                options.run_id = Some(result.run_id);
                continue_run_interactively(&cwd, options)
            } else {
                Ok(result.exit_code)
            }
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
        Commands::SkipPhase {
            phase_id,
            run_id,
            reason,
            json,
        } => {
            let result = skip_phase(
                &cwd,
                SkipPhaseOptions {
                    run_id,
                    phase_id,
                    reason,
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
                print!("{}", format_skip_phase_text(&result));
            }
            Ok(0)
        }
    }
}

fn drive_run_interactively(
    cwd: &std::path::Path,
    mut result: StartResult,
) -> Result<i32, AppError> {
    loop {
        print_start_result(&result);
        if start_result_failed(&result) {
            return Ok(1);
        }
        if !start_result_needs_input(&result) {
            return continue_run_interactively(cwd, default_watch_options(result.run_id));
        }
        handle_pending_input(cwd, &result.run_id)?;
        result = resume_run(
            cwd,
            ResumeOptions {
                run_id: result.run_id,
                codex_bin: None,
            },
        )?;
    }
}

fn default_watch_options(run_id: String) -> WatchOptions {
    WatchOptions {
        run_id: Some(run_id),
        interval_seconds: 0,
        max_failures: None,
        group: None,
        phase: None,
        until_phase: None,
        codex_bin: None,
    }
}

fn continue_run_interactively(
    cwd: &std::path::Path,
    options: WatchOptions,
) -> Result<i32, AppError> {
    let run_id = options
        .run_id
        .clone()
        .ok_or_else(|| AppError::Config("interactive watch requires a run id".to_string()))?;
    loop {
        println!("Continuing run {run_id}");
        let scheduler = watch_run(cwd, options.clone())?;
        println!("{}", scheduler.message);
        if scheduler.message != "Run is waiting for phase input" {
            return Ok(scheduler.exit_code);
        }
        handle_pending_input(cwd, &run_id)?;
        let mut result = resume_run(
            cwd,
            ResumeOptions {
                run_id: run_id.clone(),
                codex_bin: None,
            },
        )?;
        loop {
            print_start_result(&result);
            if start_result_failed(&result) {
                return Ok(1);
            }
            if !start_result_needs_input(&result) {
                break;
            }
            handle_pending_input(cwd, &result.run_id)?;
            result = resume_run(
                cwd,
                ResumeOptions {
                    run_id: result.run_id,
                    codex_bin: None,
                },
            )?;
        }
    }
}

fn print_start_result(result: &StartResult) {
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
    if let Some(path) = &result.decision_path {
        println!("Decision: {}", path.display());
    }
    if let Some(path) = &result.resolved_problem_path {
        println!("Resolved problem: {}", path.display());
    }
    println!("Requirement review: {}", result.requirement_status);
    if let Some(path) = &result.questions_path {
        println!("Questions: {}", path.display());
    }
    if let Some(path) = &result.answers_path {
        println!("Answers: {}", path.display());
    }
    if let Some(path) = &result.resolved_spec_path {
        println!("Resolved spec: {}", path.display());
    }
    println!("Tasks: {}", result.tasks_path.display());
    println!("State: {}", result.state_path.display());
    for warning in &result.warnings {
        println!("warning: {warning}");
    }
}

fn start_result_needs_input(result: &StartResult) -> bool {
    result.problem_status == "needs_decision" || result.requirement_status == "needs_clarification"
}

fn start_result_failed(result: &StartResult) -> bool {
    result.problem_status == "failed" || result.requirement_status == "failed"
}

fn handle_pending_input(cwd: &std::path::Path, run_id: &str) -> Result<(), AppError> {
    let Some(pending) = pending_user_input(cwd, Some(run_id))? else {
        return Err(AppError::Runtime(format!(
            "run {run_id} reported waiting for input but no pending input was found"
        )));
    };
    match pending.kind {
        PendingUserInputKind::Decision => fill_marked_response(
            &pending,
            "decision",
            "Decision required. Enter your decision below. Finish with a single '.' line.",
        ),
        PendingUserInputKind::Clarification => fill_marked_response(
            &pending,
            "answers",
            "Clarification required. Enter your answers below. Finish with a single '.' line.",
        ),
    }
}

fn fill_marked_response(
    pending: &PendingUserInput,
    marker: &str,
    prompt: &str,
) -> Result<(), AppError> {
    let source = fs::read_to_string(&pending.prompt_path).map_err(|err| {
        AppError::Io(format!(
            "failed to read {}: {err}",
            pending.prompt_path.display()
        ))
    })?;
    println!("\n{}\n", pending.prompt_path.display());
    println!("{source}");
    println!("{prompt}");
    let response = read_multiline_response()?;
    if response.trim().is_empty() {
        return Err(AppError::Runtime(
            "empty response; not resuming".to_string(),
        ));
    }
    let target = fs::read_to_string(&pending.response_path).map_err(|err| {
        AppError::Io(format!(
            "failed to read {}: {err}",
            pending.response_path.display()
        ))
    })?;
    let updated = replace_marker_section(&target, marker, &response)?;
    fs::write(&pending.response_path, updated).map_err(|err| {
        AppError::Io(format!(
            "failed to write {}: {err}",
            pending.response_path.display()
        ))
    })
}

fn read_multiline_response() -> Result<String, AppError> {
    let mut out = String::new();
    loop {
        print!("> ");
        io::stdout()
            .flush()
            .map_err(|err| AppError::Io(format!("failed to flush stdout: {err}")))?;
        let mut line = String::new();
        let read = io::stdin()
            .read_line(&mut line)
            .map_err(|err| AppError::Io(format!("failed to read stdin: {err}")))?;
        if read == 0 {
            return Err(AppError::Runtime("stdin closed; not resuming".to_string()));
        }
        if line.trim_end() == "." {
            break;
        }
        out.push_str(&line);
    }
    Ok(out)
}

fn replace_marker_section(raw: &str, marker: &str, response: &str) -> Result<String, AppError> {
    let start_marker = format!("<!-- codex-task:{marker}:start -->");
    let end_marker = format!("<!-- codex-task:{marker}:end -->");
    let start = raw
        .find(&start_marker)
        .ok_or_else(|| AppError::Config(format!("response file missing marker {start_marker}")))?;
    let content_start = start + start_marker.len();
    let end_rel = raw[content_start..]
        .find(&end_marker)
        .ok_or_else(|| AppError::Config(format!("response file missing marker {end_marker}")))?;
    let content_end = content_start + end_rel;
    let mut out = String::new();
    out.push_str(&raw[..content_start]);
    out.push('\n');
    out.push_str(response.trim());
    out.push('\n');
    out.push_str(&raw[content_end..]);
    Ok(out)
}
