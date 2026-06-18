use anyhow::{Context, Result};
use fs2::FileExt;
use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

pub const PROMPT_TEMPLATE_NAMES: [&str; 5] = [
    "decompose-feature.md",
    "analyze-task.md",
    "implement-task.md",
    "review-task.md",
    "review-feature.md",
];

const TASK_STATUS_ORDER: [TaskStatus; 8] = [
    TaskStatus::Pending,
    TaskStatus::Running,
    TaskStatus::Reviewed,
    TaskStatus::AnalysisReview,
    TaskStatus::Blocked,
    TaskStatus::Ignored,
    TaskStatus::Done,
    TaskStatus::ReviewFailed,
];

const DEFAULT_PROJECT_CONFIG: &str = r#"verificationCommands = []

[project]
default_branch = "main"
feature_branch_prefix = "feat/"
agent_rules = "AGENTS.md"
overview_doc = "docs/overview.md"

[runner]
verify = "auto"
review = "auto"
search = false
sandbox = "workspace-write"
analysis_sandbox = "read-only"
review_sandbox = "read-only"
approval = "never"
state_store = "global"
dangerous_bypass_approvals_and_sandbox = false
require_clean = "auto"
allow_dirty_resume = true
default_task_timeout_seconds = 1800
default_analyze_timeout_seconds = 900
default_review_timeout_seconds = 600
default_verify_timeout_seconds = 1800
max_consecutive_failures = 3

[prompts]
profile = "default"

[git]
commit = false
add_required = true
add_include = []
add_exclude = []
commit_message = "{task_id}: {title}"
"#;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("{0}")]
    Config(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    RunLocked(String),
    #[error("{0}")]
    DirtyWorktree(String),
    #[error("{0}")]
    Runtime(String),
}

impl AppError {
    pub fn exit_code(&self) -> i32 {
        match self {
            AppError::Config(_) => 2,
            AppError::RunLocked(_) => 4,
            AppError::DirtyWorktree(_) => 3,
            AppError::Io(_) | AppError::Runtime(_) => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Toggle {
    True,
    False,
    #[default]
    Auto,
}

impl<'de> Deserialize<'de> for Toggle {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ToggleVisitor;

        impl<'de> Visitor<'de> for ToggleVisitor {
            type Value = Toggle;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("true, false, or \"auto\"")
            }

            fn visit_bool<E>(self, value: bool) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(if value { Toggle::True } else { Toggle::False })
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                match value {
                    "true" => Ok(Toggle::True),
                    "false" => Ok(Toggle::False),
                    "auto" => Ok(Toggle::Auto),
                    other => Err(E::custom(format!(
                        "invalid toggle value {other:?}; expected true, false, or \"auto\""
                    ))),
                }
            }
        }

        deserializer.deserialize_any(ToggleVisitor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    #[default]
    Pending,
    Running,
    Reviewed,
    AnalysisReview,
    Blocked,
    Ignored,
    Done,
    ReviewFailed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::Running => "running",
            TaskStatus::Reviewed => "reviewed",
            TaskStatus::AnalysisReview => "analysis_review",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Ignored => "ignored",
            TaskStatus::Done => "done",
            TaskStatus::ReviewFailed => "review_failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    Analyze,
    AnalysisReview,
    Implement,
    Verify,
    Review,
    Commit,
    Done,
}

impl TaskPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskPhase::Analyze => "analyze",
            TaskPhase::AnalysisReview => "analysis_review",
            TaskPhase::Implement => "implement",
            TaskPhase::Verify => "verify",
            TaskPhase::Review => "review",
            TaskPhase::Commit => "commit",
            TaskPhase::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FeatureReviewStatus {
    #[default]
    Pending,
    Running,
    Approved,
    ChangesRequested,
    Failed,
    Blocked,
}

impl FeatureReviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FeatureReviewStatus::Pending => "pending",
            FeatureReviewStatus::Running => "running",
            FeatureReviewStatus::Approved => "approved",
            FeatureReviewStatus::ChangesRequested => "changes_requested",
            FeatureReviewStatus::Failed => "failed",
            FeatureReviewStatus::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewVerdict {
    #[serde(rename = "APPROVED")]
    Approved,
    #[serde(rename = "CHANGES_REQUESTED")]
    ChangesRequested,
}

impl ReviewVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewVerdict::Approved => "APPROVED",
            ReviewVerdict::ChangesRequested => "CHANGES_REQUESTED",
        }
    }
}

const DEFAULT_VERIFICATION_COMMAND_NAME: &str = "task-check";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerificationCommand {
    pub name: String,
    pub command: String,
    pub required: bool,
    #[serde(rename = "timeoutSeconds", default)]
    pub timeout_seconds: Option<u64>,
}

impl VerificationCommand {
    pub fn required_shell(command: String) -> Self {
        Self {
            name: DEFAULT_VERIFICATION_COMMAND_NAME.to_string(),
            command,
            required: true,
            timeout_seconds: None,
        }
    }
}

impl<'de> Deserialize<'de> for VerificationCommand {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CommandVisitor;

        impl<'de> Visitor<'de> for CommandVisitor {
            type Value = VerificationCommand;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a shell command string or a verification command object")
            }

            fn visit_str<E>(self, value: &str) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(VerificationCommand::required_shell(value.to_string()))
            }

            fn visit_string<E>(self, value: String) -> std::result::Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(VerificationCommand::required_shell(value))
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut name = None;
                let mut command = None;
                let mut required = None;
                let mut timeout_seconds = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => name = Some(map.next_value()?),
                        "command" => command = Some(map.next_value()?),
                        "required" => required = Some(map.next_value()?),
                        "timeoutSeconds" => timeout_seconds = Some(map.next_value()?),
                        other => {
                            return Err(de::Error::unknown_field(
                                other,
                                &["name", "command", "required", "timeoutSeconds"],
                            ));
                        }
                    }
                }

                let command = command.ok_or_else(|| de::Error::missing_field("command"))?;

                Ok(VerificationCommand {
                    name: name.unwrap_or_else(|| DEFAULT_VERIFICATION_COMMAND_NAME.to_string()),
                    command,
                    required: required.unwrap_or(true),
                    timeout_seconds,
                })
            }
        }

        deserializer.deserialize_any(CommandVisitor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectConfig {
    pub default_branch: String,
    pub feature_branch_prefix: String,
    pub agent_rules: String,
    pub overview_doc: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunnerConfig {
    pub verify: Toggle,
    pub review: Toggle,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub search: bool,
    pub sandbox: String,
    pub analysis_sandbox: String,
    pub review_sandbox: String,
    pub approval: String,
    pub state_store: String,
    pub dangerous_bypass_approvals_and_sandbox: bool,
    pub require_clean: Toggle,
    pub allow_dirty_resume: bool,
    pub default_task_timeout_seconds: u64,
    pub default_analyze_timeout_seconds: u64,
    pub default_review_timeout_seconds: u64,
    pub default_verify_timeout_seconds: u64,
    pub max_consecutive_failures: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptConfig {
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitConfig {
    pub commit: bool,
    pub add_required: bool,
    pub add_include: Vec<String>,
    pub add_exclude: Vec<String>,
    pub commit_message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MergedConfig {
    pub project: ProjectConfig,
    pub runner: RunnerConfig,
    pub prompts: PromptConfig,
    pub git: GitConfig,
    #[serde(rename = "verificationCommands")]
    pub verification_commands: Vec<VerificationCommand>,
}

impl MergedConfig {
    pub fn builtin() -> Self {
        Self {
            project: ProjectConfig {
                default_branch: "main".to_string(),
                feature_branch_prefix: "feat/".to_string(),
                agent_rules: "AGENTS.md".to_string(),
                overview_doc: Some("docs/overview.md".to_string()),
            },
            runner: RunnerConfig {
                verify: Toggle::Auto,
                review: Toggle::Auto,
                model: None,
                reasoning_effort: None,
                search: false,
                sandbox: "workspace-write".to_string(),
                analysis_sandbox: "read-only".to_string(),
                review_sandbox: "read-only".to_string(),
                approval: "never".to_string(),
                state_store: "global".to_string(),
                dangerous_bypass_approvals_and_sandbox: false,
                require_clean: Toggle::Auto,
                allow_dirty_resume: true,
                default_task_timeout_seconds: 1800,
                default_analyze_timeout_seconds: 900,
                default_review_timeout_seconds: 600,
                default_verify_timeout_seconds: 1800,
                max_consecutive_failures: 3,
            },
            prompts: PromptConfig {
                profile: "default".to_string(),
            },
            git: GitConfig {
                commit: false,
                add_required: true,
                add_include: Vec::new(),
                add_exclude: Vec::new(),
                commit_message: "{task_id}: {title}".to_string(),
            },
            verification_commands: Vec::new(),
        }
    }

    fn apply_patch(&mut self, patch: ConfigPatch, allow_dangerous_bypass: bool) {
        let runner_commit_alias = patch.runner.as_ref().and_then(|runner| runner.commit);
        let explicit_git_commit = patch.git.as_ref().and_then(|git| git.commit);

        if let Some(project) = patch.project {
            if let Some(value) = project.default_branch {
                self.project.default_branch = value;
            }
            if let Some(value) = project.feature_branch_prefix {
                self.project.feature_branch_prefix = value;
            }
            if let Some(value) = project.agent_rules {
                self.project.agent_rules = value;
            }
            if project.overview_doc.is_some() {
                self.project.overview_doc = project.overview_doc;
            }
        }

        if let Some(runner) = patch.runner {
            if let Some(value) = runner.verify {
                self.runner.verify = value;
            }
            if let Some(value) = runner.review {
                self.runner.review = value;
            }
            if let Some(value) = runner.model {
                self.runner.model = Some(value);
            }
            if let Some(value) = runner.reasoning_effort {
                self.runner.reasoning_effort = Some(value);
            }
            if let Some(value) = runner.search {
                self.runner.search = value;
            }
            if let Some(value) = runner.sandbox {
                self.runner.sandbox = value;
            }
            if let Some(value) = runner.analysis_sandbox {
                self.runner.analysis_sandbox = value;
            }
            if let Some(value) = runner.review_sandbox {
                self.runner.review_sandbox = value;
            }
            if let Some(value) = runner.approval {
                self.runner.approval = value;
            }
            if let Some(value) = runner.state_store {
                self.runner.state_store = value;
            }
            if allow_dangerous_bypass
                && let Some(value) = runner.dangerous_bypass_approvals_and_sandbox
            {
                self.runner.dangerous_bypass_approvals_and_sandbox = value;
            }
            if let Some(value) = runner.require_clean {
                self.runner.require_clean = value;
            }
            if let Some(value) = runner.allow_dirty_resume {
                self.runner.allow_dirty_resume = value;
            }
            if let Some(value) = runner.default_task_timeout_seconds {
                self.runner.default_task_timeout_seconds = value;
            }
            if let Some(value) = runner.default_analyze_timeout_seconds {
                self.runner.default_analyze_timeout_seconds = value;
            }
            if let Some(value) = runner.default_review_timeout_seconds {
                self.runner.default_review_timeout_seconds = value;
            }
            if let Some(value) = runner.default_verify_timeout_seconds {
                self.runner.default_verify_timeout_seconds = value;
            }
            if let Some(value) = runner.max_consecutive_failures {
                self.runner.max_consecutive_failures = value;
            }
        }

        if let Some(prompts) = patch.prompts
            && let Some(value) = prompts.profile
        {
            self.prompts.profile = value;
        }

        if let Some(git) = patch.git {
            if let Some(value) = git.commit {
                self.git.commit = value;
            }
            if let Some(value) = git.add_required {
                self.git.add_required = value;
            }
            if let Some(value) = git.add_include {
                self.git.add_include = value;
            }
            if let Some(value) = git.add_exclude {
                self.git.add_exclude = value;
            }
            if let Some(value) = git.commit_message {
                self.git.commit_message = value;
            }
        }

        if explicit_git_commit.is_none()
            && let Some(value) = runner_commit_alias
        {
            self.git.commit = value;
        }

        if let Some(commands) = patch.verification_commands {
            self.verification_commands = commands;
        }
    }

    pub fn validate(&self) -> std::result::Result<(), AppError> {
        if self.runner.state_store != "global" {
            return Err(AppError::Config(format!(
                "runner.state_store={} is unsupported; MVP only supports global",
                self.runner.state_store
            )));
        }
        if self
            .runner
            .model
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(AppError::Config(
                "runner.model must not be empty".to_string(),
            ));
        }
        if self
            .runner
            .reasoning_effort
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(AppError::Config(
                "runner.reasoning_effort must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigPatch {
    pub project: Option<ProjectConfigPatch>,
    pub runner: Option<RunnerConfigPatch>,
    pub prompts: Option<PromptConfigPatch>,
    pub git: Option<GitConfigPatch>,
    #[serde(rename = "verificationCommands")]
    pub verification_commands: Option<Vec<VerificationCommand>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProjectConfigPatch {
    pub default_branch: Option<String>,
    pub feature_branch_prefix: Option<String>,
    pub agent_rules: Option<String>,
    pub overview_doc: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunnerConfigPatch {
    pub commit: Option<bool>,
    pub verify: Option<Toggle>,
    pub review: Option<Toggle>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub search: Option<bool>,
    pub sandbox: Option<String>,
    pub analysis_sandbox: Option<String>,
    pub review_sandbox: Option<String>,
    pub approval: Option<String>,
    pub state_store: Option<String>,
    pub dangerous_bypass_approvals_and_sandbox: Option<bool>,
    pub require_clean: Option<Toggle>,
    pub allow_dirty_resume: Option<bool>,
    pub default_task_timeout_seconds: Option<u64>,
    pub default_analyze_timeout_seconds: Option<u64>,
    pub default_review_timeout_seconds: Option<u64>,
    pub default_verify_timeout_seconds: Option<u64>,
    pub max_consecutive_failures: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfigPatch {
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitConfigPatch {
    pub commit: Option<bool>,
    pub add_required: Option<bool>,
    pub add_include: Option<Vec<String>>,
    pub add_exclude: Option<Vec<String>>,
    pub commit_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigContext {
    pub repo_root: PathBuf,
    pub config_path: PathBuf,
    pub home_dir: PathBuf,
    pub global_root: PathBuf,
    pub profile_path: PathBuf,
    pub project_config_found: bool,
    pub global_profile_found: bool,
    pub merged: MergedConfig,
}

pub fn init_project(start: &Path, force: bool) -> std::result::Result<PathBuf, AppError> {
    let repo_root = find_repo_root(start)?;
    let config_dir = repo_root.join(".codex");
    let config_path = config_dir.join("task-runner.toml");

    if config_path.exists() && !force {
        return Err(AppError::Config(format!(
            "{} already exists; rerun with --force to overwrite",
            config_path.display()
        )));
    }

    fs::create_dir_all(&config_dir)
        .map_err(|err| AppError::Io(format!("failed to create {}: {err}", config_dir.display())))?;
    fs::write(&config_path, DEFAULT_PROJECT_CONFIG)
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", config_path.display())))?;

    Ok(config_path)
}

pub fn load_config(
    repo_root: &Path,
    home_dir: &Path,
    require_project_config: bool,
) -> std::result::Result<ConfigContext, AppError> {
    let config_path = repo_root.join(".codex/task-runner.toml");
    let project_patch = read_config_patch(&config_path)?;

    if project_patch.is_none() && require_project_config {
        return Err(AppError::Config(format!(
            "missing project config {}; run `codex-task init` first",
            config_path.display()
        )));
    }

    let selected_profile = project_patch
        .as_ref()
        .and_then(|patch| patch.prompts.as_ref())
        .and_then(|prompts| prompts.profile.clone())
        .unwrap_or_else(|| MergedConfig::builtin().prompts.profile);

    let global_root = home_dir.join(".codex/task-runner");
    let profile_path = global_root
        .join("profiles")
        .join(format!("{selected_profile}.toml"));
    let global_profile_patch = read_config_patch(&profile_path)?;

    let mut merged = MergedConfig::builtin();
    if let Some(patch) = global_profile_patch.clone() {
        merged.apply_patch(patch, false);
    }
    if let Some(patch) = project_patch.clone() {
        merged.apply_patch(patch, true);
    }
    merged.validate()?;

    Ok(ConfigContext {
        repo_root: repo_root.to_path_buf(),
        config_path,
        home_dir: home_dir.to_path_buf(),
        global_root,
        profile_path,
        project_config_found: project_patch.is_some(),
        global_profile_found: global_profile_patch.is_some(),
        merged,
    })
}

fn read_config_patch(path: &Path) -> std::result::Result<Option<ConfigPatch>, AppError> {
    if !path.exists() {
        return Ok(None);
    }

    let raw = fs::read_to_string(path)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", path.display())))?;
    let patch = toml::from_str::<ConfigPatch>(&raw)
        .map_err(|err| AppError::Config(format!("invalid config {}: {err}", path.display())))?;
    Ok(Some(patch))
}

pub fn find_repo_root(start: &Path) -> std::result::Result<PathBuf, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(AppError::Runtime(if stderr.is_empty() {
            format!("{} is not inside a git repository", start.display())
        } else {
            stderr
        }));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let root = stdout.trim();
    if root.is_empty() {
        return Err(AppError::Runtime(
            "git returned an empty repository root".to_string(),
        ));
    }
    Ok(PathBuf::from(root))
}

pub fn home_dir() -> std::result::Result<PathBuf, AppError> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| AppError::Config("HOME is not set".to_string()))
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub repo_root: Option<PathBuf>,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn has_errors(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.status == DoctorStatus::Error)
    }

    pub fn exit_code(&self) -> i32 {
        if self.has_errors() { 1 } else { 0 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorCheck {
    pub name: String,
    pub status: DoctorStatus,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DoctorStatus {
    Ok,
    Warn,
    Error,
}

pub fn run_doctor(start: &Path) -> DoctorReport {
    let mut checks = Vec::new();

    checks.push(check_binary("git"));
    checks.push(check_binary("codex"));

    let repo_root_result = find_repo_root(start);
    let repo_root = match repo_root_result {
        Ok(root) => {
            checks.push(DoctorCheck {
                name: "git-repo".to_string(),
                status: DoctorStatus::Ok,
                message: root.display().to_string(),
            });
            Some(root)
        }
        Err(err) => {
            checks.push(DoctorCheck {
                name: "git-repo".to_string(),
                status: DoctorStatus::Error,
                message: err.to_string(),
            });
            None
        }
    };

    let home = match home_dir() {
        Ok(home) => {
            checks.push(DoctorCheck {
                name: "home".to_string(),
                status: DoctorStatus::Ok,
                message: home.display().to_string(),
            });
            Some(home)
        }
        Err(err) => {
            checks.push(DoctorCheck {
                name: "home".to_string(),
                status: DoctorStatus::Error,
                message: err.to_string(),
            });
            None
        }
    };

    if let (Some(repo_root), Some(home)) = (&repo_root, &home) {
        let config_path = repo_root.join(".codex/task-runner.toml");
        match read_config_patch(&config_path) {
            Ok(Some(_)) => checks.push(DoctorCheck {
                name: "project-config".to_string(),
                status: DoctorStatus::Ok,
                message: config_path.display().to_string(),
            }),
            Ok(None) => checks.push(DoctorCheck {
                name: "project-config".to_string(),
                status: DoctorStatus::Error,
                message: format!(
                    "missing {}; run `codex-task init` first",
                    config_path.display()
                ),
            }),
            Err(err) => checks.push(DoctorCheck {
                name: "project-config".to_string(),
                status: DoctorStatus::Error,
                message: err.to_string(),
            }),
        }

        match load_config(repo_root, home, false) {
            Ok(context) => {
                if context.global_profile_found {
                    checks.push(DoctorCheck {
                        name: "global-profile".to_string(),
                        status: DoctorStatus::Ok,
                        message: context.profile_path.display().to_string(),
                    });
                } else if context.merged.prompts.profile == "default" {
                    checks.push(DoctorCheck {
                        name: "global-profile".to_string(),
                        status: DoctorStatus::Ok,
                        message: "built-in default profile".to_string(),
                    });
                } else {
                    checks.push(DoctorCheck {
                        name: "global-profile".to_string(),
                        status: DoctorStatus::Warn,
                        message: format!(
                            "{} not found; using built-in defaults",
                            context.profile_path.display()
                        ),
                    });
                }

                checks.push(check_agent_rules(&context));
                checks.extend(check_prompt_templates(&context));
                checks.push(check_run_store(&context));

                if context.merged.runner.dangerous_bypass_approvals_and_sandbox {
                    checks.push(DoctorCheck {
                        name: "dangerous-bypass".to_string(),
                        status: DoctorStatus::Warn,
                        message: "dangerous bypass is enabled explicitly".to_string(),
                    });
                } else {
                    checks.push(DoctorCheck {
                        name: "dangerous-bypass".to_string(),
                        status: DoctorStatus::Ok,
                        message: "disabled".to_string(),
                    });
                }

                if context.merged.git.commit
                    && context.merged.git.add_required
                    && context.merged.git.add_include.is_empty()
                {
                    checks.push(DoctorCheck {
                        name: "git-add-scope".to_string(),
                        status: DoctorStatus::Warn,
                        message: "git.commit=true but git.add_include is empty".to_string(),
                    });
                } else {
                    checks.push(DoctorCheck {
                        name: "git-add-scope".to_string(),
                        status: DoctorStatus::Ok,
                        message: "automatic staging scope is explicit or disabled".to_string(),
                    });
                }
            }
            Err(err) => checks.push(DoctorCheck {
                name: "merged-config".to_string(),
                status: DoctorStatus::Error,
                message: err.to_string(),
            }),
        }
    }

    DoctorReport { repo_root, checks }
}

fn check_binary(name: &str) -> DoctorCheck {
    match Command::new(name).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first_line = stdout
                .lines()
                .chain(stderr.lines())
                .next()
                .unwrap_or("available")
                .to_string();
            DoctorCheck {
                name: name.to_string(),
                status: DoctorStatus::Ok,
                message: first_line,
            }
        }
        Ok(output) => DoctorCheck {
            name: name.to_string(),
            status: DoctorStatus::Error,
            message: format!("{} --version exited with {}", name, output.status),
        },
        Err(err) => DoctorCheck {
            name: name.to_string(),
            status: DoctorStatus::Error,
            message: format!("{name} is unavailable: {err}"),
        },
    }
}

fn check_agent_rules(context: &ConfigContext) -> DoctorCheck {
    let path = context.repo_root.join(&context.merged.project.agent_rules);
    if path.is_file() {
        DoctorCheck {
            name: "agent-rules".to_string(),
            status: DoctorStatus::Ok,
            message: path.display().to_string(),
        }
    } else {
        DoctorCheck {
            name: "agent-rules".to_string(),
            status: DoctorStatus::Warn,
            message: format!("configured agent_rules file is missing: {}", path.display()),
        }
    }
}

fn check_prompt_templates(context: &ConfigContext) -> Vec<DoctorCheck> {
    PROMPT_TEMPLATE_NAMES
        .iter()
        .map(|name| match resolve_prompt_template(context, name) {
            Ok(source) => DoctorCheck {
                name: format!("prompt:{name}"),
                status: DoctorStatus::Ok,
                message: source,
            },
            Err(err) => DoctorCheck {
                name: format!("prompt:{name}"),
                status: DoctorStatus::Error,
                message: err.to_string(),
            },
        })
        .collect()
}

fn check_run_store(context: &ConfigContext) -> DoctorCheck {
    match RunStore::for_repo(&context.repo_root, &context.home_dir) {
        Ok(store) => {
            if let Err(err) = store.ensure_repo_dir() {
                return DoctorCheck {
                    name: "run-store".to_string(),
                    status: DoctorStatus::Error,
                    message: format!("failed to create {}: {err}", store.repo_runs_dir.display()),
                };
            }

            let probe = store.repo_runs_dir.join(".doctor-write-test");
            match fs::write(&probe, b"ok").and_then(|_| fs::remove_file(&probe)) {
                Ok(()) => DoctorCheck {
                    name: "run-store".to_string(),
                    status: DoctorStatus::Ok,
                    message: store.repo_runs_dir.display().to_string(),
                },
                Err(err) => DoctorCheck {
                    name: "run-store".to_string(),
                    status: DoctorStatus::Error,
                    message: format!("run store is not writable: {err}"),
                },
            }
        }
        Err(err) => DoctorCheck {
            name: "run-store".to_string(),
            status: DoctorStatus::Error,
            message: err.to_string(),
        },
    }
}

fn resolve_prompt_template(context: &ConfigContext, name: &str) -> Result<String> {
    let kind = PromptTemplateKind::from_file_name(name)
        .ok_or_else(|| anyhow::anyhow!("unknown prompt template {name}"))?;
    Ok(load_prompt_template(context, kind)?.source_label())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartOptions {
    pub spec_path: PathBuf,
    pub run_id: Option<String>,
    pub branch: Option<String>,
    pub resume: bool,
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartResult {
    pub run_id: String,
    pub branch: String,
    pub spec_file: String,
    pub run_dir: PathBuf,
    pub tasks_path: PathBuf,
    pub state_path: PathBuf,
    pub metadata_path: PathBuf,
    pub resumed: bool,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetadata {
    #[serde(rename = "version", default = "default_run_metadata_version")]
    pub schema_version: u64,
    #[serde(rename = "runId")]
    pub run_id: String,
    pub branch: String,
    #[serde(rename = "specFile")]
    pub spec_file: String,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_run_metadata_version() -> u64 {
    1
}

pub fn start_run(
    start: &Path,
    options: StartOptions,
) -> std::result::Result<StartResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    start_run_in_repo(&repo_root, &home, start, options)
}

pub fn start_run_in_repo(
    repo_root: &Path,
    home: &Path,
    cwd: &Path,
    options: StartOptions,
) -> std::result::Result<StartResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    store.ensure_repo_dir().map_err(|err| {
        AppError::Io(format!(
            "failed to create {}: {err}",
            store.repo_runs_dir.display()
        ))
    })?;

    let spec_path = resolve_spec_path(cwd, &context.repo_root, &options.spec_path)?;
    let spec_file = repo_relative_slash_path(&context.repo_root, &spec_path)?;
    let mut spec = SpecDocument::read(&spec_path)?;
    let discovered = discover_run_metadata_for_spec(&store, &spec_file)?;

    let metadata_run_id = spec
        .frontmatter
        .as_ref()
        .and_then(|frontmatter| frontmatter.get("run_id"))
        .or_else(|| {
            spec.frontmatter
                .as_ref()
                .and_then(|frontmatter| frontmatter.get("runId"))
        });
    let metadata_branch = spec
        .frontmatter
        .as_ref()
        .and_then(|frontmatter| frontmatter.get("branch"));

    if let (Some(explicit), Some(existing)) = (&options.run_id, &metadata_run_id)
        && explicit != existing
    {
        return Err(AppError::Config(format!(
            "spec metadata run_id={} conflicts with --run-id={explicit}",
            existing
        )));
    }
    if let (Some(explicit), Some(existing)) = (&options.branch, &metadata_branch)
        && explicit != existing
    {
        return Err(AppError::Config(format!(
            "spec metadata branch={} conflicts with --branch={explicit}",
            existing
        )));
    }
    if let (Some(explicit), Some(existing)) = (&options.run_id, discovered.as_ref())
        && explicit != &existing.run_id
    {
        return Err(AppError::Config(format!(
            "spec {} already has run {}; refusing to create parallel run {}",
            spec_file, existing.run_id, explicit
        )));
    }
    if let (Some(explicit), Some(existing)) = (&options.branch, discovered.as_ref())
        && explicit != &existing.branch
    {
        return Err(AppError::Config(format!(
            "spec {} already has branch {}; refusing to switch it to {}",
            spec_file, existing.branch, explicit
        )));
    }

    let run_id = options
        .run_id
        .clone()
        .or(metadata_run_id.clone())
        .or_else(|| discovered.as_ref().map(|metadata| metadata.run_id.clone()))
        .unwrap_or_else(|| derive_run_id_from_spec(&spec_path));
    RunId::parse(&run_id)?;

    let branch = options
        .branch
        .clone()
        .or(metadata_branch.clone())
        .or_else(|| discovered.as_ref().map(|metadata| metadata.branch.clone()))
        .unwrap_or_else(|| format!("{}{}", context.merged.project.feature_branch_prefix, run_id));
    if branch.trim().is_empty() {
        return Err(AppError::Config("branch must not be empty".to_string()));
    }

    let run_dir = store.run_dir(&run_id)?;
    let tasks_path = store.tasks_path(&run_id)?;
    let state_path = store.state_path(&run_id)?;
    let metadata_path = store.metadata_path(&run_id)?;
    if options.resume && !tasks_path.exists() {
        return Err(AppError::Runtime(format!(
            "cannot resume run {run_id}: {} does not exist",
            tasks_path.display()
        )));
    }
    fs::create_dir_all(&run_dir)
        .map_err(|err| AppError::Io(format!("failed to create {}: {err}", run_dir.display())))?;

    let mut warnings = Vec::new();
    let metadata = RunMetadata {
        schema_version: 1,
        run_id: run_id.clone(),
        branch: branch.clone(),
        spec_file: spec_file.clone(),
        extra: Map::new(),
    };
    store.write_metadata(&run_id, &metadata)?;

    let git_warnings = ensure_feature_branch(
        &context.repo_root,
        &context.merged.project.default_branch,
        &branch,
    )?;
    for warning in git_warnings {
        append_event_log(&run_dir, &format!("warning: {warning}"))?;
        warnings.push(warning);
    }
    if context.merged.git.commit {
        if metadata_run_id.as_deref() != Some(run_id.as_str())
            || metadata_branch.as_deref() != Some(branch.as_str())
        {
            let warning = "git.commit=true; run metadata was written only to the global run store to keep the worktree clean".to_string();
            append_event_log(&run_dir, &format!("warning: {warning}"))?;
            warnings.push(warning);
        }
    } else if spec.set_run_metadata(&run_id, &branch) {
        spec.write(&spec_path)?;
    }

    if tasks_path.exists() {
        let task_file = store.read_task_file(&run_id)?;
        ensure_task_file_matches_run(&task_file, &run_id, &branch, &spec_file)?;
        if !state_path.exists() {
            store.write_run_state(&run_id, &initial_run_state(&task_file))?;
        }
        return Ok(StartResult {
            run_id,
            branch,
            spec_file,
            run_dir,
            tasks_path,
            state_path,
            metadata_path,
            resumed: true,
            warnings,
        });
    }

    let prompt = render_decompose_prompt(&context, &store, &run_id, &branch, &spec_file, &spec)?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir.join("prompts/decompose-feature.md"),
        stdout_log_path: run_dir.join("logs/decompose.stdout.log"),
        stderr_log_path: run_dir.join("logs/decompose.stderr.log"),
        last_message_path: run_dir.join("last-message.md"),
        required_output_path: None,
        sandbox: context.merged.runner.sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_analyze_timeout_seconds,
    };
    let mut executor_config = CodexExecutorConfig::from_context(&context);
    if let Some(codex_bin) = options.codex_bin {
        executor_config.codex_bin = codex_bin;
    }
    let codex_output = CodexExecutor::new(executor_config)
        .execute(&request)
        .map_err(|err| {
            AppError::Runtime(format!(
                "{err}; logs: stdout={}, stderr={}, last={}",
                err.stdout_log_path.display(),
                err.stderr_log_path.display(),
                err.last_message_path.display()
            ))
        })?;

    let parsed = parse_decompose_output(&codex_output.last_message).map_err(|err| {
        let _ = append_event_log(&run_dir, &format!("error: invalid decompose output: {err}"));
        AppError::Runtime(format!(
            "invalid decompose output: {err}; raw output preserved at {}",
            codex_output.last_message_path.display()
        ))
    })?;
    if parsed.used_code_block {
        let warning = "decompose output used a markdown code block; normalized pure JSON on write"
            .to_string();
        append_event_log(&run_dir, &format!("warning: {warning}"))?;
        warnings.push(warning);
    }

    let mut task_file = parsed.task_file;
    task_file.schema_version = 2;
    task_file.run_id = run_id.clone();
    task_file.branch = branch.clone();
    task_file.spec_file = spec_file.clone();
    validate_task_file(&task_file)?;
    store.write_task_file(&run_id, &task_file)?;
    store.write_run_state(&run_id, &initial_run_state(&task_file))?;

    Ok(StartResult {
        run_id,
        branch,
        spec_file,
        run_dir,
        tasks_path,
        state_path,
        metadata_path,
        resumed: false,
        warnings,
    })
}

#[derive(Debug, Clone)]
struct ParsedDecomposeOutput {
    task_file: TaskFile,
    used_code_block: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SpecDocument {
    frontmatter: Option<FrontMatter>,
    body: String,
}

impl SpecDocument {
    fn read(path: &Path) -> std::result::Result<Self, AppError> {
        let raw = fs::read_to_string(path)
            .map_err(|err| AppError::Io(format!("failed to read {}: {err}", path.display())))?;
        parse_spec_document(&raw)
    }

    fn write(&self, path: &Path) -> std::result::Result<(), AppError> {
        fs::write(path, self.to_text())
            .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
    }

    fn set_run_metadata(&mut self, run_id: &str, branch: &str) -> bool {
        let frontmatter = self.frontmatter.get_or_insert_with(FrontMatter::new);
        let run_id_key = if frontmatter.contains_key("runId") && !frontmatter.contains_key("run_id")
        {
            "runId"
        } else {
            "run_id"
        };
        let mut changed = frontmatter.set(run_id_key, run_id);
        changed |= frontmatter.set("branch", branch);
        changed
    }

    fn set_finalized_metadata(&mut self, finished_at: &str) -> bool {
        let frontmatter = self.frontmatter.get_or_insert_with(FrontMatter::new);
        let mut changed = frontmatter.set("status", "done");
        changed |= frontmatter.set("finished_at", finished_at);
        changed
    }

    fn to_text(&self) -> String {
        match &self.frontmatter {
            Some(frontmatter) => {
                let mut text = String::new();
                text.push_str("---\n");
                text.push_str(&frontmatter.to_text());
                text.push_str("---\n");
                text.push_str(&self.body);
                text
            }
            None => self.body.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FrontMatter {
    lines: Vec<FrontMatterLine>,
}

impl FrontMatter {
    fn new() -> Self {
        Self { lines: Vec::new() }
    }

    fn contains_key(&self, key: &str) -> bool {
        self.lines.iter().any(|line| match line {
            FrontMatterLine::KeyValue { key: existing, .. } => existing == key,
            FrontMatterLine::Raw(_) => false,
        })
    }

    fn get(&self, key: &str) -> Option<String> {
        self.lines.iter().find_map(|line| match line {
            FrontMatterLine::KeyValue {
                key: existing,
                value,
            } if existing == key => Some(value.clone()),
            _ => None,
        })
    }

    fn set(&mut self, key: &str, value: &str) -> bool {
        for line in &mut self.lines {
            match line {
                FrontMatterLine::KeyValue {
                    key: existing,
                    value: existing_value,
                } if existing == key => {
                    if existing_value == value {
                        return false;
                    }
                    *existing_value = value.to_string();
                    return true;
                }
                _ => {}
            }
        }
        self.lines.push(FrontMatterLine::KeyValue {
            key: key.to_string(),
            value: value.to_string(),
        });
        true
    }

    fn to_text(&self) -> String {
        let mut text = String::new();
        for line in &self.lines {
            match line {
                FrontMatterLine::KeyValue { key, value } => {
                    text.push_str(key);
                    text.push_str(": ");
                    text.push_str(value);
                    text.push('\n');
                }
                FrontMatterLine::Raw(raw) => {
                    text.push_str(raw);
                    text.push('\n');
                }
            }
        }
        text
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FrontMatterLine {
    KeyValue { key: String, value: String },
    Raw(String),
}

fn parse_spec_document(raw: &str) -> std::result::Result<SpecDocument, AppError> {
    let Some(after_open) = strip_frontmatter_open(raw) else {
        return Ok(SpecDocument {
            frontmatter: None,
            body: raw.to_string(),
        });
    };

    let mut offset = raw.len() - after_open.len();
    let mut lines = Vec::new();
    loop {
        if offset >= raw.len() {
            return Err(AppError::Config(
                "spec frontmatter is missing a closing marker".to_string(),
            ));
        }
        let line_start = offset;
        let line_end = raw[offset..]
            .find('\n')
            .map(|index| offset + index)
            .unwrap_or(raw.len());
        let next_offset = if line_end < raw.len() {
            line_end + 1
        } else {
            line_end
        };
        let line = raw[line_start..line_end].trim_end_matches('\r');
        if line == "---" || line == "..." {
            return Ok(SpecDocument {
                frontmatter: Some(FrontMatter { lines }),
                body: raw[next_offset..].to_string(),
            });
        }
        lines.push(parse_frontmatter_line(line));
        offset = next_offset;
    }
}

fn strip_frontmatter_open(raw: &str) -> Option<&str> {
    raw.strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))
}

fn parse_frontmatter_line(line: &str) -> FrontMatterLine {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return FrontMatterLine::Raw(line.to_string());
    }
    let Some(separator) = line.find(':') else {
        return FrontMatterLine::Raw(line.to_string());
    };
    let key = line[..separator].trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return FrontMatterLine::Raw(line.to_string());
    }
    let value = strip_frontmatter_scalar(line[separator + 1..].trim());
    FrontMatterLine::KeyValue {
        key: key.to_string(),
        value,
    }
}

fn strip_frontmatter_scalar(value: &str) -> String {
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn resolve_spec_path(
    cwd: &Path,
    repo_root: &Path,
    spec_path: &Path,
) -> std::result::Result<PathBuf, AppError> {
    let raw_path = if spec_path.is_absolute() {
        spec_path.to_path_buf()
    } else {
        cwd.join(spec_path)
    };
    let canonical = raw_path.canonicalize().map_err(|err| {
        AppError::Io(format!(
            "failed to canonicalize {}: {err}",
            raw_path.display()
        ))
    })?;
    if !canonical.is_file() {
        return Err(AppError::Config(format!(
            "spec path is not a file: {}",
            canonical.display()
        )));
    }
    let repo = repo_root.canonicalize().map_err(|err| {
        AppError::Io(format!(
            "failed to canonicalize repo root {}: {err}",
            repo_root.display()
        ))
    })?;
    canonical.strip_prefix(&repo).map_err(|_| {
        AppError::Config(format!(
            "spec file {} is outside repo {}",
            canonical.display(),
            repo.display()
        ))
    })?;
    Ok(canonical)
}

fn repo_relative_slash_path(
    repo_root: &Path,
    path: &Path,
) -> std::result::Result<String, AppError> {
    let repo = repo_root.canonicalize().map_err(|err| {
        AppError::Io(format!(
            "failed to canonicalize repo root {}: {err}",
            repo_root.display()
        ))
    })?;
    let canonical = path
        .canonicalize()
        .map_err(|err| AppError::Io(format!("failed to canonicalize {}: {err}", path.display())))?;
    let relative = canonical.strip_prefix(&repo).map_err(|_| {
        AppError::Config(format!(
            "{} is outside repo {}",
            canonical.display(),
            repo.display()
        ))
    })?;
    Ok(path_to_slash(relative))
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn derive_run_id_from_spec(spec_path: &Path) -> String {
    let stem = spec_path
        .file_stem()
        .map(|value| value.to_string_lossy())
        .unwrap_or_else(|| "run".into());
    let mut out = String::new();
    let mut last_was_separator = false;
    for ch in stem.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if matches!(ch, '-' | '_' | '.') {
            out.push(ch);
            last_was_separator = false;
        } else if !last_was_separator {
            out.push('-');
            last_was_separator = true;
        }
    }
    let trimmed = out.trim_matches(['-', '_', '.']).to_string();
    if trimmed.is_empty() {
        "run".to_string()
    } else {
        trimmed
    }
}

fn discover_run_metadata_for_spec(
    store: &RunStore,
    spec_file: &str,
) -> std::result::Result<Option<RunMetadata>, AppError> {
    let ids = discover_run_ids(&store.repo_runs_dir)?;
    for id in ids {
        if let Ok(metadata) = store.read_metadata(&id)
            && metadata.spec_file == spec_file
        {
            return Ok(Some(metadata));
        }
        if let Ok(task_file) = store.read_task_file(&id)
            && task_file.spec_file == spec_file
        {
            return Ok(Some(RunMetadata {
                schema_version: 1,
                run_id: task_file.run_id,
                branch: task_file.branch,
                spec_file: task_file.spec_file,
                extra: Map::new(),
            }));
        }
    }
    Ok(None)
}

fn ensure_feature_branch(
    repo_root: &Path,
    default_branch: &str,
    branch: &str,
) -> std::result::Result<Vec<String>, AppError> {
    let mut warnings = Vec::new();
    if current_git_branch(repo_root)?.as_deref() == Some(branch) {
        return Ok(warnings);
    }

    if git_branch_exists(repo_root, branch)? {
        run_git(repo_root, ["switch", branch])?;
        return Ok(warnings);
    }

    if git_branch_exists(repo_root, default_branch)? {
        run_git(repo_root, ["switch", default_branch])?;
        run_git(repo_root, ["switch", "-c", branch])?;
    } else {
        warnings.push(format!(
            "default branch {default_branch} not found; creating {branch} from current HEAD"
        ));
        run_git(repo_root, ["switch", "-c", branch])?;
    }
    Ok(warnings)
}

fn current_git_branch(repo_root: &Path) -> std::result::Result<Option<String>, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["branch", "--show-current"])
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format_git_error(
            "git branch --show-current",
            &output,
        )));
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!branch.is_empty()).then_some(branch))
}

fn git_branch_exists(repo_root: &Path, branch: &str) -> std::result::Result<bool, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;
    Ok(output.status.success())
}

fn run_git<const N: usize>(repo_root: &Path, args: [&str; N]) -> std::result::Result<(), AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::Runtime(format_git_error("git", &output)))
    }
}

fn format_git_error(command: &str, output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        format!("{command} exited with {}", output.status)
    } else {
        stderr
    }
}

fn render_decompose_prompt(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    branch: &str,
    spec_file: &str,
    spec: &SpecDocument,
) -> std::result::Result<String, AppError> {
    let run_dir = store.run_dir(run_id)?;
    let input = DecomposePromptInput {
        common: CommonPromptVariables {
            date: current_date()?,
            repo_root: context.repo_root.display().to_string(),
            runner_dir: run_dir.display().to_string(),
            runner_dir_rel: run_dir.display().to_string(),
            task_file: store.tasks_path(run_id)?.display().to_string(),
            state_file: store.state_path(run_id)?.display().to_string(),
            repo_map: build_repo_map(&context.repo_root)?,
            agent_rules_path: context.merged.project.agent_rules.clone(),
            overview_doc: context
                .merged
                .project
                .overview_doc
                .clone()
                .unwrap_or_default(),
        },
        spec_file: spec_file.to_string(),
        feature_spec: spec.body.clone(),
        run_id: run_id.to_string(),
        branch: branch.to_string(),
        output_tasks_path: store.tasks_path(run_id)?.display().to_string(),
    };
    let template = load_prompt_template(context, PromptTemplateKind::DecomposeFeature)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

fn current_date() -> std::result::Result<String, AppError> {
    let output = Command::new("date")
        .arg("+%F")
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run date: {err}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format_git_error("date +%F", &output)));
    }
    let date = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if date.is_empty() {
        Err(AppError::Runtime(
            "date +%F returned empty output".to_string(),
        ))
    } else {
        Ok(date)
    }
}

fn build_repo_map(repo_root: &Path) -> std::result::Result<String, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["ls-files"])
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format_git_error("git ls-files", &output)));
    }
    let files = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if files.is_empty() {
        Ok("(empty git index)".to_string())
    } else {
        Ok(files)
    }
}

fn append_event_log(run_dir: &Path, line: &str) -> std::result::Result<(), AppError> {
    let logs_dir = run_dir.join("logs");
    fs::create_dir_all(&logs_dir)
        .map_err(|err| AppError::Io(format!("failed to create {}: {err}", logs_dir.display())))?;
    let path = logs_dir.join("events.log");
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| AppError::Io(format!("failed to open {}: {err}", path.display())))?;
    writeln!(file, "{line}")
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
}

fn parse_decompose_output(raw: &str) -> std::result::Result<ParsedDecomposeOutput, String> {
    match parse_task_file_json(raw.trim()) {
        Ok(task_file) => Ok(ParsedDecomposeOutput {
            task_file,
            used_code_block: false,
        }),
        Err(first_error) => {
            for block in json_code_blocks(raw) {
                if let Ok(task_file) = parse_task_file_json(block.trim()) {
                    return Ok(ParsedDecomposeOutput {
                        task_file,
                        used_code_block: true,
                    });
                }
            }
            Err(first_error)
        }
    }
}

fn parse_task_file_json(raw: &str) -> std::result::Result<TaskFile, String> {
    let value = serde_json::from_str::<Value>(raw).map_err(|err| format!("invalid JSON: {err}"))?;
    serde_json::from_value::<TaskFile>(value)
        .map_err(|err| format!("invalid tasks.json schema: {err}"))
}

fn json_code_blocks(raw: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut search_from = 0;
    while let Some(start) = raw[search_from..].find("```") {
        let fence_start = search_from + start;
        let info_start = fence_start + 3;
        let Some(content_start_rel) = raw[info_start..].find('\n') else {
            break;
        };
        let content_start = info_start + content_start_rel + 1;
        let Some(end_rel) = raw[content_start..].find("```") else {
            break;
        };
        let content_end = content_start + end_rel;
        blocks.push(&raw[content_start..content_end]);
        search_from = content_end + 3;
    }
    blocks
}

fn ensure_task_file_matches_run(
    task_file: &TaskFile,
    run_id: &str,
    branch: &str,
    spec_file: &str,
) -> std::result::Result<(), AppError> {
    if task_file.run_id != run_id {
        return Err(AppError::Config(format!(
            "existing tasks.json runId={} does not match {run_id}",
            task_file.run_id
        )));
    }
    if task_file.branch != branch {
        return Err(AppError::Config(format!(
            "existing tasks.json branch={} does not match {branch}",
            task_file.branch
        )));
    }
    if task_file.spec_file != spec_file {
        return Err(AppError::Config(format!(
            "existing tasks.json specFile={} does not match {spec_file}",
            task_file.spec_file
        )));
    }
    Ok(())
}

fn initial_run_state(task_file: &TaskFile) -> RunState {
    RunState {
        schema_version: 1,
        feature_review_status: FeatureReviewStatus::Pending,
        feature_review_attempts: 0,
        tasks: task_file
            .tasks
            .iter()
            .map(|task| TaskState {
                id: task.id.clone(),
                status: TaskStatus::Pending,
                phase: None,
                attempts: 0,
                review_attempts: 0,
                started_at: None,
                finished_at: None,
                updated_at: None,
                approved_at: None,
                ignored_at: None,
                ignore_reason: None,
                output: None,
                analysis_output: None,
                review_output: None,
                last_exit_code: None,
                last_error: None,
                last_log: None,
                last_verdict: None,
                last_review_comments: None,
                extra: Map::new(),
            })
            .collect(),
        extra: Map::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum PromptTemplateKind {
    DecomposeFeature,
    AnalyzeTask,
    ImplementTask,
    ReviewTask,
    ReviewFeature,
}

impl PromptTemplateKind {
    pub const ALL: [PromptTemplateKind; 5] = [
        PromptTemplateKind::DecomposeFeature,
        PromptTemplateKind::AnalyzeTask,
        PromptTemplateKind::ImplementTask,
        PromptTemplateKind::ReviewTask,
        PromptTemplateKind::ReviewFeature,
    ];

    pub fn file_name(self) -> &'static str {
        match self {
            PromptTemplateKind::DecomposeFeature => "decompose-feature.md",
            PromptTemplateKind::AnalyzeTask => "analyze-task.md",
            PromptTemplateKind::ImplementTask => "implement-task.md",
            PromptTemplateKind::ReviewTask => "review-task.md",
            PromptTemplateKind::ReviewFeature => "review-feature.md",
        }
    }

    pub fn from_file_name(name: &str) -> Option<Self> {
        match name {
            "decompose-feature.md" => Some(Self::DecomposeFeature),
            "analyze-task.md" => Some(Self::AnalyzeTask),
            "implement-task.md" => Some(Self::ImplementTask),
            "review-task.md" => Some(Self::ReviewTask),
            "review-feature.md" => Some(Self::ReviewFeature),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptTemplateSource {
    Project,
    Global,
    BuiltIn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PromptTemplate {
    pub kind: PromptTemplateKind,
    pub source: PromptTemplateSource,
    pub path: Option<PathBuf>,
    pub content: String,
}

impl PromptTemplate {
    pub fn source_label(&self) -> String {
        match (&self.source, &self.path) {
            (PromptTemplateSource::Project, Some(path)) => format!("project:{}", path.display()),
            (PromptTemplateSource::Global, Some(path)) => format!("global:{}", path.display()),
            (PromptTemplateSource::BuiltIn, _) => "built-in".to_string(),
            (source, None) => format!("{source:?}"),
        }
    }

    pub fn render<I>(&self, input: &I) -> std::result::Result<String, PromptRenderError>
    where
        I: PromptTemplateInput,
    {
        if self.kind != input.kind() {
            return Err(PromptRenderError::WrongTemplateKind {
                template: self.kind.file_name().to_string(),
                expected: self.kind.file_name().to_string(),
                actual: input.kind().file_name().to_string(),
            });
        }
        render_prompt_template(self.kind.file_name(), &self.content, &input.variables())
    }

    pub fn render_map(
        &self,
        variables: &BTreeMap<String, String>,
    ) -> std::result::Result<String, PromptRenderError> {
        render_prompt_template(self.kind.file_name(), &self.content, variables)
    }
}

pub fn load_prompt_template(
    context: &ConfigContext,
    kind: PromptTemplateKind,
) -> Result<PromptTemplate> {
    let name = kind.file_name();
    let project_path = context
        .repo_root
        .join(".codex/task-runner/prompts")
        .join(name);
    if project_path.exists() {
        let content = read_prompt_template_file(&project_path)?;
        return Ok(PromptTemplate {
            kind,
            source: PromptTemplateSource::Project,
            path: Some(project_path),
            content,
        });
    }

    let global_path = context.global_root.join("prompts").join(name);
    if global_path.exists() {
        let content = read_prompt_template_file(&global_path)?;
        return Ok(PromptTemplate {
            kind,
            source: PromptTemplateSource::Global,
            path: Some(global_path),
            content,
        });
    }

    Ok(PromptTemplate {
        kind,
        source: PromptTemplateSource::BuiltIn,
        path: None,
        content: builtin_prompt_template(kind).to_string(),
    })
}

fn read_prompt_template_file(path: &Path) -> Result<String> {
    if !path.is_file() {
        anyhow::bail!("{} is not a prompt template file", path.display());
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    if raw.trim().is_empty() {
        anyhow::bail!("{} is empty", path.display());
    }
    Ok(raw)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PromptRenderError {
    #[error("template {template} expected {expected} variables but got {actual}")]
    WrongTemplateKind {
        template: String,
        expected: String,
        actual: String,
    },
    #[error("template {template} is missing variable {{{variable}}}")]
    MissingVariable { template: String, variable: String },
}

pub trait PromptTemplateInput {
    fn kind(&self) -> PromptTemplateKind;
    fn variables(&self) -> BTreeMap<String, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommonPromptVariables {
    pub date: String,
    pub repo_root: String,
    pub runner_dir: String,
    pub runner_dir_rel: String,
    pub task_file: String,
    pub state_file: String,
    pub repo_map: String,
    pub agent_rules_path: String,
    pub overview_doc: String,
}

impl CommonPromptVariables {
    fn insert_into(&self, variables: &mut BTreeMap<String, String>) {
        variables.insert("date".to_string(), self.date.clone());
        variables.insert("repo_root".to_string(), self.repo_root.clone());
        variables.insert("runner_dir".to_string(), self.runner_dir.clone());
        variables.insert("runner_dir_rel".to_string(), self.runner_dir_rel.clone());
        variables.insert("task_file".to_string(), self.task_file.clone());
        variables.insert("state_file".to_string(), self.state_file.clone());
        variables.insert("repo_map".to_string(), self.repo_map.clone());
        variables.insert(
            "agent_rules_path".to_string(),
            self.agent_rules_path.clone(),
        );
        variables.insert("overview_doc".to_string(), self.overview_doc.clone());
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecomposePromptInput {
    pub common: CommonPromptVariables,
    pub spec_file: String,
    pub feature_spec: String,
    pub run_id: String,
    pub branch: String,
    pub output_tasks_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeTaskPromptInput {
    pub common: CommonPromptVariables,
    pub task_id: String,
    pub title: String,
    pub task_prompt: String,
    pub task_json: String,
    pub spec_file: String,
    pub feature_spec: String,
    pub output_analysis_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplementTaskPromptInput {
    pub common: CommonPromptVariables,
    pub task_id: String,
    pub title: String,
    pub task_prompt: String,
    pub task_json: String,
    pub spec_file: String,
    pub feature_spec: String,
    pub analysis_output: String,
    pub last_review_comments: String,
    pub last_error: String,
    pub last_log_tail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTaskPromptInput {
    pub common: CommonPromptVariables,
    pub task_id: String,
    pub title: String,
    pub task_prompt: String,
    pub review_criteria: String,
    pub git_diff: String,
    pub spec_file: String,
    pub feature_spec: String,
    pub output_analysis_path: String,
    pub output_impl_path: String,
    pub output_review_path: String,
    pub analysis_output: String,
    pub implementation_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFeaturePromptInput {
    pub common: CommonPromptVariables,
    pub run_id: String,
    pub branch: String,
    pub spec_file: String,
    pub feature_spec: String,
    pub git_diff: String,
    pub tasks_summaries: String,
    pub output_feature_review_path: String,
}

impl PromptTemplateInput for DecomposePromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::DecomposeFeature
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("feature_spec".to_string(), self.feature_spec.clone());
        variables.insert("run_id".to_string(), self.run_id.clone());
        variables.insert("branch".to_string(), self.branch.clone());
        variables.insert(
            "output_tasks_path".to_string(),
            self.output_tasks_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for AnalyzeTaskPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::AnalyzeTask
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = task_variables(
            &self.common,
            &self.task_id,
            &self.title,
            &self.task_prompt,
            &self.task_json,
            &self.spec_file,
            &self.feature_spec,
        );
        variables.insert(
            "output_analysis_path".to_string(),
            self.output_analysis_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for ImplementTaskPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::ImplementTask
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = task_variables(
            &self.common,
            &self.task_id,
            &self.title,
            &self.task_prompt,
            &self.task_json,
            &self.spec_file,
            &self.feature_spec,
        );
        variables.insert("analysis_output".to_string(), self.analysis_output.clone());
        variables.insert(
            "last_review_comments".to_string(),
            self.last_review_comments.clone(),
        );
        variables.insert("last_error".to_string(), self.last_error.clone());
        variables.insert("last_log_tail".to_string(), self.last_log_tail.clone());
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for ReviewTaskPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::ReviewTask
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("task_id".to_string(), self.task_id.clone());
        variables.insert("title".to_string(), self.title.clone());
        variables.insert("task_prompt".to_string(), self.task_prompt.clone());
        variables.insert("review_criteria".to_string(), self.review_criteria.clone());
        variables.insert("git_diff".to_string(), self.git_diff.clone());
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("feature_spec".to_string(), self.feature_spec.clone());
        variables.insert(
            "output_analysis_path".to_string(),
            self.output_analysis_path.clone(),
        );
        variables.insert(
            "output_impl_path".to_string(),
            self.output_impl_path.clone(),
        );
        variables.insert(
            "output_review_path".to_string(),
            self.output_review_path.clone(),
        );
        variables.insert("analysis_output".to_string(), self.analysis_output.clone());
        variables.insert(
            "implementation_summary".to_string(),
            self.implementation_summary.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for ReviewFeaturePromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::ReviewFeature
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("run_id".to_string(), self.run_id.clone());
        variables.insert("branch".to_string(), self.branch.clone());
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("feature_spec".to_string(), self.feature_spec.clone());
        variables.insert("git_diff".to_string(), self.git_diff.clone());
        variables.insert("tasks_summaries".to_string(), self.tasks_summaries.clone());
        variables.insert(
            "output_feature_review_path".to_string(),
            self.output_feature_review_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

fn common_variables(common: &CommonPromptVariables) -> BTreeMap<String, String> {
    let mut variables = BTreeMap::new();
    common.insert_into(&mut variables);
    variables
}

fn task_variables(
    common: &CommonPromptVariables,
    task_id: &str,
    title: &str,
    task_prompt: &str,
    task_json: &str,
    spec_file: &str,
    feature_spec: &str,
) -> BTreeMap<String, String> {
    let mut variables = common_variables(common);
    variables.insert("task_id".to_string(), task_id.to_string());
    variables.insert("title".to_string(), title.to_string());
    variables.insert("task_prompt".to_string(), task_prompt.to_string());
    variables.insert("task_json".to_string(), task_json.to_string());
    variables.insert("spec_file".to_string(), spec_file.to_string());
    variables.insert("feature_spec".to_string(), feature_spec.to_string());
    variables
}

fn insert_compat_aliases(variables: &mut BTreeMap<String, String>) {
    if let Some(value) = variables.get("feature_spec").cloned() {
        variables.insert("spec_content".to_string(), value);
    }
    if let Some(value) = variables.get("task_json").cloned() {
        variables.insert("current_task_json".to_string(), value);
    }
    if let Some(value) = variables.get("analysis_output").cloned() {
        variables.insert("analysis_content".to_string(), value);
    }
}

fn render_prompt_template(
    template: &str,
    raw: &str,
    variables: &BTreeMap<String, String>,
) -> std::result::Result<String, PromptRenderError> {
    let chars: Vec<char> = raw.chars().collect();
    let mut rendered = String::with_capacity(raw.len());
    let mut index = 0;

    while index < chars.len() {
        if chars[index] != '{' {
            rendered.push(chars[index]);
            index += 1;
            continue;
        }

        let Some(end) = chars[index + 1..].iter().position(|ch| *ch == '}') else {
            rendered.push(chars[index]);
            index += 1;
            continue;
        };
        let end = index + 1 + end;
        let name: String = chars[index + 1..end].iter().collect();
        if !is_prompt_variable_name(&name) {
            for ch in &chars[index..=end] {
                rendered.push(*ch);
            }
            index = end + 1;
            continue;
        }

        let Some(value) = variables.get(&name) else {
            return Err(PromptRenderError::MissingVariable {
                template: template.to_string(),
                variable: name,
            });
        };
        rendered.push_str(value);
        index = end + 1;
    }

    Ok(rendered)
}

fn is_prompt_variable_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn builtin_prompt_template(kind: PromptTemplateKind) -> &'static str {
    match kind {
        PromptTemplateKind::DecomposeFeature => DECOMPOSE_FEATURE_TEMPLATE,
        PromptTemplateKind::AnalyzeTask => ANALYZE_TASK_TEMPLATE,
        PromptTemplateKind::ImplementTask => IMPLEMENT_TASK_TEMPLATE,
        PromptTemplateKind::ReviewTask => REVIEW_TASK_TEMPLATE,
        PromptTemplateKind::ReviewFeature => REVIEW_FEATURE_TEMPLATE,
    }
}

const DECOMPOSE_FEATURE_TEMPLATE: &str = r#"# Codex Task Decomposer

Current date: {date}
Repository root: {repo_root}
Run id: {run_id}
Branch: {branch}
Spec file: {spec_file}
Output tasks path: {output_tasks_path}

Read the project rules from `{agent_rules_path}` and the overview from `{overview_doc}`. Convert the feature specification into a `tasks.json` v2 object for this local task runner.

## Repository Map
```text
{repo_map}
```

## Feature Specification
{feature_spec}

## Output Rules

- Output only a valid JSON object. Do not wrap it in markdown.
- The object must contain `version`, `runId`, `branch`, `specFile`, and `tasks`.
- Each task must be narrowly scoped, actionable, and include explicit negative constraints from the spec.
- Each task's `reviewCriteria`, `dependsOn`, and `verificationCommands` fields must be JSON arrays. Do not emit a single string for array fields.
- Preserve existing project boundaries. Do not invent unrelated APIs, tables, or features.
"#;

const ANALYZE_TASK_TEMPLATE: &str = r#"# Codex Task Analyzer

Current date: {date}
Repository root: {repo_root}
Run store: {runner_dir_rel}
Task: {task_id} - {title}
Spec file: {spec_file}
Analysis output path: {output_analysis_path}

This is a read-only analysis phase. Do not modify code, tests, task state, or other task outputs.

## Task Prompt
{task_prompt}

## Current Task JSON
```json
{task_json}
```

## Repository Map
```text
{repo_map}
```

## Feature Specification
{feature_spec}

Write the analysis report to `{output_analysis_path}` when the sandbox allows it. If the sandbox blocks file writes, return the complete Markdown analysis report as your final message with no code fences; the runner will persist it. The report must cover current state, gaps, implementation plan, risks, and acceptance criteria.
"#;

const IMPLEMENT_TASK_TEMPLATE: &str = r#"# Autonomous Task Runner

Current date: {date}
Repository root: {repo_root}
Task file: {task_file}
Current task: {task_id} - {title}

Hard rules:
- Execute exactly one task: {task_id}.
- Do not start any other task.
- Strictly obey the task prompt, feature spec, project rules, and all negative constraints.
- Do not update `tasks.json`, `state.json`, or roadmap docs unless this task explicitly requires it.
- At the end, summarize changed files, focused checks, and remaining gaps.

## Repository Map
```text
{repo_map}
```

## Feature Specification
{feature_spec}

## Pre-computed Analysis
{analysis_output}

## Review Comments To Fix
{last_review_comments}

## Previous Failure
{last_error}

## Previous Failure Log Tail
{last_log_tail}

## Task Prompt
{task_prompt}

## Current Task JSON
```json
{task_json}
```
"#;

const REVIEW_TASK_TEMPLATE: &str = r#"# Codex Task Reviewer

Current date: {date}
Repository root: {repo_root}
Run store: {runner_dir_rel}
Task: {task_id} - {title}
Spec file: {spec_file}
Review output path: {output_review_path}

This is a read-only review. Do not modify code, tests, task state, or other task outputs.

## Task Prompt
{task_prompt}

## Acceptance Criteria
{review_criteria}

## Git Diff
```diff
{git_diff}
```

## Feature Specification
{feature_spec}

## Analysis Report ({output_analysis_path})
{analysis_output}

## Implementation Summary ({output_impl_path})
{implementation_summary}

Write a Markdown review report to `{output_review_path}` when the sandbox allows it. If the sandbox blocks file writes, return the exact Markdown review report as your final message with no code fences; the message must start with this YAML frontmatter:

```markdown
---
task_id: {task_id}
phase: review
verdict: APPROVED
reviewed_at: <RFC3339>
---
```

`verdict` must be exactly `APPROVED` or `CHANGES_REQUESTED`. Any `[MUST]` issue requires `CHANGES_REQUESTED`.
"#;

const REVIEW_FEATURE_TEMPLATE: &str = r#"# Codex Feature Reviewer

Current date: {date}
Repository root: {repo_root}
Run id: {run_id}
Branch: {branch}
Spec file: {spec_file}
Feature review output path: {output_feature_review_path}

This is a read-only final feature review. Do not edit `tasks.json`, `state.json`, source code, tests, or roadmap docs.

## Feature Specification
{feature_spec}

## Feature Diff
```diff
{git_diff}
```

## Completed Task Summaries
{tasks_summaries}

Write a Markdown final review report to `{output_feature_review_path}` when the sandbox allows it. If the sandbox blocks file writes, return the exact Markdown final review report as your final message with no code fences. The report must contain YAML frontmatter with `verdict: APPROVED` or `verdict: CHANGES_REQUESTED`.

MVP rule: final review may report integration issues, but it must not append tasks or modify run state.
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexExecutorConfig {
    pub repo_root: PathBuf,
    pub codex_bin: PathBuf,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub search: bool,
    pub dangerous_bypass_approvals_and_sandbox: bool,
}

impl CodexExecutorConfig {
    pub fn from_context(context: &ConfigContext) -> Self {
        Self {
            repo_root: context.repo_root.clone(),
            codex_bin: PathBuf::from("codex"),
            model: context.merged.runner.model.clone(),
            reasoning_effort: context.merged.runner.reasoning_effort.clone(),
            search: context.merged.runner.search,
            dangerous_bypass_approvals_and_sandbox: context
                .merged
                .runner
                .dangerous_bypass_approvals_and_sandbox,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRunRequest {
    pub prompt: String,
    pub prompt_path: PathBuf,
    pub stdout_log_path: PathBuf,
    pub stderr_log_path: PathBuf,
    pub last_message_path: PathBuf,
    pub required_output_path: Option<PathBuf>,
    pub sandbox: String,
    pub approval: String,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub search: Option<bool>,
    pub timeout_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexRunOutput {
    pub exit_code: i32,
    pub last_message: String,
    pub prompt_path: PathBuf,
    pub stdout_log_path: PathBuf,
    pub stderr_log_path: PathBuf,
    pub last_message_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodexFailureKind {
    Io,
    StartFailed,
    Timeout,
    NonZeroExit,
    MissingLastMessage,
    EmptyLastMessage,
    MissingRequiredOutput,
    EmptyRequiredOutput,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexExecutionError {
    pub kind: CodexFailureKind,
    pub message: String,
    pub exit_code: Option<i32>,
    pub prompt_path: PathBuf,
    pub stdout_log_path: PathBuf,
    pub stderr_log_path: PathBuf,
    pub last_message_path: PathBuf,
    pub required_output_path: Option<PathBuf>,
}

impl fmt::Display for CodexExecutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CodexExecutionError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexExecutor {
    config: CodexExecutorConfig,
}

impl CodexExecutor {
    pub fn new(config: CodexExecutorConfig) -> Self {
        Self { config }
    }

    pub fn execute(
        &self,
        request: &CodexRunRequest,
    ) -> std::result::Result<CodexRunOutput, Box<CodexExecutionError>> {
        if request.timeout_seconds == 0 {
            return Err(self.error(
                request,
                CodexFailureKind::Io,
                None,
                "codex timeoutSeconds must be greater than zero",
            ));
        }

        if let Err(err) = prepare_codex_paths(request) {
            return Err(self.error(
                request,
                CodexFailureKind::Io,
                None,
                &format!("failed to prepare codex run files: {err}"),
            ));
        }

        if let Err(err) = fs::write(&request.prompt_path, &request.prompt) {
            return Err(self.error(
                request,
                CodexFailureKind::Io,
                None,
                &format!(
                    "failed to write prompt {}: {err}",
                    request.prompt_path.display()
                ),
            ));
        }
        if let Err(err) = remove_file_if_exists(&request.last_message_path) {
            return Err(self.error(
                request,
                CodexFailureKind::Io,
                None,
                &format!(
                    "failed to remove stale last message {}: {err}",
                    request.last_message_path.display()
                ),
            ));
        }
        if let Some(output_path) = &request.required_output_path
            && let Err(err) = remove_file_if_exists(output_path)
        {
            return Err(self.error(
                request,
                CodexFailureKind::Io,
                None,
                &format!(
                    "failed to remove stale required output {}: {err}",
                    output_path.display()
                ),
            ));
        }

        let prompt_file = match File::open(&request.prompt_path) {
            Ok(file) => file,
            Err(err) => {
                return Err(self.error(
                    request,
                    CodexFailureKind::Io,
                    None,
                    &format!(
                        "failed to open prompt {}: {err}",
                        request.prompt_path.display()
                    ),
                ));
            }
        };
        let stdout_file = match File::create(&request.stdout_log_path) {
            Ok(file) => file,
            Err(err) => {
                return Err(self.error(
                    request,
                    CodexFailureKind::Io,
                    None,
                    &format!(
                        "failed to create stdout log {}: {err}",
                        request.stdout_log_path.display()
                    ),
                ));
            }
        };
        let stderr_file = match File::create(&request.stderr_log_path) {
            Ok(file) => file,
            Err(err) => {
                return Err(self.error(
                    request,
                    CodexFailureKind::Io,
                    None,
                    &format!(
                        "failed to create stderr log {}: {err}",
                        request.stderr_log_path.display()
                    ),
                ));
            }
        };

        let mut command = Command::new(&self.config.codex_bin);
        if let Some(model) = request.model.as_ref().or(self.config.model.as_ref()) {
            command.arg("-m").arg(model);
        }
        if let Some(reasoning_effort) = request
            .reasoning_effort
            .as_ref()
            .or(self.config.reasoning_effort.as_ref())
        {
            command.arg("-c").arg(format!(
                "model_reasoning_effort={}",
                toml_string_literal(reasoning_effort)
            ));
        }
        if request.search.unwrap_or(self.config.search) {
            command.arg("--search");
        }
        command
            .arg("-a")
            .arg(&request.approval)
            .arg("exec")
            .arg("-C")
            .arg(&self.config.repo_root)
            .arg("-s")
            .arg(&request.sandbox)
            .arg("--output-last-message")
            .arg(&request.last_message_path);
        if self.config.dangerous_bypass_approvals_and_sandbox {
            command.arg("--dangerously-bypass-approvals-and-sandbox");
        }
        command
            .stdin(Stdio::from(prompt_file))
            .stdout(Stdio::from(stdout_file))
            .stderr(Stdio::from(stderr_file));

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Put the child in its own process group so a timeout kills shells
            // and grandchildren, not just the immediate `codex` process.
            unsafe {
                command.pre_exec(|| {
                    if libc::setpgid(0, 0) == 0 {
                        Ok(())
                    } else {
                        Err(std::io::Error::last_os_error())
                    }
                });
            }
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                return Err(self.error(
                    request,
                    CodexFailureKind::StartFailed,
                    None,
                    &format!("failed to start {}: {err}", self.config.codex_bin.display()),
                ));
            }
        };

        let started = Instant::now();
        let timeout = Duration::from_secs(request.timeout_seconds);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if started.elapsed() >= timeout {
                        kill_child_tree(&mut child);
                        let _ = child.wait();
                        append_timeout_log(&request.stderr_log_path, request.timeout_seconds);
                        return Err(self.error(
                            request,
                            CodexFailureKind::Timeout,
                            Some(124),
                            &format!("codex timed out after {} seconds", request.timeout_seconds),
                        ));
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    return Err(self.error(
                        request,
                        CodexFailureKind::Io,
                        None,
                        &format!("failed while waiting for codex: {err}"),
                    ));
                }
            }
        };

        let exit_code = status.code().unwrap_or(1);
        if !status.success() {
            return Err(self.error(
                request,
                CodexFailureKind::NonZeroExit,
                Some(exit_code),
                &format!("codex exited with status {exit_code}"),
            ));
        }

        let last_message = match fs::read_to_string(&request.last_message_path) {
            Ok(value) => value,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(self.error(
                    request,
                    CodexFailureKind::MissingLastMessage,
                    Some(exit_code),
                    &format!(
                        "codex did not write last message {}",
                        request.last_message_path.display()
                    ),
                ));
            }
            Err(err) => {
                return Err(self.error(
                    request,
                    CodexFailureKind::Io,
                    Some(exit_code),
                    &format!(
                        "failed to read last message {}: {err}",
                        request.last_message_path.display()
                    ),
                ));
            }
        };
        if last_message.trim().is_empty() {
            return Err(self.error(
                request,
                CodexFailureKind::EmptyLastMessage,
                Some(exit_code),
                &format!(
                    "codex last message {} is empty",
                    request.last_message_path.display()
                ),
            ));
        }

        if let Some(output_path) = &request.required_output_path {
            match fs::read_to_string(output_path) {
                Ok(value) if !value.trim().is_empty() => {}
                Ok(_) => {
                    if request.sandbox == "read-only" {
                        if let Err(err) =
                            write_required_output_from_last_message(output_path, &last_message)
                        {
                            return Err(self.error(
                                request,
                                CodexFailureKind::Io,
                                Some(exit_code),
                                &format!(
                                    "failed to write required output {} from last message: {err}",
                                    output_path.display()
                                ),
                            ));
                        }
                    } else {
                        return Err(self.error(
                            request,
                            CodexFailureKind::EmptyRequiredOutput,
                            Some(exit_code),
                            &format!("required codex output {} is empty", output_path.display()),
                        ));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    if request.sandbox == "read-only" {
                        if let Err(err) =
                            write_required_output_from_last_message(output_path, &last_message)
                        {
                            return Err(self.error(
                                request,
                                CodexFailureKind::Io,
                                Some(exit_code),
                                &format!(
                                    "failed to write required output {} from last message: {err}",
                                    output_path.display()
                                ),
                            ));
                        }
                    } else {
                        return Err(self.error(
                            request,
                            CodexFailureKind::MissingRequiredOutput,
                            Some(exit_code),
                            &format!(
                                "required codex output {} was not written",
                                output_path.display()
                            ),
                        ));
                    }
                }
                Err(err) => {
                    return Err(self.error(
                        request,
                        CodexFailureKind::Io,
                        Some(exit_code),
                        &format!(
                            "failed to read required output {}: {err}",
                            output_path.display()
                        ),
                    ));
                }
            }
        }

        Ok(CodexRunOutput {
            exit_code,
            last_message,
            prompt_path: request.prompt_path.clone(),
            stdout_log_path: request.stdout_log_path.clone(),
            stderr_log_path: request.stderr_log_path.clone(),
            last_message_path: request.last_message_path.clone(),
        })
    }

    fn error(
        &self,
        request: &CodexRunRequest,
        kind: CodexFailureKind,
        exit_code: Option<i32>,
        message: &str,
    ) -> Box<CodexExecutionError> {
        Box::new(CodexExecutionError {
            kind,
            message: message.to_string(),
            exit_code,
            prompt_path: request.prompt_path.clone(),
            stdout_log_path: request.stdout_log_path.clone(),
            stderr_log_path: request.stderr_log_path.clone(),
            last_message_path: request.last_message_path.clone(),
            required_output_path: request.required_output_path.clone(),
        })
    }
}

fn write_required_output_from_last_message(path: &Path, last_message: &str) -> std::io::Result<()> {
    if last_message.trim().is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "last message is empty",
        ));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, last_message)
}

fn kill_child_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as libc::pid_t;
        if pid > 0 {
            unsafe {
                let _ = libc::kill(-pid, libc::SIGTERM);
            }
            thread::sleep(Duration::from_millis(100));
            unsafe {
                let _ = libc::kill(-pid, libc::SIGKILL);
            }
        }
    }

    let _ = child.kill();
}

fn prepare_codex_paths(request: &CodexRunRequest) -> std::io::Result<()> {
    for path in [
        Some(&request.prompt_path),
        Some(&request.stdout_log_path),
        Some(&request.stderr_log_path),
        Some(&request.last_message_path),
        request.required_output_path.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn toml_string_literal(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t");
    format!("\"{escaped}\"")
}

fn append_timeout_log(path: &Path, timeout_seconds: u64) {
    if let Ok(mut file) = OpenOptions::new()
        .append(true)
        .create(true)
        .truncate(false)
        .open(path)
    {
        let _ = writeln!(
            file,
            "\ncodex-task: timeout after {timeout_seconds} seconds; process was killed"
        );
    }
}

pub fn repo_run_store_dir(context: &ConfigContext) -> Result<PathBuf> {
    Ok(RunStore::for_repo(&context.repo_root, &context.home_dir)?.repo_runs_dir)
}

pub fn repo_hash(repo_root: &Path) -> Result<String> {
    let canonical = repo_root
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", repo_root.display()))?;
    let remote = git_remote_origin(repo_root).unwrap_or_default();
    let identity = format!("{}\n{remote}", canonical.display());
    Ok(format!("{:016x}", fnv1a64(identity.as_bytes())))
}

fn git_remote_origin(repo_root: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if remote.is_empty() {
        None
    } else {
        Some(remote)
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunId(String);

impl RunId {
    pub fn parse(value: &str) -> std::result::Result<Self, AppError> {
        if value.is_empty() {
            return Err(AppError::Config(
                "invalid run id: must not be empty".to_string(),
            ));
        }
        if value == "." || value == ".." {
            return Err(AppError::Config(format!("invalid run id {value:?}")));
        }
        if !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err(AppError::Config(format!(
                "invalid run id {value:?}: expected only ASCII letters, digits, '.', '_' or '-'"
            )));
        }

        Ok(Self(value.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunStore {
    pub global_root: PathBuf,
    pub repo_hash: String,
    pub repo_runs_dir: PathBuf,
}

impl RunStore {
    pub fn for_repo(repo_root: &Path, home_dir: &Path) -> Result<Self> {
        let global_root = home_dir.join(".codex/task-runner");
        let repo_hash = repo_hash(repo_root)?;
        let repo_runs_dir = global_root.join("runs").join(&repo_hash);
        Ok(Self {
            global_root,
            repo_hash,
            repo_runs_dir,
        })
    }

    pub fn ensure_repo_dir(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.repo_runs_dir)
    }

    pub fn run_dir(&self, run_id: &str) -> std::result::Result<PathBuf, AppError> {
        let run_id = RunId::parse(run_id)?;
        Ok(self.repo_runs_dir.join(run_id.as_str()))
    }

    pub fn tasks_path(&self, run_id: &str) -> std::result::Result<PathBuf, AppError> {
        Ok(self.run_dir(run_id)?.join("tasks.json"))
    }

    pub fn state_path(&self, run_id: &str) -> std::result::Result<PathBuf, AppError> {
        Ok(self.run_dir(run_id)?.join("state.json"))
    }

    pub fn metadata_path(&self, run_id: &str) -> std::result::Result<PathBuf, AppError> {
        Ok(self.run_dir(run_id)?.join("metadata.json"))
    }

    pub fn lock_path(&self, run_id: &str) -> std::result::Result<PathBuf, AppError> {
        Ok(self.run_dir(run_id)?.join("lock"))
    }

    pub fn execution_lock_path(&self, run_id: &str) -> std::result::Result<PathBuf, AppError> {
        Ok(self.run_dir(run_id)?.join("execution.lock"))
    }

    pub fn try_acquire_execution_lock(
        &self,
        run_id: &str,
    ) -> std::result::Result<RunExecutionLock, AppError> {
        let run_dir = self.run_dir(run_id)?;
        if !run_dir.exists() {
            return Err(AppError::Runtime(format!(
                "run {run_id} does not exist under {}",
                self.repo_runs_dir.display()
            )));
        }

        let lock_path = self.execution_lock_path(run_id)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| {
                AppError::Io(format!("failed to open {}: {err}", lock_path.display()))
            })?;
        match lock_file.try_lock_exclusive() {
            Ok(()) => Ok(RunExecutionLock { lock_file }),
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                Err(AppError::RunLocked(format!(
                    "run {run_id} is already being executed; lock {} is held",
                    lock_path.display()
                )))
            }
            Err(err) => Err(AppError::Io(format!(
                "failed to lock {}: {err}",
                lock_path.display()
            ))),
        }
    }

    pub fn read_task_file(&self, run_id: &str) -> std::result::Result<TaskFile, AppError> {
        read_task_file(&self.tasks_path(run_id)?)
    }

    pub fn read_metadata(&self, run_id: &str) -> std::result::Result<RunMetadata, AppError> {
        read_run_metadata(&self.metadata_path(run_id)?)
    }

    pub fn read_run_state(&self, run_id: &str) -> std::result::Result<RunState, AppError> {
        let path = self.state_path(run_id)?;
        if path.exists() {
            read_run_state(&path)
        } else {
            Ok(RunState::default())
        }
    }

    pub fn write_task_file(
        &self,
        run_id: &str,
        task_file: &TaskFile,
    ) -> std::result::Result<(), AppError> {
        validate_task_file(task_file)?;
        self.write_locked_json(run_id, "tasks.json", task_file)
    }

    pub fn write_metadata(
        &self,
        run_id: &str,
        metadata: &RunMetadata,
    ) -> std::result::Result<(), AppError> {
        validate_run_metadata(metadata)?;
        self.write_locked_json(run_id, "metadata.json", metadata)
    }

    pub fn write_run_state(
        &self,
        run_id: &str,
        state: &RunState,
    ) -> std::result::Result<(), AppError> {
        validate_run_state(state)?;
        self.write_locked_json(run_id, "state.json", state)
    }

    pub fn update_run_state<F, R>(
        &self,
        run_id: &str,
        update: F,
    ) -> std::result::Result<R, AppError>
    where
        F: FnOnce(&mut RunState) -> std::result::Result<R, AppError>,
    {
        let run_dir = self.run_dir(run_id)?;
        fs::create_dir_all(&run_dir).map_err(|err| {
            AppError::Io(format!("failed to create {}: {err}", run_dir.display()))
        })?;

        let lock_path = self.lock_path(run_id)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| {
                AppError::Io(format!("failed to open {}: {err}", lock_path.display()))
            })?;
        lock_file.lock_exclusive().map_err(|err| {
            AppError::Io(format!("failed to lock {}: {err}", lock_path.display()))
        })?;

        let state_path = self.state_path(run_id)?;
        let mut state = if state_path.exists() {
            read_run_state(&state_path)?
        } else {
            RunState::default()
        };
        let result = update(&mut state);
        let write_result = match result {
            Ok(value) => {
                validate_run_state(&state)?;
                write_json_atomic(&run_dir, "state.json", &state).map(|()| value)
            }
            Err(err) => Err(err),
        };
        let unlock_result = lock_file.unlock();

        let value = write_result?;
        unlock_result.map_err(|err| {
            AppError::Io(format!("failed to unlock {}: {err}", lock_path.display()))
        })?;
        Ok(value)
    }

    fn write_locked_json<T>(
        &self,
        run_id: &str,
        file_name: &str,
        value: &T,
    ) -> std::result::Result<(), AppError>
    where
        T: Serialize,
    {
        let run_dir = self.run_dir(run_id)?;
        fs::create_dir_all(&run_dir).map_err(|err| {
            AppError::Io(format!("failed to create {}: {err}", run_dir.display()))
        })?;

        let lock_path = self.lock_path(run_id)?;
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|err| {
                AppError::Io(format!("failed to open {}: {err}", lock_path.display()))
            })?;
        lock_file.lock_exclusive().map_err(|err| {
            AppError::Io(format!("failed to lock {}: {err}", lock_path.display()))
        })?;

        let write_result = write_json_atomic(&run_dir, file_name, value);
        let unlock_result = lock_file.unlock();

        write_result?;
        unlock_result.map_err(|err| {
            AppError::Io(format!("failed to unlock {}: {err}", lock_path.display()))
        })?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct RunExecutionLock {
    lock_file: File,
}

impl Drop for RunExecutionLock {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MigrationReport {
    pub migration_from: Option<u64>,
    pub migration_to: u64,
}

#[derive(Debug, Clone)]
pub struct Migrated<T> {
    pub value: T,
    pub report: Option<MigrationReport>,
}

fn write_json_atomic<T>(dir: &Path, file_name: &str, value: &T) -> std::result::Result<(), AppError>
where
    T: Serialize,
{
    let target = dir.join(file_name);
    let temp = dir.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    ));

    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|err| AppError::Io(format!("failed to create {}: {err}", temp.display())))?;

    let mut writer = BufWriter::new(file);
    serde_json::to_writer_pretty(&mut writer, value)
        .map_err(|err| AppError::Runtime(format!("failed to encode JSON: {err}")))?;
    writer
        .write_all(b"\n")
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", temp.display())))?;
    writer
        .flush()
        .map_err(|err| AppError::Io(format!("failed to flush {}: {err}", temp.display())))?;
    let file = writer
        .into_inner()
        .map_err(|err| AppError::Io(format!("failed to finish {}: {err}", temp.display())))?;
    file.sync_all()
        .map_err(|err| AppError::Io(format!("failed to fsync {}: {err}", temp.display())))?;

    if let Err(err) = fs::rename(&temp, &target) {
        let _ = fs::remove_file(&temp);
        return Err(AppError::Io(format!(
            "failed to rename {} to {}: {err}",
            temp.display(),
            target.display()
        )));
    }

    if let Ok(dir_file) = File::open(dir) {
        dir_file
            .sync_all()
            .map_err(|err| AppError::Io(format!("failed to fsync {}: {err}", dir.display())))?;
    }

    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskFile {
    #[serde(rename = "version", default = "default_task_file_version")]
    pub schema_version: u64,
    #[serde(rename = "runId")]
    pub run_id: String,
    pub branch: String,
    #[serde(rename = "specFile")]
    pub spec_file: String,
    #[serde(rename = "verificationCommands", default)]
    pub verification_commands: Vec<VerificationCommand>,
    pub tasks: Vec<Task>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    #[serde(default)]
    pub priority: u64,
    #[serde(default)]
    pub group: String,
    pub title: String,
    #[serde(rename = "maxAttempts", default)]
    pub max_attempts: Option<u64>,
    #[serde(rename = "timeoutSeconds", default)]
    pub timeout_seconds: Option<u64>,
    pub output: String,
    pub prompt: String,
    #[serde(rename = "specFile", default)]
    pub spec_file: Option<String>,
    #[serde(rename = "dependsOn", default)]
    pub depends_on: Vec<String>,
    #[serde(
        rename = "reviewCriteria",
        default,
        deserialize_with = "deserialize_string_vec_from_string_or_array"
    )]
    pub review_criteria: Vec<String>,
    #[serde(rename = "analyzeTimeoutSeconds", default)]
    pub analyze_timeout_seconds: Option<u64>,
    #[serde(rename = "analyzeRequired", default = "default_true")]
    pub analyze_required: bool,
    #[serde(rename = "requireReviewApproval", default)]
    pub require_review_approval: bool,
    #[serde(rename = "maxReviewAttempts", default = "default_max_review_attempts")]
    pub max_review_attempts: u64,
    #[serde(rename = "reviewTimeoutSeconds", default)]
    pub review_timeout_seconds: Option<u64>,
    #[serde(rename = "verificationCommands", default)]
    pub verification_commands: Vec<VerificationCommand>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn deserialize_string_vec_from_string_or_array<'de, D>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrArray {
        One(String),
        Many(Vec<String>),
    }

    Ok(match StringOrArray::deserialize(deserializer)? {
        StringOrArray::One(value) if value.trim().is_empty() => Vec::new(),
        StringOrArray::One(value) => vec![value],
        StringOrArray::Many(values) => values,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunState {
    #[serde(rename = "version", default = "default_run_state_version")]
    pub schema_version: u64,
    #[serde(default)]
    pub tasks: Vec<TaskState>,
    #[serde(
        rename = "featureReviewStatus",
        default = "default_feature_review_status"
    )]
    pub feature_review_status: FeatureReviewStatus,
    #[serde(rename = "featureReviewAttempts", default)]
    pub feature_review_attempts: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for RunState {
    fn default() -> Self {
        Self {
            schema_version: default_run_state_version(),
            tasks: Vec::new(),
            feature_review_status: default_feature_review_status(),
            feature_review_attempts: 0,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskState {
    pub id: String,
    #[serde(default = "default_task_status")]
    pub status: TaskStatus,
    #[serde(default)]
    pub phase: Option<TaskPhase>,
    #[serde(default)]
    pub attempts: u64,
    #[serde(rename = "reviewAttempts", default)]
    pub review_attempts: u64,
    #[serde(rename = "startedAt", default)]
    pub started_at: Option<String>,
    #[serde(rename = "finishedAt", default)]
    pub finished_at: Option<String>,
    #[serde(rename = "updatedAt", default)]
    pub updated_at: Option<String>,
    #[serde(rename = "approvedAt", default)]
    pub approved_at: Option<String>,
    #[serde(rename = "ignoredAt", default)]
    pub ignored_at: Option<String>,
    #[serde(rename = "ignoreReason", default)]
    pub ignore_reason: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(rename = "analysisOutput", default)]
    pub analysis_output: Option<String>,
    #[serde(rename = "reviewOutput", default)]
    pub review_output: Option<String>,
    #[serde(rename = "lastExitCode", default)]
    pub last_exit_code: Option<i32>,
    #[serde(rename = "lastError", default)]
    pub last_error: Option<String>,
    #[serde(rename = "lastLog", default)]
    pub last_log: Option<String>,
    #[serde(rename = "lastVerdict", default)]
    pub last_verdict: Option<ReviewVerdict>,
    #[serde(rename = "lastReviewComments", default)]
    pub last_review_comments: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_task_file_version() -> u64 {
    2
}

fn default_run_state_version() -> u64 {
    1
}

fn default_true() -> bool {
    true
}

fn default_max_review_attempts() -> u64 {
    2
}

fn default_feature_review_status() -> FeatureReviewStatus {
    FeatureReviewStatus::Pending
}

fn default_task_status() -> TaskStatus {
    TaskStatus::Pending
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusView {
    pub run_id: String,
    pub branch: String,
    pub spec_file: String,
    pub run_dir: PathBuf,
    pub feature_review_status: String,
    pub feature_review_attempts: u64,
    pub counts: BTreeMap<String, usize>,
    pub tasks: Vec<TaskStatusView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TaskStatusView {
    pub id: String,
    pub priority: u64,
    pub group: String,
    pub title: String,
    pub status: String,
    pub phase: Option<String>,
    pub attempts: u64,
    pub review_attempts: u64,
    pub depends_on: Vec<String>,
    pub last_error: Option<String>,
}

pub enum StatusResult {
    View(StatusView),
    Message(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    pub run_id: Option<String>,
    pub interval_seconds: u64,
    pub max_failures: Option<u64>,
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunTaskOptions {
    pub run_id: Option<String>,
    pub task_id: String,
    pub from: Option<TaskPhase>,
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyOptions {
    pub run_id: Option<String>,
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOptions {
    pub run_id: Option<String>,
    pub task_id: String,
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeOptions {
    pub run_id: Option<String>,
    pub no_cleanup: bool,
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchedulerResult {
    pub run_id: String,
    pub message: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskExecutionOutcome {
    Continue,
    CompletedThroughImplement,
    CompletedThroughVerify,
    CompletedThroughReview,
    CompletedThroughCommit,
    ReviewChangesRequested,
    PausedForAnalysisReview,
    FailedRetryable,
    Blocked,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunnableCheck {
    Runnable,
    DependencyBlocked,
    AnalysisReview,
    Terminal,
    FuturePhase,
    Running,
}

#[derive(Debug, Clone)]
struct PreparedPhase {
    task: Task,
    phase: TaskPhase,
    state_before_running: TaskState,
}

pub fn watch_run(
    start: &Path,
    options: WatchOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    watch_run_in_repo(&repo_root, &home, options)
}

pub fn watch_run_in_repo(
    repo_root: &Path,
    home: &Path,
    options: WatchOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, options.run_id.as_deref())?;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    let max_failures = options
        .max_failures
        .unwrap_or(context.merged.runner.max_consecutive_failures)
        .max(1);
    let mut consecutive_failures = 0;
    let mut completed = 0;

    loop {
        let recovered = recover_stale_running_tasks(&store, &run_id)?;
        if recovered > 0 {
            append_event_log(
                &store.run_dir(&run_id)?,
                &format!("recovered {recovered} stale running task(s)"),
            )?;
        }

        let task_file = store.read_task_file(&run_id)?;
        let state = store.read_run_state(&run_id)?;
        let Some(task_id) = select_next_runnable_task(&task_file, &state)? else {
            let blocked = count_tasks_with_status(&task_file, &state, TaskStatus::Blocked)?;
            if blocked > 0 {
                return Ok(SchedulerResult {
                    run_id,
                    message: format!("Run has {blocked} blocked task(s)"),
                    exit_code: 1,
                });
            }
            let message = if completed == 0 {
                format!("No runnable tasks for run {run_id}")
            } else {
                format!("Stopped run {run_id} after {completed} task step(s)")
            };
            return Ok(SchedulerResult {
                run_id,
                message,
                exit_code: 0,
            });
        };

        match execute_task_until_boundary(
            &context,
            &store,
            &run_id,
            &task_id,
            options.codex_bin.clone(),
            false,
            false,
        )? {
            TaskExecutionOutcome::CompletedThroughImplement
            | TaskExecutionOutcome::CompletedThroughVerify
            | TaskExecutionOutcome::CompletedThroughReview
            | TaskExecutionOutcome::CompletedThroughCommit
            | TaskExecutionOutcome::ReviewChangesRequested
            | TaskExecutionOutcome::PausedForAnalysisReview => {
                completed += 1;
                consecutive_failures = 0;
            }
            TaskExecutionOutcome::Continue => {
                consecutive_failures = 0;
            }
            TaskExecutionOutcome::FailedRetryable => {
                completed += 1;
                consecutive_failures += 1;
                if consecutive_failures >= max_failures {
                    return Ok(SchedulerResult {
                        run_id,
                        message: format!(
                            "Stopped run after {consecutive_failures} consecutive task failure(s)"
                        ),
                        exit_code: 1,
                    });
                }
            }
            TaskExecutionOutcome::Blocked => {
                return Ok(SchedulerResult {
                    run_id,
                    message: format!("Task {task_id} is blocked"),
                    exit_code: 1,
                });
            }
            TaskExecutionOutcome::Deferred => {
                return Ok(SchedulerResult {
                    run_id,
                    message: format!("Task {task_id} is waiting for a later phase"),
                    exit_code: 0,
                });
            }
        }

        if options.interval_seconds > 0 {
            thread::sleep(Duration::from_secs(options.interval_seconds));
        }
    }
}

pub fn run_one_task(
    start: &Path,
    options: RunTaskOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    run_one_task_in_repo(&repo_root, &home, options)
}

pub fn run_one_task_in_repo(
    repo_root: &Path,
    home: &Path,
    options: RunTaskOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, options.run_id.as_deref())?;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    recover_stale_running_tasks(&store, &run_id)?;

    if let Some(from) = options.from {
        if !matches!(
            from,
            TaskPhase::Analyze
                | TaskPhase::Implement
                | TaskPhase::Verify
                | TaskPhase::Review
                | TaskPhase::Commit
        ) {
            return Err(AppError::Runtime(format!(
                "--from {} is outside this task scope",
                from.as_str()
            )));
        }
        force_task_phase(&store, &run_id, &options.task_id, from)?;
    }

    let task_file = store.read_task_file(&run_id)?;
    let state = store.read_run_state(&run_id)?;
    let task = find_task(&task_file, &options.task_id)?;
    match runnable_status(task, &task_file, &state)? {
        RunnableCheck::Runnable => {}
        RunnableCheck::DependencyBlocked => {
            return Err(AppError::Runtime(format!(
                "task {} cannot run until all dependencies are done",
                options.task_id
            )));
        }
        RunnableCheck::AnalysisReview => {
            return Ok(SchedulerResult {
                run_id,
                message: format!("Task {} is waiting for analysis approval", options.task_id),
                exit_code: 0,
            });
        }
        RunnableCheck::FuturePhase => {
            return Ok(SchedulerResult {
                run_id,
                message: format!("Task {} is waiting for a later phase", options.task_id),
                exit_code: 0,
            });
        }
        RunnableCheck::Running => {
            return Err(AppError::Runtime(format!(
                "task {} is already running",
                options.task_id
            )));
        }
        RunnableCheck::Terminal => {
            return Err(AppError::Runtime(format!(
                "task {} is not runnable from its current status",
                options.task_id
            )));
        }
    }

    let outcome = execute_task_until_boundary(
        &context,
        &store,
        &run_id,
        &options.task_id,
        options.codex_bin,
        matches!(options.from, Some(TaskPhase::Verify)),
        matches!(options.from, Some(TaskPhase::Review)),
    )?;
    let (message, exit_code) = match outcome {
        TaskExecutionOutcome::CompletedThroughImplement => (
            format!("Task {} completed through implement", options.task_id),
            0,
        ),
        TaskExecutionOutcome::CompletedThroughVerify => (
            format!("Task {} completed through verify", options.task_id),
            0,
        ),
        TaskExecutionOutcome::CompletedThroughReview => {
            (format!("Task {} review approved", options.task_id), 0)
        }
        TaskExecutionOutcome::CompletedThroughCommit => (
            format!("Task {} completed through commit", options.task_id),
            0,
        ),
        TaskExecutionOutcome::ReviewChangesRequested => (
            format!("Task {} review requested changes", options.task_id),
            1,
        ),
        TaskExecutionOutcome::Continue => (
            format!("Task {} advanced to the next phase", options.task_id),
            0,
        ),
        TaskExecutionOutcome::PausedForAnalysisReview => (
            format!("Task {} is waiting for analysis approval", options.task_id),
            0,
        ),
        TaskExecutionOutcome::FailedRetryable => (
            format!("Task {} failed and is ready to retry", options.task_id),
            1,
        ),
        TaskExecutionOutcome::Blocked => (format!("Task {} is blocked", options.task_id), 1),
        TaskExecutionOutcome::Deferred => (
            format!("Task {} is waiting for a later phase", options.task_id),
            0,
        ),
    };

    Ok(SchedulerResult {
        run_id,
        message,
        exit_code,
    })
}

pub fn verify_tasks(
    start: &Path,
    options: VerifyOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    verify_tasks_in_repo(&repo_root, &home, options)
}

pub fn verify_tasks_in_repo(
    repo_root: &Path,
    home: &Path,
    options: VerifyOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, options.run_id.as_deref())?;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    recover_stale_running_tasks(&store, &run_id)?;

    if options.target == "all" {
        let task_file = store.read_task_file(&run_id)?;
        let state = store.read_run_state(&run_id)?;
        let state_by_id = normalized_state_map(&task_file, &state)?;
        let mut task_ids = task_file
            .tasks
            .iter()
            .filter(|task| {
                state_by_id.get(task.id.as_str()).is_some_and(|task_state| {
                    task_state.status == TaskStatus::Pending
                        && task_state.phase == Some(TaskPhase::Verify)
                        && matches!(
                            runnable_status(task, &task_file, &state),
                            Ok(RunnableCheck::Runnable)
                        )
                })
            })
            .map(|task| (task.priority, task.id.clone()))
            .collect::<Vec<_>>();
        task_ids.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        if task_ids.is_empty() {
            return Ok(SchedulerResult {
                run_id,
                message: "No tasks are waiting for verify".to_string(),
                exit_code: 0,
            });
        }

        let mut verified = 0;
        for (_, task_id) in task_ids {
            let outcome = execute_task_until_boundary(
                &context, &store, &run_id, &task_id, None, true, false,
            )?;
            match outcome {
                TaskExecutionOutcome::CompletedThroughVerify => verified += 1,
                TaskExecutionOutcome::FailedRetryable => {
                    return Ok(SchedulerResult {
                        run_id,
                        message: format!(
                            "Task {task_id} verification failed and is ready to retry"
                        ),
                        exit_code: 1,
                    });
                }
                TaskExecutionOutcome::Blocked => {
                    return Ok(SchedulerResult {
                        run_id,
                        message: format!("Task {task_id} verification blocked"),
                        exit_code: 1,
                    });
                }
                TaskExecutionOutcome::Deferred
                | TaskExecutionOutcome::Continue
                | TaskExecutionOutcome::CompletedThroughImplement
                | TaskExecutionOutcome::CompletedThroughReview
                | TaskExecutionOutcome::CompletedThroughCommit
                | TaskExecutionOutcome::ReviewChangesRequested
                | TaskExecutionOutcome::PausedForAnalysisReview => {}
            }
        }

        return Ok(SchedulerResult {
            run_id,
            message: format!("Verified {verified} task(s)"),
            exit_code: 0,
        });
    }

    let task_file = store.read_task_file(&run_id)?;
    let state = store.read_run_state(&run_id)?;
    let task = find_task(&task_file, &options.target)?;
    ensure_dependencies_done(task, &task_file, &state)?;
    force_task_phase(&store, &run_id, &options.target, TaskPhase::Verify)?;

    let outcome = execute_task_until_boundary(
        &context,
        &store,
        &run_id,
        &options.target,
        None,
        true,
        false,
    )?;
    let (message, exit_code) = match outcome {
        TaskExecutionOutcome::CompletedThroughVerify => (
            format!("Task {} completed through verify", options.target),
            0,
        ),
        TaskExecutionOutcome::FailedRetryable => (
            format!(
                "Task {} verification failed and is ready to retry",
                options.target
            ),
            1,
        ),
        TaskExecutionOutcome::Blocked => {
            (format!("Task {} verification blocked", options.target), 1)
        }
        TaskExecutionOutcome::Deferred => (
            format!("Task {} is waiting for a later phase", options.target),
            0,
        ),
        TaskExecutionOutcome::CompletedThroughImplement
        | TaskExecutionOutcome::CompletedThroughReview
        | TaskExecutionOutcome::CompletedThroughCommit
        | TaskExecutionOutcome::ReviewChangesRequested
        | TaskExecutionOutcome::Continue
        | TaskExecutionOutcome::PausedForAnalysisReview => {
            (format!("Task {} did not reach verify", options.target), 1)
        }
    };

    Ok(SchedulerResult {
        run_id,
        message,
        exit_code,
    })
}

pub fn review_task(
    start: &Path,
    options: ReviewOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    review_task_in_repo(&repo_root, &home, options)
}

pub fn review_task_in_repo(
    repo_root: &Path,
    home: &Path,
    options: ReviewOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, options.run_id.as_deref())?;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    recover_stale_running_tasks(&store, &run_id)?;

    let task_file = store.read_task_file(&run_id)?;
    let state = store.read_run_state(&run_id)?;
    let task = find_task(&task_file, &options.task_id)?;
    ensure_dependencies_done(task, &task_file, &state)?;
    prepare_task_for_explicit_review(&store, &run_id, &task_file, &options.task_id)?;

    let outcome = execute_task_until_boundary(
        &context,
        &store,
        &run_id,
        &options.task_id,
        options.codex_bin,
        false,
        true,
    )?;
    let (message, exit_code) = match outcome {
        TaskExecutionOutcome::CompletedThroughReview => {
            (format!("Task {} review approved", options.task_id), 0)
        }
        TaskExecutionOutcome::ReviewChangesRequested => (
            format!("Task {} review requested changes", options.task_id),
            1,
        ),
        TaskExecutionOutcome::FailedRetryable => (
            format!("Task {} review execution failed", options.task_id),
            1,
        ),
        TaskExecutionOutcome::Blocked => (format!("Task {} review blocked", options.task_id), 1),
        TaskExecutionOutcome::Deferred => (
            format!("Task {} is not ready for review", options.task_id),
            1,
        ),
        TaskExecutionOutcome::CompletedThroughImplement
        | TaskExecutionOutcome::CompletedThroughCommit
        | TaskExecutionOutcome::CompletedThroughVerify
        | TaskExecutionOutcome::Continue
        | TaskExecutionOutcome::PausedForAnalysisReview => {
            (format!("Task {} did not reach review", options.task_id), 1)
        }
    };

    Ok(SchedulerResult {
        run_id,
        message,
        exit_code,
    })
}

pub fn finalize_run(
    start: &Path,
    options: FinalizeOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    finalize_run_in_repo(&repo_root, &home, options)
}

pub fn finalize_run_in_repo(
    repo_root: &Path,
    home: &Path,
    options: FinalizeOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, options.run_id.as_deref())?;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    recover_stale_running_tasks(&store, &run_id)?;

    let task_file = store.read_task_file(&run_id)?;
    let state = store.read_run_state(&run_id)?;
    ensure_all_tasks_complete_for_finalize(&task_file, &state)?;

    let attempt = state.feature_review_attempts + 1;
    let run_dir = store.run_dir(&run_id)?;
    let output_path = feature_review_output_path(&run_dir, attempt);
    let prompt = render_review_feature_prompt(&context, &store, &run_id, &task_file, &output_path)?;

    store.update_run_state(&run_id, |state| {
        state.feature_review_status = FeatureReviewStatus::Running;
        state.feature_review_attempts = attempt;
        state.extra.insert(
            "featureReviewOutput".to_string(),
            Value::String(output_path.display().to_string()),
        );
        state.extra.remove("featureReviewLastError");
        state.extra.remove("featureReviewLastLog");
        if options.no_cleanup {
            state
                .extra
                .insert("featureReviewNoCleanup".to_string(), Value::Bool(true));
        }
        Ok(())
    })?;

    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(format!("feature-review.{attempt}.md")),
        stdout_log_path: run_dir
            .join("logs")
            .join(format!("feature-review.{attempt}.stdout.log")),
        stderr_log_path: run_dir
            .join("logs")
            .join(format!("feature-review.{attempt}.stderr.log")),
        last_message_path: run_dir
            .join("logs")
            .join(format!("feature-review.{attempt}.last-message.md")),
        required_output_path: Some(output_path.clone()),
        sandbox: context.merged.runner.review_sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_review_timeout_seconds,
    };

    let result = build_executor(&context, options.codex_bin).execute(&request);
    let (message, exit_code) = match result {
        Ok(_) => match parse_final_review_output_file(&output_path) {
            Ok(ReviewVerdict::Approved) => {
                finish_feature_review(&store, &run_id, FeatureReviewStatus::Approved, None, None)?;
                finalize_approved_run(&context, &store, &run_id, &task_file, options.no_cleanup)?;
                (
                    if options.no_cleanup {
                        format!("Run {run_id} final review approved")
                    } else {
                        format!("Run {run_id} final review approved and archived")
                    },
                    0,
                )
            }
            Ok(ReviewVerdict::ChangesRequested) => {
                finish_feature_review(
                    &store,
                    &run_id,
                    FeatureReviewStatus::ChangesRequested,
                    None,
                    Some(output_path.display().to_string()),
                )?;
                (format!("Run {run_id} final review requested changes"), 1)
            }
            Err(err) => {
                finish_feature_review(
                    &store,
                    &run_id,
                    FeatureReviewStatus::Failed,
                    Some(err),
                    Some(output_path.display().to_string()),
                )?;
                (format!("Run {run_id} final review failed"), 1)
            }
        },
        Err(err) => {
            let err = *err;
            finish_feature_review(
                &store,
                &run_id,
                FeatureReviewStatus::Failed,
                Some(err.message),
                Some(err.stderr_log_path.display().to_string()),
            )?;
            (format!("Run {run_id} final review failed"), 1)
        }
    };

    Ok(SchedulerResult {
        run_id,
        message,
        exit_code,
    })
}

pub fn load_status(
    repo_root: &Path,
    home_dir: &Path,
    run_id: Option<&str>,
) -> std::result::Result<StatusResult, AppError> {
    let context = load_config(repo_root, home_dir, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;

    let selected_run_id = match run_id {
        Some(value) => value.to_string(),
        None => match discover_run_ids(&store.repo_runs_dir)? {
            ids if ids.is_empty() => {
                return Ok(StatusResult::Message(format!(
                    "No runs found under {}",
                    store.repo_runs_dir.display()
                )));
            }
            ids if ids.len() == 1 => ids[0].clone(),
            ids => {
                return Ok(StatusResult::Message(format!(
                    "Multiple runs found under {}: {}. Pass --run-id.",
                    store.repo_runs_dir.display(),
                    ids.join(", ")
                )));
            }
        },
    };

    let run_dir = store.run_dir(&selected_run_id)?;
    let task_file = store.read_task_file(&selected_run_id)?;
    let run_state = store.read_run_state(&selected_run_id)?;

    Ok(StatusResult::View(merge_status_view(
        run_dir, task_file, run_state,
    )?))
}

fn select_run_id(
    store: &RunStore,
    requested: Option<&str>,
) -> std::result::Result<String, AppError> {
    match requested {
        Some(value) => {
            RunId::parse(value)?;
            Ok(value.to_string())
        }
        None => match discover_run_ids(&store.repo_runs_dir)? {
            ids if ids.is_empty() => Err(AppError::Runtime(format!(
                "No runs found under {}",
                store.repo_runs_dir.display()
            ))),
            ids if ids.len() == 1 => Ok(ids[0].clone()),
            ids => Err(AppError::Config(format!(
                "Multiple runs found under {}: {}. Pass --run-id.",
                store.repo_runs_dir.display(),
                ids.join(", ")
            ))),
        },
    }
}

fn recover_stale_running_tasks(
    store: &RunStore,
    run_id: &str,
) -> std::result::Result<usize, AppError> {
    let task_file = store.read_task_file(run_id)?;
    store.update_run_state(run_id, |state| {
        ensure_state_matches_tasks(&task_file, state)?;
        let mut recovered = 0;
        for task in &task_file.tasks {
            let task_state = state
                .tasks
                .iter_mut()
                .find(|candidate| candidate.id == task.id)
                .expect("state was normalized");
            if task_state.status != TaskStatus::Running {
                continue;
            }
            let Some(pid) = task_runner_pid(task_state) else {
                mark_stale_running(task, task_state)?;
                recovered += 1;
                continue;
            };
            if !process_is_alive(pid) {
                mark_stale_running(task, task_state)?;
                recovered += 1;
            }
        }
        Ok(recovered)
    })
}

fn mark_stale_running(task: &Task, state: &mut TaskState) -> std::result::Result<(), AppError> {
    let now = current_timestamp()?;
    let phase = state.phase.unwrap_or(TaskPhase::Implement);
    clear_runner_marker(state);
    if phase == TaskPhase::Analyze && task.analyze_required {
        state.attempts += 1;
    }
    if phase == TaskPhase::Review {
        state.review_attempts += 1;
    }
    state.finished_at = Some(now.clone());
    state.updated_at = Some(now);
    state.last_exit_code = Some(1);
    state.last_error = Some(format!(
        "stale running task recovered for phase {}",
        phase.as_str()
    ));

    let max_attempts = task_max_attempts(task);
    if phase == TaskPhase::Review && state.review_attempts >= task_max_review_attempts(task) {
        state.status = TaskStatus::Blocked;
        state.phase = Some(TaskPhase::Review);
    } else if phase == TaskPhase::Review {
        state.status = TaskStatus::ReviewFailed;
        state.phase = Some(TaskPhase::Review);
    } else if state.attempts >= max_attempts {
        state.status = TaskStatus::Blocked;
    } else if phase == TaskPhase::Analyze && !task.analyze_required {
        state.status = TaskStatus::Pending;
        state.phase = Some(TaskPhase::Implement);
    } else {
        state.status = TaskStatus::Pending;
        state.phase = Some(phase);
    }
    Ok(())
}

fn select_next_runnable_task(
    task_file: &TaskFile,
    state: &RunState,
) -> std::result::Result<Option<String>, AppError> {
    let mut tasks = task_file.tasks.iter().collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });

    for task in tasks {
        if runnable_status(task, task_file, state)? == RunnableCheck::Runnable {
            return Ok(Some(task.id.clone()));
        }
    }
    Ok(None)
}

fn count_tasks_with_status(
    task_file: &TaskFile,
    state: &RunState,
    status: TaskStatus,
) -> std::result::Result<usize, AppError> {
    let state_by_id = normalized_state_map(task_file, state)?;
    Ok(task_file
        .tasks
        .iter()
        .filter(|task| {
            state_by_id
                .get(task.id.as_str())
                .map(|task_state| task_state.status == status)
                .unwrap_or(false)
        })
        .count())
}

fn runnable_status(
    task: &Task,
    task_file: &TaskFile,
    state: &RunState,
) -> std::result::Result<RunnableCheck, AppError> {
    let state_by_id = normalized_state_map(task_file, state)?;
    let task_state = state_by_id
        .get(task.id.as_str())
        .ok_or_else(|| AppError::Config(format!("missing state for task {}", task.id)))?;

    match task_state.status {
        TaskStatus::Pending => {}
        TaskStatus::AnalysisReview => return Ok(RunnableCheck::AnalysisReview),
        TaskStatus::Running => return Ok(RunnableCheck::Running),
        TaskStatus::Done | TaskStatus::Ignored | TaskStatus::Blocked => {
            return Ok(RunnableCheck::Terminal);
        }
        TaskStatus::Reviewed => {
            if task_state.phase != Some(TaskPhase::Commit) {
                return Ok(RunnableCheck::Terminal);
            }
        }
        TaskStatus::ReviewFailed => {
            return Ok(RunnableCheck::Terminal);
        }
    }

    for dependency in &task.depends_on {
        let Some(dependency_state) = state_by_id.get(dependency.as_str()) else {
            return Err(AppError::Config(format!(
                "task {} depends on missing task {}",
                task.id, dependency
            )));
        };
        if dependency_state.status != TaskStatus::Done {
            return Ok(RunnableCheck::DependencyBlocked);
        }
    }

    match task_state.phase.unwrap_or(TaskPhase::Analyze) {
        TaskPhase::Analyze | TaskPhase::Implement => Ok(RunnableCheck::Runnable),
        TaskPhase::Verify => Ok(RunnableCheck::Runnable),
        TaskPhase::Review => Ok(RunnableCheck::Runnable),
        TaskPhase::Commit => Ok(RunnableCheck::Runnable),
        TaskPhase::AnalysisReview => Ok(RunnableCheck::AnalysisReview),
        TaskPhase::Done => Ok(RunnableCheck::FuturePhase),
    }
}

fn ensure_dependencies_done(
    task: &Task,
    task_file: &TaskFile,
    state: &RunState,
) -> std::result::Result<(), AppError> {
    let state_by_id = normalized_state_map(task_file, state)?;
    for dependency in &task.depends_on {
        let Some(dependency_state) = state_by_id.get(dependency.as_str()) else {
            return Err(AppError::Config(format!(
                "task {} depends on missing task {}",
                task.id, dependency
            )));
        };
        if dependency_state.status != TaskStatus::Done {
            return Err(AppError::Runtime(format!(
                "task {} cannot verify until dependency {} is done",
                task.id, dependency
            )));
        }
    }
    Ok(())
}

fn execute_task_until_boundary(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_id: &str,
    codex_bin: Option<PathBuf>,
    force_verify: bool,
    force_review: bool,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    loop {
        let Some(prepared) = prepare_next_phase(context, store, run_id, task_id)? else {
            return Ok(TaskExecutionOutcome::Deferred);
        };

        let outcome = match prepared.phase {
            TaskPhase::Analyze => {
                execute_analyze_phase(context, store, run_id, prepared, codex_bin.clone())?
            }
            TaskPhase::Implement => {
                execute_implement_phase(context, store, run_id, prepared, codex_bin.clone())?
            }
            TaskPhase::Verify => {
                execute_verify_phase(context, store, run_id, prepared, force_verify)?
            }
            TaskPhase::Review => execute_review_phase(
                context,
                store,
                run_id,
                prepared,
                codex_bin.clone(),
                force_review,
            )?,
            TaskPhase::Commit => execute_commit_phase(context, store, run_id, prepared)?,
            _ => TaskExecutionOutcome::Deferred,
        };

        match outcome {
            TaskExecutionOutcome::CompletedThroughImplement
            | TaskExecutionOutcome::CompletedThroughVerify
            | TaskExecutionOutcome::CompletedThroughReview
            | TaskExecutionOutcome::CompletedThroughCommit
            | TaskExecutionOutcome::ReviewChangesRequested
            | TaskExecutionOutcome::PausedForAnalysisReview
            | TaskExecutionOutcome::FailedRetryable
            | TaskExecutionOutcome::Blocked
            | TaskExecutionOutcome::Deferred => return Ok(outcome),
            TaskExecutionOutcome::Continue => continue,
        }
    }
}

fn prepare_next_phase(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_id: &str,
) -> std::result::Result<Option<PreparedPhase>, AppError> {
    let task_file = store.read_task_file(run_id)?;
    let task = find_task(&task_file, task_id)?.clone();
    store.update_run_state(run_id, |state| {
        ensure_state_matches_tasks(&task_file, state)?;
        let state_snapshot = state
            .tasks
            .iter()
            .find(|candidate| candidate.id == task.id)
            .cloned()
            .expect("state was normalized");

        if runnable_status(&task, &task_file, state)? != RunnableCheck::Runnable {
            return Ok(None);
        }

        let phase = state_snapshot.phase.unwrap_or(TaskPhase::Analyze);
        if matches!(phase, TaskPhase::Analyze | TaskPhase::Implement) {
            enforce_dirty_worktree_policy(context, &state_snapshot)?;
        }
        if phase == TaskPhase::Review
            && state_snapshot.review_attempts >= task_max_review_attempts(&task)
        {
            let task_state = state
                .tasks
                .iter_mut()
                .find(|candidate| candidate.id == task.id)
                .expect("state was normalized");
            task_state.status = TaskStatus::Blocked;
            task_state.phase = Some(TaskPhase::Review);
            task_state.updated_at = Some(current_timestamp()?);
            task_state.last_error = Some(format!(
                "maxReviewAttempts {} reached before phase review",
                task_max_review_attempts(&task)
            ));
            return Ok(None);
        }
        if !matches!(
            phase,
            TaskPhase::Verify | TaskPhase::Review | TaskPhase::Commit
        ) && state_snapshot.attempts >= task_max_attempts(&task)
        {
            let task_state = state
                .tasks
                .iter_mut()
                .find(|candidate| candidate.id == task.id)
                .expect("state was normalized");
            task_state.status = TaskStatus::Blocked;
            task_state.phase = Some(phase);
            task_state.updated_at = Some(current_timestamp()?);
            task_state.last_error = Some(format!(
                "maxAttempts {} reached before phase {}",
                task_max_attempts(&task),
                phase.as_str()
            ));
            return Ok(None);
        }

        let now = current_timestamp()?;
        let task_state = state
            .tasks
            .iter_mut()
            .find(|candidate| candidate.id == task.id)
            .expect("state was normalized");
        task_state.status = TaskStatus::Running;
        task_state.phase = Some(phase);
        if phase == TaskPhase::Implement {
            task_state.attempts += 1;
        }
        task_state.started_at = Some(now.clone());
        task_state.finished_at = None;
        task_state.updated_at = Some(now.clone());
        set_runner_marker(task_state, phase, &now);

        Ok(Some(PreparedPhase {
            task,
            phase,
            state_before_running: state_snapshot,
        }))
    })
}

fn execute_analyze_phase(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    prepared: PreparedPhase,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let run_dir = store.run_dir(run_id)?;
    let output_path = analysis_output_path(&run_dir, &prepared.task.id);
    let prompt = render_analyze_prompt(context, store, run_id, &prepared.task, &output_path)?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(format!("{}.analyze.md", prepared.task.id)),
        stdout_log_path: run_dir
            .join("logs")
            .join(format!("{}.analyze.stdout.log", prepared.task.id)),
        stderr_log_path: run_dir
            .join("logs")
            .join(format!("{}.analyze.stderr.log", prepared.task.id)),
        last_message_path: run_dir
            .join("logs")
            .join(format!("{}.analyze.last-message.md", prepared.task.id)),
        required_output_path: Some(output_path.clone()),
        sandbox: context.merged.runner.analysis_sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: prepared
            .task
            .analyze_timeout_seconds
            .unwrap_or(context.merged.runner.default_analyze_timeout_seconds),
    };
    let result = build_executor(context, codex_bin).execute(&request);

    match result {
        Ok(output) => finish_analyze_success(store, run_id, &prepared.task, &output_path, &output),
        Err(err) => finish_analyze_failure(store, run_id, &prepared.task, &err),
    }
}

fn finish_analyze_success(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    output_path: &Path,
    output: &CodexRunOutput,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(output.exit_code);
        task_state.last_error = None;
        task_state.last_log = None;
        task_state.analysis_output = Some(output_path.display().to_string());
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        if task.require_review_approval {
            task_state.status = TaskStatus::AnalysisReview;
            task_state.phase = Some(TaskPhase::AnalysisReview);
        } else {
            task_state.status = TaskStatus::Pending;
            task_state.phase = Some(TaskPhase::Implement);
        }
        Ok(())
    })?;

    if task.require_review_approval {
        Ok(TaskExecutionOutcome::PausedForAnalysisReview)
    } else {
        Ok(TaskExecutionOutcome::Continue)
    }
}

fn finish_analyze_failure(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    err: &CodexExecutionError,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let exit_code = err.exit_code.unwrap_or(1);
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(exit_code);
        task_state.last_error = Some(err.message.clone());
        task_state.last_log = Some(err.stderr_log_path.display().to_string());
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();

        if !task.analyze_required {
            task_state.status = TaskStatus::Pending;
            task_state.phase = Some(TaskPhase::Implement);
            return Ok(());
        }

        task_state.attempts += 1;
        if task_state.attempts >= task_max_attempts(task) {
            task_state.status = TaskStatus::Blocked;
        } else {
            task_state.status = TaskStatus::Pending;
        }
        task_state.phase = Some(TaskPhase::Analyze);
        Ok(())
    })?;

    if !task.analyze_required {
        Ok(TaskExecutionOutcome::Continue)
    } else {
        let state = store.read_run_state(run_id)?;
        let task_state = find_task_state(&state, &task.id)?;
        if task_state.status == TaskStatus::Blocked {
            Ok(TaskExecutionOutcome::Blocked)
        } else {
            Ok(TaskExecutionOutcome::FailedRetryable)
        }
    }
}

fn execute_implement_phase(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    prepared: PreparedPhase,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let run_dir = store.run_dir(run_id)?;
    let output_path = implementation_output_path(&run_dir, &prepared.task.id);
    let prompt = render_implement_prompt(
        context,
        store,
        run_id,
        &prepared.task,
        &prepared.state_before_running,
    )?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(format!("{}.implement.md", prepared.task.id)),
        stdout_log_path: run_dir
            .join("logs")
            .join(format!("{}.implement.stdout.log", prepared.task.id)),
        stderr_log_path: run_dir
            .join("logs")
            .join(format!("{}.implement.stderr.log", prepared.task.id)),
        last_message_path: run_dir
            .join("logs")
            .join(format!("{}.implement.last-message.md", prepared.task.id)),
        required_output_path: None,
        sandbox: context.merged.runner.sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: prepared
            .task
            .timeout_seconds
            .unwrap_or(context.merged.runner.default_task_timeout_seconds),
    };
    let result = build_executor(context, codex_bin).execute(&request);

    match result {
        Ok(output) => finish_implement_success(
            store,
            run_id,
            &prepared.task,
            &output_path,
            &output,
            &context.merged.runner.sandbox,
        ),
        Err(err) => finish_implement_failure(store, run_id, &prepared.task, &err),
    }
}

fn finish_implement_success(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    output_path: &Path,
    output: &CodexRunOutput,
    sandbox: &str,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| AppError::Io(format!("failed to create {}: {err}", parent.display())))?;
    }
    fs::write(output_path, &output.last_message)
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", output_path.display())))?;

    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(output.exit_code);
        task_state.last_error = None;
        task_state.last_log = None;
        task_state.output = Some(output_path.display().to_string());
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        if sandbox == "read-only" {
            task_state.status = TaskStatus::Reviewed;
            task_state.phase = Some(TaskPhase::Implement);
        } else {
            task_state.status = TaskStatus::Pending;
            task_state.phase = Some(TaskPhase::Verify);
        }
        Ok(())
    })?;

    if sandbox == "read-only" {
        Ok(TaskExecutionOutcome::CompletedThroughImplement)
    } else {
        Ok(TaskExecutionOutcome::Continue)
    }
}

fn finish_implement_failure(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    err: &CodexExecutionError,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(err.exit_code.unwrap_or(1));
        task_state.last_error = Some(err.message.clone());
        task_state.last_log = Some(err.stderr_log_path.display().to_string());
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        task_state.phase = Some(TaskPhase::Implement);
        if task_state.attempts >= task_max_attempts(task) {
            task_state.status = TaskStatus::Blocked;
        } else {
            task_state.status = TaskStatus::Pending;
        }
        Ok(())
    })?;

    let state = store.read_run_state(run_id)?;
    let task_state = find_task_state(&state, &task.id)?;
    if task_state.status == TaskStatus::Blocked {
        Ok(TaskExecutionOutcome::Blocked)
    } else {
        Ok(TaskExecutionOutcome::FailedRetryable)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerificationFailureKind {
    CommandFailed,
    Timeout,
    ExternalDependencyBlocker,
    StartFailed,
    Io,
}

impl VerificationFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            VerificationFailureKind::CommandFailed => "command_failed",
            VerificationFailureKind::Timeout => "timeout",
            VerificationFailureKind::ExternalDependencyBlocker => "external_dependency_blocker",
            VerificationFailureKind::StartFailed => "start_failed",
            VerificationFailureKind::Io => "io",
        }
    }
}

#[derive(Debug, Clone)]
struct VerificationCommandOutcome {
    name: String,
    required: bool,
    timeout_seconds: u64,
    log_path: PathBuf,
    exit_code: i32,
    failure_kind: Option<VerificationFailureKind>,
    failure_message: Option<String>,
}

impl VerificationCommandOutcome {
    fn succeeded(&self) -> bool {
        self.failure_kind.is_none()
    }
}

#[derive(Debug, Clone)]
struct VerificationRunSummary {
    outcomes: Vec<VerificationCommandOutcome>,
    skipped: bool,
}

impl VerificationRunSummary {
    fn success(skipped: bool) -> Self {
        Self {
            outcomes: Vec::new(),
            skipped,
        }
    }

    fn required_failure(&self) -> Option<&VerificationCommandOutcome> {
        self.outcomes
            .iter()
            .find(|outcome| outcome.required && !outcome.succeeded())
    }

    fn optional_failures(&self) -> Vec<&VerificationCommandOutcome> {
        self.outcomes
            .iter()
            .filter(|outcome| !outcome.required && !outcome.succeeded())
            .collect()
    }

    fn log_paths(&self) -> Vec<String> {
        self.outcomes
            .iter()
            .map(|outcome| outcome.log_path.display().to_string())
            .collect()
    }
}

fn execute_verify_phase(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    prepared: PreparedPhase,
    force_verify: bool,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let run_dir = store.run_dir(run_id)?;
    if !force_verify && !verification_enabled(context) {
        let summary = VerificationRunSummary::success(true);
        return finish_verify_success(store, run_id, &prepared.task, &summary);
    }

    let task_file = store.read_task_file(run_id)?;
    let commands = verification_commands_for(context, &task_file, &prepared.task);
    let summary = run_verification_commands(
        context,
        &run_dir,
        &prepared.task.id,
        &commands,
        context.merged.runner.default_verify_timeout_seconds,
    )?;

    match summary.required_failure() {
        Some(failure)
            if failure.failure_kind == Some(VerificationFailureKind::ExternalDependencyBlocker) =>
        {
            finish_verify_external_blocker(store, run_id, &prepared.task, &summary)
        }
        Some(_) => finish_verify_required_failure(store, run_id, &prepared.task, &summary),
        None => finish_verify_success(store, run_id, &prepared.task, &summary),
    }
}

fn verification_enabled(context: &ConfigContext) -> bool {
    match context.merged.runner.verify {
        Toggle::True => true,
        Toggle::False => false,
        Toggle::Auto => context.merged.runner.sandbox != "read-only",
    }
}

fn verification_commands_for(
    context: &ConfigContext,
    task_file: &TaskFile,
    task: &Task,
) -> Vec<VerificationCommand> {
    let mut commands = Vec::new();
    commands.extend(context.merged.verification_commands.clone());
    commands.extend(task_file.verification_commands.clone());
    commands.extend(task.verification_commands.clone());
    commands
}

fn run_verification_commands(
    context: &ConfigContext,
    run_dir: &Path,
    task_id: &str,
    commands: &[VerificationCommand],
    default_timeout_seconds: u64,
) -> std::result::Result<VerificationRunSummary, AppError> {
    let logs_dir = run_dir.join("logs");
    fs::create_dir_all(&logs_dir)
        .map_err(|err| AppError::Io(format!("failed to create {}: {err}", logs_dir.display())))?;

    let mut outcomes = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        let sequence = index + 1;
        let outcome = run_verification_command(
            context,
            &logs_dir,
            task_id,
            sequence,
            command,
            default_timeout_seconds,
        )?;
        let required_failed = outcome.required && !outcome.succeeded();
        outcomes.push(outcome);
        if required_failed {
            break;
        }
    }

    Ok(VerificationRunSummary {
        outcomes,
        skipped: false,
    })
}

fn run_verification_command(
    context: &ConfigContext,
    logs_dir: &Path,
    task_id: &str,
    sequence: usize,
    verification: &VerificationCommand,
    default_timeout_seconds: u64,
) -> std::result::Result<VerificationCommandOutcome, AppError> {
    let name = normalized_verification_name(&verification.name);
    let timeout_seconds = verification
        .timeout_seconds
        .unwrap_or(default_timeout_seconds)
        .max(1);
    let log_path = logs_dir.join(format!("{task_id}.verify.{sequence:02}-{name}.log"));

    let mut log = File::create(&log_path)
        .map_err(|err| AppError::Io(format!("failed to create {}: {err}", log_path.display())))?;
    write_verification_log_header(&mut log, task_id, sequence, verification, timeout_seconds)
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", log_path.display())))?;

    if verification.command.trim().is_empty() {
        writeln!(log, "codex-task: empty verification command").map_err(|err| {
            AppError::Io(format!("failed to write {}: {err}", log_path.display()))
        })?;
        return Ok(VerificationCommandOutcome {
            name,
            required: verification.required,
            timeout_seconds,
            log_path,
            exit_code: 1,
            failure_kind: Some(VerificationFailureKind::Io),
            failure_message: Some("verification command is empty".to_string()),
        });
    }

    let stdout_file = log
        .try_clone()
        .map_err(|err| AppError::Io(format!("failed to clone {}: {err}", log_path.display())))?;
    let stderr_file = log
        .try_clone()
        .map_err(|err| AppError::Io(format!("failed to clone {}: {err}", log_path.display())))?;
    drop(log);

    let mut command = Command::new("/bin/zsh");
    command
        .arg("-lc")
        .arg(&verification.command)
        .current_dir(&context.repo_root)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) == 0 {
                    Ok(())
                } else {
                    Err(std::io::Error::last_os_error())
                }
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            append_verification_log(
                &log_path,
                &format!("codex-task: failed to start /bin/zsh: {err}"),
            )?;
            let message = format!("verification command {name} failed to start: {err}");
            let kind = if external_blocker_match(&message).is_some() {
                VerificationFailureKind::ExternalDependencyBlocker
            } else {
                VerificationFailureKind::StartFailed
            };
            return Ok(VerificationCommandOutcome {
                name,
                required: verification.required,
                timeout_seconds,
                log_path,
                exit_code: 1,
                failure_kind: Some(kind),
                failure_message: Some(message),
            });
        }
    };

    let started = Instant::now();
    let timeout = Duration::from_secs(timeout_seconds);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    kill_child_tree(&mut child);
                    let _ = child.wait();
                    append_verification_log(
                        &log_path,
                        &format!(
                            "codex-task: timeout after {timeout_seconds} seconds; process was killed"
                        ),
                    )?;
                    return Ok(VerificationCommandOutcome {
                        name,
                        required: verification.required,
                        timeout_seconds,
                        log_path,
                        exit_code: 124,
                        failure_kind: Some(VerificationFailureKind::Timeout),
                        failure_message: Some(format!(
                            "verification command timed out after {timeout_seconds} seconds"
                        )),
                    });
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                append_verification_log(
                    &log_path,
                    &format!("codex-task: failed while waiting for command: {err}"),
                )?;
                return Ok(VerificationCommandOutcome {
                    name,
                    required: verification.required,
                    timeout_seconds,
                    log_path,
                    exit_code: 1,
                    failure_kind: Some(VerificationFailureKind::Io),
                    failure_message: Some(format!(
                        "failed while waiting for verification command: {err}"
                    )),
                });
            }
        }
    };

    let exit_code = status.code().unwrap_or(1);
    append_verification_log(
        &log_path,
        &format!("codex-task: command exited with {exit_code}"),
    )?;
    if status.success() {
        return Ok(VerificationCommandOutcome {
            name,
            required: verification.required,
            timeout_seconds,
            log_path,
            exit_code,
            failure_kind: None,
            failure_message: None,
        });
    }

    let log_text = fs::read_to_string(&log_path).unwrap_or_default();
    let (failure_kind, failure_message) = match external_blocker_match(&log_text) {
        Some(pattern) => (
            VerificationFailureKind::ExternalDependencyBlocker,
            format!("external dependency blocker during verification: {pattern}"),
        ),
        None => (
            VerificationFailureKind::CommandFailed,
            format!("verification command {name} exited with {exit_code}"),
        ),
    };

    Ok(VerificationCommandOutcome {
        name,
        required: verification.required,
        timeout_seconds,
        log_path,
        exit_code,
        failure_kind: Some(failure_kind),
        failure_message: Some(failure_message),
    })
}

fn write_verification_log_header(
    log: &mut File,
    task_id: &str,
    sequence: usize,
    verification: &VerificationCommand,
    timeout_seconds: u64,
) -> std::io::Result<()> {
    writeln!(log, "codex-task verification")?;
    writeln!(log, "task: {task_id}")?;
    writeln!(log, "sequence: {sequence}")?;
    writeln!(
        log,
        "name: {}",
        normalized_verification_name(&verification.name)
    )?;
    writeln!(log, "required: {}", verification.required)?;
    writeln!(log, "timeoutSeconds: {timeout_seconds}")?;
    writeln!(log, "command:")?;
    writeln!(log, "{}", verification.command)?;
    writeln!(log, "--- output ---")?;
    log.flush()?;
    Ok(())
}

fn append_verification_log(path: &Path, line: &str) -> std::result::Result<(), AppError> {
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|err| AppError::Io(format!("failed to open {}: {err}", path.display())))?;
    writeln!(file, "{line}")
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
}

fn normalized_verification_name(name: &str) -> String {
    let trimmed = name.trim();
    let raw = if trimmed.is_empty() {
        DEFAULT_VERIFICATION_COMMAND_NAME
    } else {
        trimmed
    };
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if matches!(ch, '-' | '_' | '.') {
            out.push(ch);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches(['-', '_', '.']).to_string();
    if trimmed.is_empty() {
        DEFAULT_VERIFICATION_COMMAND_NAME.to_string()
    } else {
        trimmed
    }
}

fn external_blocker_match(text: &str) -> Option<&'static str> {
    let lower = text.to_ascii_lowercase();
    [
        "operation not permitted",
        "permission denied",
        "cannot connect to the docker daemon",
        "could not find a valid docker environment",
        "testcontainers",
    ]
    .into_iter()
    .find(|pattern| lower.contains(pattern))
}

fn finish_verify_success(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    summary: &VerificationRunSummary,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(0);
        task_state.last_error = None;
        task_state.last_log = summary
            .optional_failures()
            .last()
            .map(|outcome| outcome.log_path.display().to_string());
        task_state.status = TaskStatus::Pending;
        task_state.phase = Some(TaskPhase::Review);
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        set_verification_state_extra(task_state, summary);
        Ok(())
    })?;

    Ok(TaskExecutionOutcome::CompletedThroughVerify)
}

fn finish_verify_required_failure(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    summary: &VerificationRunSummary,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let failure = summary
        .required_failure()
        .expect("caller checked required failure")
        .clone();
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(failure.exit_code);
        task_state.last_error = failure.failure_message.clone();
        task_state.last_log = Some(failure.log_path.display().to_string());
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        task_state.phase = Some(TaskPhase::Implement);
        if task_state.attempts >= task_max_attempts(task) {
            task_state.status = TaskStatus::Blocked;
        } else {
            task_state.status = TaskStatus::Pending;
        }
        set_verification_state_extra(task_state, summary);
        Ok(())
    })?;

    let state = store.read_run_state(run_id)?;
    let task_state = find_task_state(&state, &task.id)?;
    if task_state.status == TaskStatus::Blocked {
        Ok(TaskExecutionOutcome::Blocked)
    } else {
        Ok(TaskExecutionOutcome::FailedRetryable)
    }
}

fn finish_verify_external_blocker(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    summary: &VerificationRunSummary,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let failure = summary
        .required_failure()
        .expect("caller checked required failure")
        .clone();
    let message = failure
        .failure_message
        .clone()
        .unwrap_or_else(|| "external dependency blocker during verification".to_string());
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(failure.exit_code);
        task_state.last_error = Some(format!("external dependency blocker: {message}"));
        task_state.last_log = Some(failure.log_path.display().to_string());
        task_state.status = TaskStatus::Blocked;
        task_state.phase = Some(TaskPhase::Verify);
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        set_verification_state_extra(task_state, summary);
        Ok(())
    })?;

    Ok(TaskExecutionOutcome::Blocked)
}

fn set_verification_state_extra(state: &mut TaskState, summary: &VerificationRunSummary) {
    state.extra.insert(
        "verificationLogs".to_string(),
        Value::Array(summary.log_paths().into_iter().map(Value::String).collect()),
    );
    state.extra.insert(
        "verificationSkipped".to_string(),
        Value::Bool(summary.skipped),
    );

    let optional_failures = summary
        .optional_failures()
        .into_iter()
        .map(|outcome| {
            let mut value = Map::new();
            value.insert("name".to_string(), Value::String(outcome.name.clone()));
            value.insert(
                "log".to_string(),
                Value::String(outcome.log_path.display().to_string()),
            );
            value.insert(
                "exitCode".to_string(),
                Value::Number(serde_json::Number::from(outcome.exit_code)),
            );
            value.insert(
                "timeoutSeconds".to_string(),
                Value::Number(serde_json::Number::from(outcome.timeout_seconds)),
            );
            if let Some(kind) = outcome.failure_kind {
                value.insert("kind".to_string(), Value::String(kind.as_str().to_string()));
            }
            Value::Object(value)
        })
        .collect::<Vec<_>>();
    if optional_failures.is_empty() {
        state.extra.remove("verificationOptionalFailures");
    } else {
        state.extra.insert(
            "verificationOptionalFailures".to_string(),
            Value::Array(optional_failures),
        );
    }

    if let Some(failure) = summary.required_failure() {
        if let Some(kind) = failure.failure_kind {
            state.extra.insert(
                "verificationFailureKind".to_string(),
                Value::String(kind.as_str().to_string()),
            );
        }
    } else {
        state.extra.remove("verificationFailureKind");
    }
}

fn execute_review_phase(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    prepared: PreparedPhase,
    codex_bin: Option<PathBuf>,
    force_review: bool,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    if !force_review && !review_enabled(context) {
        return Ok(TaskExecutionOutcome::Deferred);
    }

    let run_dir = store.run_dir(run_id)?;
    let output_path = review_output_path(&run_dir, &prepared.task.id);
    let prompt = render_review_task_prompt(
        context,
        store,
        run_id,
        &prepared.task,
        &prepared.state_before_running,
        &output_path,
    )?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(format!("{}.review.md", prepared.task.id)),
        stdout_log_path: run_dir
            .join("logs")
            .join(format!("{}.review.stdout.log", prepared.task.id)),
        stderr_log_path: run_dir
            .join("logs")
            .join(format!("{}.review.stderr.log", prepared.task.id)),
        last_message_path: run_dir
            .join("logs")
            .join(format!("{}.review.last-message.md", prepared.task.id)),
        required_output_path: Some(output_path.clone()),
        sandbox: context.merged.runner.review_sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: prepared
            .task
            .review_timeout_seconds
            .unwrap_or(context.merged.runner.default_review_timeout_seconds),
    };
    let result = build_executor(context, codex_bin).execute(&request);

    match result {
        Ok(output) => {
            finish_review_codex_success(store, run_id, &prepared.task, &output_path, &output)
        }
        Err(err) => {
            let err = *err;
            finish_review_execution_failure(
                store,
                run_id,
                &prepared.task,
                err.exit_code.unwrap_or(1),
                err.message,
                Some(err.stderr_log_path.display().to_string()),
                None,
            )
        }
    }
}

fn review_enabled(context: &ConfigContext) -> bool {
    match context.merged.runner.review {
        Toggle::True => true,
        Toggle::False => false,
        Toggle::Auto => context.merged.runner.sandbox != "read-only",
    }
}

fn finish_review_codex_success(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    output_path: &Path,
    output: &CodexRunOutput,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let raw = fs::read_to_string(output_path).map_err(|err| {
        AppError::Io(format!(
            "failed to read review output {}: {err}",
            output_path.display()
        ))
    })?;
    match parse_task_review_output(&raw, &task.id) {
        Ok(ReviewVerdict::Approved) => {
            finish_review_approved(store, run_id, task, output_path, output)
        }
        Ok(ReviewVerdict::ChangesRequested) => {
            let comments = extract_must_review_comments(&raw);
            finish_review_changes_requested(store, run_id, task, output_path, output, comments)
        }
        Err(err) => finish_review_execution_failure(
            store,
            run_id,
            task,
            output.exit_code,
            err,
            Some(output_path.display().to_string()),
            Some(output_path.display().to_string()),
        ),
    }
}

fn finish_review_approved(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    output_path: &Path,
    output: &CodexRunOutput,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.last_exit_code = Some(output.exit_code);
        task_state.last_error = None;
        task_state.last_log = None;
        task_state.review_output = Some(output_path.display().to_string());
        task_state.last_verdict = Some(ReviewVerdict::Approved);
        task_state.last_review_comments = None;
        task_state.status = TaskStatus::Reviewed;
        task_state.phase = Some(TaskPhase::Commit);
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        Ok(())
    })?;
    Ok(TaskExecutionOutcome::CompletedThroughReview)
}

fn finish_review_changes_requested(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    output_path: &Path,
    output: &CodexRunOutput,
    comments: String,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let blocked = store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.review_attempts += 1;
        task_state.last_exit_code = Some(output.exit_code);
        task_state.last_error = None;
        task_state.last_log = None;
        task_state.review_output = Some(output_path.display().to_string());
        task_state.last_verdict = Some(ReviewVerdict::ChangesRequested);
        task_state.last_review_comments = Some(comments);
        task_state.phase = Some(TaskPhase::Implement);
        if task_state.review_attempts >= task_max_review_attempts(task) {
            task_state.status = TaskStatus::Blocked;
        } else {
            task_state.status = TaskStatus::Pending;
        }
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        Ok(task_state.status == TaskStatus::Blocked)
    })?;

    if blocked {
        Ok(TaskExecutionOutcome::Blocked)
    } else {
        Ok(TaskExecutionOutcome::ReviewChangesRequested)
    }
}

fn finish_review_execution_failure(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    exit_code: i32,
    message: String,
    log_path: Option<String>,
    review_output: Option<String>,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let blocked = store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.review_attempts += 1;
        task_state.last_exit_code = Some(exit_code);
        task_state.last_error = Some(message);
        task_state.last_log = log_path;
        task_state.review_output = review_output;
        task_state.last_verdict = None;
        task_state.status = if task_state.review_attempts >= task_max_review_attempts(task) {
            TaskStatus::Blocked
        } else {
            TaskStatus::ReviewFailed
        };
        task_state.phase = Some(TaskPhase::Review);
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        Ok(task_state.status == TaskStatus::Blocked)
    })?;

    if blocked {
        Ok(TaskExecutionOutcome::Blocked)
    } else {
        Ok(TaskExecutionOutcome::FailedRetryable)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitStatusEntry {
    path: String,
    unstaged: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CommitPlan {
    preview_files: Vec<String>,
    files_to_stage: Vec<String>,
}

fn execute_commit_phase(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    prepared: PreparedPhase,
) -> std::result::Result<TaskExecutionOutcome, AppError> {
    let run_dir = store.run_dir(run_id)?;

    if !context.merged.git.commit {
        append_event_log(
            &run_dir,
            &format!("commit skipped for {}: git.commit=false", prepared.task.id),
        )?;
        finish_commit_done(
            store,
            run_id,
            &prepared.task,
            None,
            Vec::new(),
            "git.commit=false",
        )?;
        return Ok(TaskExecutionOutcome::CompletedThroughCommit);
    }

    match run_git_commit(
        context,
        &run_dir,
        run_id,
        &prepared.task,
        prepared.state_before_running.last_verdict,
    ) {
        Ok(commit) => {
            finish_commit_done(
                store,
                run_id,
                &prepared.task,
                commit.hash,
                commit.files,
                commit.reason.as_deref().unwrap_or("committed"),
            )?;
            Ok(TaskExecutionOutcome::CompletedThroughCommit)
        }
        Err(err) => {
            finish_commit_failure(store, run_id, &prepared.task, err.to_string())?;
            Ok(TaskExecutionOutcome::Blocked)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitCommitOutcome {
    hash: Option<String>,
    files: Vec<String>,
    reason: Option<String>,
}

fn run_git_commit(
    context: &ConfigContext,
    run_dir: &Path,
    run_id: &str,
    task: &Task,
    verdict: Option<ReviewVerdict>,
) -> std::result::Result<GitCommitOutcome, AppError> {
    let plan = build_commit_plan(context)?;
    let preview = format_file_list(&plan.preview_files);
    append_event_log(
        run_dir,
        &format!(
            "commit preview before staging for {}:\n{}",
            task.id, preview
        ),
    )?;
    println!("Commit preview for {}:\n{}", task.id, preview);

    if !plan.files_to_stage.is_empty() {
        git_add_paths(&context.repo_root, &plan.files_to_stage)?;
    }

    let commit_files = git_staged_files(&context.repo_root)?;
    if commit_files.is_empty() {
        append_event_log(
            run_dir,
            &format!("commit skipped for {}: empty diff", task.id),
        )?;
        return Ok(GitCommitOutcome {
            hash: None,
            files: Vec::new(),
            reason: Some("empty-diff".to_string()),
        });
    }

    append_event_log(
        run_dir,
        &format!(
            "commit files after staging for {}:\n{}",
            task.id,
            format_file_list(&commit_files)
        ),
    )?;
    let subject = render_commit_subject(&context.merged.git.commit_message, run_id, task);
    let body = render_commit_body(&commit_files, verdict);
    git_commit(&context.repo_root, &subject, &body)?;
    let hash = git_rev_parse_short_head(&context.repo_root)?;
    append_event_log(
        run_dir,
        &format!(
            "committed {} as {} with subject {:?}",
            task.id,
            hash.as_deref().unwrap_or("(unknown)"),
            subject
        ),
    )?;

    Ok(GitCommitOutcome {
        hash,
        files: commit_files,
        reason: None,
    })
}

fn build_commit_plan(context: &ConfigContext) -> std::result::Result<CommitPlan, AppError> {
    let status = git_status_entries(&context.repo_root)?;
    let dirty_unstaged = status.iter().any(|entry| entry.unstaged);
    let staged_before = git_staged_files(&context.repo_root)?;

    let disallowed_staged = staged_before
        .iter()
        .filter(|path| !staged_path_allowed_by_git_scope(&context.merged.git, path))
        .cloned()
        .collect::<Vec<_>>();
    if !disallowed_staged.is_empty() {
        return Err(AppError::DirtyWorktree(format!(
            "refusing to commit staged file(s) outside configured git add scope: {}",
            disallowed_staged.join(", ")
        )));
    }

    if context.merged.git.add_include.is_empty() {
        if context.merged.git.add_required && (dirty_unstaged || !staged_before.is_empty()) {
            return Err(AppError::Config(
                "git.commit=true requires explicit git.add_include before automatic commit"
                    .to_string(),
            ));
        }
        return Ok(CommitPlan {
            preview_files: staged_before,
            files_to_stage: Vec::new(),
        });
    }

    let mut files_to_stage = BTreeSet::new();
    for entry in &status {
        if path_allowed_by_git_scope(&context.merged.git, &entry.path) {
            files_to_stage.insert(entry.path.clone());
        }
    }

    let mut preview = staged_before.into_iter().collect::<BTreeSet<_>>();
    preview.extend(files_to_stage.iter().cloned());

    Ok(CommitPlan {
        preview_files: preview.into_iter().collect(),
        files_to_stage: files_to_stage.into_iter().collect(),
    })
}

fn path_allowed_by_git_scope(config: &GitConfig, path: &str) -> bool {
    staged_path_allowed_by_git_scope(config, path) && path_matches_any(&config.add_include, path)
}

fn staged_path_allowed_by_git_scope(config: &GitConfig, path: &str) -> bool {
    !is_tool_run_store_path(path)
        && !path_matches_any(&config.add_exclude, path)
        && (config.add_include.is_empty() || path_matches_any(&config.add_include, path))
}

fn is_tool_run_store_path(path: &str) -> bool {
    path == ".codex/task-runner" || path.starts_with(".codex/task-runner/")
}

fn path_matches_any(patterns: &[String], path: &str) -> bool {
    patterns.iter().any(|pattern| glob_match(pattern, path))
}

fn glob_match(pattern: &str, path: &str) -> bool {
    fn matches(pattern: &[u8], path: &[u8]) -> bool {
        if pattern.is_empty() {
            return path.is_empty();
        }

        if pattern.starts_with(b"**") {
            let rest = &pattern[2..];
            if matches(rest, path) {
                return true;
            }
            return !path.is_empty() && matches(pattern, &path[1..]);
        }

        match pattern[0] {
            b'*' => {
                if matches(&pattern[1..], path) {
                    return true;
                }
                !path.is_empty() && path[0] != b'/' && matches(pattern, &path[1..])
            }
            b'?' => !path.is_empty() && path[0] != b'/' && matches(&pattern[1..], &path[1..]),
            byte => !path.is_empty() && byte == path[0] && matches(&pattern[1..], &path[1..]),
        }
    }

    matches(pattern.as_bytes(), path.as_bytes())
}

fn git_status_entries(repo_root: &Path) -> std::result::Result<Vec<GitStatusEntry>, AppError> {
    let output = git_output(
        repo_root,
        &[
            "-c",
            "core.quotePath=false",
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
        ],
    )?;
    let mut entries = Vec::new();
    for line in output.lines() {
        if line.len() < 4 {
            continue;
        }
        let status = &line[..2];
        let raw_path = &line[3..];
        let path = raw_path
            .rsplit_once(" -> ")
            .map(|(_, new_path)| new_path)
            .unwrap_or(raw_path)
            .to_string();
        if path.trim().is_empty() {
            continue;
        }
        entries.push(GitStatusEntry {
            path,
            unstaged: status.as_bytes()[1] != b' ' || status == "??",
        });
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries.dedup_by(|left, right| left.path == right.path);
    Ok(entries)
}

fn git_staged_files(repo_root: &Path) -> std::result::Result<Vec<String>, AppError> {
    let output = git_output(repo_root, &["diff", "--cached", "--name-only", "--"])?;
    Ok(sorted_non_empty_lines(&output))
}

fn sorted_non_empty_lines(output: &str) -> Vec<String> {
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    lines.sort();
    lines.dedup();
    lines
}

fn git_add_paths(repo_root: &Path, paths: &[String]) -> std::result::Result<(), AppError> {
    if paths.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("git");
    command.arg("-C").arg(repo_root).arg("add").arg("--");
    for path in paths {
        command.arg(path);
    }
    let output = command
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git add: {err}")))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(AppError::Runtime(format_git_error("git add", &output)))
    }
}

fn git_commit(repo_root: &Path, subject: &str, body: &str) -> std::result::Result<(), AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["commit", "-m"])
        .arg(subject)
        .arg("-m")
        .arg(body)
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git commit: {err}")))?;
    if output.status.success() {
        Ok(())
    } else {
        let message = format_git_error("git commit", &output);
        if message.contains("nothing to commit") || message.contains("no changes added") {
            Ok(())
        } else {
            Err(AppError::Runtime(message))
        }
    }
}

fn git_rev_parse_short_head(repo_root: &Path) -> std::result::Result<Option<String>, AppError> {
    let output = git_output(repo_root, &["rev-parse", "--short", "HEAD"])?;
    let hash = output.trim();
    Ok((!hash.is_empty()).then(|| hash.to_string()))
}

fn git_output(repo_root: &Path, args: &[&str]) -> std::result::Result<String, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(AppError::Runtime(format_git_error("git", &output)))
    }
}

fn render_commit_subject(template: &str, run_id: &str, task: &Task) -> String {
    let rendered = template
        .replace("{task_id}", &task.id)
        .replace("{title}", &task.title)
        .replace("{run_id}", run_id);
    if rendered.trim().is_empty() {
        format!("{}: {}", task.id, task.title)
    } else {
        rendered
    }
}

fn render_commit_body(files: &[String], verdict: Option<ReviewVerdict>) -> String {
    let review = verdict.map(ReviewVerdict::as_str).unwrap_or("APPROVED");
    format!(
        "Changed files:\n{}\n\nReview: {review}",
        format_file_list(files)
    )
}

fn format_file_list(files: &[String]) -> String {
    if files.is_empty() {
        "(none)".to_string()
    } else {
        files
            .iter()
            .map(|path| format!("- {path}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn finish_commit_done(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    commit_hash: Option<String>,
    files: Vec<String>,
    reason: &str,
) -> std::result::Result<(), AppError> {
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.status = TaskStatus::Done;
        task_state.phase = Some(TaskPhase::Done);
        task_state.last_exit_code = Some(0);
        task_state.last_error = None;
        task_state.last_log = None;
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        if let Some(commit_hash) = commit_hash {
            task_state
                .extra
                .insert("gitCommit".to_string(), Value::String(commit_hash));
        } else {
            task_state.extra.remove("gitCommit");
        }
        task_state.extra.insert(
            "gitCommitReason".to_string(),
            Value::String(reason.to_string()),
        );
        task_state.extra.insert(
            "gitCommitFiles".to_string(),
            Value::Array(files.into_iter().map(Value::String).collect()),
        );
        Ok(())
    })
}

fn finish_commit_failure(
    store: &RunStore,
    run_id: &str,
    task: &Task,
    message: String,
) -> std::result::Result<(), AppError> {
    store.update_run_state(run_id, |state| {
        let task_state = find_task_state_mut(state, &task.id)?;
        clear_runner_marker(task_state);
        task_state.status = TaskStatus::Blocked;
        task_state.phase = Some(TaskPhase::Commit);
        task_state.last_exit_code = Some(1);
        task_state.last_error = Some(message);
        task_state.finished_at = Some(current_timestamp()?);
        task_state.updated_at = task_state.finished_at.clone();
        Ok(())
    })
}

fn render_analyze_prompt(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task: &Task,
    output_path: &Path,
) -> std::result::Result<String, AppError> {
    let task_file = store.read_task_file(run_id)?;
    let spec_file = task
        .spec_file
        .clone()
        .unwrap_or_else(|| task_file.spec_file.clone());
    let spec = SpecDocument::read(&context.repo_root.join(&spec_file))?;
    let input = AnalyzeTaskPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        task_id: task.id.clone(),
        title: task.title.clone(),
        task_prompt: task.prompt.clone(),
        task_json: serde_json::to_string_pretty(task)
            .map_err(|err| AppError::Runtime(format!("failed to encode task JSON: {err}")))?,
        spec_file,
        feature_spec: spec.body,
        output_analysis_path: output_path.display().to_string(),
    };
    let template = load_prompt_template(context, PromptTemplateKind::AnalyzeTask)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

fn render_implement_prompt(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task: &Task,
    state: &TaskState,
) -> std::result::Result<String, AppError> {
    let task_file = store.read_task_file(run_id)?;
    let spec_file = task
        .spec_file
        .clone()
        .unwrap_or_else(|| task_file.spec_file.clone());
    let spec = SpecDocument::read(&context.repo_root.join(&spec_file))?;
    let analysis_output = state
        .analysis_output
        .as_deref()
        .and_then(read_optional_file)
        .unwrap_or_default();
    let last_log_tail = state
        .last_log
        .as_deref()
        .and_then(|path| read_file_tail(Path::new(path), 6000).ok())
        .unwrap_or_default();
    let input = ImplementTaskPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        task_id: task.id.clone(),
        title: task.title.clone(),
        task_prompt: task.prompt.clone(),
        task_json: serde_json::to_string_pretty(task)
            .map_err(|err| AppError::Runtime(format!("failed to encode task JSON: {err}")))?,
        spec_file,
        feature_spec: spec.body,
        analysis_output,
        last_review_comments: state.last_review_comments.clone().unwrap_or_default(),
        last_error: state.last_error.clone().unwrap_or_default(),
        last_log_tail,
    };
    let template = load_prompt_template(context, PromptTemplateKind::ImplementTask)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

fn render_review_task_prompt(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task: &Task,
    state: &TaskState,
    output_path: &Path,
) -> std::result::Result<String, AppError> {
    let task_file = store.read_task_file(run_id)?;
    let spec_file = task
        .spec_file
        .clone()
        .unwrap_or_else(|| task_file.spec_file.clone());
    let spec = SpecDocument::read(&context.repo_root.join(&spec_file))?;
    let run_dir = store.run_dir(run_id)?;
    let analysis_path = state.analysis_output.clone().unwrap_or_else(|| {
        analysis_output_path(&run_dir, &task.id)
            .display()
            .to_string()
    });
    let implementation_path = state.output.clone().unwrap_or_else(|| {
        implementation_output_path(&run_dir, &task.id)
            .display()
            .to_string()
    });
    let analysis_output = read_optional_file(&analysis_path).unwrap_or_default();
    let implementation_summary = read_optional_file(&implementation_path).unwrap_or_default();
    let review_criteria = if task.review_criteria.is_empty() {
        "(none)".to_string()
    } else {
        task.review_criteria
            .iter()
            .map(|criterion| format!("- {criterion}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let input = ReviewTaskPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        task_id: task.id.clone(),
        title: task.title.clone(),
        task_prompt: task.prompt.clone(),
        review_criteria,
        git_diff: git_diff(&context.repo_root)?,
        spec_file,
        feature_spec: spec.body,
        output_analysis_path: analysis_path,
        output_impl_path: implementation_path,
        output_review_path: output_path.display().to_string(),
        analysis_output,
        implementation_summary,
    };
    let template = load_prompt_template(context, PromptTemplateKind::ReviewTask)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

fn render_review_feature_prompt(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
    output_path: &Path,
) -> std::result::Result<String, AppError> {
    let spec = SpecDocument::read(&context.repo_root.join(&task_file.spec_file))?;
    let state = store.read_run_state(run_id)?;
    let input = ReviewFeaturePromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        run_id: task_file.run_id.clone(),
        branch: task_file.branch.clone(),
        spec_file: task_file.spec_file.clone(),
        feature_spec: spec.body,
        git_diff: feature_branch_diff(&context.repo_root, &context.merged.project.default_branch)?,
        tasks_summaries: task_summaries(task_file, &state),
        output_feature_review_path: output_path.display().to_string(),
    };
    let template = load_prompt_template(context, PromptTemplateKind::ReviewFeature)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

fn common_prompt_variables(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
) -> std::result::Result<CommonPromptVariables, AppError> {
    let run_dir = store.run_dir(run_id)?;
    Ok(CommonPromptVariables {
        date: current_date()?,
        repo_root: context.repo_root.display().to_string(),
        runner_dir: run_dir.display().to_string(),
        runner_dir_rel: run_dir.display().to_string(),
        task_file: store.tasks_path(run_id)?.display().to_string(),
        state_file: store.state_path(run_id)?.display().to_string(),
        repo_map: build_repo_map(&context.repo_root)?,
        agent_rules_path: context.merged.project.agent_rules.clone(),
        overview_doc: context
            .merged
            .project
            .overview_doc
            .clone()
            .unwrap_or_default(),
    })
}

fn build_executor(context: &ConfigContext, codex_bin: Option<PathBuf>) -> CodexExecutor {
    let mut config = CodexExecutorConfig::from_context(context);
    if let Some(codex_bin) = codex_bin {
        config.codex_bin = codex_bin;
    }
    CodexExecutor::new(config)
}

fn force_task_phase(
    store: &RunStore,
    run_id: &str,
    task_id: &str,
    phase: TaskPhase,
) -> std::result::Result<(), AppError> {
    let task_file = store.read_task_file(run_id)?;
    find_task(&task_file, task_id)?;
    store.update_run_state(run_id, |state| {
        ensure_state_matches_tasks(&task_file, state)?;
        let task_state = find_task_state_mut(state, task_id)?;
        if task_state.status == TaskStatus::AnalysisReview {
            return Err(AppError::Runtime(format!(
                "task {task_id} is waiting for analysis approval"
            )));
        }
        if matches!(
            task_state.status,
            TaskStatus::Blocked | TaskStatus::Ignored | TaskStatus::Done | TaskStatus::ReviewFailed
        ) {
            return Err(AppError::Runtime(format!(
                "task {task_id} cannot be forced from status {}",
                task_state.status.as_str()
            )));
        }
        task_state.status = TaskStatus::Pending;
        task_state.phase = Some(phase);
        task_state.updated_at = Some(current_timestamp()?);
        Ok(())
    })
}

fn prepare_task_for_explicit_review(
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
    task_id: &str,
) -> std::result::Result<(), AppError> {
    store.update_run_state(run_id, |state| {
        ensure_state_matches_tasks(task_file, state)?;
        let task_state = find_task_state_mut(state, task_id)?;
        match (task_state.status, task_state.phase) {
            (TaskStatus::Pending, Some(TaskPhase::Review))
            | (TaskStatus::ReviewFailed, Some(TaskPhase::Review)) => {
                task_state.status = TaskStatus::Pending;
                task_state.phase = Some(TaskPhase::Review);
                task_state.updated_at = Some(current_timestamp()?);
                Ok(())
            }
            (TaskStatus::Blocked, Some(TaskPhase::Review)) => Err(AppError::Runtime(format!(
                "task {task_id} review is blocked"
            ))),
            _ => Err(AppError::Runtime(format!(
                "task {task_id} is not waiting for review"
            ))),
        }
    })
}

fn ensure_all_tasks_complete_for_finalize(
    task_file: &TaskFile,
    state: &RunState,
) -> std::result::Result<(), AppError> {
    let state_by_id = normalized_state_map(task_file, state)?;
    let incomplete = task_file
        .tasks
        .iter()
        .filter_map(|task| {
            let status = state_by_id.get(task.id.as_str())?.status;
            (!matches!(status, TaskStatus::Done | TaskStatus::Ignored))
                .then(|| format!("{}:{}", task.id, status.as_str()))
        })
        .collect::<Vec<_>>();
    if incomplete.is_empty() {
        Ok(())
    } else {
        Err(AppError::Runtime(format!(
            "cannot finalize; unfinished task(s): {}",
            incomplete.join(", ")
        )))
    }
}

fn finalize_approved_run(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
    no_cleanup: bool,
) -> std::result::Result<(), AppError> {
    let finished_at = current_timestamp()?;
    let spec_path = context.repo_root.join(&task_file.spec_file);
    let mut spec = SpecDocument::read(&spec_path)?;
    if spec.set_finalized_metadata(&finished_at) {
        spec.write(&spec_path)?;
    }

    let run_dir = store.run_dir(run_id)?;
    append_event_log(
        &run_dir,
        &format!("finalized run {run_id} at {finished_at}; no_cleanup={no_cleanup}"),
    )?;
    if !no_cleanup {
        archive_run_dir(store, run_id, &finished_at)?;
    }
    Ok(())
}

fn archive_run_dir(
    store: &RunStore,
    run_id: &str,
    finished_at: &str,
) -> std::result::Result<PathBuf, AppError> {
    let run_dir = store.run_dir(run_id)?;
    if !run_dir.exists() {
        return Ok(run_dir);
    }

    let archive_root = store.repo_runs_dir.join("archive");
    fs::create_dir_all(&archive_root).map_err(|err| {
        AppError::Io(format!(
            "failed to create archive directory {}: {err}",
            archive_root.display()
        ))
    })?;
    let stamp = finished_at
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    let mut target = archive_root.join(format!("{run_id}-{stamp}"));
    let mut suffix = 1_u64;
    while target.exists() {
        target = archive_root.join(format!("{run_id}-{stamp}-{suffix}"));
        suffix += 1;
    }
    fs::rename(&run_dir, &target).map_err(|err| {
        AppError::Io(format!(
            "failed to archive run {} to {}: {err}",
            run_dir.display(),
            target.display()
        ))
    })?;
    Ok(target)
}

fn finish_feature_review(
    store: &RunStore,
    run_id: &str,
    status: FeatureReviewStatus,
    error: Option<String>,
    log_path: Option<String>,
) -> std::result::Result<(), AppError> {
    store.update_run_state(run_id, |state| {
        state.feature_review_status = status;
        if let Some(error) = error {
            state
                .extra
                .insert("featureReviewLastError".to_string(), Value::String(error));
        } else {
            state.extra.remove("featureReviewLastError");
        }
        if let Some(log_path) = log_path {
            state
                .extra
                .insert("featureReviewLastLog".to_string(), Value::String(log_path));
        } else {
            state.extra.remove("featureReviewLastLog");
        }
        Ok(())
    })
}

fn enforce_dirty_worktree_policy(
    context: &ConfigContext,
    state: &TaskState,
) -> std::result::Result<(), AppError> {
    let require_clean = match context.merged.runner.require_clean {
        Toggle::True => true,
        Toggle::False => false,
        Toggle::Auto => context.merged.git.commit,
    };
    if !require_clean || !git_worktree_dirty(&context.repo_root)? {
        return Ok(());
    }
    if context.merged.runner.allow_dirty_resume && state.attempts > 0 {
        return Ok(());
    }
    Err(AppError::DirtyWorktree(
        "dirty worktree policy blocks automatic task execution".to_string(),
    ))
}

fn git_worktree_dirty(repo_root: &Path) -> std::result::Result<bool, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format_git_error(
            "git status --porcelain",
            &output,
        )));
    }
    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

fn find_task<'a>(
    task_file: &'a TaskFile,
    task_id: &str,
) -> std::result::Result<&'a Task, AppError> {
    task_file
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| AppError::Config(format!("unknown task id {task_id}")))
}

fn find_task_state<'a>(
    state: &'a RunState,
    task_id: &str,
) -> std::result::Result<&'a TaskState, AppError> {
    state
        .tasks
        .iter()
        .find(|task| task.id == task_id)
        .ok_or_else(|| AppError::Config(format!("missing state for task {task_id}")))
}

fn find_task_state_mut<'a>(
    state: &'a mut RunState,
    task_id: &str,
) -> std::result::Result<&'a mut TaskState, AppError> {
    state
        .tasks
        .iter_mut()
        .find(|task| task.id == task_id)
        .ok_or_else(|| AppError::Config(format!("missing state for task {task_id}")))
}

fn ensure_state_matches_tasks(
    task_file: &TaskFile,
    state: &mut RunState,
) -> std::result::Result<(), AppError> {
    let task_ids: BTreeSet<String> = task_file.tasks.iter().map(|task| task.id.clone()).collect();
    for task_state in &state.tasks {
        if !task_ids.contains(&task_state.id) {
            return Err(AppError::Config(format!(
                "state.json references unknown task {}",
                task_state.id
            )));
        }
    }
    for task in &task_file.tasks {
        if state
            .tasks
            .iter()
            .all(|task_state| task_state.id != task.id)
        {
            state.tasks.push(default_task_state(&task.id));
        }
    }
    Ok(())
}

fn normalized_state_map<'a>(
    task_file: &'a TaskFile,
    state: &'a RunState,
) -> std::result::Result<HashMap<&'a str, TaskState>, AppError> {
    let task_ids: BTreeSet<&str> = task_file
        .tasks
        .iter()
        .map(|task| task.id.as_str())
        .collect();
    let mut out = HashMap::new();
    for task_state in &state.tasks {
        if !task_ids.contains(task_state.id.as_str()) {
            return Err(AppError::Config(format!(
                "state.json references unknown task {}",
                task_state.id
            )));
        }
        out.insert(task_state.id.as_str(), task_state.clone());
    }
    for task in &task_file.tasks {
        out.entry(task.id.as_str())
            .or_insert_with(|| default_task_state(&task.id));
    }
    Ok(out)
}

fn default_task_state(task_id: &str) -> TaskState {
    TaskState {
        id: task_id.to_string(),
        status: TaskStatus::Pending,
        phase: None,
        attempts: 0,
        review_attempts: 0,
        started_at: None,
        finished_at: None,
        updated_at: None,
        approved_at: None,
        ignored_at: None,
        ignore_reason: None,
        output: None,
        analysis_output: None,
        review_output: None,
        last_exit_code: None,
        last_error: None,
        last_log: None,
        last_verdict: None,
        last_review_comments: None,
        extra: Map::new(),
    }
}

fn task_max_attempts(task: &Task) -> u64 {
    task.max_attempts.unwrap_or(3).max(1)
}

fn task_max_review_attempts(task: &Task) -> u64 {
    task.max_review_attempts.max(1)
}

fn set_runner_marker(state: &mut TaskState, phase: TaskPhase, started_at: &str) {
    state.extra.insert(
        "runnerPid".to_string(),
        Value::Number(serde_json::Number::from(std::process::id())),
    );
    state.extra.insert(
        "runnerPhase".to_string(),
        Value::String(phase.as_str().to_string()),
    );
    state.extra.insert(
        "runnerStartedAt".to_string(),
        Value::String(started_at.to_string()),
    );
}

fn clear_runner_marker(state: &mut TaskState) {
    state.extra.remove("runnerPid");
    state.extra.remove("runnerPhase");
    state.extra.remove("runnerStartedAt");
}

fn task_runner_pid(state: &TaskState) -> Option<u32> {
    state
        .extra
        .get("runnerPid")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn analysis_output_path(run_dir: &Path, task_id: &str) -> PathBuf {
    run_dir
        .join("output")
        .join(format!("{task_id}.analysis.md"))
}

fn implementation_output_path(run_dir: &Path, task_id: &str) -> PathBuf {
    run_dir.join("output").join(format!("{task_id}.md"))
}

fn review_output_path(run_dir: &Path, task_id: &str) -> PathBuf {
    run_dir.join("output").join(format!("{task_id}.review.md"))
}

fn feature_review_output_path(run_dir: &Path, attempt: u64) -> PathBuf {
    run_dir
        .join("output")
        .join(format!("feature-review.{attempt}.md"))
}

fn read_optional_file(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn read_file_tail(path: &Path, max_bytes: usize) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let start = bytes.len().saturating_sub(max_bytes);
    Ok(String::from_utf8_lossy(&bytes[start..]).to_string())
}

fn parse_task_review_output(
    raw: &str,
    task_id: &str,
) -> std::result::Result<ReviewVerdict, String> {
    let (frontmatter, _) = parse_review_frontmatter(raw)?;
    match frontmatter.get("task_id").as_deref() {
        Some(value) if value == task_id => {}
        Some(value) => {
            return Err(format!(
                "review output task_id={value} does not match expected {task_id}"
            ));
        }
        None => return Err("review output missing task_id".to_string()),
    }
    match frontmatter.get("phase").as_deref() {
        Some("review") => {}
        Some(value) => return Err(format!("review output phase={value} is invalid")),
        None => return Err("review output missing phase".to_string()),
    }
    let verdict = parse_review_verdict(&frontmatter)?;
    require_reviewed_at(&frontmatter)?;
    Ok(verdict)
}

fn parse_final_review_output_file(path: &Path) -> std::result::Result<ReviewVerdict, String> {
    let raw = fs::read_to_string(path).map_err(|err| {
        format!(
            "failed to read final review output {}: {err}",
            path.display()
        )
    })?;
    let (frontmatter, _) = parse_review_frontmatter(&raw)?;
    parse_review_verdict(&frontmatter)
}

fn parse_review_frontmatter(raw: &str) -> std::result::Result<(FrontMatter, String), String> {
    if raw.trim().is_empty() {
        return Err("review output is empty".to_string());
    }
    let document = parse_spec_document(raw).map_err(|err| err.to_string())?;
    let Some(frontmatter) = document.frontmatter else {
        return Err("review output missing frontmatter".to_string());
    };
    Ok((frontmatter, document.body))
}

fn parse_review_verdict(frontmatter: &FrontMatter) -> std::result::Result<ReviewVerdict, String> {
    match frontmatter.get("verdict").as_deref() {
        Some("APPROVED") => Ok(ReviewVerdict::Approved),
        Some("CHANGES_REQUESTED") => Ok(ReviewVerdict::ChangesRequested),
        Some(value) => Err(format!("review output verdict={value} is invalid")),
        None => Err("review output missing verdict".to_string()),
    }
}

fn require_reviewed_at(frontmatter: &FrontMatter) -> std::result::Result<(), String> {
    match frontmatter.get("reviewed_at").as_deref() {
        Some(value) if is_rfc3339_timestamp(value) => Ok(()),
        Some(value) => Err(format!("review output reviewed_at={value} is invalid")),
        None => Err("review output missing reviewed_at".to_string()),
    }
}

fn is_rfc3339_timestamp(value: &str) -> bool {
    let Some((date, time)) = value.split_once('T') else {
        return false;
    };
    if !is_rfc3339_date(date) {
        return false;
    }
    is_rfc3339_time(time)
}

fn is_rfc3339_date(value: &str) -> bool {
    if value.len() != 10 {
        return false;
    }
    let bytes = value.as_bytes();
    bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[8..10].iter().all(u8::is_ascii_digit)
        && parse_u8(&value[5..7]).is_some_and(|month| (1..=12).contains(&month))
        && parse_u8(&value[8..10]).is_some_and(|day| (1..=31).contains(&day))
}

fn is_rfc3339_time(value: &str) -> bool {
    if value.len() < 9 {
        return false;
    }
    let bytes = value.as_bytes();
    if bytes[2] != b':'
        || bytes[5] != b':'
        || !bytes[..2].iter().all(u8::is_ascii_digit)
        || !bytes[3..5].iter().all(u8::is_ascii_digit)
        || !bytes[6..8].iter().all(u8::is_ascii_digit)
    {
        return false;
    }
    if parse_u8(&value[..2]).is_none_or(|hour| hour > 23)
        || parse_u8(&value[3..5]).is_none_or(|minute| minute > 59)
        || parse_u8(&value[6..8]).is_none_or(|second| second > 60)
    {
        return false;
    }

    let rest = &value[8..];
    if rest == "Z" {
        return true;
    }
    let timezone = if let Some(fraction) = rest.strip_prefix('.') {
        let digit_count = fraction
            .as_bytes()
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digit_count == 0 {
            return false;
        }
        &fraction[digit_count..]
    } else {
        rest
    };
    is_rfc3339_timezone(timezone)
}

fn is_rfc3339_timezone(value: &str) -> bool {
    if value.len() != 6 {
        return false;
    }
    let bytes = value.as_bytes();
    matches!(bytes[0], b'+' | b'-')
        && bytes[3] == b':'
        && bytes[1..3].iter().all(u8::is_ascii_digit)
        && bytes[4..6].iter().all(u8::is_ascii_digit)
        && parse_u8(&value[1..3]).is_some_and(|hour| hour <= 23)
        && parse_u8(&value[4..6]).is_some_and(|minute| minute <= 59)
}

fn parse_u8(value: &str) -> Option<u8> {
    value.parse::<u8>().ok()
}

fn extract_must_review_comments(raw: &str) -> String {
    let (_, body) =
        parse_review_frontmatter(raw).unwrap_or_else(|_| (FrontMatter::new(), raw.to_string()));
    let sections = extract_must_heading_sections(&body);
    if !sections.is_empty() {
        return sections.join("\n\n");
    }
    let comments = body
        .lines()
        .filter(|line| line.contains("[MUST]"))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if comments.is_empty() {
        body.trim().to_string()
    } else {
        comments.join("\n")
    }
}

fn extract_must_heading_sections(body: &str) -> Vec<String> {
    let lines = body.lines().collect::<Vec<_>>();
    let mut sections = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some(level) = markdown_heading_level(lines[index]) else {
            index += 1;
            continue;
        };
        if !lines[index].contains("[MUST]") {
            index += 1;
            continue;
        }

        let start = index;
        index += 1;
        while index < lines.len() {
            if markdown_heading_level(lines[index]).is_some_and(|next_level| next_level <= level) {
                break;
            }
            index += 1;
        }
        let section = lines[start..index].join("\n").trim().to_string();
        if !section.is_empty() {
            sections.push(section);
        }
    }
    sections
}

fn markdown_heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let level = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    trimmed
        .as_bytes()
        .get(level)
        .is_some_and(u8::is_ascii_whitespace)
        .then_some(level)
}

fn git_diff(repo_root: &Path) -> std::result::Result<String, AppError> {
    let staged = git_diff_command(repo_root, ["diff", "--cached", "--"])?;
    let unstaged = git_diff_command(repo_root, ["diff", "--"])?;
    let mut sections = Vec::new();
    if !staged.trim().is_empty() {
        sections.push(format!("## Staged\n{staged}"));
    }
    if !unstaged.trim().is_empty() {
        sections.push(format!("## Unstaged\n{unstaged}"));
    }
    if sections.is_empty() {
        Ok("(no git diff)".to_string())
    } else {
        Ok(sections.join("\n"))
    }
}

fn feature_branch_diff(
    repo_root: &Path,
    default_branch: &str,
) -> std::result::Result<String, AppError> {
    let mut sections = Vec::new();
    if git_branch_exists(repo_root, default_branch)? {
        let range = format!("{default_branch}...HEAD");
        let committed = git_output(repo_root, &["diff", &range, "--"])?;
        if !committed.trim().is_empty() {
            sections.push(format!("## Feature branch ({range})\n{committed}"));
        }
    }

    let staged = git_diff_command(repo_root, ["diff", "--cached", "--"])?;
    let unstaged = git_diff_command(repo_root, ["diff", "--"])?;
    if !staged.trim().is_empty() {
        sections.push(format!("## Staged\n{staged}"));
    }
    if !unstaged.trim().is_empty() {
        sections.push(format!("## Unstaged\n{unstaged}"));
    }

    if sections.is_empty() {
        Ok("(no feature branch diff)".to_string())
    } else {
        Ok(sections.join("\n"))
    }
}

fn git_diff_command<const N: usize>(
    repo_root: &Path,
    args: [&str; N],
) -> std::result::Result<String, AppError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run git: {err}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format_git_error("git diff", &output)));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn task_summaries(task_file: &TaskFile, state: &RunState) -> String {
    let state_by_id = normalized_state_map(task_file, state).unwrap_or_default();
    let mut lines = Vec::new();
    for task in &task_file.tasks {
        let state = state_by_id.get(task.id.as_str());
        let status = state
            .map(|task_state| task_state.status.as_str())
            .unwrap_or("pending");
        let phase = state
            .and_then(|task_state| task_state.phase)
            .map(TaskPhase::as_str)
            .unwrap_or("-");
        let verdict = state
            .and_then(|task_state| task_state.last_verdict)
            .map(ReviewVerdict::as_str)
            .unwrap_or("-");
        lines.push(format!(
            "- {}: {} (status: {status}, phase: {phase}, verdict: {verdict})",
            task.id, task.title
        ));
    }
    lines.join("\n")
}

fn current_timestamp() -> std::result::Result<String, AppError> {
    let output = Command::new("date")
        .args(["-u", "+%Y-%m-%dT%H:%M:%SZ"])
        .output()
        .map_err(|err| AppError::Runtime(format!("failed to run date: {err}")))?;
    if !output.status.success() {
        return Err(AppError::Runtime(format_git_error(
            "date -u +%Y-%m-%dT%H:%M:%SZ",
            &output,
        )));
    }
    let timestamp = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if timestamp.is_empty() {
        Err(AppError::Runtime(
            "date returned empty timestamp".to_string(),
        ))
    } else {
        Ok(timestamp)
    }
}

fn discover_run_ids(repo_runs: &Path) -> std::result::Result<Vec<String>, AppError> {
    if !repo_runs.exists() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    let entries = fs::read_dir(repo_runs)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", repo_runs.display())))?;
    for entry in entries {
        let entry = entry.map_err(|err| {
            AppError::Io(format!(
                "failed to read entry in {}: {err}",
                repo_runs.display()
            ))
        })?;
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            let id = entry.file_name().to_string_lossy().to_string();
            if id != "archive" {
                ids.push(id);
            }
        }
    }
    ids.sort();
    Ok(ids)
}

pub fn read_task_file(path: &Path) -> std::result::Result<TaskFile, AppError> {
    Ok(read_task_file_with_migration(path)?.value)
}

pub fn read_task_file_with_migration(
    path: &Path,
) -> std::result::Result<Migrated<TaskFile>, AppError> {
    let raw = fs::read_to_string(path)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", path.display())))?;
    let raw_value = serde_json::from_str::<Value>(&raw)
        .map_err(|err| AppError::Config(format!("invalid tasks file {}: {err}", path.display())))?;
    let source_version = raw_value.get("version").and_then(Value::as_u64);
    let mut task_file = serde_json::from_value::<TaskFile>(raw_value)
        .map_err(|err| AppError::Config(format!("invalid tasks file {}: {err}", path.display())))?;
    validate_task_file(&task_file)?;

    let report = if source_version != Some(2) {
        task_file.schema_version = 2;
        Some(MigrationReport {
            migration_from: source_version,
            migration_to: 2,
        })
    } else {
        None
    };

    Ok(Migrated {
        value: task_file,
        report,
    })
}

pub fn read_run_state(path: &Path) -> std::result::Result<RunState, AppError> {
    let raw = fs::read_to_string(path)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", path.display())))?;
    let state = serde_json::from_str::<RunState>(&raw)
        .map_err(|err| AppError::Config(format!("invalid state file {}: {err}", path.display())))?;
    validate_run_state(&state)?;
    Ok(state)
}

pub fn read_run_metadata(path: &Path) -> std::result::Result<RunMetadata, AppError> {
    let raw = fs::read_to_string(path)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", path.display())))?;
    let metadata = serde_json::from_str::<RunMetadata>(&raw).map_err(|err| {
        AppError::Config(format!("invalid metadata file {}: {err}", path.display()))
    })?;
    validate_run_metadata(&metadata)?;
    Ok(metadata)
}

pub fn validate_task_file(task_file: &TaskFile) -> std::result::Result<(), AppError> {
    if !(1..=2).contains(&task_file.schema_version) {
        return Err(AppError::Config(format!(
            "unsupported tasks.json version {}",
            task_file.schema_version
        )));
    }
    if task_file.extra.contains_key("schema_version") {
        return Err(AppError::Config(
            "tasks.json must not contain both version and schema_version".to_string(),
        ));
    }
    if Path::new(&task_file.spec_file).is_absolute() {
        return Err(AppError::Config(format!(
            "specFile must be repo-relative: {}",
            task_file.spec_file
        )));
    }

    let mut ids = BTreeSet::new();
    for task in &task_file.tasks {
        if !ids.insert(task.id.clone()) {
            return Err(AppError::Config(format!("duplicate task id {}", task.id)));
        }
    }

    for task in &task_file.tasks {
        for dependency in &task.depends_on {
            if !ids.contains(dependency) {
                return Err(AppError::Config(format!(
                    "task {} depends on missing task {}",
                    task.id, dependency
                )));
            }
        }
    }

    ensure_dependency_graph_acyclic(task_file)?;

    Ok(())
}

pub fn validate_run_metadata(metadata: &RunMetadata) -> std::result::Result<(), AppError> {
    if metadata.schema_version != 1 {
        return Err(AppError::Config(format!(
            "unsupported metadata.json version {}",
            metadata.schema_version
        )));
    }
    if metadata.extra.contains_key("schema_version") {
        return Err(AppError::Config(
            "metadata.json must not contain both version and schema_version".to_string(),
        ));
    }
    RunId::parse(&metadata.run_id)?;
    if metadata.branch.trim().is_empty() {
        return Err(AppError::Config(
            "metadata branch must not be empty".to_string(),
        ));
    }
    if Path::new(&metadata.spec_file).is_absolute() {
        return Err(AppError::Config(format!(
            "metadata specFile must be repo-relative: {}",
            metadata.spec_file
        )));
    }
    Ok(())
}

pub fn validate_run_state(state: &RunState) -> std::result::Result<(), AppError> {
    if state.schema_version != 1 {
        return Err(AppError::Config(format!(
            "unsupported state.json version {}",
            state.schema_version
        )));
    }
    if state.extra.contains_key("schema_version") {
        return Err(AppError::Config(
            "state.json must not contain both version and schema_version".to_string(),
        ));
    }

    let mut ids = BTreeSet::new();
    for task in &state.tasks {
        if !ids.insert(task.id.clone()) {
            return Err(AppError::Config(format!(
                "duplicate task state id {}",
                task.id
            )));
        }
    }

    Ok(())
}

fn ensure_dependency_graph_acyclic(task_file: &TaskFile) -> std::result::Result<(), AppError> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Mark {
        Visiting,
        Done,
    }

    fn visit(
        id: &str,
        graph: &HashMap<&str, Vec<&str>>,
        marks: &mut HashMap<String, Mark>,
        stack: &mut Vec<String>,
    ) -> std::result::Result<(), AppError> {
        if matches!(marks.get(id), Some(Mark::Done)) {
            return Ok(());
        }
        if matches!(marks.get(id), Some(Mark::Visiting)) {
            stack.push(id.to_string());
            return Err(AppError::Config(format!(
                "dependency cycle detected: {}",
                stack.join(" -> ")
            )));
        }

        marks.insert(id.to_string(), Mark::Visiting);
        stack.push(id.to_string());
        for dependency in graph.get(id).into_iter().flatten() {
            visit(dependency, graph, marks, stack)?;
        }
        stack.pop();
        marks.insert(id.to_string(), Mark::Done);
        Ok(())
    }

    let graph: HashMap<&str, Vec<&str>> = task_file
        .tasks
        .iter()
        .map(|task| {
            (
                task.id.as_str(),
                task.depends_on.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let mut marks = HashMap::new();
    for task in &task_file.tasks {
        visit(&task.id, &graph, &mut marks, &mut Vec::new())?;
    }
    Ok(())
}

fn merge_status_view(
    run_dir: PathBuf,
    task_file: TaskFile,
    run_state: RunState,
) -> std::result::Result<StatusView, AppError> {
    let task_ids: BTreeSet<String> = task_file.tasks.iter().map(|task| task.id.clone()).collect();
    let mut state_by_id = HashMap::new();
    for state in run_state.tasks {
        if !task_ids.contains(&state.id) {
            return Err(AppError::Config(format!(
                "state.json references unknown task {}",
                state.id
            )));
        }
        state_by_id.insert(state.id.clone(), state);
    }

    let mut counts = BTreeMap::new();
    for status in TASK_STATUS_ORDER {
        counts.insert(status.as_str().to_string(), 0);
    }

    let mut tasks = Vec::new();
    for task in task_file.tasks {
        let state = state_by_id.remove(&task.id);
        let status = state
            .as_ref()
            .map(|state| state.status)
            .unwrap_or_else(default_task_status);
        *counts.entry(status.as_str().to_string()).or_insert(0) += 1;

        tasks.push(TaskStatusView {
            id: task.id,
            priority: task.priority,
            group: task.group,
            title: task.title,
            status: status.as_str().to_string(),
            phase: state
                .as_ref()
                .and_then(|state| state.phase.map(|phase| phase.as_str().to_string())),
            attempts: state.as_ref().map(|state| state.attempts).unwrap_or(0),
            review_attempts: state
                .as_ref()
                .map(|state| state.review_attempts)
                .unwrap_or(0),
            depends_on: task.depends_on,
            last_error: state.and_then(|state| state.last_error),
        });
    }

    tasks.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });

    Ok(StatusView {
        run_id: task_file.run_id,
        branch: task_file.branch,
        spec_file: task_file.spec_file,
        run_dir,
        feature_review_status: run_state.feature_review_status.as_str().to_string(),
        feature_review_attempts: run_state.feature_review_attempts,
        counts,
        tasks,
    })
}

pub fn format_doctor_text(report: &DoctorReport) -> String {
    let mut out = String::new();
    out.push_str("Doctor checks\n");
    for check in &report.checks {
        out.push_str(&format!(
            "[{}] {}: {}\n",
            doctor_status_label(check.status),
            check.name,
            check.message
        ));
    }
    out
}

fn doctor_status_label(status: DoctorStatus) -> &'static str {
    match status {
        DoctorStatus::Ok => "ok",
        DoctorStatus::Warn => "warn",
        DoctorStatus::Error => "error",
    }
}

pub fn format_status_text(view: &StatusView) -> String {
    let mut out = String::new();
    out.push_str(&format!("Run: {}\n", view.run_id));
    out.push_str(&format!("Branch: {}\n", view.branch));
    out.push_str(&format!("Spec: {}\n", view.spec_file));
    out.push_str(&format!("Run store: {}\n", view.run_dir.display()));
    out.push_str(&format!(
        "Feature review: {} (attempts: {})\n",
        view.feature_review_status, view.feature_review_attempts
    ));
    out.push_str("Counts:");
    for status in TASK_STATUS_ORDER {
        let key = status.as_str();
        let count = view.counts.get(key).copied().unwrap_or(0);
        out.push_str(&format!(" {key}={count}"));
    }
    out.push('\n');
    out.push_str("Tasks:\n");
    for task in &view.tasks {
        let phase = task.phase.as_deref().unwrap_or("-");
        out.push_str(&format!(
            "- [{}] {}: {} (phase: {}, attempts: {}, reviewAttempts: {})\n",
            task.status, task.id, task.title, phase, task.attempts, task.review_attempts
        ));
        if let Some(error) = &task.last_error {
            out.push_str(&format!("  lastError: {error}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toggle_from_bool_and_auto_string() {
        #[derive(Deserialize)]
        struct Wrapper {
            verify: Toggle,
            review: Toggle,
            require_clean: Toggle,
        }

        let parsed: Wrapper =
            toml::from_str("verify = true\nreview = false\nrequire_clean = \"auto\"\n").unwrap();
        assert_eq!(parsed.verify, Toggle::True);
        assert_eq!(parsed.review, Toggle::False);
        assert_eq!(parsed.require_clean, Toggle::Auto);
    }

    #[test]
    fn merges_builtin_global_profile_and_project_config_deterministically() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".codex/task-runner/profiles")).unwrap();

        fs::write(
            home.join(".codex/task-runner/profiles/team.toml"),
            r#"
[runner]
sandbox = "read-only"
model = "gpt-5"
reasoning_effort = "high"
search = true
dangerous_bypass_approvals_and_sandbox = true

[git]
commit = true
add_include = ["backend/**"]
"#,
        )
        .unwrap();

        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[project]
default_branch = "trunk"

[runner]
dangerous_bypass_approvals_and_sandbox = false

[prompts]
profile = "team"

[git]
add_include = ["src/**"]
"#,
        )
        .unwrap();

        let context = load_config(&repo, &home, true).unwrap();
        assert_eq!(context.merged.project.default_branch, "trunk");
        assert_eq!(context.merged.runner.sandbox, "read-only");
        assert_eq!(context.merged.runner.model.as_deref(), Some("gpt-5"));
        assert_eq!(
            context.merged.runner.reasoning_effort.as_deref(),
            Some("high")
        );
        assert!(context.merged.runner.search);
        assert!(!context.merged.runner.dangerous_bypass_approvals_and_sandbox);
        assert!(context.merged.git.commit);
        assert_eq!(context.merged.git.add_include, vec!["src/**"]);
    }

    #[test]
    fn global_profile_cannot_enable_dangerous_bypass_implicitly() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::create_dir_all(home.join(".codex/task-runner/profiles")).unwrap();

        fs::write(
            home.join(".codex/task-runner/profiles/team.toml"),
            r#"
[runner]
dangerous_bypass_approvals_and_sandbox = true
"#,
        )
        .unwrap();

        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[prompts]
profile = "team"
"#,
        )
        .unwrap();

        let context = load_config(&repo, &home, true).unwrap();
        assert!(!context.merged.runner.dangerous_bypass_approvals_and_sandbox);

        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[runner]
dangerous_bypass_approvals_and_sandbox = true

[prompts]
profile = "team"
"#,
        )
        .unwrap();

        let context = load_config(&repo, &home, true).unwrap();
        assert!(context.merged.runner.dangerous_bypass_approvals_and_sandbox);
    }

    #[test]
    fn prompt_lookup_uses_project_global_then_builtin_order() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(repo.join(".codex/task-runner/prompts")).unwrap();
        fs::create_dir_all(home.join(".codex/task-runner/prompts")).unwrap();
        fs::write(repo.join(".codex/task-runner.toml"), "").unwrap();
        fs::write(
            repo.join(".codex/task-runner/prompts/analyze-task.md"),
            "project {task_id}",
        )
        .unwrap();
        fs::write(
            home.join(".codex/task-runner/prompts/analyze-task.md"),
            "global {task_id}",
        )
        .unwrap();

        let context = load_config(&repo, &home, true).unwrap();
        let loaded = load_prompt_template(&context, PromptTemplateKind::AnalyzeTask).unwrap();
        assert_eq!(loaded.source, PromptTemplateSource::Project);
        assert_eq!(loaded.content, "project {task_id}");

        fs::remove_file(repo.join(".codex/task-runner/prompts/analyze-task.md")).unwrap();
        let loaded = load_prompt_template(&context, PromptTemplateKind::AnalyzeTask).unwrap();
        assert_eq!(loaded.source, PromptTemplateSource::Global);
        assert_eq!(loaded.content, "global {task_id}");

        fs::remove_file(home.join(".codex/task-runner/prompts/analyze-task.md")).unwrap();
        let loaded = load_prompt_template(&context, PromptTemplateKind::AnalyzeTask).unwrap();
        assert_eq!(loaded.source, PromptTemplateSource::BuiltIn);
        assert!(loaded.content.contains("Codex Task Analyzer"));
    }

    #[test]
    fn prompt_rendering_fails_on_missing_variables() {
        let template = PromptTemplate {
            kind: PromptTemplateKind::AnalyzeTask,
            source: PromptTemplateSource::BuiltIn,
            path: None,
            content: "Task {task_id} has {missing_variable}".to_string(),
        };

        let error = template.render(&sample_analyze_input()).unwrap_err();
        assert_eq!(
            error,
            PromptRenderError::MissingVariable {
                template: "analyze-task.md".to_string(),
                variable: "missing_variable".to_string(),
            }
        );
    }

    #[test]
    fn builtin_prompt_templates_render_all_five_typed_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::write(repo.join(".codex/task-runner.toml"), "").unwrap();
        let context = load_config(&repo, &home, true).unwrap();

        let decompose = load_prompt_template(&context, PromptTemplateKind::DecomposeFeature)
            .unwrap()
            .render(&sample_decompose_input())
            .unwrap();
        assert!(decompose.contains("run-1"));
        assert!(decompose.contains("Feature body"));

        let analyze = load_prompt_template(&context, PromptTemplateKind::AnalyzeTask)
            .unwrap()
            .render(&sample_analyze_input())
            .unwrap();
        assert!(analyze.contains("p1"));
        assert!(analyze.contains("analysis.md"));

        let implement = load_prompt_template(&context, PromptTemplateKind::ImplementTask)
            .unwrap()
            .render(&sample_implement_input())
            .unwrap();
        assert!(implement.contains("Review comments"));
        assert!(implement.contains("Previous error"));

        let review = load_prompt_template(&context, PromptTemplateKind::ReviewTask)
            .unwrap()
            .render(&sample_review_input())
            .unwrap();
        assert!(review.contains("Criterion"));
        assert!(review.contains("verdict: APPROVED"));

        let feature = load_prompt_template(&context, PromptTemplateKind::ReviewFeature)
            .unwrap()
            .render(&sample_review_feature_input())
            .unwrap();
        assert!(feature.contains("Task summaries"));
        assert!(feature.contains("must not append tasks"));
    }

    #[cfg(unix)]
    #[test]
    fn codex_executor_logs_stdio_last_message_and_keeps_dangerous_flag_explicit() {
        let temp = tempfile::tempdir().unwrap();
        let script = fake_codex_script(
            temp.path(),
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
printf 'last message\n' > "$last"
printf 'stdout line\n'
printf 'stderr line\n' >&2
"#,
        );
        let script_dir = script.parent().unwrap().to_path_buf();
        let mut request = sample_codex_request(temp.path());
        let executor = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: script.clone(),
            model: Some("gpt-test".to_string()),
            reasoning_effort: Some("high".to_string()),
            search: true,
            dangerous_bypass_approvals_and_sandbox: false,
        });

        let output = executor.execute(&request).unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(output.last_message, "last message\n");
        assert_eq!(
            fs::read_to_string(script_dir.join("stdin.log")).unwrap(),
            "prompt"
        );
        assert_eq!(
            fs::read_to_string(&request.stdout_log_path).unwrap(),
            "stdout line\n"
        );
        assert_eq!(
            fs::read_to_string(&request.stderr_log_path).unwrap(),
            "stderr line\n"
        );
        let args = fs::read_to_string(script_dir.join("args.log")).unwrap();
        assert!(args.contains("workspace-write"));
        assert!(args.contains("gpt-test"));
        assert!(args.contains("model_reasoning_effort=\"high\""));
        assert!(args.contains("--search"));
        assert!(!args.contains("--dangerously-bypass-approvals-and-sandbox"));

        request.prompt = "prompt2".to_string();
        let executor = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: script,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: true,
        });
        executor.execute(&request).unwrap();
        let args = fs::read_to_string(script_dir.join("args.log")).unwrap();
        assert!(args.contains("--dangerously-bypass-approvals-and-sandbox"));
    }

    #[cfg(unix)]
    #[test]
    fn codex_executor_persists_last_message_as_required_output_for_read_only_runs() {
        let temp = tempfile::tempdir().unwrap();
        let script = fake_codex_script(
            temp.path(),
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
printf '# Analysis\n\nRead-only report.\n' > "$last"
"#,
        );
        let mut request = sample_codex_request(temp.path());
        let required_output_path = temp.path().join("readonly-output.md");
        request.required_output_path = Some(required_output_path.clone());
        request.sandbox = "read-only".to_string();

        let output = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: script,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: false,
        })
        .execute(&request)
        .unwrap();

        assert_eq!(output.exit_code, 0);
        assert_eq!(
            fs::read_to_string(required_output_path).unwrap(),
            "# Analysis\n\nRead-only report.\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_executor_rejects_empty_last_message_for_read_only_required_output() {
        let temp = tempfile::tempdir().unwrap();
        let script = fake_codex_script(
            temp.path(),
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
printf '   \n' > "$last"
"#,
        );
        let mut request = sample_codex_request(temp.path());
        request.required_output_path = Some(temp.path().join("readonly-output.md"));
        request.sandbox = "read-only".to_string();

        let error = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: script,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: false,
        })
        .execute(&request)
        .unwrap_err();

        assert_eq!(error.kind, CodexFailureKind::EmptyLastMessage);
    }

    #[cfg(unix)]
    #[test]
    fn codex_executor_fails_on_nonzero_missing_output_and_timeout() {
        let temp = tempfile::tempdir().unwrap();

        let nonzero = fake_codex_script(temp.path(), "exit 7");
        let request = sample_codex_request(temp.path());
        let err = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: nonzero,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: false,
        })
        .execute(&request)
        .unwrap_err();
        assert_eq!(err.kind, CodexFailureKind::NonZeroExit);
        assert_eq!(err.exit_code, Some(7));

        let missing_last = fake_codex_script(temp.path(), "exit 0");
        let err = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: missing_last,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: false,
        })
        .execute(&request)
        .unwrap_err();
        assert_eq!(err.kind, CodexFailureKind::MissingLastMessage);

        let missing_output = fake_codex_script(
            temp.path(),
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
printf 'last message\n' > "$last"
"#,
        );
        let mut request_with_output = sample_codex_request(temp.path());
        let required_output_path = temp.path().join("required.md");
        fs::write(&required_output_path, "stale output\n").unwrap();
        request_with_output.required_output_path = Some(required_output_path.clone());
        let err = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: missing_output,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: false,
        })
        .execute(&request_with_output)
        .unwrap_err();
        assert_eq!(err.kind, CodexFailureKind::MissingRequiredOutput);
        assert!(!required_output_path.exists());

        let whitespace_output = fake_codex_script(
            temp.path(),
            &format!(
                r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
printf 'last message\n' > "$last"
printf '   \n' > "{}"
"#,
                required_output_path.display()
            ),
        );
        let err = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: whitespace_output,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: false,
        })
        .execute(&request_with_output)
        .unwrap_err();
        assert_eq!(err.kind, CodexFailureKind::EmptyRequiredOutput);

        let timeout = fake_codex_script(temp.path(), "sleep 5");
        let mut timeout_request = sample_codex_request(temp.path());
        timeout_request.timeout_seconds = 1;
        let err = CodexExecutor::new(CodexExecutorConfig {
            repo_root: temp.path().to_path_buf(),
            codex_bin: timeout,
            model: None,
            reasoning_effort: None,
            search: false,
            dangerous_bypass_approvals_and_sandbox: false,
        })
        .execute(&timeout_request)
        .unwrap_err();
        assert_eq!(err.kind, CodexFailureKind::Timeout);
        assert_eq!(err.exit_code, Some(124));
    }

    #[test]
    fn status_merges_task_file_and_state_without_writing_defaults() {
        let task_file = TaskFile {
            schema_version: 2,
            run_id: "sample".to_string(),
            branch: "feat/sample".to_string(),
            spec_file: "docs/roadmap/sample.md".to_string(),
            verification_commands: Vec::new(),
            tasks: vec![
                Task {
                    id: "p1".to_string(),
                    priority: 1,
                    group: "g".to_string(),
                    title: "Pending".to_string(),
                    max_attempts: None,
                    timeout_seconds: None,
                    output: "out.md".to_string(),
                    prompt: "do it".to_string(),
                    spec_file: None,
                    depends_on: Vec::new(),
                    review_criteria: Vec::new(),
                    analyze_timeout_seconds: None,
                    analyze_required: true,
                    require_review_approval: false,
                    max_review_attempts: 2,
                    review_timeout_seconds: None,
                    verification_commands: Vec::new(),
                    extra: Map::new(),
                },
                Task {
                    id: "p2".to_string(),
                    priority: 2,
                    group: "g".to_string(),
                    title: "Done".to_string(),
                    max_attempts: None,
                    timeout_seconds: None,
                    output: "out.md".to_string(),
                    prompt: "do it".to_string(),
                    spec_file: None,
                    depends_on: vec!["p1".to_string()],
                    review_criteria: Vec::new(),
                    analyze_timeout_seconds: None,
                    analyze_required: true,
                    require_review_approval: false,
                    max_review_attempts: 2,
                    review_timeout_seconds: None,
                    verification_commands: Vec::new(),
                    extra: Map::new(),
                },
            ],
            extra: Map::new(),
        };

        let state = RunState {
            schema_version: 1,
            feature_review_status: FeatureReviewStatus::Pending,
            feature_review_attempts: 0,
            tasks: vec![TaskState {
                id: "p2".to_string(),
                status: TaskStatus::Done,
                phase: Some(TaskPhase::Commit),
                attempts: 1,
                review_attempts: 0,
                started_at: None,
                finished_at: None,
                updated_at: None,
                approved_at: None,
                ignored_at: None,
                ignore_reason: None,
                output: None,
                analysis_output: None,
                review_output: None,
                last_exit_code: Some(0),
                last_error: None,
                last_log: None,
                last_verdict: Some(ReviewVerdict::Approved),
                last_review_comments: None,
                extra: Map::new(),
            }],
            extra: Map::new(),
        };

        let view = merge_status_view(PathBuf::from("/tmp/run"), task_file, state).unwrap();
        assert_eq!(view.counts["pending"], 1);
        assert_eq!(view.counts["done"], 1);
        assert_eq!(view.tasks[0].status, "pending");
        assert_eq!(view.tasks[1].status, "done");
        assert_eq!(view.tasks[1].phase.as_deref(), Some("commit"));
    }

    #[test]
    fn reads_legacy_tasks_and_state_fixtures() {
        let Some(root) = legacy_fixture_root() else {
            eprintln!("legacy fixtures not present; skipping local fixture test");
            return;
        };

        let sample = root.join("v1-outbox-roadmap-sample");
        let migrated = read_task_file_with_migration(&sample.join("tasks.json")).unwrap();
        assert!(migrated.report.is_none());
        assert_eq!(migrated.value.schema_version, 2);
        assert!(migrated.value.verification_commands.is_empty());

        let first_command = &migrated.value.tasks[0].verification_commands[0];
        assert_eq!(first_command.name, DEFAULT_VERIFICATION_COMMAND_NAME);
        assert!(first_command.required);
        assert_eq!(
            first_command.command,
            "test -s tools/task-runner/runs/v1-outbox-roadmap/output/architecture-design.md"
        );

        let state = read_run_state(&sample.join("state.json")).unwrap();
        assert_eq!(state.schema_version, 1);
        assert_eq!(state.feature_review_status, FeatureReviewStatus::Pending);
        assert_eq!(
            state.tasks[1].last_verdict,
            Some(ReviewVerdict::ChangesRequested)
        );

        let encoded_tasks = serde_json::to_value(&migrated.value).unwrap();
        assert!(encoded_tasks.get("version").is_some());
        assert!(encoded_tasks.get("schema_version").is_none());
        let encoded_state = serde_json::to_value(&state).unwrap();
        assert!(encoded_state.get("version").is_some());
        assert!(encoded_state.get("schema_version").is_none());
    }

    #[test]
    fn status_fixture_displays_all_task_statuses() {
        let Some(root) = legacy_fixture_root() else {
            eprintln!("legacy fixtures not present; skipping local fixture test");
            return;
        };

        let sample = root.join("phase0-status-coverage-sample");
        let task_file = read_task_file(&sample.join("tasks.json")).unwrap();
        let state = read_run_state(&sample.join("state.json")).unwrap();
        let top_command = &task_file.verification_commands[0];
        assert_eq!(top_command.name, "global-doc-smoke");
        assert_eq!(top_command.timeout_seconds, Some(30));

        let view = merge_status_view(sample, task_file, state).unwrap();
        for status in TASK_STATUS_ORDER {
            assert_eq!(view.counts[status.as_str()], 1, "{}", status.as_str());
        }
        assert_eq!(view.feature_review_status, "failed");
        assert!(format_status_text(&view).contains("review_failed=1"));
    }

    #[test]
    fn migrates_missing_task_file_version_without_schema_version_field() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("tasks.json");
        fs::write(
            &path,
            r#"{
  "runId": "legacy",
  "branch": "feat/legacy",
  "specFile": "docs/roadmap/legacy.md",
  "tasks": [
    {
      "id": "p1",
      "priority": 1,
      "title": "Legacy task",
      "output": "output/p1.md",
      "prompt": "Do the legacy task."
    }
  ]
}"#,
        )
        .unwrap();

        let migrated = read_task_file_with_migration(&path).unwrap();
        assert_eq!(
            migrated.report,
            Some(MigrationReport {
                migration_from: None,
                migration_to: 2
            })
        );
        assert_eq!(migrated.value.schema_version, 2);
        assert_eq!(migrated.value.tasks[0].group, "");
        assert!(migrated.value.tasks[0].analyze_required);
        assert_eq!(migrated.value.tasks[0].max_review_attempts, 2);

        let encoded = serde_json::to_string(&migrated.value).unwrap();
        assert!(encoded.contains("\"version\":2"));
        assert!(!encoded.contains("schema_version"));
        assert!(!encoded.contains("migration_from"));
        assert!(!encoded.contains("migration_to"));
    }

    #[test]
    fn run_store_writes_state_with_lock_and_atomic_target() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&home).unwrap();

        let store = RunStore::for_repo(&repo, &home).unwrap();
        let state = RunState {
            schema_version: 1,
            feature_review_status: FeatureReviewStatus::Pending,
            feature_review_attempts: 0,
            tasks: vec![TaskState {
                id: "p1".to_string(),
                status: TaskStatus::Pending,
                phase: None,
                attempts: 0,
                review_attempts: 0,
                started_at: None,
                finished_at: None,
                updated_at: None,
                approved_at: None,
                ignored_at: None,
                ignore_reason: None,
                output: None,
                analysis_output: None,
                review_output: None,
                last_exit_code: None,
                last_error: None,
                last_log: None,
                last_verdict: None,
                last_review_comments: None,
                extra: Map::new(),
            }],
            extra: Map::new(),
        };

        store.write_run_state("run1", &state).unwrap();

        assert_eq!(
            store.repo_runs_dir,
            home.join(".codex/task-runner/runs").join(&store.repo_hash)
        );
        assert!(store.lock_path("run1").unwrap().exists());
        let raw = fs::read_to_string(store.state_path("run1").unwrap()).unwrap();
        let encoded: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(encoded["version"], 1);
        assert!(encoded.get("schema_version").is_none());
        assert_eq!(
            read_run_state(&store.state_path("run1").unwrap())
                .unwrap()
                .tasks
                .len(),
            1
        );
    }

    #[test]
    fn run_store_rejects_path_traversal_run_ids() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&home).unwrap();

        let store = RunStore::for_repo(&repo, &home).unwrap();
        for invalid in [
            "",
            ".",
            "..",
            "../escape",
            "nested/run",
            "nested\\run",
            "run id",
        ] {
            assert!(RunId::parse(invalid).is_err(), "{invalid:?}");
            assert!(store.run_dir(invalid).is_err(), "{invalid:?}");
        }

        let err = store
            .write_run_state("../escape", &RunState::default())
            .unwrap_err();
        assert!(matches!(err, AppError::Config(_)));
        assert!(
            !store
                .repo_runs_dir
                .parent()
                .unwrap()
                .join("escape")
                .exists()
        );
    }

    #[test]
    fn task_review_criteria_accepts_legacy_string_and_serializes_array() {
        let raw = r#"{
  "version": 2,
  "runId": "criteria",
  "branch": "feat/criteria",
  "specFile": "docs/roadmap/criteria.md",
  "tasks": [
    {
      "id": "p1",
      "priority": 1,
      "title": "Task",
      "output": "output/p1.md",
      "prompt": "Do it.",
      "reviewCriteria": "Must pass review."
    }
  ]
}"#;

        let task_file: TaskFile = serde_json::from_str(raw).unwrap();
        assert_eq!(
            task_file.tasks[0].review_criteria,
            vec!["Must pass review."]
        );

        let encoded = serde_json::to_value(&task_file).unwrap();
        assert_eq!(
            encoded["tasks"][0]["reviewCriteria"],
            serde_json::json!(["Must pass review."])
        );
    }

    #[test]
    fn rejects_dependency_cycles() {
        let raw = r#"{
  "version": 2,
  "runId": "cycle",
  "branch": "feat/cycle",
  "specFile": "docs/roadmap/cycle.md",
  "tasks": [
    {
      "id": "p1",
      "priority": 1,
      "title": "First",
      "output": "output/p1.md",
      "prompt": "Do p1.",
      "dependsOn": ["p2"]
    },
    {
      "id": "p2",
      "priority": 2,
      "title": "Second",
      "output": "output/p2.md",
      "prompt": "Do p2.",
      "dependsOn": ["p1"]
    }
  ]
}"#;
        let task_file: TaskFile = serde_json::from_str(raw).unwrap();
        let error = validate_task_file(&task_file).unwrap_err().to_string();
        assert!(error.contains("dependency cycle detected"));
    }

    #[cfg(unix)]
    #[test]
    fn start_creates_global_run_store_tasks_state_and_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        let spec = repo.join("docs/roadmap/feature.md");
        fs::create_dir_all(spec.parent().unwrap()).unwrap();
        fs::write(&spec, "# Feature\n\nDo the thing.\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let codex = fake_codex_script(
            temp.path(),
            &write_last_message_script(&sample_decompose_json(
                "feature",
                "feat/feature",
                "docs/roadmap/feature.md",
            )),
        );

        let result = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("docs/roadmap/feature.md"),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert!(!result.resumed);
        assert_eq!(result.run_id, "feature");
        assert_eq!(result.branch, "feat/feature");
        assert!(
            result
                .tasks_path
                .starts_with(home.join(".codex/task-runner"))
        );
        assert!(
            !result
                .tasks_path
                .starts_with(repo.join("tools/task-runner"))
        );
        let raw_tasks = fs::read_to_string(&result.tasks_path).unwrap();
        assert!(raw_tasks.trim_start().starts_with('{'));
        assert!(!raw_tasks.contains("```"));
        let task_file = read_task_file(&result.tasks_path).unwrap();
        assert_eq!(task_file.run_id, "feature");
        assert_eq!(task_file.spec_file, "docs/roadmap/feature.md");
        let state = read_run_state(&result.state_path).unwrap();
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].id, "p1");
        assert_eq!(state.tasks[0].status, TaskStatus::Pending);
        let metadata = read_run_metadata(&result.metadata_path).unwrap();
        assert_eq!(metadata.run_id, "feature");
        let spec_text = fs::read_to_string(&spec).unwrap();
        assert!(spec_text.contains("run_id: feature"));
        assert!(spec_text.contains("branch: feat/feature"));
    }

    #[cfg(unix)]
    #[test]
    fn start_with_commit_enabled_keeps_worktree_clean_for_watch() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[git]
commit = true
add_include = ["src/**"]
"#,
        )
        .unwrap();
        fs::create_dir_all(&home).unwrap();
        let spec = repo.join("docs/roadmap/feature.md");
        fs::create_dir_all(spec.parent().unwrap()).unwrap();
        fs::write(&spec, "# Feature\n\nDo the thing.\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let decompose_codex = fake_codex_script(
            temp.path(),
            &write_last_message_script(&sample_decompose_json(
                "feature",
                "feat/feature",
                "docs/roadmap/feature.md",
            )),
        );

        let start = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("docs/roadmap/feature.md"),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(decompose_codex),
            },
        )
        .unwrap();

        assert!(
            start
                .warnings
                .iter()
                .any(|warning| warning.contains("global run store"))
        );
        let spec_text = fs::read_to_string(&spec).unwrap();
        assert!(!spec_text.contains("run_id: feature"));
        assert!(
            git_output(&repo, &["status", "--porcelain", "--untracked-files=all"])
                .unwrap()
                .trim()
                .is_empty()
        );

        let watch_codex = fake_codex_script(temp.path(), &phase_aware_success_script());
        let result = watch_run_in_repo(
            &repo,
            &home,
            WatchOptions {
                run_id: Some("feature".to_string()),
                interval_seconds: 0,
                max_failures: Some(1),
                codex_bin: Some(watch_codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0, "{}", result.message);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        let state = store.read_run_state("feature").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Done);
    }

    #[cfg(unix)]
    #[test]
    fn start_recovers_same_spec_without_rerunning_codex() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(repo.join("docs")).unwrap();
        fs::write(repo.join("docs/spec.md"), "# Spec\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let codex = fake_codex_script(
            temp.path(),
            &write_last_message_script(&sample_decompose_json("spec", "feat/spec", "docs/spec.md")),
        );
        let first = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("docs/spec.md"),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        let failing_codex = fake_codex_script(temp.path(), "exit 99");
        let second = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("docs/spec.md"),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(failing_codex),
            },
        )
        .unwrap();

        assert!(second.resumed);
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.tasks_path, second.tasks_path);
    }

    #[cfg(unix)]
    #[test]
    fn start_accepts_code_block_decompose_output_and_normalizes_json() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(repo.join("feature.md"), "# Feature\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let json = sample_decompose_json("feature", "feat/feature", "feature.md");
        let codex = fake_codex_script(
            temp.path(),
            &write_last_message_script(&format!("```json\n{json}\n```\n")),
        );
        let result = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("feature.md"),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        let raw_tasks = fs::read_to_string(&result.tasks_path).unwrap();
        assert!(!raw_tasks.contains("```"));
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("code block"))
        );
        let events = fs::read_to_string(result.run_dir.join("logs/events.log")).unwrap();
        assert!(events.contains("code block"));
    }

    #[cfg(unix)]
    #[test]
    fn start_fails_invalid_json_without_writing_tasks_and_preserves_output() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(repo.join("invalid-json.md"), "# Bad\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let codex = fake_codex_script(temp.path(), &write_last_message_script("not json\n"));
        let err = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("invalid-json.md"),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("invalid decompose output"));
        let store = RunStore::for_repo(&repo, &home).unwrap();
        let run_dir = store.run_dir("invalid-json").unwrap();
        assert!(!store.tasks_path("invalid-json").unwrap().exists());
        assert_eq!(
            fs::read_to_string(run_dir.join("last-message.md"))
                .unwrap()
                .trim_end(),
            "not json"
        );
        assert!(
            fs::read_to_string(run_dir.join("logs/events.log"))
                .unwrap()
                .contains("invalid decompose output")
        );
    }

    #[cfg(unix)]
    #[test]
    fn start_fails_codex_error_without_writing_tasks_and_keeps_logs() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(repo.join("codex-fail.md"), "# Bad\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let codex = fake_codex_script(temp.path(), "printf 'codex failed\\n' >&2\nexit 7");
        let err = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("codex-fail.md"),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("codex exited with status 7"));
        let store = RunStore::for_repo(&repo, &home).unwrap();
        let run_dir = store.run_dir("codex-fail").unwrap();
        assert!(!store.tasks_path("codex-fail").unwrap().exists());
        assert!(
            fs::read_to_string(run_dir.join("logs/decompose.stderr.log"))
                .unwrap()
                .contains("codex failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn watch_respects_dependency_gating_and_runs_one_task_through_implement() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task_file = sample_run_task_file(
            "run",
            "spec.md",
            vec![
                sample_task("p1", 1),
                {
                    let mut task = sample_task("p2", 2);
                    task.depends_on = vec!["p1".to_string()];
                    task
                },
                {
                    let mut task = sample_task("p3", 3);
                    task.depends_on = vec!["p2".to_string()];
                    task
                },
            ],
        );
        task_file.tasks[0].title = "Already done".to_string();
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Done;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        store.write_run_state("run", &state).unwrap();

        let codex = fake_codex_script(temp.path(), &phase_aware_success_script());
        let result = watch_run_in_repo(
            &repo,
            &home,
            WatchOptions {
                run_id: Some("run".to_string()),
                interval_seconds: 0,
                max_failures: Some(1),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0, "{}", result.message);
        let state = store.read_run_state("run").unwrap();
        let p2 = find_task_state(&state, "p2").unwrap();
        assert_eq!(p2.status, TaskStatus::Done);
        assert_eq!(p2.phase, Some(TaskPhase::Done));
        assert_eq!(p2.attempts, 1);
        assert_eq!(p2.last_verdict, Some(ReviewVerdict::Approved));
        assert!(Path::new(p2.output.as_deref().unwrap()).exists());
        assert!(Path::new(p2.analysis_output.as_deref().unwrap()).exists());
        assert!(Path::new(p2.review_output.as_deref().unwrap()).exists());

        let p3 = find_task_state(&state, "p3").unwrap();
        assert_eq!(p3.status, TaskStatus::Done);
        assert_eq!(p3.phase, Some(TaskPhase::Done));
    }

    #[cfg(unix)]
    #[test]
    fn analysis_review_pauses_before_implement() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.require_review_approval = true;
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        store
            .write_run_state("run", &initial_run_state(&task_file))
            .unwrap();

        let codex = fake_codex_script(temp.path(), &phase_aware_success_script());
        let result = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::AnalysisReview);
        assert_eq!(p1.phase, Some(TaskPhase::AnalysisReview));
        assert_eq!(p1.attempts, 0);
        assert!(p1.output.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn analyze_required_false_injects_analysis_failure_into_implement_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.analyze_required = false;
        task.max_attempts = Some(1);
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        store
            .write_run_state("run", &initial_run_state(&task_file))
            .unwrap();

        let codex = fake_codex_script(
            temp.path(),
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q 'Analysis output path:' "$script_dir/stdin.log"; then
  printf 'analysis failed hard\n' >&2
  exit 7
fi
cp "$script_dir/stdin.log" "$script_dir/implement-prompt.log"
printf 'implemented after optional analysis failure\n' > "$last"
"#,
        );
        let codex_dir = codex.parent().unwrap().to_path_buf();
        let result = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let prompt = fs::read_to_string(codex_dir.join("implement-prompt.log")).unwrap();
        assert!(prompt.contains("codex exited with status 7"));
        assert!(prompt.contains("analysis failed hard"));
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Pending);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p1.attempts, 1);
    }

    #[cfg(unix)]
    #[test]
    fn implement_timeout_blocks_at_max_attempts_with_exit_124() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.max_attempts = Some(1);
        task.timeout_seconds = Some(1);
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].phase = Some(TaskPhase::Implement);
        store.write_run_state("run", &state).unwrap();

        let codex = fake_codex_script(temp.path(), "sleep 5");
        let result = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Blocked);
        assert_eq!(p1.phase, Some(TaskPhase::Implement));
        assert_eq!(p1.last_exit_code, Some(124));
        assert!(
            fs::read_to_string(p1.last_log.as_deref().unwrap())
                .unwrap()
                .contains("timeout after 1 seconds")
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_lock_rejects_parallel_task_execution_for_same_run() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file(
            "run",
            "spec.md",
            vec![sample_task("p1", 1), sample_task("p2", 2)],
        );
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].phase = Some(TaskPhase::Implement);
        state.tasks[1].phase = Some(TaskPhase::Implement);
        store.write_run_state("run", &state).unwrap();

        let slow_dir = temp.path().join("slow-codex");
        let fast_dir = temp.path().join("fast-codex");
        fs::create_dir_all(&slow_dir).unwrap();
        fs::create_dir_all(&fast_dir).unwrap();
        let slow_codex = fake_codex_script(
            &slow_dir,
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
: > "$script_dir/started"
while [ ! -f "$script_dir/release" ]; do
  sleep 0.1
done
printf 'p1 implementation\n' > "$last"
"#,
        );
        let rejected_codex = fake_codex_script(
            &fast_dir,
            r#"
: > "$script_dir/invoked"
printf 'this codex should not run\n' >&2
exit 99
"#,
        );
        let slow_codex_dir = slow_codex.parent().unwrap().to_path_buf();
        let rejected_codex_dir = rejected_codex.parent().unwrap().to_path_buf();

        let first_repo = repo.clone();
        let first_home = home.clone();
        let first = thread::spawn(move || {
            run_one_task_in_repo(
                &first_repo,
                &first_home,
                RunTaskOptions {
                    run_id: Some("run".to_string()),
                    task_id: "p1".to_string(),
                    from: None,
                    codex_bin: Some(slow_codex),
                },
            )
        });

        let started = slow_codex_dir.join("started");
        for _ in 0..50 {
            if started.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if !started.exists() {
            fs::write(slow_codex_dir.join("release"), "").unwrap();
            let first_result = first.join().unwrap();
            panic!("first task did not enter codex execution: {first_result:?}");
        }

        let err = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p2".to_string(),
                from: None,
                codex_bin: Some(rejected_codex),
            },
        )
        .unwrap_err();
        assert_eq!(err.exit_code(), 4);
        assert!(err.to_string().contains("already being executed"));
        assert!(!rejected_codex_dir.join("invoked").exists());

        fs::write(slow_codex_dir.join("release"), "").unwrap();
        let first_result = first.join().unwrap().unwrap();
        assert_eq!(first_result.exit_code, 0);

        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        let p2 = find_task_state(&state, "p2").unwrap();
        assert_eq!(p1.status, TaskStatus::Pending);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p2.status, TaskStatus::Pending);
        assert_eq!(p2.phase, Some(TaskPhase::Implement));
        assert_eq!(p2.attempts, 0);
    }

    #[cfg(unix)]
    #[test]
    fn watch_recovers_stale_running_task_to_blocked_when_attempts_are_exhausted() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.max_attempts = Some(1);
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Running;
        state.tasks[0].phase = Some(TaskPhase::Implement);
        state.tasks[0].attempts = 1;
        state.tasks[0].extra.insert(
            "runnerPid".to_string(),
            Value::Number(serde_json::Number::from(999_999_u64)),
        );
        store.write_run_state("run", &state).unwrap();

        let result = watch_run_in_repo(
            &repo,
            &home,
            WatchOptions {
                run_id: Some("run".to_string()),
                interval_seconds: 0,
                max_failures: Some(1),
                codex_bin: Some(fake_codex_script(temp.path(), "exit 99")),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Blocked);
        assert_eq!(p1.phase, Some(TaskPhase::Implement));
        assert!(p1.last_error.as_deref().unwrap().contains("stale running"));
        assert!(p1.extra.get("runnerPid").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn dirty_worktree_policy_blocks_fresh_automatic_execution() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[runner]
require_clean = true
allow_dirty_resume = false

[git]
commit = true
"#,
        )
        .unwrap();
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.analyze_required = false;
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].phase = Some(TaskPhase::Implement);
        store.write_run_state("run", &state).unwrap();
        fs::write(repo.join("dirty.txt"), "dirty").unwrap();

        let err = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: Some(fake_codex_script(
                    temp.path(),
                    &phase_aware_success_script(),
                )),
            },
        )
        .unwrap_err();

        assert_eq!(err.exit_code(), 3);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Pending);
        assert_eq!(p1.attempts, 0);
    }

    #[cfg(unix)]
    #[test]
    fn verify_runs_file_commands_before_task_commands_and_logs_optional_failures() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.verification_commands = vec![verification_command(
            "task ok",
            "printf 'task ok\\n'",
            true,
            Some(5),
        )];
        let mut task_file = sample_run_task_file("run", "spec.md", vec![task]);
        task_file.verification_commands = vec![verification_command(
            "global optional",
            "printf 'global optional failed\\n' >&2; exit 7",
            false,
            Some(5),
        )];

        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].phase = Some(TaskPhase::Verify);
        state.tasks[0].attempts = 1;
        store.write_run_state("run", &state).unwrap();

        let result = verify_tasks_in_repo(
            &repo,
            &home,
            VerifyOptions {
                run_id: Some("run".to_string()),
                target: "all".to_string(),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let run_dir = store.run_dir("run").unwrap();
        let global_log = run_dir.join("logs/p1.verify.01-global-optional.log");
        let task_log = run_dir.join("logs/p1.verify.02-task-ok.log");
        assert!(global_log.exists());
        assert!(task_log.exists());
        assert!(
            fs::read_to_string(&global_log)
                .unwrap()
                .contains("global optional failed")
        );
        assert!(fs::read_to_string(&task_log).unwrap().contains("task ok"));

        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Pending);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p1.last_exit_code, Some(0));
        assert!(p1.last_error.is_none());
        assert_eq!(p1.last_log.as_deref(), Some(global_log.to_str().unwrap()));
        assert_eq!(
            p1.extra
                .get("verificationLogs")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            p1.extra
                .get("verificationOptionalFailures")
                .and_then(Value::as_array)
                .unwrap()
                .len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_required_failure_returns_to_implement_for_retry() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.max_attempts = Some(2);
        task.verification_commands = vec![verification_command(
            "unit",
            "printf 'assertion failed\\n'; exit 5",
            true,
            Some(5),
        )];
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].phase = Some(TaskPhase::Verify);
        state.tasks[0].attempts = 1;
        store.write_run_state("run", &state).unwrap();

        let result = verify_tasks_in_repo(
            &repo,
            &home,
            VerifyOptions {
                run_id: Some("run".to_string()),
                target: "p1".to_string(),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Pending);
        assert_eq!(p1.phase, Some(TaskPhase::Implement));
        assert_eq!(p1.attempts, 1);
        assert_eq!(p1.last_exit_code, Some(5));
        assert!(p1.last_error.as_deref().unwrap().contains("unit"));
        assert_eq!(
            p1.extra
                .get("verificationFailureKind")
                .and_then(Value::as_str),
            Some("command_failed")
        );
        assert!(
            fs::read_to_string(p1.last_log.as_deref().unwrap())
                .unwrap()
                .contains("assertion failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_timeout_uses_exit_124_and_blocks_when_attempts_are_exhausted() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.max_attempts = Some(1);
        task.verification_commands = vec![verification_command("slow", "sleep 5", true, Some(1))];
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].phase = Some(TaskPhase::Verify);
        state.tasks[0].attempts = 1;
        store.write_run_state("run", &state).unwrap();

        let result = verify_tasks_in_repo(
            &repo,
            &home,
            VerifyOptions {
                run_id: Some("run".to_string()),
                target: "p1".to_string(),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Blocked);
        assert_eq!(p1.phase, Some(TaskPhase::Implement));
        assert_eq!(p1.last_exit_code, Some(124));
        assert_eq!(
            p1.extra
                .get("verificationFailureKind")
                .and_then(Value::as_str),
            Some("timeout")
        );
        assert!(
            fs::read_to_string(p1.last_log.as_deref().unwrap())
                .unwrap()
                .contains("timeout after 1 seconds")
        );
    }

    #[cfg(unix)]
    #[test]
    fn verify_external_blocker_is_distinguishable_and_blocks_without_retry() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.max_attempts = Some(3);
        task.verification_commands = vec![verification_command(
            "docker",
            "printf 'Could not find a valid Docker environment\\n' >&2; exit 1",
            true,
            Some(5),
        )];
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].phase = Some(TaskPhase::Verify);
        state.tasks[0].attempts = 1;
        store.write_run_state("run", &state).unwrap();

        let result = verify_tasks_in_repo(
            &repo,
            &home,
            VerifyOptions {
                run_id: Some("run".to_string()),
                target: "p1".to_string(),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Blocked);
        assert_eq!(p1.phase, Some(TaskPhase::Verify));
        assert_eq!(
            p1.extra
                .get("verificationFailureKind")
                .and_then(Value::as_str),
            Some("external_dependency_blocker")
        );
        assert!(
            p1.last_error
                .as_deref()
                .unwrap()
                .contains("external dependency blocker")
        );
        assert!(
            fs::read_to_string(p1.last_log.as_deref().unwrap())
                .unwrap()
                .contains("Docker environment")
        );
    }

    #[test]
    fn review_verdict_parser_requires_frontmatter_and_exact_verdict() {
        let approved = r#"---
task_id: p1
phase: review
verdict: APPROVED
reviewed_at: 2026-06-16T00:00:00Z
---

No issues.
"#;
        assert_eq!(
            parse_task_review_output(approved, "p1").unwrap(),
            ReviewVerdict::Approved
        );

        let changes = r#"---
task_id: p1
phase: review
verdict: CHANGES_REQUESTED
reviewed_at: 2026-06-16T00:00:00Z
---

- [MUST] Fix this.
"#;
        assert_eq!(
            parse_task_review_output(changes, "p1").unwrap(),
            ReviewVerdict::ChangesRequested
        );
        assert!(parse_task_review_output("verdict: APPROVED\n", "p1").is_err());
        assert!(
            parse_task_review_output(
                "---\ntask_id: p1\nphase: review\nreviewed_at: now\n---\n",
                "p1"
            )
            .unwrap_err()
            .contains("missing verdict")
        );
        assert!(
            parse_task_review_output(
                "---\ntask_id: p1\nphase: review\nverdict: PASS\n---\n",
                "p1"
            )
            .unwrap_err()
            .contains("invalid")
        );
        assert!(
            parse_task_review_output(
                "---\ntask_id: p1\nphase: review\nverdict: APPROVED\n---\n",
                "p1"
            )
            .unwrap_err()
            .contains("missing reviewed_at")
        );
        assert!(
            parse_task_review_output(
                "---\ntask_id: p1\nphase: review\nverdict: APPROVED\nreviewed_at: now\n---\n",
                "p1"
            )
            .unwrap_err()
            .contains("reviewed_at")
        );
    }

    #[cfg(unix)]
    #[test]
    fn task_review_approved_uses_read_only_sandbox_and_marks_reviewed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        write_review_ready_state(&store, "run", &task_file, TaskPhase::Review);

        let codex = fake_codex_script(
            temp.path(),
            &review_report_script("APPROVED", "No blocking issues."),
        );
        let codex_dir = codex.parent().unwrap().to_path_buf();
        let result = review_task_in_repo(
            &repo,
            &home,
            ReviewOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let args = fs::read_to_string(codex_dir.join("args.log")).unwrap();
        assert!(args.contains("read-only"));
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Reviewed);
        assert_eq!(p1.phase, Some(TaskPhase::Commit));
        assert_eq!(p1.last_verdict, Some(ReviewVerdict::Approved));
        assert!(p1.last_review_comments.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn changes_requested_persists_must_comments_and_injects_next_implement_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        write_review_ready_state(&store, "run", &task_file, TaskPhase::Review);

        let codex = fake_codex_script(
            temp.path(),
            &review_report_script(
                "CHANGES_REQUESTED",
                "## Findings\n\n### [MUST] Preserve malformed review as failure.\n\nThe parser must reject malformed review output and keep this remediation text.\n\n### [SHOULD] Add polish.\n\nThis should not be injected.",
            ),
        );
        let review = review_task_in_repo(
            &repo,
            &home,
            ReviewOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(review.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Pending);
        assert_eq!(p1.phase, Some(TaskPhase::Implement));
        assert_eq!(p1.review_attempts, 1);
        assert_eq!(p1.last_verdict, Some(ReviewVerdict::ChangesRequested));
        assert_eq!(
            p1.last_review_comments.as_deref(),
            Some(
                "### [MUST] Preserve malformed review as failure.\n\nThe parser must reject malformed review output and keep this remediation text."
            )
        );

        let implement_codex = fake_codex_script(
            temp.path(),
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
cp "$script_dir/stdin.log" "$script_dir/implement-prompt.log"
printf 'implemented review fixes\n' > "$last"
"#,
        );
        let implement_codex_dir = implement_codex.parent().unwrap().to_path_buf();
        let run = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: Some(implement_codex),
            },
        )
        .unwrap();

        assert_eq!(run.exit_code, 0);
        let prompt = fs::read_to_string(implement_codex_dir.join("implement-prompt.log")).unwrap();
        assert!(prompt.contains("### [MUST] Preserve malformed review as failure."));
        assert!(prompt.contains("keep this remediation text"));
        assert!(!prompt.contains("This should not be injected."));
    }

    #[cfg(unix)]
    #[test]
    fn malformed_review_output_enters_review_failed_and_never_approved() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        write_review_ready_state(&store, "run", &task_file, TaskPhase::Review);

        let codex = fake_codex_script(
            temp.path(),
            &review_report_without_verdict_script("No verdict here."),
        );
        let result = review_task_in_repo(
            &repo,
            &home,
            ReviewOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::ReviewFailed);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p1.review_attempts, 1);
        assert_eq!(p1.last_verdict, None);
        assert!(
            p1.last_error
                .as_deref()
                .unwrap()
                .contains("missing verdict")
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_review_failure_enters_review_failed_and_never_approved() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        write_review_ready_state(&store, "run", &task_file, TaskPhase::Review);

        let codex = fake_codex_script(
            temp.path(),
            r#"
printf 'review command failed\n' >&2
exit 9
"#,
        );
        let result = review_task_in_repo(
            &repo,
            &home,
            ReviewOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::ReviewFailed);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p1.review_attempts, 1);
        assert_eq!(p1.last_exit_code, Some(9));
        assert_eq!(p1.last_verdict, None);
        assert!(
            p1.last_error
                .as_deref()
                .unwrap()
                .contains("codex exited with status 9")
        );
    }

    #[cfg(unix)]
    #[test]
    fn empty_review_output_enters_review_failed_and_never_approved() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        write_review_ready_state(&store, "run", &task_file, TaskPhase::Review);

        let codex = fake_codex_script(
            temp.path(),
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
out=$(sed -n 's/^Review output path: //p' "$script_dir/stdin.log" | head -n 1)
mkdir -p "$(dirname "$out")"
: > "$out"
printf 'review complete\n' > "$last"
"#,
        );
        let result = review_task_in_repo(
            &repo,
            &home,
            ReviewOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::ReviewFailed);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p1.review_attempts, 1);
        assert_eq!(p1.last_verdict, None);
        assert!(p1.last_error.as_deref().unwrap().contains("review"));
    }

    #[cfg(unix)]
    #[test]
    fn review_failure_blocks_at_max_review_attempts() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.max_review_attempts = 1;
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        write_review_ready_state(&store, "run", &task_file, TaskPhase::Review);

        let codex = fake_codex_script(
            temp.path(),
            &review_report_without_verdict_script("No verdict here."),
        );
        let result = review_task_in_repo(
            &repo,
            &home,
            ReviewOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Blocked);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p1.last_verdict, None);
    }

    #[cfg(unix)]
    #[test]
    fn watch_does_not_auto_retry_review_failed_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.max_review_attempts = 2;
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::ReviewFailed;
        state.tasks[0].phase = Some(TaskPhase::Review);
        state.tasks[0].review_attempts = 1;
        state.tasks[0].last_error = Some("review output missing verdict".to_string());
        store.write_run_state("run", &state).unwrap();

        let codex = fake_codex_script(
            temp.path(),
            r#"
: > "$script_dir/invoked"
exit 99
"#,
        );
        let codex_dir = codex.parent().unwrap().to_path_buf();
        let result = watch_run_in_repo(
            &repo,
            &home,
            WatchOptions {
                run_id: Some("run".to_string()),
                interval_seconds: 0,
                max_failures: Some(1),
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0, "{}", result.message);
        assert!(!codex_dir.join("invoked").exists());
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::ReviewFailed);
        assert_eq!(p1.phase, Some(TaskPhase::Review));
        assert_eq!(p1.review_attempts, 1);
    }

    #[cfg(unix)]
    #[test]
    fn commit_false_marks_reviewed_task_done_without_git_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        let before = git_output(&repo, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Reviewed;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        state.tasks[0].last_verdict = Some(ReviewVerdict::Approved);
        store.write_run_state("run", &state).unwrap();

        let result = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: None,
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let after = git_output(&repo, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(after, before);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Done);
        assert_eq!(p1.phase, Some(TaskPhase::Done));
        assert_eq!(
            p1.extra.get("gitCommitReason").and_then(Value::as_str),
            Some("git.commit=false")
        );
        assert!(
            git_output(&repo, &["status", "--porcelain", "--untracked-files=all"])
                .unwrap()
                .contains("src/lib.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_uses_explicit_add_scope_preview_and_leaves_out_of_scope_changes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[git]
commit = true
add_include = ["src/**"]
add_exclude = ["src/generated/**"]
"#,
        )
        .unwrap();
        write_spec_and_commit(&repo, "spec.md");
        fs::create_dir_all(repo.join("src/generated")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn committed() {}\n").unwrap();
        fs::write(repo.join("src/generated/skip.rs"), "generated\n").unwrap();
        fs::write(repo.join("notes.md"), "outside scope\n").unwrap();

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Reviewed;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        state.tasks[0].last_verdict = Some(ReviewVerdict::Approved);
        store.write_run_state("run", &state).unwrap();

        let result = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: None,
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Done);
        assert!(p1.extra.get("gitCommit").and_then(Value::as_str).is_some());
        let files = p1
            .extra
            .get("gitCommitFiles")
            .and_then(Value::as_array)
            .unwrap()
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()
            .unwrap();
        assert_eq!(files, vec!["src/lib.rs"]);

        let events =
            fs::read_to_string(store.run_dir("run").unwrap().join("logs/events.log")).unwrap();
        assert!(events.contains("commit preview before staging"));
        assert!(events.contains("src/lib.rs"));
        let status =
            git_output(&repo, &["status", "--porcelain", "--untracked-files=all"]).unwrap();
        assert!(status.contains("notes.md"));
        assert!(status.contains("src/generated/skip.rs"));
        assert!(
            !git_output(&repo, &["diff", "--cached", "--name-only"])
                .unwrap()
                .contains("src/lib.rs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_empty_diff_marks_done_without_creating_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[git]
commit = true
add_include = ["src/**"]
"#,
        )
        .unwrap();
        write_spec_and_commit(&repo, "spec.md");
        let before = git_output(&repo, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Reviewed;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        store.write_run_state("run", &state).unwrap();

        let result = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: None,
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let after = git_output(&repo, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(after, before);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(
            p1.extra.get("gitCommitReason").and_then(Value::as_str),
            Some("empty-diff")
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_with_empty_add_scope_refuses_dirty_automatic_commit() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[git]
commit = true
"#,
        )
        .unwrap();
        write_spec_and_commit(&repo, "spec.md");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn dirty() {}\n").unwrap();

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Reviewed;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        store.write_run_state("run", &state).unwrap();

        let result = run_one_task_in_repo(
            &repo,
            &home,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: None,
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Blocked);
        assert_eq!(p1.phase, Some(TaskPhase::Commit));
        assert!(p1.last_error.as_deref().unwrap().contains("add_include"));
        assert!(
            git_output(&repo, &["diff", "--cached", "--name-only"])
                .unwrap()
                .trim()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn manual_staged_commit_refuses_pre_staged_run_store_files() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[git]
commit = true
add_required = false
"#,
        )
        .unwrap();
        write_spec_and_commit(&repo, "spec.md");
        let before = git_output(&repo, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .to_string();

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &repo).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Reviewed;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        state.tasks[0].last_verdict = Some(ReviewVerdict::Approved);
        store.write_run_state("run", &state).unwrap();
        git(&repo, ["add", ".codex/task-runner"]);

        let result = run_one_task_in_repo(
            &repo,
            &repo,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: None,
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let after = git_output(&repo, &["rev-list", "--count", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        assert_eq!(after, before);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.status, TaskStatus::Blocked);
        assert_eq!(p1.phase, Some(TaskPhase::Commit));
        assert!(
            p1.last_error
                .as_deref()
                .unwrap()
                .contains(".codex/task-runner")
        );
        assert!(
            git_output(&repo, &["diff", "--cached", "--name-only"])
                .unwrap()
                .contains(".codex/task-runner")
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_never_stages_global_run_store_even_when_home_is_inside_repo() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[git]
commit = true
add_include = [".codex/**", "src/**"]
"#,
        )
        .unwrap();
        write_spec_and_commit(&repo, "spec.md");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn committed() {}\n").unwrap();

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &repo).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Reviewed;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        state.tasks[0].last_verdict = Some(ReviewVerdict::Approved);
        store.write_run_state("run", &state).unwrap();

        let result = run_one_task_in_repo(
            &repo,
            &repo,
            RunTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                from: None,
                codex_bin: None,
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let committed_files =
            git_output(&repo, &["show", "--name-only", "--format=", "HEAD"]).unwrap();
        assert!(committed_files.contains("src/lib.rs"));
        assert!(!committed_files.contains(".codex/task-runner/runs"));
        assert!(
            git_output(&repo, &["status", "--porcelain", "--untracked-files=all"],)
                .unwrap()
                .contains(".codex/task-runner/runs")
        );
    }

    #[cfg(unix)]
    #[test]
    fn feature_branch_diff_includes_committed_branch_changes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        init_test_repo(&repo);
        fs::write(repo.join("base.txt"), "base\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "base"]);
        git(&repo, ["switch", "-c", "feat/run"]);
        fs::write(repo.join("feature.txt"), "feature\n").unwrap();
        git(&repo, ["add", "feature.txt"]);
        git(&repo, ["commit", "-m", "feature"]);

        let diff = feature_branch_diff(&repo, "main").unwrap();
        assert!(diff.contains("Feature branch (main...HEAD)"));
        assert!(diff.contains("feature.txt"));
    }

    #[test]
    fn run_store_hash_isolates_multiple_repositories_with_same_run_id() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        fs::create_dir_all(&repo_a).unwrap();
        fs::create_dir_all(&repo_b).unwrap();

        let store_a = RunStore::for_repo(&repo_a, &home).unwrap();
        let store_b = RunStore::for_repo(&repo_b, &home).unwrap();
        assert_ne!(store_a.repo_hash, store_b.repo_hash);
        assert_ne!(store_a.repo_runs_dir, store_b.repo_runs_dir);

        store_a
            .write_run_state("same", &RunState::default())
            .unwrap();
        store_b
            .write_run_state("same", &RunState::default())
            .unwrap();
        assert!(store_a.state_path("same").unwrap().exists());
        assert!(store_b.state_path("same").unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn final_review_changes_requested_updates_state_without_appending_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Done;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        store.write_run_state("run", &state).unwrap();

        let codex = fake_codex_script(
            temp.path(),
            &feature_review_report_script("CHANGES_REQUESTED", "- [MUST] Finish integration."),
        );
        let result = finalize_run_in_repo(
            &repo,
            &home,
            FinalizeOptions {
                run_id: Some("run".to_string()),
                no_cleanup: true,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        assert_eq!(
            state.feature_review_status,
            FeatureReviewStatus::ChangesRequested
        );
        assert_eq!(state.feature_review_attempts, 1);
        assert_eq!(store.read_task_file("run").unwrap().tasks.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn final_review_invalid_verdict_is_failed_not_approved() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Done;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        store.write_run_state("run", &state).unwrap();

        let codex = fake_codex_script(
            temp.path(),
            &feature_review_report_script("PASS", "Invalid verdict."),
        );
        let result = finalize_run_in_repo(
            &repo,
            &home,
            FinalizeOptions {
                run_id: Some("run".to_string()),
                no_cleanup: true,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 1);
        let state = store.read_run_state("run").unwrap();
        assert_eq!(state.feature_review_status, FeatureReviewStatus::Failed);
        assert_eq!(state.feature_review_attempts, 1);
        assert!(
            state
                .extra
                .get("featureReviewLastError")
                .and_then(Value::as_str)
                .unwrap()
                .contains("invalid")
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_review_approved_updates_spec_metadata_and_archives_run_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Done;
        state.tasks[0].phase = Some(TaskPhase::Done);
        store.write_run_state("run", &state).unwrap();
        let run_dir = store.run_dir("run").unwrap();
        assert!(run_dir.exists());

        let codex = fake_codex_script(
            temp.path(),
            &feature_review_report_script("APPROVED", "Feature is complete."),
        );
        let result = finalize_run_in_repo(
            &repo,
            &home,
            FinalizeOptions {
                run_id: Some("run".to_string()),
                no_cleanup: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.exit_code, 0);
        assert!(!run_dir.exists());
        let archive_root = store.repo_runs_dir.join("archive");
        let archived = fs::read_dir(&archive_root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(archived.len(), 1);
        assert!(archived[0].join("state.json").exists());
        let archived_state = read_run_state(&archived[0].join("state.json")).unwrap();
        assert_eq!(
            archived_state.feature_review_status,
            FeatureReviewStatus::Approved
        );

        let spec_text = fs::read_to_string(repo.join("spec.md")).unwrap();
        assert!(spec_text.contains("status: done"));
        assert!(spec_text.contains("finished_at: 20"));
    }

    fn verification_command(
        name: &str,
        command: &str,
        required: bool,
        timeout_seconds: Option<u64>,
    ) -> VerificationCommand {
        VerificationCommand {
            name: name.to_string(),
            command: command.to_string(),
            required,
            timeout_seconds,
        }
    }

    fn write_review_ready_state(
        store: &RunStore,
        run_id: &str,
        task_file: &TaskFile,
        phase: TaskPhase,
    ) {
        let run_dir = store.run_dir(run_id).unwrap();
        let analysis = analysis_output_path(&run_dir, "p1");
        let implementation = implementation_output_path(&run_dir, "p1");
        fs::create_dir_all(analysis.parent().unwrap()).unwrap();
        fs::write(&analysis, "analysis report\n").unwrap();
        fs::write(&implementation, "implementation summary\n").unwrap();

        let mut state = initial_run_state(task_file);
        state.tasks[0].status = TaskStatus::Pending;
        state.tasks[0].phase = Some(phase);
        state.tasks[0].attempts = 1;
        state.tasks[0].analysis_output = Some(analysis.display().to_string());
        state.tasks[0].output = Some(implementation.display().to_string());
        store.write_run_state(run_id, &state).unwrap();
    }

    fn review_report_script(verdict: &str, body: &str) -> String {
        format!(
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
out=$(sed -n 's/^Review output path: //p' "$script_dir/stdin.log" | head -n 1)
mkdir -p "$(dirname "$out")"
cat > "$out" <<'CODEX_REVIEW'
---
task_id: p1
phase: review
verdict: {verdict}
reviewed_at: 2026-06-16T00:00:00Z
---

{body}
CODEX_REVIEW
printf 'review complete\n' > "$last"
"#
        )
    }

    fn review_report_without_verdict_script(body: &str) -> String {
        format!(
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
out=$(sed -n 's/^Review output path: //p' "$script_dir/stdin.log" | head -n 1)
mkdir -p "$(dirname "$out")"
cat > "$out" <<'CODEX_REVIEW'
---
task_id: p1
phase: review
reviewed_at: 2026-06-16T00:00:00Z
---

{body}
CODEX_REVIEW
printf 'review complete\n' > "$last"
"#
        )
    }

    fn feature_review_report_script(verdict: &str, body: &str) -> String {
        format!(
            r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
out=$(sed -n 's/^Feature review output path: //p' "$script_dir/stdin.log" | head -n 1)
mkdir -p "$(dirname "$out")"
cat > "$out" <<'CODEX_REVIEW'
---
verdict: {verdict}
reviewed_at: 2026-06-16T00:00:00Z
---

{body}
CODEX_REVIEW
printf 'feature review complete\n' > "$last"
"#
        )
    }

    fn sample_task(id: &str, priority: u64) -> Task {
        Task {
            id: id.to_string(),
            priority,
            group: "scheduler".to_string(),
            title: format!("Task {id}"),
            max_attempts: Some(3),
            timeout_seconds: Some(5),
            output: format!("output/{id}.md"),
            prompt: format!("Implement {id}."),
            spec_file: None,
            depends_on: Vec::new(),
            review_criteria: Vec::new(),
            analyze_timeout_seconds: Some(5),
            analyze_required: true,
            require_review_approval: false,
            max_review_attempts: 2,
            review_timeout_seconds: Some(5),
            verification_commands: Vec::new(),
            extra: Map::new(),
        }
    }

    fn sample_run_task_file(run_id: &str, spec_file: &str, tasks: Vec<Task>) -> TaskFile {
        TaskFile {
            schema_version: 2,
            run_id: run_id.to_string(),
            branch: format!("feat/{run_id}"),
            spec_file: spec_file.to_string(),
            verification_commands: Vec::new(),
            tasks,
            extra: Map::new(),
        }
    }

    #[cfg(unix)]
    fn write_spec_and_commit(repo: &Path, spec_file: &str) {
        if let Some(parent) = Path::new(spec_file).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(repo.join(parent)).unwrap();
        }
        fs::write(repo.join(spec_file), "# Spec\n\nImplement scheduler.\n").unwrap();
        git(repo, ["add", "."]);
        git(repo, ["commit", "-m", "spec"]);
    }

    #[cfg(unix)]
    fn phase_aware_success_script() -> String {
        r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q 'Analysis output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Analysis output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  printf 'analysis report\n' > "$out"
  printf 'analysis complete\n' > "$last"
  exit 0
fi
if grep -q '^Review output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Review output path: //p' "$script_dir/stdin.log" | head -n 1)
  task=$(sed -n 's/^Task: \([^ ]*\) -.*/\1/p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  printf -- '---\ntask_id: %s\nphase: review\nverdict: APPROVED\nreviewed_at: 2026-06-16T00:00:00Z\n---\n\nApproved.\n' "$task" > "$out"
  printf 'review complete\n' > "$last"
  exit 0
fi
printf 'implementation summary\n' > "$last"
"#
        .to_string()
    }

    fn sample_common_variables() -> CommonPromptVariables {
        CommonPromptVariables {
            date: "2026-06-16".to_string(),
            repo_root: "/repo".to_string(),
            runner_dir: "/runs/run-1".to_string(),
            runner_dir_rel: ".codex/runs/run-1".to_string(),
            task_file: "/runs/run-1/tasks.json".to_string(),
            state_file: "/runs/run-1/state.json".to_string(),
            repo_map: "src/lib.rs".to_string(),
            agent_rules_path: "AGENTS.md".to_string(),
            overview_doc: "docs/overview.md".to_string(),
        }
    }

    fn sample_decompose_input() -> DecomposePromptInput {
        DecomposePromptInput {
            common: sample_common_variables(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            run_id: "run-1".to_string(),
            branch: "feat/run-1".to_string(),
            output_tasks_path: "/runs/run-1/tasks.json".to_string(),
        }
    }

    fn sample_analyze_input() -> AnalyzeTaskPromptInput {
        AnalyzeTaskPromptInput {
            common: sample_common_variables(),
            task_id: "p1".to_string(),
            title: "Task title".to_string(),
            task_prompt: "Analyze the task".to_string(),
            task_json: r#"{"id":"p1"}"#.to_string(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            output_analysis_path: "output/p1.analysis.md".to_string(),
        }
    }

    fn sample_implement_input() -> ImplementTaskPromptInput {
        ImplementTaskPromptInput {
            common: sample_common_variables(),
            task_id: "p1".to_string(),
            title: "Task title".to_string(),
            task_prompt: "Implement the task".to_string(),
            task_json: r#"{"id":"p1"}"#.to_string(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            analysis_output: "Analysis output".to_string(),
            last_review_comments: "Review comments".to_string(),
            last_error: "Previous error".to_string(),
            last_log_tail: "Log tail".to_string(),
        }
    }

    fn sample_review_input() -> ReviewTaskPromptInput {
        ReviewTaskPromptInput {
            common: sample_common_variables(),
            task_id: "p1".to_string(),
            title: "Task title".to_string(),
            task_prompt: "Review the task".to_string(),
            review_criteria: "Criterion".to_string(),
            git_diff: "diff --git a/src/lib.rs b/src/lib.rs".to_string(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            output_analysis_path: "output/p1.analysis.md".to_string(),
            output_impl_path: "output/p1.md".to_string(),
            output_review_path: "output/p1.review.md".to_string(),
            analysis_output: "Analysis output".to_string(),
            implementation_summary: "Implementation summary".to_string(),
        }
    }

    fn sample_review_feature_input() -> ReviewFeaturePromptInput {
        ReviewFeaturePromptInput {
            common: sample_common_variables(),
            run_id: "run-1".to_string(),
            branch: "feat/run-1".to_string(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            git_diff: "diff --git a/src/lib.rs b/src/lib.rs".to_string(),
            tasks_summaries: "Task summaries".to_string(),
            output_feature_review_path: "output/feature-review.1.md".to_string(),
        }
    }

    #[cfg(unix)]
    fn sample_codex_request(root: &Path) -> CodexRunRequest {
        CodexRunRequest {
            prompt: "prompt".to_string(),
            prompt_path: root.join("prompt.md"),
            stdout_log_path: root.join("stdout.log"),
            stderr_log_path: root.join("stderr.log"),
            last_message_path: root.join("last-message.md"),
            required_output_path: None,
            sandbox: "workspace-write".to_string(),
            approval: "never".to_string(),
            model: None,
            reasoning_effort: None,
            search: None,
            timeout_seconds: 5,
        }
    }

    #[cfg(unix)]
    fn fake_codex_script(root: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static NEXT_FAKE_CODEX_ID: AtomicUsize = AtomicUsize::new(0);

        let id = NEXT_FAKE_CODEX_ID.fetch_add(1, Ordering::Relaxed);
        let script_dir = root.join(format!("fake-codex-{id}"));
        fs::create_dir_all(&script_dir).unwrap();
        let script = script_dir.join("fake-codex.sh");
        fs::write(
            &script,
            format!(
                r#"#!/bin/sh
set -u
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\n' "$@" > "$script_dir/args.log"
cat > "$script_dir/stdin.log"
{body}
"#
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).unwrap();
        script
    }

    #[cfg(unix)]
    fn init_test_repo(repo: &Path) {
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::write(repo.join(".codex/task-runner.toml"), "").unwrap();
        git(repo, ["init", "-b", "main"]);
        git(repo, ["config", "user.email", "test@example.com"]);
        git(repo, ["config", "user.name", "Test User"]);
    }

    #[cfg(unix)]
    fn git<const N: usize>(repo: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    fn write_last_message_script(message: &str) -> String {
        format!(
            r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
cat > "$last" <<'CODEX_LAST_MESSAGE'
{message}
CODEX_LAST_MESSAGE
"#
        )
    }

    #[cfg(unix)]
    fn sample_decompose_json(run_id: &str, branch: &str, spec_file: &str) -> String {
        format!(
            r#"{{
  "version": 2,
  "runId": "{run_id}",
  "branch": "{branch}",
  "specFile": "{spec_file}",
  "tasks": [
    {{
      "id": "p1",
      "priority": 1,
      "group": "core",
      "title": "Do p1",
      "maxAttempts": 3,
      "timeoutSeconds": 1800,
      "output": "output/p1.md",
      "prompt": "Implement p1.",
      "dependsOn": [],
      "reviewCriteria": [],
      "analyzeTimeoutSeconds": 900,
      "maxReviewAttempts": 2,
      "reviewTimeoutSeconds": 600,
      "verificationCommands": []
    }}
  ]
}}"#
        )
    }

    fn legacy_fixture_root() -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from)?;
        let root = home
            .join("Developer/IdeaProjects/fullstack-base")
            .join("tests/fixtures/legacy-runs");
        root.exists().then_some(root)
    }
}
