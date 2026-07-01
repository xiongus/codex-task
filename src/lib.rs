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
use std::time::{Duration, Instant, SystemTime};
use thiserror::Error;

pub const PROMPT_TEMPLATE_NAMES: [&str; PromptTemplateKind::ALL.len()] = prompt_template_names();

const fn prompt_template_names() -> [&'static str; PromptTemplateKind::ALL.len()] {
    let mut names = [""; PromptTemplateKind::ALL.len()];
    let mut index = 0;
    while index < PromptTemplateKind::ALL.len() {
        names[index] = PromptTemplateKind::ALL[index].file_name();
        index += 1;
    }
    names
}

const FINAL_REVIEW_TYPES: [&str; 9] = [
    "architecture/integration",
    "business-scenario",
    "backward-compatibility",
    "code-defect",
    "performance",
    "security",
    "data-migration",
    "test-coverage",
    "docs-contract",
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
max_final_review_rounds = 2
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ProblemFramingStatus {
    #[default]
    Pending,
    Running,
    Clear,
    NeedsDecision,
    Resolved,
    Failed,
}

impl ProblemFramingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ProblemFramingStatus::Pending => "pending",
            ProblemFramingStatus::Running => "running",
            ProblemFramingStatus::Clear => "clear",
            ProblemFramingStatus::NeedsDecision => "needs_decision",
            ProblemFramingStatus::Resolved => "resolved",
            ProblemFramingStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemFramingState {
    #[serde(default)]
    pub status: ProblemFramingStatus,
    #[serde(rename = "reviewedAt", default)]
    pub reviewed_at: Option<String>,
    #[serde(rename = "resolvedAt", default)]
    pub resolved_at: Option<String>,
    #[serde(rename = "decisionPath", default)]
    pub decision_path: Option<String>,
    #[serde(rename = "resolvedProblemPath", default)]
    pub resolved_problem_path: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(rename = "lastError", default)]
    pub last_error: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for ProblemFramingState {
    fn default() -> Self {
        Self {
            status: ProblemFramingStatus::Pending,
            reviewed_at: None,
            resolved_at: None,
            decision_path: None,
            resolved_problem_path: None,
            output: None,
            last_error: None,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RequirementReviewStatus {
    #[default]
    Pending,
    Running,
    Clear,
    NeedsClarification,
    Resolved,
    Failed,
}

impl RequirementReviewStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            RequirementReviewStatus::Pending => "pending",
            RequirementReviewStatus::Running => "running",
            RequirementReviewStatus::Clear => "clear",
            RequirementReviewStatus::NeedsClarification => "needs_clarification",
            RequirementReviewStatus::Resolved => "resolved",
            RequirementReviewStatus::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequirementReviewState {
    #[serde(default)]
    pub status: RequirementReviewStatus,
    #[serde(rename = "reviewedAt", default)]
    pub reviewed_at: Option<String>,
    #[serde(rename = "resolvedAt", default)]
    pub resolved_at: Option<String>,
    #[serde(rename = "questionsPath", default)]
    pub questions_path: Option<String>,
    #[serde(rename = "answersPath", default)]
    pub answers_path: Option<String>,
    #[serde(rename = "resolvedSpecPath", default)]
    pub resolved_spec_path: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(rename = "lastError", default)]
    pub last_error: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Default for RequirementReviewState {
    fn default() -> Self {
        Self {
            status: RequirementReviewStatus::Pending,
            reviewed_at: None,
            resolved_at: None,
            questions_path: None,
            answers_path: None,
            resolved_spec_path: None,
            output: None,
            last_error: None,
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FindingSeverity {
    #[serde(rename = "MUST_FIX")]
    MustFix,
    #[serde(rename = "SHOULD_FIX")]
    ShouldFix,
    #[serde(rename = "INFO")]
    Info,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalReviewFinding {
    pub id: String,
    #[serde(rename = "reviewType")]
    pub review_type: String,
    pub severity: FindingSeverity,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FinalReviewShardState {
    #[serde(rename = "reviewType")]
    pub review_type: String,
    pub status: FeatureReviewStatus,
    #[serde(default)]
    pub verdict: Option<ReviewVerdict>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(rename = "stdoutLog", default)]
    pub stdout_log: Option<String>,
    #[serde(rename = "stderrLog", default)]
    pub stderr_log: Option<String>,
    #[serde(rename = "lastMessage", default)]
    pub last_message: Option<String>,
    #[serde(rename = "findingsCount", default)]
    pub findings_count: usize,
    #[serde(rename = "lastError", default)]
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FinalReviewRoundState {
    pub round: u64,
    pub status: FeatureReviewStatus,
    #[serde(rename = "startedAt", default)]
    pub started_at: Option<String>,
    #[serde(rename = "finishedAt", default)]
    pub finished_at: Option<String>,
    #[serde(rename = "changeMapPath", default)]
    pub change_map_path: Option<String>,
    #[serde(rename = "reviewPlanPath", default)]
    pub review_plan_path: Option<String>,
    #[serde(rename = "findingsPath", default)]
    pub findings_path: Option<String>,
    #[serde(rename = "aggregateOutput", default)]
    pub aggregate_output: Option<String>,
    #[serde(rename = "finalFixTaskId", default)]
    pub final_fix_task_id: Option<String>,
    #[serde(default)]
    pub shards: Vec<FinalReviewShardState>,
    #[serde(rename = "remainingMustFix", default)]
    pub remaining_must_fix: Vec<FinalReviewFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalReviewState {
    #[serde(default = "default_feature_review_status")]
    pub status: FeatureReviewStatus,
    #[serde(rename = "maxRounds", default)]
    pub max_rounds: u64,
    #[serde(rename = "changeMapPath", default)]
    pub change_map_path: Option<String>,
    #[serde(rename = "reviewPlanPath", default)]
    pub review_plan_path: Option<String>,
    #[serde(rename = "findingsPath", default)]
    pub findings_path: Option<String>,
    #[serde(default)]
    pub rounds: Vec<FinalReviewRoundState>,
    #[serde(rename = "remainingMustFix", default)]
    pub remaining_must_fix: Vec<FinalReviewFinding>,
    #[serde(rename = "lastError", default)]
    pub last_error: Option<String>,
}

impl Default for FinalReviewState {
    fn default() -> Self {
        Self {
            status: FeatureReviewStatus::Pending,
            max_rounds: 0,
            change_map_path: None,
            review_plan_path: None,
            findings_path: None,
            rounds: Vec::new(),
            remaining_must_fix: Vec::new(),
            last_error: None,
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
                        "timeoutSeconds" => timeout_seconds = map.next_value()?,
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
    pub max_final_review_rounds: u64,
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
                max_final_review_rounds: 2,
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
            if let Some(value) = runner.max_final_review_rounds {
                self.runner.max_final_review_rounds = value;
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
        if self.runner.max_final_review_rounds == 0 {
            return Err(AppError::Config(
                "runner.max_final_review_rounds must be greater than zero".to_string(),
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
    pub max_final_review_rounds: Option<u64>,
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
    pub spec_paths: Vec<PathBuf>,
    pub run_id: Option<String>,
    pub branch: Option<String>,
    pub resume: bool,
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeOptions {
    pub run_id: String,
    pub codex_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StartResult {
    pub run_id: String,
    pub branch: String,
    pub spec_file: String,
    pub run_dir: PathBuf,
    pub visible_run_dir: PathBuf,
    pub tasks_path: PathBuf,
    pub state_path: PathBuf,
    pub metadata_path: PathBuf,
    pub problem_status: String,
    pub decision_path: Option<PathBuf>,
    pub resolved_problem_path: Option<PathBuf>,
    pub requirement_status: String,
    pub questions_path: Option<PathBuf>,
    pub answers_path: Option<PathBuf>,
    pub resolved_spec_path: Option<PathBuf>,
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
    #[serde(rename = "specFiles", default)]
    pub spec_files: Vec<String>,
    #[serde(rename = "problemFraming", default)]
    pub problem_framing: ProblemFramingState,
    #[serde(rename = "resolvedProblemFile", default)]
    pub resolved_problem_file: Option<String>,
    #[serde(rename = "requirementReview", default)]
    pub requirement_review: RequirementReviewState,
    #[serde(rename = "resolvedSpecFile", default)]
    pub resolved_spec_file: Option<String>,
    #[serde(default)]
    pub phases: Vec<RunPhaseMetadata>,
    #[serde(rename = "activePhase", default)]
    pub active_phase: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunPhaseMetadata {
    pub id: String,
    #[serde(rename = "specFile")]
    pub spec_file: String,
    #[serde(rename = "specFiles", default)]
    pub spec_files: Vec<String>,
    #[serde(rename = "problemFraming", default)]
    pub problem_framing: ProblemFramingState,
    #[serde(rename = "resolvedProblemFile", default)]
    pub resolved_problem_file: Option<String>,
    #[serde(rename = "requirementReview", default)]
    pub requirement_review: RequirementReviewState,
    #[serde(rename = "resolvedSpecFile", default)]
    pub resolved_spec_file: Option<String>,
    #[serde(default)]
    pub decomposed: bool,
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

pub fn resume_run(
    start: &Path,
    options: ResumeOptions,
) -> std::result::Result<StartResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    resume_run_in_repo(&repo_root, &home, options)
}

pub fn resume_run_in_repo(
    repo_root: &Path,
    home: &Path,
    options: ResumeOptions,
) -> std::result::Result<StartResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    RunId::parse(&options.run_id)?;
    let run_id = options.run_id;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    let metadata = store.read_metadata(&run_id)?;

    let mut state = store.read_run_state(&run_id)?;
    if state.problem_framing.status == ProblemFramingStatus::NeedsDecision {
        return resume_problem_decision(&context, &store, &run_id, &metadata, options.codex_bin);
    }
    if state.problem_framing.status == ProblemFramingStatus::Resolved
        && state.requirement_review.status == RequirementReviewStatus::Clear
    {
        return resume_decompose_after_reviews_clear(
            &context,
            &store,
            &run_id,
            &metadata,
            options.codex_bin,
        );
    }
    if state.requirement_review.status != RequirementReviewStatus::NeedsClarification {
        let tasks_path = store.tasks_path(&run_id)?;
        if tasks_path.exists() {
            let task_file = store.read_task_file(&run_id)?;
            ensure_task_file_matches_run(
                &task_file,
                &metadata.run_id,
                &metadata.branch,
                &task_file.spec_file,
            )?;
            return start_result(
                &context,
                &store,
                &run_id,
                &metadata.branch,
                &task_file.spec_file,
                true,
                Vec::new(),
            );
        }
        return Err(AppError::Runtime(format!(
            "run {run_id} is not waiting for user input; problemFraming={}, requirementReview={}",
            state.problem_framing.status.as_str(),
            state.requirement_review.status.as_str()
        )));
    }

    let active_phase_id = metadata.active_phase.clone();
    let visible_dir = phase_visible_dir(&context.repo_root, &run_id, active_phase_id.as_deref());
    fs::create_dir_all(&visible_dir).map_err(|err| {
        AppError::Io(format!(
            "failed to create visible run directory {}: {err}",
            visible_dir.display()
        ))
    })?;
    let questions_path = state
        .requirement_review
        .questions_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| visible_dir.join("questions.md"));
    let answers_path = state
        .requirement_review
        .answers_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| visible_dir.join("answers.md"));
    let questions = fs::read_to_string(&questions_path).map_err(|err| {
        AppError::Io(format!(
            "failed to read {}: {err}",
            questions_path.display()
        ))
    })?;
    let answers = fs::read_to_string(&answers_path)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", answers_path.display())))?;
    if !answers_file_is_filled(&answers) {
        return Err(AppError::Runtime(format!(
            "{} has no answers between codex-task answers markers",
            answers_path.display()
        )));
    }

    let (base_spec_file, original_spec) = active_spec_for_requirement(&context, &metadata, &state)?;
    let resolved_spec_path = visible_dir.join("resolved-spec.md");
    let run_dir = store.run_dir(&run_id)?;
    let prompt = render_resolve_requirement_prompt(ResolveRequirementRender {
        context: &context,
        store: &store,
        run_id: &run_id,
        spec_file: &base_spec_file,
        spec: &original_spec,
        questions: &questions,
        answers: &answers,
        output_path: &resolved_spec_path,
    })?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir.join("prompts/resolve-requirement.md"),
        stdout_log_path: run_dir.join("logs/resolve-requirement.stdout.log"),
        stderr_log_path: run_dir.join("logs/resolve-requirement.stderr.log"),
        last_message_path: run_dir.join("logs/resolve-requirement.last-message.md"),
        required_output_path: Some(resolved_spec_path.clone()),
        fallback_required_output_from_last_message: true,
        sandbox: context.merged.runner.sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_analyze_timeout_seconds,
    };
    build_executor(&context, options.codex_bin.clone())
        .execute(&request)
        .map_err(|err| {
            AppError::Runtime(format!(
                "{err}; logs: stdout={}, stderr={}, last={}",
                err.stdout_log_path.display(),
                err.stderr_log_path.display(),
                err.last_message_path.display()
            ))
        })?;
    let resolved_spec = SpecDocument::read(&resolved_spec_path)?;
    if resolved_spec.body.trim().is_empty() {
        return Err(AppError::Runtime(format!(
            "resolved spec {} is empty",
            resolved_spec_path.display()
        )));
    }
    let resolved_spec_file = repo_relative_slash_path(&context.repo_root, &resolved_spec_path)?;

    state.requirement_review.status = RequirementReviewStatus::Resolved;
    state.requirement_review.resolved_at = Some(current_timestamp()?);
    state.requirement_review.resolved_spec_path = Some(resolved_spec_path.display().to_string());
    state.requirement_review.last_error = None;
    store.write_run_state(&run_id, &state)?;
    write_phase_requirement_state(
        &store,
        &run_id,
        active_phase_id.as_deref(),
        &state.requirement_review,
        Some(resolved_spec_file.clone()),
    )?;
    append_event_log(
        &run_dir,
        &format!(
            "requirement answers resolved; resolved_spec={}",
            resolved_spec_path.display()
        ),
    )?;

    let mut warnings = Vec::new();
    if let Some(phase_id) = active_phase_id {
        let append = store.tasks_path(&run_id)?.exists();
        let outcome = prepare_run_phase(
            &context,
            &store,
            &run_id,
            &phase_id,
            append,
            options.codex_bin,
            &mut warnings,
        )?;
        if outcome == PhasePrepareOutcome::Waiting {
            return start_result(
                &context,
                &store,
                &run_id,
                &metadata.branch,
                &resolved_spec_file,
                false,
                warnings,
            );
        }
    } else {
        let resolved_spec_files = vec![resolved_spec_file.clone()];
        run_decompose(DecomposeRun {
            context: &context,
            store: &store,
            run_id: &run_id,
            branch: &metadata.branch,
            phase_id: None,
            spec_file: &resolved_spec_file,
            spec_files: &resolved_spec_files,
            spec: &resolved_spec,
            append: false,
            codex_bin: options.codex_bin,
            warnings: &mut warnings,
        })?;
    }

    start_result(
        &context,
        &store,
        &run_id,
        &metadata.branch,
        &resolved_spec_file,
        false,
        warnings,
    )
}

fn resume_problem_decision(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    metadata: &RunMetadata,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<StartResult, AppError> {
    let mut state = store.read_run_state(run_id)?;
    let active_phase_id = metadata.active_phase.clone();
    let visible_dir = phase_visible_dir(&context.repo_root, run_id, active_phase_id.as_deref());
    fs::create_dir_all(&visible_dir).map_err(|err| {
        AppError::Io(format!(
            "failed to create visible run directory {}: {err}",
            visible_dir.display()
        ))
    })?;
    let decision_path = state
        .problem_framing
        .decision_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| visible_dir.join("decision.md"));
    let decision = fs::read_to_string(&decision_path).map_err(|err| {
        AppError::Io(format!("failed to read {}: {err}", decision_path.display()))
    })?;
    if !decision_file_is_filled(&decision) {
        return Err(AppError::Runtime(format!(
            "{} has no decision between codex-task decision markers",
            decision_path.display()
        )));
    }
    let options = decision_file_options(&decision)?;

    let (source_spec_file, original_spec) = if let Some(phase_id) = active_phase_id.as_deref() {
        let phase = find_phase_metadata(metadata, phase_id)?;
        (
            phase.spec_file.clone(),
            read_phase_spec_document(context, phase)?,
        )
    } else {
        (
            metadata.spec_file.clone(),
            SpecDocument::read(&context.repo_root.join(&metadata.spec_file))?,
        )
    };
    let resolved_problem_path = visible_dir.join("resolved-problem.md");
    let run_dir = store.run_dir(run_id)?;
    let prompt = render_resolve_problem_prompt(ResolveProblemRender {
        context,
        store,
        run_id,
        spec_file: &source_spec_file,
        spec: &original_spec,
        options: &options,
        decision: &decision,
        output_path: &resolved_problem_path,
    })?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir.join("prompts/resolve-problem.md"),
        stdout_log_path: run_dir.join("logs/resolve-problem.stdout.log"),
        stderr_log_path: run_dir.join("logs/resolve-problem.stderr.log"),
        last_message_path: run_dir.join("logs/resolve-problem.last-message.md"),
        required_output_path: Some(resolved_problem_path.clone()),
        fallback_required_output_from_last_message: true,
        sandbox: context.merged.runner.sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_analyze_timeout_seconds,
    };
    build_executor(context, codex_bin.clone())
        .execute(&request)
        .map_err(|err| {
            AppError::Runtime(format!(
                "{err}; logs: stdout={}, stderr={}, last={}",
                err.stdout_log_path.display(),
                err.stderr_log_path.display(),
                err.last_message_path.display()
            ))
        })?;
    let resolved_problem = SpecDocument::read(&resolved_problem_path)?;
    if resolved_problem.body.trim().is_empty() {
        return Err(AppError::Runtime(format!(
            "resolved problem {} is empty",
            resolved_problem_path.display()
        )));
    }
    let resolved_problem_file =
        repo_relative_slash_path(&context.repo_root, &resolved_problem_path)?;

    state.problem_framing.status = ProblemFramingStatus::Resolved;
    state.problem_framing.resolved_at = Some(current_timestamp()?);
    state.problem_framing.resolved_problem_path = Some(resolved_problem_path.display().to_string());
    state.problem_framing.last_error = None;
    store.write_run_state(run_id, &state)?;
    write_phase_problem_framing_state(
        store,
        run_id,
        active_phase_id.as_deref(),
        &state.problem_framing,
        Some(resolved_problem_file.clone()),
    )?;
    append_event_log(
        &run_dir,
        &format!(
            "problem framing decision resolved; resolved_problem={}",
            resolved_problem_path.display()
        ),
    )?;

    let mut warnings = Vec::new();
    let review_status = run_requirement_review(
        context,
        store,
        run_id,
        active_phase_id.as_deref(),
        &resolved_problem_file,
        &resolved_problem,
        codex_bin.clone(),
    )?;
    if review_status != RequirementReviewStatus::Clear {
        return start_result(
            context,
            store,
            run_id,
            &metadata.branch,
            &resolved_problem_file,
            false,
            warnings,
        );
    }

    if let Some(phase_id) = active_phase_id {
        let append = store.tasks_path(run_id)?.exists();
        let outcome = prepare_run_phase(
            context,
            store,
            run_id,
            &phase_id,
            append,
            codex_bin,
            &mut warnings,
        )?;
        if outcome == PhasePrepareOutcome::Waiting {
            return start_result(
                context,
                store,
                run_id,
                &metadata.branch,
                &resolved_problem_file,
                false,
                warnings,
            );
        }
    } else {
        let resolved_problem_files = vec![resolved_problem_file.clone()];
        run_decompose(DecomposeRun {
            context,
            store,
            run_id,
            branch: &metadata.branch,
            phase_id: None,
            spec_file: &resolved_problem_file,
            spec_files: &resolved_problem_files,
            spec: &resolved_problem,
            append: false,
            codex_bin,
            warnings: &mut warnings,
        })?;
    }

    start_result(
        context,
        store,
        run_id,
        &metadata.branch,
        &resolved_problem_file,
        false,
        warnings,
    )
}

fn resume_decompose_after_reviews_clear(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    metadata: &RunMetadata,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<StartResult, AppError> {
    let run_dir = store.run_dir(run_id)?;
    let (spec_file, spec_files, spec) = active_spec_for_decompose(context, metadata)?;
    let active_phase_id = metadata.active_phase.as_deref();
    let mut warnings = Vec::new();
    append_event_log(
        &run_dir,
        &format!("resuming task decomposition; spec={spec_file}"),
    )?;
    run_decompose(DecomposeRun {
        context,
        store,
        run_id,
        branch: &metadata.branch,
        phase_id: active_phase_id,
        spec_file: &spec_file,
        spec_files: &spec_files,
        spec: &spec,
        append: store.tasks_path(run_id)?.exists(),
        codex_bin,
        warnings: &mut warnings,
    })?;

    start_result(
        context,
        store,
        run_id,
        &metadata.branch,
        &spec_file,
        true,
        warnings,
    )
}

fn active_spec_for_requirement(
    context: &ConfigContext,
    metadata: &RunMetadata,
    state: &RunState,
) -> std::result::Result<(String, SpecDocument), AppError> {
    if state.problem_framing.status == ProblemFramingStatus::Resolved
        && let Some(path) = &state.problem_framing.resolved_problem_path
    {
        let path = PathBuf::from(path);
        let spec = SpecDocument::read(&path)?;
        let spec_file = repo_relative_slash_path(&context.repo_root, &path)?;
        return Ok((spec_file, spec));
    }
    if let Some(phase_id) = &metadata.active_phase {
        let phase = find_phase_metadata(metadata, phase_id)?;
        return phase_spec_for_requirement(context, phase);
    }
    let spec = SpecDocument::read(&context.repo_root.join(&metadata.spec_file))?;
    Ok((metadata.spec_file.clone(), spec))
}

fn active_spec_for_decompose(
    context: &ConfigContext,
    metadata: &RunMetadata,
) -> std::result::Result<(String, Vec<String>, SpecDocument), AppError> {
    if let Some(spec_file) = &metadata.resolved_spec_file {
        let spec = SpecDocument::read(&context.repo_root.join(spec_file))?;
        return Ok((spec_file.clone(), vec![spec_file.clone()], spec));
    }
    if let Some(spec_file) = &metadata.resolved_problem_file {
        let spec = SpecDocument::read(&context.repo_root.join(spec_file))?;
        return Ok((spec_file.clone(), vec![spec_file.clone()], spec));
    }
    if let Some(phase_id) = &metadata.active_phase {
        let phase = find_phase_metadata(metadata, phase_id)?;
        return phase_spec_for_decompose(context, phase);
    }
    let spec_files = normalize_spec_files(&metadata.spec_file, &metadata.spec_files);
    let spec = read_combined_spec_document(context, &spec_files)?;
    Ok((metadata.spec_file.clone(), spec_files, spec))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhasePrepareOutcome {
    Decomposed,
    Waiting,
}

fn prepare_run_phase(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    phase_id: &str,
    append: bool,
    codex_bin: Option<PathBuf>,
    warnings: &mut Vec<String>,
) -> std::result::Result<PhasePrepareOutcome, AppError> {
    let mut metadata = store.read_metadata(run_id)?;
    let phase = find_phase_metadata(&metadata, phase_id)?.clone();
    metadata.active_phase = Some(phase.id.clone());
    metadata.problem_framing = phase.problem_framing.clone();
    metadata.resolved_problem_file = phase.resolved_problem_file.clone();
    metadata.requirement_review = phase.requirement_review.clone();
    metadata.resolved_spec_file = phase.resolved_spec_file.clone();
    store.write_metadata(run_id, &metadata)?;

    let phase_artifact_id = phase_artifact_id(&metadata, &phase);
    let mut state = store.read_run_state(run_id)?;
    state.problem_framing = phase.problem_framing.clone();
    state.requirement_review = phase.requirement_review.clone();
    store.write_run_state(run_id, &state)?;

    let phase_spec = read_phase_spec_document(context, &phase)?;
    let problem_status = if phase.problem_framing.status == ProblemFramingStatus::Clear
        || phase.problem_framing.status == ProblemFramingStatus::Resolved
    {
        phase.problem_framing.status
    } else {
        run_problem_framing(
            context,
            store,
            run_id,
            phase_artifact_id,
            &phase.spec_file,
            &phase_spec,
            codex_bin.clone(),
        )?
    };
    if problem_status != ProblemFramingStatus::Clear
        && problem_status != ProblemFramingStatus::Resolved
    {
        return Ok(PhasePrepareOutcome::Waiting);
    }

    let metadata = store.read_metadata(run_id)?;
    let phase = find_phase_metadata(&metadata, phase_id)?.clone();
    let (review_spec_file, review_spec) = phase_spec_for_requirement(context, &phase)?;
    let requirement_status = if phase.requirement_review.status == RequirementReviewStatus::Clear
        || phase.requirement_review.status == RequirementReviewStatus::Resolved
    {
        phase.requirement_review.status
    } else {
        run_requirement_review(
            context,
            store,
            run_id,
            phase_artifact_id,
            &review_spec_file,
            &review_spec,
            codex_bin.clone(),
        )?
    };
    if requirement_status != RequirementReviewStatus::Clear
        && requirement_status != RequirementReviewStatus::Resolved
    {
        return Ok(PhasePrepareOutcome::Waiting);
    }

    let metadata = store.read_metadata(run_id)?;
    let phase = find_phase_metadata(&metadata, phase_id)?.clone();
    let (decompose_spec_file, decompose_spec_files, decompose_spec) =
        phase_spec_for_decompose(context, &phase)?;
    run_decompose(DecomposeRun {
        context,
        store,
        run_id,
        branch: &metadata.branch,
        phase_id: phase_artifact_id,
        spec_file: &decompose_spec_file,
        spec_files: &decompose_spec_files,
        spec: &decompose_spec,
        append,
        codex_bin,
        warnings,
    })?;
    Ok(PhasePrepareOutcome::Decomposed)
}

fn phase_artifact_id<'a>(
    metadata: &'a RunMetadata,
    phase: &'a RunPhaseMetadata,
) -> Option<&'a str> {
    (!metadata.phases.is_empty()).then_some(phase.id.as_str())
}

fn read_phase_spec_document(
    context: &ConfigContext,
    phase: &RunPhaseMetadata,
) -> std::result::Result<SpecDocument, AppError> {
    read_combined_spec_document(
        context,
        &normalize_spec_files(&phase.spec_file, &phase.spec_files),
    )
}

fn phase_spec_for_requirement(
    context: &ConfigContext,
    phase: &RunPhaseMetadata,
) -> std::result::Result<(String, SpecDocument), AppError> {
    if phase.problem_framing.status == ProblemFramingStatus::Resolved
        && let Some(spec_file) = &phase.resolved_problem_file
    {
        let spec = SpecDocument::read(&context.repo_root.join(spec_file))?;
        return Ok((spec_file.clone(), spec));
    }
    Ok((
        phase.spec_file.clone(),
        read_phase_spec_document(context, phase)?,
    ))
}

fn phase_spec_for_decompose(
    context: &ConfigContext,
    phase: &RunPhaseMetadata,
) -> std::result::Result<(String, Vec<String>, SpecDocument), AppError> {
    if let Some(spec_file) = &phase.resolved_spec_file {
        let spec = SpecDocument::read(&context.repo_root.join(spec_file))?;
        return Ok((spec_file.clone(), vec![spec_file.clone()], spec));
    }
    if let Some(spec_file) = &phase.resolved_problem_file {
        let spec = SpecDocument::read(&context.repo_root.join(spec_file))?;
        return Ok((spec_file.clone(), vec![spec_file.clone()], spec));
    }
    let spec_files = normalize_spec_files(&phase.spec_file, &phase.spec_files);
    let spec = read_combined_spec_document(context, &spec_files)?;
    Ok((phase.spec_file.clone(), spec_files, spec))
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

    let spec_inputs = resolve_spec_files(
        cwd,
        &context.repo_root,
        &options.spec_path,
        &options.spec_paths,
    )?;
    let (spec_file, spec_path, mut spec) = spec_inputs
        .first()
        .cloned()
        .expect("resolve_spec_files always returns at least one spec");
    let spec_files = spec_inputs
        .iter()
        .map(|(path, _, _)| path.clone())
        .collect::<Vec<_>>();
    let phases = run_phases_from_spec_inputs(&spec_inputs);
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
    let existing_metadata = if metadata_path.exists() {
        Some(store.read_metadata(&run_id)?)
    } else {
        None
    };
    let metadata = RunMetadata {
        schema_version: 1,
        run_id: run_id.clone(),
        branch: branch.clone(),
        spec_file: spec_file.clone(),
        spec_files: spec_files.clone(),
        phases: existing_metadata
            .as_ref()
            .filter(|metadata| !metadata.phases.is_empty())
            .map(|metadata| metadata.phases.clone())
            .unwrap_or_else(|| phases.clone()),
        active_phase: existing_metadata
            .as_ref()
            .and_then(|metadata| metadata.active_phase.clone()),
        problem_framing: existing_metadata
            .as_ref()
            .map(|metadata| metadata.problem_framing.clone())
            .unwrap_or_default(),
        resolved_problem_file: existing_metadata
            .as_ref()
            .and_then(|metadata| metadata.resolved_problem_file.clone()),
        requirement_review: existing_metadata
            .as_ref()
            .map(|metadata| metadata.requirement_review.clone())
            .unwrap_or_default(),
        resolved_spec_file: existing_metadata
            .as_ref()
            .and_then(|metadata| metadata.resolved_spec_file.clone()),
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
        return start_result(
            &context, &store, &run_id, &branch, &spec_file, true, warnings,
        );
    }

    let first_phase_id = metadata
        .phases
        .first()
        .map(|phase| phase.id.clone())
        .ok_or_else(|| AppError::Config("run has no phases".to_string()))?;
    let outcome = prepare_run_phase(
        &context,
        &store,
        &run_id,
        &first_phase_id,
        false,
        options.codex_bin,
        &mut warnings,
    )?;
    if outcome == PhasePrepareOutcome::Waiting {
        let metadata = store.read_metadata(&run_id)?;
        return start_result(
            &context,
            &store,
            &run_id,
            &branch,
            metadata
                .active_phase
                .as_deref()
                .and_then(|phase_id| find_phase_metadata(&metadata, phase_id).ok())
                .map(|phase| phase.spec_file.as_str())
                .unwrap_or(&spec_file),
            false,
            warnings,
        );
    }

    start_result(
        &context, &store, &run_id, &branch, &spec_file, false, warnings,
    )
}

fn start_result(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    branch: &str,
    spec_file: &str,
    resumed: bool,
    warnings: Vec<String>,
) -> std::result::Result<StartResult, AppError> {
    let state = store.read_run_state(run_id).unwrap_or_default();
    let problem = state.problem_framing;
    let requirement = state.requirement_review;
    Ok(StartResult {
        run_id: run_id.to_string(),
        branch: branch.to_string(),
        spec_file: spec_file.to_string(),
        run_dir: store.run_dir(run_id)?,
        visible_run_dir: project_task_run_dir(&context.repo_root, run_id),
        tasks_path: store.tasks_path(run_id)?,
        state_path: store.state_path(run_id)?,
        metadata_path: store.metadata_path(run_id)?,
        problem_status: problem.status.as_str().to_string(),
        decision_path: problem.decision_path.map(PathBuf::from),
        resolved_problem_path: problem.resolved_problem_path.map(PathBuf::from),
        requirement_status: requirement.status.as_str().to_string(),
        questions_path: requirement.questions_path.map(PathBuf::from),
        answers_path: requirement.answers_path.map(PathBuf::from),
        resolved_spec_path: requirement.resolved_spec_path.map(PathBuf::from),
        resumed,
        warnings,
    })
}

fn run_problem_framing(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    phase_id: Option<&str>,
    spec_file: &str,
    spec: &SpecDocument,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<ProblemFramingStatus, AppError> {
    let run_dir = store.run_dir(run_id)?;
    let visible_dir = phase_visible_dir(&context.repo_root, run_id, phase_id);
    fs::create_dir_all(&visible_dir).map_err(|err| {
        AppError::Io(format!(
            "failed to create visible run directory {}: {err}",
            visible_dir.display()
        ))
    })?;

    let output_path = run_dir
        .join("output")
        .join(phase_file_stem(phase_id, "problem-framing.md"));
    let decision_path = visible_dir.join("decision.md");
    let now = current_timestamp()?;
    let mut problem = ProblemFramingState {
        status: ProblemFramingStatus::Running,
        decision_path: Some(decision_path.display().to_string()),
        output: Some(output_path.display().to_string()),
        ..ProblemFramingState::default()
    };
    write_phase_problem_framing_state(store, run_id, phase_id, &problem, None)?;

    let prompt =
        render_problem_framing_prompt(context, store, run_id, spec_file, spec, &output_path)?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(phase_file_stem(phase_id, "problem-framing.md")),
        stdout_log_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "problem-framing.stdout.log")),
        stderr_log_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "problem-framing.stderr.log")),
        last_message_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "problem-framing.last-message.md")),
        required_output_path: Some(output_path.clone()),
        fallback_required_output_from_last_message: true,
        sandbox: context.merged.runner.review_sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_review_timeout_seconds,
    };

    let _output = match build_executor(context, codex_bin).execute(&request) {
        Ok(output) => output,
        Err(err) => {
            let err = *err;
            let message = err.message.clone();
            let stdout_log = err.stdout_log_path.clone();
            let stderr_log = err.stderr_log_path.clone();
            let last_message = err.last_message_path.clone();
            problem.status = ProblemFramingStatus::Failed;
            problem.last_error = Some(message.clone());
            problem.output = Some(err.stderr_log_path.display().to_string());
            write_phase_problem_framing_state(store, run_id, phase_id, &problem, None)?;
            return Err(AppError::Runtime(format!(
                "{message}; logs: stdout={}, stderr={}, last={}",
                stdout_log.display(),
                stderr_log.display(),
                last_message.display()
            )));
        }
    };

    let decision = match parse_problem_framing_output_file(&output_path) {
        Ok(decision) => decision,
        Err(err) => {
            problem.status = ProblemFramingStatus::Failed;
            problem.last_error = Some(err.clone());
            write_phase_problem_framing_state(store, run_id, phase_id, &problem, None)?;
            return Err(AppError::Runtime(format!(
                "invalid problem framing output: {err}; raw output preserved at {}",
                output_path.display()
            )));
        }
    };

    problem.status = decision.status;
    problem.reviewed_at = Some(now);
    problem.output = Some(output_path.display().to_string());
    problem.last_error = None;

    match decision.status {
        ProblemFramingStatus::Clear => {
            remove_file_if_exists(&decision_path).map_err(|err| {
                AppError::Io(format!(
                    "failed to remove {}: {err}",
                    decision_path.display()
                ))
            })?;
            problem.decision_path = None;
            append_event_log(&run_dir, "problem framing clear")?;
        }
        ProblemFramingStatus::NeedsDecision => {
            write_decision_file(&decision_path, run_id, &decision.body)?;
            append_event_log(
                &run_dir,
                &format!(
                    "problem framing needs decision; decision={}",
                    decision_path.display()
                ),
            )?;
        }
        _ => {}
    }

    write_phase_problem_framing_state(store, run_id, phase_id, &problem, None)?;
    Ok(decision.status)
}

fn run_requirement_review(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    phase_id: Option<&str>,
    spec_file: &str,
    spec: &SpecDocument,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<RequirementReviewStatus, AppError> {
    let run_dir = store.run_dir(run_id)?;
    let visible_dir = phase_visible_dir(&context.repo_root, run_id, phase_id);
    fs::create_dir_all(&visible_dir).map_err(|err| {
        AppError::Io(format!(
            "failed to create visible run directory {}: {err}",
            visible_dir.display()
        ))
    })?;

    let output_path = run_dir
        .join("output")
        .join(phase_file_stem(phase_id, "requirement-review.md"));
    let questions_path = visible_dir.join("questions.md");
    let answers_path = visible_dir.join("answers.md");
    let now = current_timestamp()?;
    let mut requirement = RequirementReviewState {
        status: RequirementReviewStatus::Running,
        questions_path: Some(questions_path.display().to_string()),
        answers_path: Some(answers_path.display().to_string()),
        output: Some(output_path.display().to_string()),
        ..RequirementReviewState::default()
    };
    write_phase_requirement_state(store, run_id, phase_id, &requirement, None)?;

    let prompt =
        render_requirement_review_prompt(context, store, run_id, spec_file, spec, &output_path)?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(phase_file_stem(phase_id, "requirement-review.md")),
        stdout_log_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "requirement-review.stdout.log")),
        stderr_log_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "requirement-review.stderr.log")),
        last_message_path: run_dir.join("logs").join(phase_file_stem(
            phase_id,
            "requirement-review.last-message.md",
        )),
        required_output_path: Some(output_path.clone()),
        fallback_required_output_from_last_message: true,
        sandbox: context.merged.runner.review_sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_review_timeout_seconds,
    };

    let _output = match build_executor(context, codex_bin).execute(&request) {
        Ok(output) => output,
        Err(err) => {
            let err = *err;
            let message = err.message.clone();
            let stdout_log = err.stdout_log_path.clone();
            let stderr_log = err.stderr_log_path.clone();
            let last_message = err.last_message_path.clone();
            requirement.status = RequirementReviewStatus::Failed;
            requirement.last_error = Some(message.clone());
            requirement.output = Some(err.stderr_log_path.display().to_string());
            write_phase_requirement_state(store, run_id, phase_id, &requirement, None)?;
            return Err(AppError::Runtime(format!(
                "{message}; logs: stdout={}, stderr={}, last={}",
                stdout_log.display(),
                stderr_log.display(),
                last_message.display()
            )));
        }
    };

    let decision = match parse_requirement_review_output_file(&output_path) {
        Ok(decision) => decision,
        Err(err) => {
            requirement.status = RequirementReviewStatus::Failed;
            requirement.last_error = Some(err.clone());
            write_phase_requirement_state(store, run_id, phase_id, &requirement, None)?;
            return Err(AppError::Runtime(format!(
                "invalid requirement review output: {err}; raw output preserved at {}",
                output_path.display()
            )));
        }
    };

    requirement.status = decision.status;
    requirement.reviewed_at = Some(now);
    requirement.output = Some(output_path.display().to_string());
    requirement.last_error = None;

    match decision.status {
        RequirementReviewStatus::Clear => {
            remove_file_if_exists(&questions_path).map_err(|err| {
                AppError::Io(format!(
                    "failed to remove {}: {err}",
                    questions_path.display()
                ))
            })?;
            remove_file_if_exists(&answers_path).map_err(|err| {
                AppError::Io(format!(
                    "failed to remove {}: {err}",
                    answers_path.display()
                ))
            })?;
            requirement.questions_path = None;
            requirement.answers_path = None;
            append_event_log(&run_dir, "requirement review clear")?;
        }
        RequirementReviewStatus::NeedsClarification => {
            write_questions_file(&questions_path, run_id, &decision.body)?;
            write_answers_file(&answers_path, run_id, &decision.body)?;
            append_event_log(
                &run_dir,
                &format!(
                    "requirement review needs clarification; questions={}, answers={}",
                    questions_path.display(),
                    answers_path.display()
                ),
            )?;
        }
        _ => {}
    }

    write_phase_requirement_state(store, run_id, phase_id, &requirement, None)?;
    Ok(decision.status)
}

struct DecomposeRun<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    branch: &'a str,
    phase_id: Option<&'a str>,
    spec_file: &'a str,
    spec_files: &'a [String],
    spec: &'a SpecDocument,
    append: bool,
    codex_bin: Option<PathBuf>,
    warnings: &'a mut Vec<String>,
}

fn run_decompose(input: DecomposeRun<'_>) -> std::result::Result<(), AppError> {
    let DecomposeRun {
        context,
        store,
        run_id,
        branch,
        phase_id,
        spec_file,
        spec_files,
        spec,
        append,
        codex_bin,
        warnings,
    } = input;
    let run_dir = store.run_dir(run_id)?;
    let prompt = render_decompose_prompt(context, store, run_id, branch, spec_file, spec)?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(phase_file_stem(phase_id, "decompose-feature.md")),
        stdout_log_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "decompose.stdout.log")),
        stderr_log_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "decompose.stderr.log")),
        last_message_path: run_dir
            .join("logs")
            .join(phase_file_stem(phase_id, "decompose.last-message.md")),
        required_output_path: None,
        fallback_required_output_from_last_message: false,
        sandbox: context.merged.runner.sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_analyze_timeout_seconds,
    };
    let codex_output = build_executor(context, codex_bin)
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

    let mut new_task_file = parsed.task_file;
    new_task_file.schema_version = 2;
    new_task_file.run_id = run_id.to_string();
    new_task_file.branch = branch.to_string();
    new_task_file.spec_file = spec_file.to_string();
    new_task_file.spec_files = normalize_spec_files(spec_file, spec_files);
    normalize_task_scopes(&mut new_task_file);
    validate_task_file(&new_task_file)?;

    let tasks_path = store.tasks_path(run_id)?;
    let task_file = if append && tasks_path.exists() {
        let mut existing = store.read_task_file(run_id)?;
        let mut merged_spec_files = existing.spec_files.clone();
        merged_spec_files.extend(new_task_file.spec_files.clone());
        existing.spec_files = normalize_spec_files(&existing.spec_file, &merged_spec_files);
        existing
            .verification_commands
            .extend(new_task_file.verification_commands);
        existing.tasks.extend(new_task_file.tasks);
        normalize_task_scopes(&mut existing);
        existing
    } else {
        new_task_file
    };
    validate_task_file(&task_file)?;
    store.write_task_file(run_id, &task_file)?;
    mark_phase_decomposed(store, run_id, phase_id)?;
    let previous_state = store.read_run_state(run_id).unwrap_or_default();
    let mut next_state = previous_state;
    ensure_state_matches_tasks(&task_file, &mut next_state)?;
    store.write_run_state(run_id, &next_state)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProblemFramingDecision {
    status: ProblemFramingStatus,
    body: String,
}

fn parse_problem_framing_output_file(
    path: &Path,
) -> std::result::Result<ProblemFramingDecision, String> {
    let raw = fs::read_to_string(path).map_err(|err| {
        format!(
            "failed to read problem framing output {}: {err}",
            path.display()
        )
    })?;
    let (frontmatter, body) = parse_review_frontmatter(&raw)?;
    require_reviewed_at(&frontmatter)?;
    let status = match frontmatter.get("verdict").as_deref() {
        Some("CLEAR") => ProblemFramingStatus::Clear,
        Some("NEEDS_DECISION") => ProblemFramingStatus::NeedsDecision,
        Some(value) => return Err(format!("problem framing verdict={value} is invalid")),
        None => return Err("problem framing output missing verdict".to_string()),
    };
    if status == ProblemFramingStatus::NeedsDecision && body.trim().is_empty() {
        return Err("problem framing options are empty".to_string());
    }
    Ok(ProblemFramingDecision { status, body })
}

fn write_problem_framing_state(
    store: &RunStore,
    run_id: &str,
    problem: &ProblemFramingState,
) -> std::result::Result<(), AppError> {
    store.update_run_state(run_id, |state| {
        state.problem_framing = problem.clone();
        Ok(())
    })
}

fn update_metadata_problem_framing(
    store: &RunStore,
    run_id: &str,
    problem: &ProblemFramingState,
    resolved_problem_file: Option<String>,
) -> std::result::Result<(), AppError> {
    let mut metadata = store.read_metadata(run_id)?;
    metadata.problem_framing = problem.clone();
    if resolved_problem_file.is_some() {
        metadata.resolved_problem_file = resolved_problem_file;
    }
    store.write_metadata(run_id, &metadata)
}

fn write_phase_problem_framing_state(
    store: &RunStore,
    run_id: &str,
    phase_id: Option<&str>,
    problem: &ProblemFramingState,
    resolved_problem_file: Option<String>,
) -> std::result::Result<(), AppError> {
    write_problem_framing_state(store, run_id, problem)?;
    let Some(phase_id) = phase_id else {
        return update_metadata_problem_framing(store, run_id, problem, resolved_problem_file);
    };
    let mut metadata = store.read_metadata(run_id)?;
    metadata.problem_framing = problem.clone();
    if resolved_problem_file.is_some() {
        metadata.resolved_problem_file = resolved_problem_file.clone();
    }
    let phase = find_phase_metadata_mut(&mut metadata, phase_id)?;
    phase.problem_framing = problem.clone();
    if resolved_problem_file.is_some() {
        phase.resolved_problem_file = resolved_problem_file;
    }
    store.write_metadata(run_id, &metadata)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequirementReviewDecision {
    status: RequirementReviewStatus,
    body: String,
}

fn parse_requirement_review_output_file(
    path: &Path,
) -> std::result::Result<RequirementReviewDecision, String> {
    let raw = fs::read_to_string(path).map_err(|err| {
        format!(
            "failed to read requirement review output {}: {err}",
            path.display()
        )
    })?;
    let (frontmatter, body) = parse_review_frontmatter(&raw)?;
    require_reviewed_at(&frontmatter)?;
    let status = match frontmatter.get("verdict").as_deref() {
        Some("CLEAR") => RequirementReviewStatus::Clear,
        Some("NEEDS_CLARIFICATION") => RequirementReviewStatus::NeedsClarification,
        Some(value) => return Err(format!("requirement review verdict={value} is invalid")),
        None => return Err("requirement review output missing verdict".to_string()),
    };
    if status == RequirementReviewStatus::NeedsClarification && body.trim().is_empty() {
        return Err("requirement review questions are empty".to_string());
    }
    Ok(RequirementReviewDecision { status, body })
}

fn write_requirement_state(
    store: &RunStore,
    run_id: &str,
    requirement: &RequirementReviewState,
) -> std::result::Result<(), AppError> {
    store.update_run_state(run_id, |state| {
        state.requirement_review = requirement.clone();
        Ok(())
    })
}

fn update_metadata_requirement(
    store: &RunStore,
    run_id: &str,
    requirement: &RequirementReviewState,
    resolved_spec_file: Option<String>,
) -> std::result::Result<(), AppError> {
    let mut metadata = store.read_metadata(run_id)?;
    metadata.requirement_review = requirement.clone();
    if resolved_spec_file.is_some() {
        metadata.resolved_spec_file = resolved_spec_file;
    }
    store.write_metadata(run_id, &metadata)
}

fn write_phase_requirement_state(
    store: &RunStore,
    run_id: &str,
    phase_id: Option<&str>,
    requirement: &RequirementReviewState,
    resolved_spec_file: Option<String>,
) -> std::result::Result<(), AppError> {
    write_requirement_state(store, run_id, requirement)?;
    let Some(phase_id) = phase_id else {
        return update_metadata_requirement(store, run_id, requirement, resolved_spec_file);
    };
    let mut metadata = store.read_metadata(run_id)?;
    metadata.requirement_review = requirement.clone();
    if resolved_spec_file.is_some() {
        metadata.resolved_spec_file = resolved_spec_file.clone();
    }
    let phase = find_phase_metadata_mut(&mut metadata, phase_id)?;
    phase.requirement_review = requirement.clone();
    if resolved_spec_file.is_some() {
        phase.resolved_spec_file = resolved_spec_file;
    }
    store.write_metadata(run_id, &metadata)
}

fn find_phase_metadata_mut<'a>(
    metadata: &'a mut RunMetadata,
    phase_id: &str,
) -> std::result::Result<&'a mut RunPhaseMetadata, AppError> {
    metadata
        .phases
        .iter_mut()
        .find(|phase| phase.id == phase_id)
        .ok_or_else(|| AppError::Config(format!("unknown phase: {phase_id}")))
}

fn find_phase_metadata<'a>(
    metadata: &'a RunMetadata,
    phase_id: &str,
) -> std::result::Result<&'a RunPhaseMetadata, AppError> {
    metadata
        .phases
        .iter()
        .find(|phase| phase.id == phase_id)
        .ok_or_else(|| AppError::Config(format!("unknown phase: {phase_id}")))
}

fn mark_phase_decomposed(
    store: &RunStore,
    run_id: &str,
    phase_id: Option<&str>,
) -> std::result::Result<(), AppError> {
    let mut metadata = store.read_metadata(run_id)?;
    let phase_id = phase_id
        .map(str::to_string)
        .or_else(|| metadata.active_phase.clone());
    let Some(phase_id) = phase_id else {
        return Ok(());
    };
    let phase = find_phase_metadata_mut(&mut metadata, &phase_id)?;
    phase.decomposed = true;
    metadata.active_phase = None;
    store.write_metadata(run_id, &metadata)
}

fn project_task_run_dir(repo_root: &Path, run_id: &str) -> PathBuf {
    repo_root.join(".codex/task-runs").join(run_id)
}

fn phase_visible_dir(repo_root: &Path, run_id: &str, phase_id: Option<&str>) -> PathBuf {
    match phase_id {
        Some(phase_id) => project_task_run_dir(repo_root, run_id)
            .join("phases")
            .join(phase_id),
        None => project_task_run_dir(repo_root, run_id),
    }
}

fn phase_file_stem(phase_id: Option<&str>, name: &str) -> String {
    match phase_id {
        Some(phase_id) => format!("{phase_id}.{name}"),
        None => name.to_string(),
    }
}

fn write_decision_file(
    path: &Path,
    run_id: &str,
    options: &str,
) -> std::result::Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| AppError::Io(format!("failed to create {}: {err}", parent.display())))?;
    }
    let text = format!(
        "# Decision for {run_id}\n\nChoose the approach between the markers, then run `codex-task resume --run-id {run_id}`.\n\n## Options\n\n{}\n\n## Decision\n\n<!-- codex-task:decision:start -->\nTODO\n<!-- codex-task:decision:end -->\n",
        options.trim()
    );
    fs::write(path, text)
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
}

fn write_questions_file(
    path: &Path,
    run_id: &str,
    questions: &str,
) -> std::result::Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| AppError::Io(format!("failed to create {}: {err}", parent.display())))?;
    }
    let text = format!("# Questions for {run_id}\n\n{}\n", questions.trim());
    fs::write(path, text)
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
}

fn write_answers_file(
    path: &Path,
    run_id: &str,
    questions: &str,
) -> std::result::Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| AppError::Io(format!("failed to create {}: {err}", parent.display())))?;
    }
    let text = format!(
        "# Answers for {run_id}\n\nFill in the answers between the markers, then run `codex-task resume --run-id {run_id}`.\n\n## Questions\n\n{}\n\n## Answers\n\n<!-- codex-task:answers:start -->\nTODO\n<!-- codex-task:answers:end -->\n",
        questions.trim()
    );
    fs::write(path, text)
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
}

fn answers_file_is_filled(raw: &str) -> bool {
    marker_section(raw, "answers").is_some_and(section_is_filled)
}

fn decision_file_is_filled(raw: &str) -> bool {
    marker_section(raw, "decision").is_some_and(section_is_filled)
}

fn decision_file_options(raw: &str) -> std::result::Result<String, AppError> {
    let options = markdown_section_between(raw, "Options", "Decision")
        .ok_or_else(|| AppError::Config("decision file missing ## Options section".to_string()))?;
    if options.is_empty() {
        return Err(AppError::Config(
            "decision file options section is empty".to_string(),
        ));
    }
    Ok(options)
}

fn markdown_section_between(raw: &str, start_heading: &str, end_heading: &str) -> Option<String> {
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut in_section = false;
    let mut out = Vec::new();
    for line in normalized.lines() {
        if markdown_h2_is(line, start_heading) {
            in_section = true;
            out.clear();
            continue;
        }
        if in_section && markdown_h2_is(line, end_heading) {
            return Some(out.join("\n").trim().to_string());
        }
        if in_section {
            out.push(line);
        }
    }
    None
}

fn markdown_h2_is(line: &str, name: &str) -> bool {
    let trimmed = line.trim();
    trimmed
        .strip_prefix("##")
        .is_some_and(|rest| !rest.starts_with('#') && rest.trim() == name)
}

fn marker_section<'a>(raw: &'a str, name: &str) -> Option<&'a str> {
    let start = format!("<!-- codex-task:{name}:start -->");
    let end = format!("<!-- codex-task:{name}:end -->");
    let (start_index, _) = raw.rmatch_indices(&start).next()?;
    let after_start = &raw[start_index + start.len()..];
    let (section, _) = after_start.split_once(&end)?;
    Some(section)
}

fn section_is_filled(section: &str) -> bool {
    let trimmed = section.trim();
    !trimmed.is_empty() && trimmed != "TODO"
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

    fn from_spec_files(files: &[(String, SpecDocument)]) -> Self {
        if files.len() == 1 {
            return files[0].1.clone();
        }
        let mut body = String::new();
        for (index, (path, document)) in files.iter().enumerate() {
            if index > 0 {
                body.push_str("\n\n");
            }
            body.push_str("# Spec: ");
            body.push_str(path);
            body.push_str("\n\n");
            body.push_str(document.body.trim());
            body.push('\n');
        }
        Self {
            frontmatter: None,
            body,
        }
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

fn resolve_spec_files(
    cwd: &Path,
    repo_root: &Path,
    primary: &Path,
    additional: &[PathBuf],
) -> std::result::Result<Vec<(String, PathBuf, SpecDocument)>, AppError> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for input_path in std::iter::once(primary).chain(additional.iter().map(PathBuf::as_path)) {
        for path in resolve_spec_input_paths(cwd, repo_root, input_path)? {
            let relative = repo_relative_slash_path(repo_root, &path)?;
            if !seen.insert(relative.clone()) {
                return Err(AppError::Config(format!(
                    "duplicate spec file in start input: {relative}"
                )));
            }
            let spec = SpecDocument::read(&path)?;
            out.push((relative, path, spec));
        }
    }
    Ok(out)
}

fn resolve_spec_input_paths(
    cwd: &Path,
    repo_root: &Path,
    input_path: &Path,
) -> std::result::Result<Vec<PathBuf>, AppError> {
    let raw_path = if input_path.is_absolute() {
        input_path.to_path_buf()
    } else {
        cwd.join(input_path)
    };
    let canonical = raw_path.canonicalize().map_err(|err| {
        AppError::Io(format!(
            "failed to canonicalize {}: {err}",
            raw_path.display()
        ))
    })?;
    if canonical.is_file() {
        return Ok(vec![resolve_spec_path(cwd, repo_root, input_path)?]);
    }
    if !canonical.is_dir() {
        return Err(AppError::Config(format!(
            "spec path is not a file or directory: {}",
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
            "spec directory {} is outside repo {}",
            canonical.display(),
            repo.display()
        ))
    })?;
    let mut paths = Vec::new();
    for entry in fs::read_dir(&canonical)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", canonical.display())))?
    {
        let entry = entry.map_err(|err| {
            AppError::Io(format!(
                "failed to read {} entry: {err}",
                canonical.display()
            ))
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
        {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.is_empty() {
        return Err(AppError::Config(format!(
            "spec directory has no Markdown files: {}",
            canonical.display()
        )));
    }
    Ok(paths)
}

fn run_phases_from_spec_inputs(
    inputs: &[(String, PathBuf, SpecDocument)],
) -> Vec<RunPhaseMetadata> {
    inputs
        .iter()
        .map(|(spec_file, _, _)| {
            let id = phase_id_from_spec_file(spec_file);
            RunPhaseMetadata {
                id,
                spec_file: spec_file.clone(),
                spec_files: vec![spec_file.clone()],
                problem_framing: ProblemFramingState::default(),
                resolved_problem_file: None,
                requirement_review: RequirementReviewState::default(),
                resolved_spec_file: None,
                decomposed: false,
                extra: Map::new(),
            }
        })
        .collect()
}

fn phase_id_from_spec_file(spec_file: &str) -> String {
    Path::new(spec_file)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(sanitize_task_output_stem)
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| sanitize_task_output_stem(spec_file))
}

fn normalize_spec_files(primary: &str, spec_files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for spec_file in std::iter::once(primary).chain(spec_files.iter().map(String::as_str)) {
        let spec_file = spec_file.trim();
        if spec_file.is_empty() || out.iter().any(|existing| existing == spec_file) {
            continue;
        }
        out.push(spec_file.to_string());
    }
    out
}

fn normalize_task_scopes(task_file: &mut TaskFile) {
    let run_spec_files = normalize_spec_files(&task_file.spec_file, &task_file.spec_files);
    task_file.spec_files = run_spec_files.clone();

    for task in &mut task_file.tasks {
        if task.phase.trim().is_empty() {
            task.phase = task.group.clone();
        }

        if task.spec_files.is_empty() {
            task.spec_files = match &task.spec_file {
                Some(spec_file) => normalize_spec_files(spec_file, &[]),
                None => run_spec_files.clone(),
            };
            continue;
        }

        let primary = task
            .spec_file
            .as_deref()
            .unwrap_or_else(|| task.spec_files[0].as_str());
        task.spec_files = normalize_spec_files(primary, &task.spec_files);
    }
}

fn read_combined_spec_document(
    context: &ConfigContext,
    spec_files: &[String],
) -> std::result::Result<SpecDocument, AppError> {
    let mut documents = Vec::new();
    for spec_file in spec_files {
        let spec = SpecDocument::read(&context.repo_root.join(spec_file))?;
        documents.push((spec_file.clone(), spec));
    }
    Ok(SpecDocument::from_spec_files(&documents))
}

fn task_spec_files(task: &Task, task_file: &TaskFile) -> Vec<String> {
    if !task.spec_files.is_empty() {
        return normalize_spec_files(
            task.spec_file
                .as_deref()
                .unwrap_or_else(|| task.spec_files[0].as_str()),
            &task.spec_files,
        );
    }
    if let Some(spec_file) = &task.spec_file {
        return normalize_spec_files(spec_file, &[]);
    }
    normalize_spec_files(&task_file.spec_file, &task_file.spec_files)
}

fn task_spec_label(spec_files: &[String]) -> String {
    spec_files.join(", ")
}

fn task_phase_label(task: &Task) -> &str {
    let phase = task.phase.trim();
    if phase.is_empty() {
        task.group.trim()
    } else {
        phase
    }
}

fn task_in_watch_scope(task_file: &TaskFile, task: &Task, scope: &WatchScope) -> bool {
    if let Some(group) = &scope.group
        && task.group != *group
    {
        return false;
    }
    if let Some(phase) = &scope.phase
        && task_phase_label(task) != phase
    {
        return false;
    }
    if let Some(until_phase) = &scope.until_phase {
        let Some(target_rank) = phase_rank(task_file, until_phase) else {
            return true;
        };
        let Some(task_rank) = phase_rank(task_file, task_phase_label(task)) else {
            return false;
        };
        if task_rank > target_rank {
            return false;
        }
    }
    true
}

fn phase_rank(task_file: &TaskFile, phase: &str) -> Option<usize> {
    let phase = phase.trim();
    let mut rank = 0;
    let mut seen = BTreeSet::new();
    for task in &task_file.tasks {
        let candidate = task_phase_label(task);
        if !seen.insert(candidate) {
            continue;
        }
        if candidate == phase {
            return Some(rank);
        }
        rank += 1;
    }
    None
}

fn validate_watch_scope(
    task_file: &TaskFile,
    metadata: &RunMetadata,
    scope: &WatchScope,
) -> std::result::Result<(), AppError> {
    if let Some(group) = &scope.group
        && !task_file.tasks.iter().any(|task| task.group == *group)
    {
        return Err(AppError::Config(format!("unknown task group: {group}")));
    }
    if let Some(phase) = &scope.phase
        && phase_rank(task_file, phase).is_none()
        && metadata_phase_rank(metadata, phase).is_none()
    {
        return Err(AppError::Config(format!("unknown task phase: {phase}")));
    }
    if let Some(phase) = &scope.until_phase
        && phase_rank(task_file, phase).is_none()
        && metadata_phase_rank(metadata, phase).is_none()
    {
        return Err(AppError::Config(format!("unknown task phase: {phase}")));
    }
    Ok(())
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

fn repo_relative_slash_path_for_output(
    repo_root: &Path,
    path: &Path,
) -> std::result::Result<String, AppError> {
    let repo = repo_root.canonicalize().map_err(|err| {
        AppError::Io(format!(
            "failed to canonicalize repo root {}: {err}",
            repo_root.display()
        ))
    })?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };
    let parent = absolute.parent().ok_or_else(|| {
        AppError::Config(format!("output path has no parent: {}", absolute.display()))
    })?;
    let file_name = absolute.file_name().ok_or_else(|| {
        AppError::Config(format!(
            "output path has no file name: {}",
            absolute.display()
        ))
    })?;
    let parent = parent.canonicalize().map_err(|err| {
        AppError::Io(format!(
            "failed to canonicalize output parent {}: {err}",
            parent.display()
        ))
    })?;
    let normalized = parent.join(file_name);
    let relative = normalized.strip_prefix(&repo).map_err(|_| {
        AppError::Config(format!(
            "{} is outside repo {}",
            normalized.display(),
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
            && (metadata.spec_file == spec_file
                || metadata
                    .spec_files
                    .iter()
                    .any(|candidate| candidate == spec_file))
        {
            return Ok(Some(metadata));
        }
        if let Ok(task_file) = store.read_task_file(&id)
            && (task_file.spec_file == spec_file
                || task_file
                    .spec_files
                    .iter()
                    .any(|candidate| candidate == spec_file))
        {
            return Ok(Some(RunMetadata {
                schema_version: 1,
                run_id: task_file.run_id,
                branch: task_file.branch,
                spec_file: task_file.spec_file,
                spec_files: task_file.spec_files,
                phases: Vec::new(),
                active_phase: None,
                problem_framing: ProblemFramingState::default(),
                resolved_problem_file: None,
                requirement_review: RequirementReviewState::default(),
                resolved_spec_file: None,
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

fn render_requirement_review_prompt(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    spec_file: &str,
    spec: &SpecDocument,
    output_path: &Path,
) -> std::result::Result<String, AppError> {
    let input = RequirementReviewPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        spec_file: spec_file.to_string(),
        feature_spec: spec.body.clone(),
        output_review_path: output_path.display().to_string(),
    };
    let template = load_prompt_template(context, PromptTemplateKind::RequirementReview)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

fn render_problem_framing_prompt(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    spec_file: &str,
    spec: &SpecDocument,
    output_path: &Path,
) -> std::result::Result<String, AppError> {
    let input = ProblemFramingPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        spec_file: spec_file.to_string(),
        feature_spec: spec.body.clone(),
        output_review_path: output_path.display().to_string(),
    };
    let template = load_prompt_template(context, PromptTemplateKind::ProblemFraming)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

struct ResolveProblemRender<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    spec_file: &'a str,
    spec: &'a SpecDocument,
    options: &'a str,
    decision: &'a str,
    output_path: &'a Path,
}

fn render_resolve_problem_prompt(
    input: ResolveProblemRender<'_>,
) -> std::result::Result<String, AppError> {
    let ResolveProblemRender {
        context,
        store,
        run_id,
        spec_file,
        spec,
        options,
        decision,
        output_path,
    } = input;
    let input = ResolveProblemPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        spec_file: spec_file.to_string(),
        feature_spec: spec.body.clone(),
        options: options.to_string(),
        decision: decision.to_string(),
        output_resolved_problem_path: repo_relative_slash_path_for_output(
            &context.repo_root,
            output_path,
        )?,
    };
    let template = load_prompt_template(context, PromptTemplateKind::ResolveProblem)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

struct ResolveRequirementRender<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    spec_file: &'a str,
    spec: &'a SpecDocument,
    questions: &'a str,
    answers: &'a str,
    output_path: &'a Path,
}

fn render_resolve_requirement_prompt(
    input: ResolveRequirementRender<'_>,
) -> std::result::Result<String, AppError> {
    let ResolveRequirementRender {
        context,
        store,
        run_id,
        spec_file,
        spec,
        questions,
        answers,
        output_path,
    } = input;
    let input = ResolveRequirementPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        spec_file: spec_file.to_string(),
        feature_spec: spec.body.clone(),
        questions: questions.to_string(),
        answers: answers.to_string(),
        output_resolved_spec_path: repo_relative_slash_path_for_output(
            &context.repo_root,
            output_path,
        )?,
    };
    let template = load_prompt_template(context, PromptTemplateKind::ResolveRequirement)
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
    let mut value =
        serde_json::from_str::<Value>(raw).map_err(|err| format!("invalid JSON: {err}"))?;
    normalize_decompose_task_file_value(&mut value);
    let mut task_file = serde_json::from_value::<TaskFile>(value)
        .map_err(|err| format!("invalid tasks.json schema: {err}"))?;
    normalize_task_scopes(&mut task_file);
    Ok(task_file)
}

fn normalize_decompose_task_file_value(value: &mut Value) {
    let Some(tasks) = value.get_mut("tasks").and_then(Value::as_array_mut) else {
        return;
    };
    for task in tasks {
        let Some(task) = task.as_object_mut() else {
            continue;
        };
        if !task.contains_key("prompt")
            && let Some(description) = task.get("description").cloned()
        {
            task.insert("prompt".to_string(), description);
        }
        if !task.contains_key("output") {
            let id = task.get("id").and_then(Value::as_str).unwrap_or("task");
            task.insert(
                "output".to_string(),
                Value::String(format!("output/{}.md", sanitize_task_output_stem(id))),
            );
        }
        if !task.contains_key("phase")
            && let Some(group) = task.get("group").cloned()
        {
            task.insert("phase".to_string(), group);
        }
        if !task.contains_key("specFiles")
            && let Some(spec_file) = task.get("specFile").cloned()
        {
            task.insert("specFiles".to_string(), Value::Array(vec![spec_file]));
        }
    }
}

fn sanitize_task_output_stem(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_separator = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_separator = false;
        } else if !last_was_separator {
            out.push('-');
            last_was_separator = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "task".to_string()
    } else {
        trimmed.to_string()
    }
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
        problem_framing: ProblemFramingState::default(),
        requirement_review: RequirementReviewState::default(),
        final_review: FinalReviewState::default(),
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
    ProblemFraming,
    ResolveProblem,
    RequirementReview,
    ResolveRequirement,
    DecomposeFeature,
    AnalyzeTask,
    ImplementTask,
    ReviewTask,
    ReviewFeature,
    FinalReviewShard,
    FinalReviewAggregate,
}

impl PromptTemplateKind {
    pub const ALL: [PromptTemplateKind; 11] = [
        PromptTemplateKind::ProblemFraming,
        PromptTemplateKind::ResolveProblem,
        PromptTemplateKind::RequirementReview,
        PromptTemplateKind::ResolveRequirement,
        PromptTemplateKind::DecomposeFeature,
        PromptTemplateKind::AnalyzeTask,
        PromptTemplateKind::ImplementTask,
        PromptTemplateKind::ReviewTask,
        PromptTemplateKind::ReviewFeature,
        PromptTemplateKind::FinalReviewShard,
        PromptTemplateKind::FinalReviewAggregate,
    ];

    pub const fn file_name(self) -> &'static str {
        match self {
            PromptTemplateKind::ProblemFraming => "problem-framing.md",
            PromptTemplateKind::ResolveProblem => "resolve-problem.md",
            PromptTemplateKind::RequirementReview => "requirement-review.md",
            PromptTemplateKind::ResolveRequirement => "resolve-requirement.md",
            PromptTemplateKind::DecomposeFeature => "decompose-feature.md",
            PromptTemplateKind::AnalyzeTask => "analyze-task.md",
            PromptTemplateKind::ImplementTask => "implement-task.md",
            PromptTemplateKind::ReviewTask => "review-task.md",
            PromptTemplateKind::ReviewFeature => "review-feature.md",
            PromptTemplateKind::FinalReviewShard => "final-review-shard.md",
            PromptTemplateKind::FinalReviewAggregate => "final-review-aggregate.md",
        }
    }

    pub fn from_file_name(name: &str) -> Option<Self> {
        match name {
            "problem-framing.md" => Some(Self::ProblemFraming),
            "resolve-problem.md" => Some(Self::ResolveProblem),
            "requirement-review.md" => Some(Self::RequirementReview),
            "resolve-requirement.md" => Some(Self::ResolveRequirement),
            "decompose-feature.md" => Some(Self::DecomposeFeature),
            "analyze-task.md" => Some(Self::AnalyzeTask),
            "implement-task.md" => Some(Self::ImplementTask),
            "review-task.md" => Some(Self::ReviewTask),
            "review-feature.md" => Some(Self::ReviewFeature),
            "final-review-shard.md" => Some(Self::FinalReviewShard),
            "final-review-aggregate.md" => Some(Self::FinalReviewAggregate),
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
pub struct ProblemFramingPromptInput {
    pub common: CommonPromptVariables,
    pub spec_file: String,
    pub feature_spec: String,
    pub output_review_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveProblemPromptInput {
    pub common: CommonPromptVariables,
    pub spec_file: String,
    pub feature_spec: String,
    pub options: String,
    pub decision: String,
    pub output_resolved_problem_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementReviewPromptInput {
    pub common: CommonPromptVariables,
    pub spec_file: String,
    pub feature_spec: String,
    pub output_review_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveRequirementPromptInput {
    pub common: CommonPromptVariables,
    pub spec_file: String,
    pub feature_spec: String,
    pub questions: String,
    pub answers: String,
    pub output_resolved_spec_path: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalReviewShardPromptInput {
    pub common: CommonPromptVariables,
    pub run_id: String,
    pub branch: String,
    pub spec_file: String,
    pub resolved_spec: String,
    pub review_type: String,
    pub change_map: String,
    pub relevant_diff: String,
    pub relevant_logs: String,
    pub relevant_files: String,
    pub output_findings_path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalReviewAggregatePromptInput {
    pub common: CommonPromptVariables,
    pub run_id: String,
    pub branch: String,
    pub spec_file: String,
    pub resolved_spec: String,
    pub change_map: String,
    pub shard_findings: String,
    pub public_api_summary: String,
    pub db_summary: String,
    pub docs_summary: String,
    pub verification_summary: String,
    pub output_review_path: String,
}

impl PromptTemplateInput for ProblemFramingPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::ProblemFraming
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("feature_spec".to_string(), self.feature_spec.clone());
        variables.insert(
            "output_review_path".to_string(),
            self.output_review_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for ResolveProblemPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::ResolveProblem
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("feature_spec".to_string(), self.feature_spec.clone());
        variables.insert("options".to_string(), self.options.clone());
        variables.insert("decision".to_string(), self.decision.clone());
        variables.insert(
            "output_resolved_problem_path".to_string(),
            self.output_resolved_problem_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for RequirementReviewPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::RequirementReview
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("feature_spec".to_string(), self.feature_spec.clone());
        variables.insert(
            "output_review_path".to_string(),
            self.output_review_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for ResolveRequirementPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::ResolveRequirement
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("feature_spec".to_string(), self.feature_spec.clone());
        variables.insert("questions".to_string(), self.questions.clone());
        variables.insert("answers".to_string(), self.answers.clone());
        variables.insert(
            "output_resolved_spec_path".to_string(),
            self.output_resolved_spec_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
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

impl PromptTemplateInput for FinalReviewShardPromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::FinalReviewShard
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("run_id".to_string(), self.run_id.clone());
        variables.insert("branch".to_string(), self.branch.clone());
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("resolved_spec".to_string(), self.resolved_spec.clone());
        variables.insert("review_type".to_string(), self.review_type.clone());
        variables.insert("change_map".to_string(), self.change_map.clone());
        variables.insert("relevant_diff".to_string(), self.relevant_diff.clone());
        variables.insert("relevant_logs".to_string(), self.relevant_logs.clone());
        variables.insert("relevant_files".to_string(), self.relevant_files.clone());
        variables.insert(
            "output_findings_path".to_string(),
            self.output_findings_path.clone(),
        );
        insert_compat_aliases(&mut variables);
        variables
    }
}

impl PromptTemplateInput for FinalReviewAggregatePromptInput {
    fn kind(&self) -> PromptTemplateKind {
        PromptTemplateKind::FinalReviewAggregate
    }

    fn variables(&self) -> BTreeMap<String, String> {
        let mut variables = common_variables(&self.common);
        variables.insert("run_id".to_string(), self.run_id.clone());
        variables.insert("branch".to_string(), self.branch.clone());
        variables.insert("spec_file".to_string(), self.spec_file.clone());
        variables.insert("resolved_spec".to_string(), self.resolved_spec.clone());
        variables.insert("change_map".to_string(), self.change_map.clone());
        variables.insert("shard_findings".to_string(), self.shard_findings.clone());
        variables.insert(
            "public_api_summary".to_string(),
            self.public_api_summary.clone(),
        );
        variables.insert("db_summary".to_string(), self.db_summary.clone());
        variables.insert("docs_summary".to_string(), self.docs_summary.clone());
        variables.insert(
            "verification_summary".to_string(),
            self.verification_summary.clone(),
        );
        variables.insert(
            "output_review_path".to_string(),
            self.output_review_path.clone(),
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
        PromptTemplateKind::ProblemFraming => PROBLEM_FRAMING_TEMPLATE,
        PromptTemplateKind::ResolveProblem => RESOLVE_PROBLEM_TEMPLATE,
        PromptTemplateKind::RequirementReview => REQUIREMENT_REVIEW_TEMPLATE,
        PromptTemplateKind::ResolveRequirement => RESOLVE_REQUIREMENT_TEMPLATE,
        PromptTemplateKind::DecomposeFeature => DECOMPOSE_FEATURE_TEMPLATE,
        PromptTemplateKind::AnalyzeTask => ANALYZE_TASK_TEMPLATE,
        PromptTemplateKind::ImplementTask => IMPLEMENT_TASK_TEMPLATE,
        PromptTemplateKind::ReviewTask => REVIEW_TASK_TEMPLATE,
        PromptTemplateKind::ReviewFeature => REVIEW_FEATURE_TEMPLATE,
        PromptTemplateKind::FinalReviewShard => FINAL_REVIEW_SHARD_TEMPLATE,
        PromptTemplateKind::FinalReviewAggregate => FINAL_REVIEW_AGGREGATE_TEMPLATE,
    }
}

const PROBLEM_FRAMING_TEMPLATE: &str = include_str!("../prompts/default/problem-framing.md");

const RESOLVE_PROBLEM_TEMPLATE: &str = include_str!("../prompts/default/resolve-problem.md");

const REQUIREMENT_REVIEW_TEMPLATE: &str = include_str!("../prompts/default/requirement-review.md");

const RESOLVE_REQUIREMENT_TEMPLATE: &str =
    include_str!("../prompts/default/resolve-requirement.md");

const DECOMPOSE_FEATURE_TEMPLATE: &str = include_str!("../prompts/default/decompose-feature.md");

const ANALYZE_TASK_TEMPLATE: &str = include_str!("../prompts/default/analyze-task.md");

const IMPLEMENT_TASK_TEMPLATE: &str = include_str!("../prompts/default/implement-task.md");

const REVIEW_TASK_TEMPLATE: &str = include_str!("../prompts/default/review-task.md");

const REVIEW_FEATURE_TEMPLATE: &str = include_str!("../prompts/default/review-feature.md");

const FINAL_REVIEW_SHARD_TEMPLATE: &str = include_str!("../prompts/default/final-review-shard.md");

const FINAL_REVIEW_AGGREGATE_TEMPLATE: &str =
    include_str!("../prompts/default/final-review-aggregate.md");

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
    pub fallback_required_output_from_last_message: bool,
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
                    if should_write_required_output_from_last_message(request) {
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
                    if should_write_required_output_from_last_message(request) {
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

fn should_write_required_output_from_last_message(request: &CodexRunRequest) -> bool {
    request.sandbox == "read-only" || request.fallback_required_output_from_last_message
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
    #[serde(rename = "specFiles", default)]
    pub spec_files: Vec<String>,
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
    #[serde(default)]
    pub phase: String,
    pub title: String,
    #[serde(rename = "maxAttempts", default)]
    pub max_attempts: Option<u64>,
    #[serde(rename = "timeoutSeconds", default)]
    pub timeout_seconds: Option<u64>,
    pub output: String,
    pub prompt: String,
    #[serde(rename = "specFile", default)]
    pub spec_file: Option<String>,
    #[serde(rename = "specFiles", default)]
    pub spec_files: Vec<String>,
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
    #[serde(rename = "problemFraming", default)]
    pub problem_framing: ProblemFramingState,
    #[serde(rename = "requirementReview", default)]
    pub requirement_review: RequirementReviewState,
    #[serde(rename = "finalReview", default)]
    pub final_review: FinalReviewState,
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
            problem_framing: ProblemFramingState::default(),
            requirement_review: RequirementReviewState::default(),
            final_review: FinalReviewState::default(),
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
    pub problem_framing_status: String,
    pub requirement_review_status: String,
    pub feature_review_status: String,
    pub feature_review_attempts: u64,
    pub final_review_status: String,
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
    View(Box<StatusView>),
    Message(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    pub run_id: Option<String>,
    pub interval_seconds: u64,
    pub max_failures: Option<u64>,
    pub group: Option<String>,
    pub phase: Option<String>,
    pub until_phase: Option<String>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InspectOptions {
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogsOptions {
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub phase: Option<String>,
    pub latest: bool,
    pub tail_lines: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetTaskOptions {
    pub run_id: Option<String>,
    pub task_id: String,
    pub phase: TaskPhase,
    pub clear_attempts: bool,
    pub clear_review_attempts: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipPhaseOptions {
    pub run_id: Option<String>,
    pub phase_id: String,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunInspectView {
    pub repo_root: PathBuf,
    pub repo_runs_dir: PathBuf,
    pub active_runs: Vec<String>,
    pub archived_runs: Vec<ArchivedRunView>,
    pub selected: Option<RunLocationView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunLocationView {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub visible_run_dir: PathBuf,
    pub location: String,
    pub archive_name: Option<String>,
    pub tasks_path: PathBuf,
    pub state_path: PathBuf,
    pub metadata_path: PathBuf,
    pub logs_dir: PathBuf,
    pub output_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchivedRunView {
    pub run_id: String,
    pub archive_name: String,
    pub run_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogsView {
    pub run_id: String,
    pub run_dir: PathBuf,
    pub location: String,
    pub archive_name: Option<String>,
    pub logs_dir: PathBuf,
    pub files: Vec<LogFileView>,
    pub tails: Vec<LogTailView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogFileView {
    pub name: String,
    pub path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogTailView {
    pub name: String,
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResetTaskResult {
    pub run_id: String,
    pub task_id: String,
    pub phase: String,
    pub tasks_path: PathBuf,
    pub state_path: PathBuf,
    pub attempts: u64,
    pub max_attempts: u64,
    pub review_attempts: u64,
    pub max_review_attempts: u64,
    pub warnings: Vec<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SkipPhaseResult {
    pub run_id: String,
    pub phase_id: String,
    pub tasks_path: PathBuf,
    pub state_path: PathBuf,
    pub metadata_path: PathBuf,
    pub ignored_tasks: usize,
    pub already_done_tasks: usize,
    pub reason: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchedulerResult {
    pub run_id: String,
    pub message: String,
    pub exit_code: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum PendingUserInputKind {
    Decision,
    Clarification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingUserInput {
    pub run_id: String,
    pub kind: PendingUserInputKind,
    pub prompt_path: PathBuf,
    pub response_path: PathBuf,
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct WatchScope {
    group: Option<String>,
    phase: Option<String>,
    until_phase: Option<String>,
}

pub fn watch_run(
    start: &Path,
    options: WatchOptions,
) -> std::result::Result<SchedulerResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    watch_run_in_repo(&repo_root, &home, options)
}

pub fn pending_user_input(
    start: &Path,
    run_id: Option<&str>,
) -> std::result::Result<Option<PendingUserInput>, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    pending_user_input_in_repo(&repo_root, &home, run_id)
}

pub fn pending_user_input_in_repo(
    repo_root: &Path,
    home: &Path,
    run_id: Option<&str>,
) -> std::result::Result<Option<PendingUserInput>, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, run_id)?;
    let state = store.read_run_state(&run_id)?;
    if state.problem_framing.status == ProblemFramingStatus::NeedsDecision
        && let Some(path) = state.problem_framing.decision_path
    {
        let path = PathBuf::from(path);
        return Ok(Some(PendingUserInput {
            run_id,
            kind: PendingUserInputKind::Decision,
            prompt_path: path.clone(),
            response_path: path,
        }));
    }
    if state.requirement_review.status == RequirementReviewStatus::NeedsClarification
        && let (Some(questions), Some(answers)) = (
            state.requirement_review.questions_path,
            state.requirement_review.answers_path,
        )
    {
        return Ok(Some(PendingUserInput {
            run_id,
            kind: PendingUserInputKind::Clarification,
            prompt_path: PathBuf::from(questions),
            response_path: PathBuf::from(answers),
        }));
    }
    Ok(None)
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
    let scope = WatchScope {
        group: options.group.clone(),
        phase: options.phase.clone(),
        until_phase: options.until_phase.clone(),
    };

    loop {
        let recovered = recover_stale_running_tasks(&store, &run_id)?;
        if recovered > 0 {
            append_event_log(
                &store.run_dir(&run_id)?,
                &format!("recovered {recovered} stale running task(s)"),
            )?;
        }

        let task_file = store.read_task_file(&run_id)?;
        let metadata = read_metadata_or_task_file(&store, &run_id, &task_file)?;
        validate_watch_scope(&task_file, &metadata, &scope)?;
        let state = store.read_run_state(&run_id)?;
        let Some(task_id) = select_next_runnable_task(&task_file, &state, &scope)? else {
            let blocked =
                count_tasks_with_status_in_scope(&task_file, &state, TaskStatus::Blocked, &scope)?;
            if blocked > 0 {
                return Ok(SchedulerResult {
                    run_id,
                    message: format!("Run has {blocked} blocked task(s)"),
                    exit_code: 1,
                });
            }
            if all_tasks_terminal(&task_file, &state)?
                && let Some(outcome) = prepare_next_phase_for_watch(
                    &context,
                    &store,
                    &run_id,
                    &task_file,
                    &scope,
                    options.codex_bin.clone(),
                )?
            {
                match outcome {
                    PhasePrepareOutcome::Decomposed => {
                        completed += 1;
                        consecutive_failures = 0;
                        continue;
                    }
                    PhasePrepareOutcome::Waiting => {
                        return Ok(SchedulerResult {
                            run_id,
                            message: "Run is waiting for phase input".to_string(),
                            exit_code: 0,
                        });
                    }
                }
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

    let max_rounds = context.merged.runner.max_final_review_rounds.max(1);
    let mut message = format!("Run {run_id} final review blocked");
    let mut exit_code = 1;

    loop {
        let task_file = store.read_task_file(&run_id)?;
        let state = store.read_run_state(&run_id)?;
        ensure_all_tasks_complete_for_finalize(&task_file, &state)?;
        let round = state.feature_review_attempts + 1;
        if round > max_rounds {
            let remaining = state.final_review.remaining_must_fix.clone();
            block_final_review(
                &store,
                &run_id,
                max_rounds,
                remaining,
                "max_final_review_rounds reached".to_string(),
            )?;
            break;
        }

        let outcome = execute_final_review_round(FinalReviewRoundInput {
            context: &context,
            store: &store,
            run_id: &run_id,
            task_file: &task_file,
            round,
            max_rounds,
            codex_bin: options.codex_bin.clone(),
            no_cleanup: options.no_cleanup,
        })?;

        if outcome.verdict == ReviewVerdict::Approved && outcome.must_fix.is_empty() {
            finish_feature_review(&store, &run_id, FeatureReviewStatus::Approved, None, None)?;
            finalize_approved_run(&context, &store, &run_id, &task_file, options.no_cleanup)?;
            message = if options.no_cleanup {
                format!("Run {run_id} final review approved")
            } else {
                format!("Run {run_id} final review approved and archived")
            };
            exit_code = 0;
            break;
        }

        if round >= max_rounds {
            block_final_review(
                &store,
                &run_id,
                max_rounds,
                outcome.must_fix,
                "max_final_review_rounds reached".to_string(),
            )?;
            message = format!("Run {run_id} final review blocked");
            break;
        }

        let final_fix_task_id = append_final_fix_task(
            &context,
            &store,
            &run_id,
            &task_file,
            round,
            &outcome.must_fix,
        )?;
        record_final_fix_task(&store, &run_id, round, &final_fix_task_id)?;
        match run_final_fix_task(
            &context,
            &store,
            &run_id,
            &final_fix_task_id,
            options.codex_bin.clone(),
        ) {
            Ok(()) => continue,
            Err(err) => {
                let mut remaining = outcome.must_fix;
                remaining.push(FinalReviewFinding {
                    id: format!("final-fix-round-{round}-failed"),
                    review_type: "final-fix".to_string(),
                    severity: FindingSeverity::MustFix,
                    title: "final-fix task failed".to_string(),
                    detail: err.to_string(),
                    source: Some(final_fix_task_id),
                });
                block_final_review(
                    &store,
                    &run_id,
                    max_rounds,
                    remaining,
                    "final-fix task failed".to_string(),
                )?;
                message = format!("Run {run_id} final review blocked");
                break;
            }
        }
    }

    Ok(SchedulerResult {
        run_id,
        message,
        exit_code,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FinalReviewRoundOutcome {
    verdict: ReviewVerdict,
    must_fix: Vec<FinalReviewFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ChangeMap {
    #[serde(rename = "runId")]
    run_id: String,
    branch: String,
    #[serde(rename = "specFile")]
    spec_file: String,
    files: Vec<ChangedFile>,
    #[serde(rename = "publicApiSummary")]
    public_api_summary: String,
    #[serde(rename = "dbSummary")]
    db_summary: String,
    #[serde(rename = "docsSummary")]
    docs_summary: String,
    #[serde(rename = "verificationSummary")]
    verification_summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ChangedFile {
    path: String,
    #[serde(rename = "changeKind")]
    change_kind: String,
    #[serde(rename = "riskTypes")]
    risk_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ShardReviewOutput {
    verdict: ReviewVerdict,
    #[serde(default)]
    findings: Vec<ShardFindingInput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ShardFindingInput {
    id: String,
    severity: FindingSeverity,
    title: String,
    detail: String,
    #[serde(default)]
    source: Option<String>,
}

struct FinalReviewRoundInput<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    task_file: &'a TaskFile,
    round: u64,
    max_rounds: u64,
    codex_bin: Option<PathBuf>,
    no_cleanup: bool,
}

fn execute_final_review_round(
    input: FinalReviewRoundInput<'_>,
) -> std::result::Result<FinalReviewRoundOutcome, AppError> {
    let FinalReviewRoundInput {
        context,
        store,
        run_id,
        task_file,
        round,
        max_rounds,
        codex_bin,
        no_cleanup,
    } = input;
    let run_dir = store.run_dir(run_id)?;
    let round_dir = run_dir
        .join("output/final-review")
        .join(format!("round-{round}"));
    let visible_round_dir = project_task_run_dir(&context.repo_root, run_id)
        .join("final-review")
        .join(format!("round-{round}"));
    fs::create_dir_all(&round_dir)
        .map_err(|err| AppError::Io(format!("failed to create {}: {err}", round_dir.display())))?;
    fs::create_dir_all(&visible_round_dir).map_err(|err| {
        AppError::Io(format!(
            "failed to create {}: {err}",
            visible_round_dir.display()
        ))
    })?;

    let change_map = build_change_map(context, store, run_id, task_file)?;
    let change_map_path = round_dir.join("change-map.json");
    let visible_change_map_path = visible_round_dir.join("change-map.json");
    write_json_file(&change_map_path, &change_map)?;
    write_json_file(&visible_change_map_path, &change_map)?;

    let review_plan = build_review_plan(&change_map);
    let review_plan_path = round_dir.join("review-plan.md");
    let visible_review_plan_path = visible_round_dir.join("review-plan.md");
    write_text_file(&review_plan_path, &review_plan)?;
    write_text_file(&visible_review_plan_path, &review_plan)?;

    let started_at = current_timestamp()?;
    store.update_run_state(run_id, |state| {
        state.feature_review_status = FeatureReviewStatus::Running;
        state.feature_review_attempts = round;
        state.final_review.status = FeatureReviewStatus::Running;
        state.final_review.max_rounds = max_rounds;
        state.final_review.change_map_path = Some(change_map_path.display().to_string());
        state.final_review.review_plan_path = Some(review_plan_path.display().to_string());
        state.final_review.last_error = None;
        state.extra.insert(
            "featureReviewOutput".to_string(),
            Value::String(round_dir.display().to_string()),
        );
        state.extra.remove("featureReviewLastError");
        state.extra.remove("featureReviewLastLog");
        if no_cleanup {
            state
                .extra
                .insert("featureReviewNoCleanup".to_string(), Value::Bool(true));
        }
        state.final_review.rounds.push(FinalReviewRoundState {
            round,
            status: FeatureReviewStatus::Running,
            started_at: Some(started_at.clone()),
            finished_at: None,
            change_map_path: Some(change_map_path.display().to_string()),
            review_plan_path: Some(review_plan_path.display().to_string()),
            findings_path: None,
            aggregate_output: None,
            final_fix_task_id: None,
            shards: Vec::new(),
            remaining_must_fix: Vec::new(),
        });
        Ok(())
    })?;

    append_event_log(
        &run_dir,
        &format!(
            "final review round {round} started; change_map={}, review_plan={}",
            change_map_path.display(),
            review_plan_path.display()
        ),
    )?;

    let change_map_json = serde_json::to_string_pretty(&change_map)
        .map_err(|err| AppError::Runtime(format!("failed to encode change map: {err}")))?;
    let feature_diff =
        feature_branch_diff(&context.repo_root, &context.merged.project.default_branch)?;
    let resolved_spec = read_final_review_spec(context, task_file)?;
    let mut all_findings = Vec::new();
    let mut shard_states = Vec::new();

    for review_type in FINAL_REVIEW_TYPES {
        let shard = execute_final_review_shard(FinalReviewShardInput {
            context,
            store,
            run_id,
            task_file,
            round,
            review_type,
            resolved_spec: &resolved_spec,
            change_map: &change_map,
            change_map_json: &change_map_json,
            feature_diff: &feature_diff,
            codex_bin: codex_bin.clone(),
        })?;
        all_findings.extend(shard.findings.clone());
        shard_states.push(shard.state);
    }

    let findings_path = round_dir.join("findings.json");
    let visible_findings_path = visible_round_dir.join("findings.json");
    write_json_file(&findings_path, &all_findings)?;
    write_json_file(&visible_findings_path, &all_findings)?;

    let aggregate_output_path = round_dir.join("aggregate-review.md");
    let aggregate = match execute_final_review_aggregate(FinalReviewAggregateInput {
        context,
        store,
        run_id,
        task_file,
        round,
        resolved_spec: &resolved_spec,
        change_map: &change_map,
        change_map_json: &change_map_json,
        findings: &all_findings,
        output_path: &aggregate_output_path,
        codex_bin,
    }) {
        Ok(verdict) => verdict,
        Err(err) => {
            all_findings.push(synthetic_final_review_finding(
                round,
                "aggregate",
                "aggregate review failed",
                &err.to_string(),
                Some(aggregate_output_path.display().to_string()),
            ));
            ReviewVerdict::ChangesRequested
        }
    };
    write_json_file(&findings_path, &all_findings)?;
    write_json_file(&visible_findings_path, &all_findings)?;
    if let Ok(text) = fs::read_to_string(&aggregate_output_path) {
        write_text_file(&visible_round_dir.join("aggregate-review.md"), &text)?;
    }

    let mut must_fix = all_findings
        .iter()
        .filter(|finding| finding.severity == FindingSeverity::MustFix)
        .cloned()
        .collect::<Vec<_>>();
    if aggregate == ReviewVerdict::ChangesRequested && must_fix.is_empty() {
        must_fix.push(FinalReviewFinding {
            id: format!("aggregate-round-{round}-changes-requested"),
            review_type: "aggregate".to_string(),
            severity: FindingSeverity::MustFix,
            title: "aggregate review requested changes".to_string(),
            detail: "Aggregate review returned CHANGES_REQUESTED without a shard MUST_FIX."
                .to_string(),
            source: Some(aggregate_output_path.display().to_string()),
        });
    }

    let verdict = if must_fix.is_empty() && aggregate == ReviewVerdict::Approved {
        ReviewVerdict::Approved
    } else {
        ReviewVerdict::ChangesRequested
    };
    let status = if verdict == ReviewVerdict::Approved {
        FeatureReviewStatus::Approved
    } else {
        FeatureReviewStatus::ChangesRequested
    };
    let finished_at = current_timestamp()?;
    store.update_run_state(run_id, |state| {
        state.feature_review_status = status;
        state.final_review.status = status;
        state.final_review.findings_path = Some(findings_path.display().to_string());
        state.final_review.remaining_must_fix = must_fix.clone();
        if let Some(round_state) = state
            .final_review
            .rounds
            .iter_mut()
            .rev()
            .find(|candidate| candidate.round == round)
        {
            round_state.status = status;
            round_state.finished_at = Some(finished_at.clone());
            round_state.findings_path = Some(findings_path.display().to_string());
            round_state.aggregate_output = Some(aggregate_output_path.display().to_string());
            round_state.shards = shard_states;
            round_state.remaining_must_fix = must_fix.clone();
        }
        if must_fix.is_empty() {
            state.extra.remove("featureReviewLastError");
            state.extra.remove("featureReviewLastLog");
        } else {
            state.extra.insert(
                "featureReviewLastError".to_string(),
                Value::String(format!("{} MUST_FIX finding(s)", must_fix.len())),
            );
            state.extra.insert(
                "featureReviewLastLog".to_string(),
                Value::String(findings_path.display().to_string()),
            );
        }
        Ok(())
    })?;

    append_event_log(
        &run_dir,
        &format!(
            "final review round {round} finished; verdict={}, must_fix={}",
            verdict.as_str(),
            must_fix.len()
        ),
    )?;

    Ok(FinalReviewRoundOutcome { verdict, must_fix })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FinalReviewShardOutcome {
    state: FinalReviewShardState,
    findings: Vec<FinalReviewFinding>,
}

struct FinalReviewShardInput<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    task_file: &'a TaskFile,
    round: u64,
    review_type: &'a str,
    resolved_spec: &'a str,
    change_map: &'a ChangeMap,
    change_map_json: &'a str,
    feature_diff: &'a str,
    codex_bin: Option<PathBuf>,
}

fn execute_final_review_shard(
    input: FinalReviewShardInput<'_>,
) -> std::result::Result<FinalReviewShardOutcome, AppError> {
    let FinalReviewShardInput {
        context,
        store,
        run_id,
        task_file,
        round,
        review_type,
        resolved_spec,
        change_map,
        change_map_json,
        feature_diff,
        codex_bin,
    } = input;
    let run_dir = store.run_dir(run_id)?;
    let safe_type = normalized_verification_name(review_type);
    let output_path = run_dir
        .join("output/final-review")
        .join(format!("round-{round}"))
        .join(format!("{safe_type}.findings.json"));
    let relevant_paths = change_map
        .files
        .iter()
        .filter(|file| file.risk_types.iter().any(|risk| risk == review_type))
        .map(|file| file.path.clone())
        .collect::<Vec<_>>();
    let relevant_diff = filter_diff_for_paths(feature_diff, &relevant_paths);
    let relevant_logs = relevant_logs_for_review_type(store, run_id, review_type)?;
    let relevant_files = read_relevant_files(&context.repo_root, &relevant_paths)?;
    let prompt = render_final_review_shard_prompt(FinalReviewShardRender {
        context,
        store,
        run_id,
        task_file,
        review_type,
        resolved_spec,
        change_map_json,
        relevant_diff: &relevant_diff,
        relevant_logs: &relevant_logs,
        relevant_files: &relevant_files,
        output_path: &output_path,
    })?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(format!("final-review.round-{round}.{safe_type}.md")),
        stdout_log_path: run_dir
            .join("logs")
            .join(format!("final-review.round-{round}.{safe_type}.stdout.log")),
        stderr_log_path: run_dir
            .join("logs")
            .join(format!("final-review.round-{round}.{safe_type}.stderr.log")),
        last_message_path: run_dir.join("logs").join(format!(
            "final-review.round-{round}.{safe_type}.last-message.md"
        )),
        required_output_path: Some(output_path.clone()),
        fallback_required_output_from_last_message: true,
        sandbox: context.merged.runner.review_sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_review_timeout_seconds,
    };

    match build_executor(context, codex_bin).execute(&request) {
        Ok(output) => match parse_final_review_shard_output(&output_path, review_type) {
            Ok((verdict, findings)) => {
                let findings_count = findings.len();
                Ok(FinalReviewShardOutcome {
                    state: FinalReviewShardState {
                        review_type: review_type.to_string(),
                        status: if verdict == ReviewVerdict::Approved {
                            FeatureReviewStatus::Approved
                        } else {
                            FeatureReviewStatus::ChangesRequested
                        },
                        verdict: Some(verdict),
                        output: Some(output_path.display().to_string()),
                        stdout_log: Some(output.stdout_log_path.display().to_string()),
                        stderr_log: Some(output.stderr_log_path.display().to_string()),
                        last_message: Some(output.last_message_path.display().to_string()),
                        findings_count,
                        last_error: None,
                    },
                    findings,
                })
            }
            Err(err) => {
                let finding = synthetic_final_review_finding(
                    round,
                    review_type,
                    "invalid shard output",
                    &err,
                    Some(output_path.display().to_string()),
                );
                Ok(FinalReviewShardOutcome {
                    state: FinalReviewShardState {
                        review_type: review_type.to_string(),
                        status: FeatureReviewStatus::Failed,
                        verdict: None,
                        output: Some(output_path.display().to_string()),
                        stdout_log: Some(output.stdout_log_path.display().to_string()),
                        stderr_log: Some(output.stderr_log_path.display().to_string()),
                        last_message: Some(output.last_message_path.display().to_string()),
                        findings_count: 1,
                        last_error: Some(err),
                    },
                    findings: vec![finding],
                })
            }
        },
        Err(err) => {
            let err = *err;
            let finding = synthetic_final_review_finding(
                round,
                review_type,
                "shard execution failed",
                &err.message,
                Some(err.stderr_log_path.display().to_string()),
            );
            Ok(FinalReviewShardOutcome {
                state: FinalReviewShardState {
                    review_type: review_type.to_string(),
                    status: FeatureReviewStatus::Failed,
                    verdict: None,
                    output: Some(output_path.display().to_string()),
                    stdout_log: Some(err.stdout_log_path.display().to_string()),
                    stderr_log: Some(err.stderr_log_path.display().to_string()),
                    last_message: Some(err.last_message_path.display().to_string()),
                    findings_count: 1,
                    last_error: Some(err.message),
                },
                findings: vec![finding],
            })
        }
    }
}

struct FinalReviewAggregateInput<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    task_file: &'a TaskFile,
    round: u64,
    resolved_spec: &'a str,
    change_map: &'a ChangeMap,
    change_map_json: &'a str,
    findings: &'a [FinalReviewFinding],
    output_path: &'a Path,
    codex_bin: Option<PathBuf>,
}

fn execute_final_review_aggregate(
    input: FinalReviewAggregateInput<'_>,
) -> std::result::Result<ReviewVerdict, AppError> {
    let FinalReviewAggregateInput {
        context,
        store,
        run_id,
        task_file,
        round,
        resolved_spec,
        change_map,
        change_map_json,
        findings,
        output_path,
        codex_bin,
    } = input;
    let run_dir = store.run_dir(run_id)?;
    let findings_json = serde_json::to_string_pretty(findings)
        .map_err(|err| AppError::Runtime(format!("failed to encode findings: {err}")))?;
    let prompt = render_final_review_aggregate_prompt(FinalReviewAggregateRender {
        context,
        store,
        run_id,
        task_file,
        resolved_spec,
        change_map_json,
        shard_findings: &findings_json,
        public_api_summary: &change_map.public_api_summary,
        db_summary: &change_map.db_summary,
        docs_summary: &change_map.docs_summary,
        verification_summary: &change_map.verification_summary,
        output_path,
    })?;
    let request = CodexRunRequest {
        prompt,
        prompt_path: run_dir
            .join("prompts")
            .join(format!("final-review.round-{round}.aggregate.md")),
        stdout_log_path: run_dir
            .join("logs")
            .join(format!("final-review.round-{round}.aggregate.stdout.log")),
        stderr_log_path: run_dir
            .join("logs")
            .join(format!("final-review.round-{round}.aggregate.stderr.log")),
        last_message_path: run_dir.join("logs").join(format!(
            "final-review.round-{round}.aggregate.last-message.md"
        )),
        required_output_path: Some(output_path.to_path_buf()),
        fallback_required_output_from_last_message: true,
        sandbox: context.merged.runner.review_sandbox.clone(),
        approval: context.merged.runner.approval.clone(),
        model: context.merged.runner.model.clone(),
        reasoning_effort: context.merged.runner.reasoning_effort.clone(),
        search: Some(context.merged.runner.search),
        timeout_seconds: context.merged.runner.default_review_timeout_seconds,
    };
    match build_executor(context, codex_bin).execute(&request) {
        Ok(_) => parse_final_review_output_file(output_path).map_err(|err| {
            AppError::Runtime(format!(
                "invalid aggregate final review output: {err}; output={}",
                output_path.display()
            ))
        }),
        Err(err) => {
            let err = *err;
            Err(AppError::Runtime(format!(
                "{}; logs: stdout={}, stderr={}, last={}",
                err.message,
                err.stdout_log_path.display(),
                err.stderr_log_path.display(),
                err.last_message_path.display()
            )))
        }
    }
}

fn build_change_map(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
) -> std::result::Result<ChangeMap, AppError> {
    let mut files = BTreeMap::<String, String>::new();
    if git_branch_exists(&context.repo_root, &context.merged.project.default_branch)? {
        let range = format!("{}...HEAD", context.merged.project.default_branch);
        for (status, path) in
            git_name_status(&context.repo_root, &["diff", "--name-status", &range, "--"])?
        {
            insert_business_change_path(&mut files, path, status);
        }
    }
    for (status, path) in git_name_status(
        &context.repo_root,
        &["diff", "--cached", "--name-status", "--"],
    )? {
        insert_business_change_path(&mut files, path, status);
    }
    for (status, path) in git_name_status(&context.repo_root, &["diff", "--name-status", "--"])? {
        insert_business_change_path(&mut files, path, status);
    }
    for entry in git_status_entries(&context.repo_root)? {
        if !is_tool_collaboration_path(&entry.path) {
            files.entry(entry.path).or_insert_with(|| "WT".to_string());
        }
    }

    let changed_files = files
        .into_iter()
        .map(|(path, change_kind)| ChangedFile {
            risk_types: risk_types_for_path(&path),
            path,
            change_kind,
        })
        .collect::<Vec<_>>();
    let public_api_summary = summarize_paths(
        &changed_files,
        is_public_api_path,
        "No public API files changed.",
    );
    let db_summary = summarize_paths(
        &changed_files,
        is_database_path,
        "No database or migration files changed.",
    );
    let docs_summary = summarize_paths(
        &changed_files,
        is_docs_contract_path,
        "No docs or contract files changed.",
    );
    let verification_summary = final_review_verification_summary(store, run_id, task_file)?;

    Ok(ChangeMap {
        run_id: task_file.run_id.clone(),
        branch: task_file.branch.clone(),
        spec_file: task_file.spec_file.clone(),
        files: changed_files,
        public_api_summary,
        db_summary,
        docs_summary,
        verification_summary,
    })
}

fn insert_business_change_path(files: &mut BTreeMap<String, String>, path: String, status: String) {
    if !is_tool_collaboration_path(&path) {
        files.insert(path, status);
    }
}

fn git_name_status(
    repo_root: &Path,
    args: &[&str],
) -> std::result::Result<Vec<(String, String)>, AppError> {
    let output = git_output(repo_root, args)?;
    let mut out = Vec::new();
    for line in output.lines() {
        let mut parts = line.split('\t');
        let Some(status) = parts.next() else {
            continue;
        };
        let Some(path) = parts.next_back().or_else(|| parts.next()) else {
            continue;
        };
        if !path.trim().is_empty() {
            out.push((status.to_string(), path.to_string()));
        }
    }
    Ok(out)
}

fn risk_types_for_path(path: &str) -> Vec<String> {
    let mut risks = BTreeSet::new();
    risks.insert("code-defect".to_string());
    risks.insert("architecture/integration".to_string());
    if is_public_api_path(path) {
        risks.insert("backward-compatibility".to_string());
        risks.insert("docs-contract".to_string());
        risks.insert("business-scenario".to_string());
    }
    if is_database_path(path) {
        risks.insert("data-migration".to_string());
        risks.insert("backward-compatibility".to_string());
    }
    if is_test_path(path) {
        risks.insert("test-coverage".to_string());
    }
    if is_docs_contract_path(path) {
        risks.insert("docs-contract".to_string());
    }
    if is_security_sensitive_path(path) {
        risks.insert("security".to_string());
    }
    if is_performance_sensitive_path(path) {
        risks.insert("performance".to_string());
    }
    risks.into_iter().collect()
}

fn is_public_api_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("controller")
        || lower.contains("/api/")
        || lower.contains("openapi")
        || lower.contains("proto")
        || lower.contains("route")
}

fn is_database_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("migration")
        || lower.contains("flyway")
        || lower.contains("/db/")
        || lower.contains("schema")
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("/test") || lower.contains("_test") || lower.contains(".test.")
}

fn is_docs_contract_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("docs/")
        || lower.contains("/docs/")
        || lower.ends_with(".md")
        || lower.contains("openapi")
}

fn is_security_sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("auth")
        || lower.contains("security")
        || lower.contains("permission")
        || lower.contains("token")
        || lower.contains("password")
}

fn is_performance_sensitive_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("cache")
        || lower.contains("query")
        || lower.contains("batch")
        || lower.contains("index")
        || lower.contains("performance")
}

fn summarize_paths<F>(files: &[ChangedFile], predicate: F, empty: &str) -> String
where
    F: Fn(&str) -> bool,
{
    let paths = files
        .iter()
        .filter(|file| predicate(&file.path))
        .map(|file| format!("- {} ({})", file.path, file.change_kind))
        .collect::<Vec<_>>();
    if paths.is_empty() {
        empty.to_string()
    } else {
        paths.join("\n")
    }
}

fn final_review_verification_summary(
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
) -> std::result::Result<String, AppError> {
    let state = store.read_run_state(run_id)?;
    let state_by_id = normalized_state_map(task_file, &state)?;
    let mut lines = Vec::new();
    for task in &task_file.tasks {
        if let Some(task_state) = state_by_id.get(task.id.as_str()) {
            let merged_extra = merged_task_state_extra(task, task_state);
            let logs = merged_extra
                .get("verificationLogs")
                .and_then(Value::as_array)
                .map(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| "(none)".to_string());
            let skipped = merged_extra
                .get("verificationSkipped")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let degraded = merged_extra
                .get("verificationDegraded")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let degraded_reason = merged_extra
                .get("verificationDegradedReason")
                .and_then(Value::as_str)
                .unwrap_or("-");
            lines.push(format!(
                "- {}: status={}, phase={}, verificationSkipped={}, verificationDegraded={}, verificationDegradedReason={}, verificationLogs={}",
                task.id,
                task_state.status.as_str(),
                task_state.phase.map(TaskPhase::as_str).unwrap_or("-"),
                skipped,
                degraded,
                degraded_reason,
                logs
            ));
        }
    }
    Ok(if lines.is_empty() {
        "No task verification state found.".to_string()
    } else {
        lines.join("\n")
    })
}

fn merged_task_state_extra(task: &Task, task_state: &TaskState) -> Map<String, Value> {
    let mut extra = task.extra.clone();
    extra.extend(task_state.extra.clone());
    extra
}

fn build_review_plan(change_map: &ChangeMap) -> String {
    let mut out = String::new();
    out.push_str("# Final Review Plan\n\n");
    out.push_str(&format!("Run: {}\n\n", change_map.run_id));
    for review_type in FINAL_REVIEW_TYPES {
        let files = change_map
            .files
            .iter()
            .filter(|file| file.risk_types.iter().any(|risk| risk == review_type))
            .map(|file| format!("- {} ({})", file.path, file.change_kind))
            .collect::<Vec<_>>();
        out.push_str(&format!("## {review_type}\n\n"));
        if files.is_empty() {
            out.push_str(
                "- No directly mapped files; review resolved spec and change-map summary.\n\n",
            );
        } else {
            out.push_str(&files.join("\n"));
            out.push_str("\n\n");
        }
    }
    out
}

fn write_json_file<T: Serialize>(path: &Path, value: &T) -> std::result::Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| AppError::Io(format!("failed to create {}: {err}", parent.display())))?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|err| AppError::Runtime(format!("failed to encode JSON: {err}")))?;
    fs::write(path, format!("{text}\n"))
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
}

fn write_text_file(path: &Path, text: &str) -> std::result::Result<(), AppError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|err| AppError::Io(format!("failed to create {}: {err}", parent.display())))?;
    }
    fs::write(path, text)
        .map_err(|err| AppError::Io(format!("failed to write {}: {err}", path.display())))
}

fn read_final_review_spec(
    context: &ConfigContext,
    task_file: &TaskFile,
) -> std::result::Result<String, AppError> {
    let spec_files = normalize_spec_files(&task_file.spec_file, &task_file.spec_files);
    let spec = read_combined_spec_document(context, &spec_files)?;
    Ok(spec.body)
}

struct FinalReviewShardRender<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    task_file: &'a TaskFile,
    review_type: &'a str,
    resolved_spec: &'a str,
    change_map_json: &'a str,
    relevant_diff: &'a str,
    relevant_logs: &'a str,
    relevant_files: &'a str,
    output_path: &'a Path,
}

fn render_final_review_shard_prompt(
    input: FinalReviewShardRender<'_>,
) -> std::result::Result<String, AppError> {
    let FinalReviewShardRender {
        context,
        store,
        run_id,
        task_file,
        review_type,
        resolved_spec,
        change_map_json,
        relevant_diff,
        relevant_logs,
        relevant_files,
        output_path,
    } = input;
    let input = FinalReviewShardPromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        run_id: task_file.run_id.clone(),
        branch: task_file.branch.clone(),
        spec_file: task_file.spec_file.clone(),
        resolved_spec: resolved_spec.to_string(),
        review_type: review_type.to_string(),
        change_map: change_map_json.to_string(),
        relevant_diff: relevant_diff.to_string(),
        relevant_logs: relevant_logs.to_string(),
        relevant_files: relevant_files.to_string(),
        output_findings_path: output_path.display().to_string(),
    };
    let template = load_prompt_template(context, PromptTemplateKind::FinalReviewShard)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

struct FinalReviewAggregateRender<'a> {
    context: &'a ConfigContext,
    store: &'a RunStore,
    run_id: &'a str,
    task_file: &'a TaskFile,
    resolved_spec: &'a str,
    change_map_json: &'a str,
    shard_findings: &'a str,
    public_api_summary: &'a str,
    db_summary: &'a str,
    docs_summary: &'a str,
    verification_summary: &'a str,
    output_path: &'a Path,
}

fn render_final_review_aggregate_prompt(
    input: FinalReviewAggregateRender<'_>,
) -> std::result::Result<String, AppError> {
    let FinalReviewAggregateRender {
        context,
        store,
        run_id,
        task_file,
        resolved_spec,
        change_map_json,
        shard_findings,
        public_api_summary,
        db_summary,
        docs_summary,
        verification_summary,
        output_path,
    } = input;
    let input = FinalReviewAggregatePromptInput {
        common: common_prompt_variables(context, store, run_id)?,
        run_id: task_file.run_id.clone(),
        branch: task_file.branch.clone(),
        spec_file: task_file.spec_file.clone(),
        resolved_spec: resolved_spec.to_string(),
        change_map: change_map_json.to_string(),
        shard_findings: shard_findings.to_string(),
        public_api_summary: public_api_summary.to_string(),
        db_summary: db_summary.to_string(),
        docs_summary: docs_summary.to_string(),
        verification_summary: verification_summary.to_string(),
        output_review_path: output_path.display().to_string(),
    };
    let template = load_prompt_template(context, PromptTemplateKind::FinalReviewAggregate)
        .map_err(|err| AppError::Config(err.to_string()))?;
    template
        .render(&input)
        .map_err(|err| AppError::Config(err.to_string()))
}

fn relevant_logs_for_review_type(
    store: &RunStore,
    run_id: &str,
    review_type: &str,
) -> std::result::Result<String, AppError> {
    if review_type != "test-coverage" {
        return Ok("(no relevant logs)".to_string());
    }
    let logs_dir = store.run_dir(run_id)?.join("logs");
    let files = discover_log_files(&logs_dir, None, Some("verify"))?;
    if files.is_empty() {
        return Ok("(no verification logs)".to_string());
    }
    let mut out = String::new();
    for file in files.into_iter().take(5) {
        out.push_str(&format!("== {} ==\n", file.name));
        out.push_str(&read_last_lines(&file.path, 80)?);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

fn read_relevant_files(
    repo_root: &Path,
    paths: &[String],
) -> std::result::Result<String, AppError> {
    if paths.is_empty() {
        return Ok("(no directly relevant files)".to_string());
    }
    let mut out = String::new();
    let mut used = 0usize;
    for path in paths.iter().take(8) {
        let full = repo_root.join(path);
        if !full.is_file() {
            continue;
        }
        let mut text = fs::read_to_string(&full).unwrap_or_default();
        if text.len() > 4000 {
            text.truncate(4000);
            text.push_str("\n... truncated ...\n");
        }
        used += text.len();
        if used > 24000 {
            out.push_str("\n... file context limit reached ...\n");
            break;
        }
        out.push_str(&format!("== {path} ==\n{text}\n"));
    }
    Ok(if out.trim().is_empty() {
        "(no readable relevant files)".to_string()
    } else {
        out
    })
}

fn filter_diff_for_paths(diff: &str, paths: &[String]) -> String {
    if paths.is_empty() || diff.trim().is_empty() {
        return "(no relevant diff)".to_string();
    }
    let wanted = paths.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut include_current = false;
    for line in diff.lines() {
        if line.starts_with("diff --git ") || line.starts_with("## ") {
            if include_current && !current.is_empty() {
                out.push(current.join("\n"));
            }
            current.clear();
            include_current = line.starts_with("## ");
        }
        if line.starts_with("diff --git ") {
            include_current = diff_header_matches_paths(line, &wanted);
        }
        current.push(line);
    }
    if include_current && !current.is_empty() {
        out.push(current.join("\n"));
    }
    if out.is_empty() {
        "(no relevant diff)".to_string()
    } else {
        out.join("\n")
    }
}

fn diff_header_matches_paths(line: &str, wanted: &BTreeSet<&str>) -> bool {
    wanted.iter().any(|path| {
        line.contains(&format!(" a/{path} "))
            || line.contains(&format!(" b/{path}"))
            || line.ends_with(&format!(" b/{path}"))
    })
}

fn parse_final_review_shard_output(
    path: &Path,
    review_type: &str,
) -> std::result::Result<(ReviewVerdict, Vec<FinalReviewFinding>), String> {
    let raw = fs::read_to_string(path)
        .map_err(|err| format!("failed to read shard output {}: {err}", path.display()))?;
    if raw.trim().is_empty() {
        return Err("shard output is empty".to_string());
    }
    let parsed = serde_json::from_str::<ShardReviewOutput>(&raw)
        .map_err(|err| format!("invalid shard findings JSON: {err}"))?;
    let mut findings = Vec::new();
    for (index, finding) in parsed.findings.into_iter().enumerate() {
        if finding.id.trim().is_empty() {
            return Err(format!("finding at index {index} has empty id"));
        }
        if finding.title.trim().is_empty() {
            return Err(format!("finding {} has empty title", finding.id));
        }
        if finding.detail.trim().is_empty() {
            return Err(format!("finding {} has empty detail", finding.id));
        }
        findings.push(FinalReviewFinding {
            id: finding.id,
            review_type: review_type.to_string(),
            severity: finding.severity,
            title: finding.title,
            detail: finding.detail,
            source: finding.source,
        });
    }
    if parsed.verdict == ReviewVerdict::Approved
        && findings
            .iter()
            .any(|finding| finding.severity == FindingSeverity::MustFix)
    {
        return Err("APPROVED shard contains MUST_FIX findings".to_string());
    }
    if parsed.verdict == ReviewVerdict::ChangesRequested
        && findings
            .iter()
            .all(|finding| finding.severity != FindingSeverity::MustFix)
    {
        findings.push(FinalReviewFinding {
            id: format!(
                "{}-changes-requested-without-must-fix",
                normalized_verification_name(review_type)
            ),
            review_type: review_type.to_string(),
            severity: FindingSeverity::MustFix,
            title: "shard requested changes without MUST_FIX".to_string(),
            detail: "CHANGES_REQUESTED is blocking; shard did not provide a MUST_FIX finding."
                .to_string(),
            source: Some(path.display().to_string()),
        });
    }
    Ok((parsed.verdict, findings))
}

fn synthetic_final_review_finding(
    round: u64,
    review_type: &str,
    title: &str,
    detail: &str,
    source: Option<String>,
) -> FinalReviewFinding {
    FinalReviewFinding {
        id: format!(
            "round-{round}-{}-{}",
            normalized_verification_name(review_type),
            normalized_verification_name(title)
        ),
        review_type: review_type.to_string(),
        severity: FindingSeverity::MustFix,
        title: title.to_string(),
        detail: detail.to_string(),
        source,
    }
}

fn block_final_review(
    store: &RunStore,
    run_id: &str,
    max_rounds: u64,
    remaining: Vec<FinalReviewFinding>,
    reason: String,
) -> std::result::Result<(), AppError> {
    let run_dir = store.run_dir(run_id)?;
    let blocked_path = run_dir.join("output/final-review/blocked-findings.json");
    write_json_file(&blocked_path, &remaining)?;
    store.update_run_state(run_id, |state| {
        state.feature_review_status = FeatureReviewStatus::Blocked;
        state.final_review.status = FeatureReviewStatus::Blocked;
        state.final_review.max_rounds = max_rounds;
        state.final_review.remaining_must_fix = remaining.clone();
        state.final_review.findings_path = Some(blocked_path.display().to_string());
        state.final_review.last_error = Some(reason.clone());
        state.extra.insert(
            "featureReviewLastError".to_string(),
            Value::String(format!("{reason}; remaining MUST_FIX={}", remaining.len())),
        );
        state.extra.insert(
            "featureReviewLastLog".to_string(),
            Value::String(blocked_path.display().to_string()),
        );
        if let Some(round_state) = state.final_review.rounds.last_mut() {
            round_state.status = FeatureReviewStatus::Blocked;
            round_state.remaining_must_fix = remaining;
        }
        Ok(())
    })?;
    append_event_log(
        &run_dir,
        &format!(
            "final review blocked: {}; remaining MUST_FIX written to {}",
            reason,
            blocked_path.display()
        ),
    )
}

fn append_final_fix_task(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
    round: u64,
    findings: &[FinalReviewFinding],
) -> std::result::Result<String, AppError> {
    let task_id = format!("final-fix-round-{round}");
    let mut next = task_file.clone();
    next.tasks.retain(|task| task.id != task_id);
    let inherited_verification_commands = final_fix_inherited_task_verification_commands(task_file);
    let verification_degraded = context.merged.verification_commands.is_empty()
        && task_file.verification_commands.is_empty()
        && inherited_verification_commands.is_empty();
    let priority = next
        .tasks
        .iter()
        .map(|task| task.priority)
        .max()
        .unwrap_or(0)
        + 1;
    let findings_json = serde_json::to_string_pretty(findings)
        .map_err(|err| AppError::Runtime(format!("failed to encode findings: {err}")))?;
    let mut extra = Map::new();
    extra.insert("finalReviewRound".to_string(), Value::Number(round.into()));
    if verification_degraded {
        extra.insert("verificationDegraded".to_string(), Value::Bool(true));
        extra.insert(
            "verificationDegradedReason".to_string(),
            Value::String(
                "no global, task-file, or task-level verification commands are configured"
                    .to_string(),
            ),
        );
    }
    next.tasks.push(Task {
        id: task_id.clone(),
        priority,
        group: "final-review".to_string(),
        phase: "final-review".to_string(),
        title: format!("Fix final review round {round} MUST_FIX findings"),
        max_attempts: Some(1),
        timeout_seconds: None,
        output: format!("output/{task_id}.md"),
        prompt: format!(
            "Fix only these final review MUST_FIX findings, preserve existing behavior, then summarize the changes.\n\nVerification degraded: {verification_degraded}\n\n```json\n{findings_json}\n```"
        ),
        spec_file: Some(task_file.spec_file.clone()),
        spec_files: normalize_spec_files(&task_file.spec_file, &task_file.spec_files),
        depends_on: Vec::new(),
        review_criteria: vec![
            "All listed MUST_FIX findings are resolved.".to_string(),
            "No unrelated behavior or public contract is changed.".to_string(),
        ],
        analyze_timeout_seconds: None,
        analyze_required: true,
        require_review_approval: false,
        max_review_attempts: 1,
        review_timeout_seconds: None,
        verification_commands: inherited_verification_commands,
        extra,
    });
    store.write_task_file(run_id, &next)?;
    store.update_run_state(run_id, |state| {
        ensure_state_matches_tasks(&next, state)?;
        let task_state = find_task_state_mut(state, &task_id)?;
        task_state.status = TaskStatus::Pending;
        task_state.phase = None;
        task_state.attempts = 0;
        task_state.review_attempts = 0;
        task_state.last_error = None;
        task_state.last_log = None;
        task_state.last_verdict = None;
        task_state.last_review_comments = None;
        if verification_degraded {
            task_state
                .extra
                .insert("verificationDegraded".to_string(), Value::Bool(true));
            task_state.extra.insert(
                "verificationDegradedReason".to_string(),
                Value::String(
                    "no global, task-file, or task-level verification commands are configured"
                        .to_string(),
                ),
            );
        } else {
            task_state.extra.remove("verificationDegraded");
            task_state.extra.remove("verificationDegradedReason");
        }
        task_state.updated_at = Some(current_timestamp()?);
        Ok(())
    })?;
    append_event_log(
        &store.run_dir(run_id)?,
        &format!(
            "generated final-fix task {task_id} for round {round}; verification_degraded={verification_degraded}"
        ),
    )?;
    Ok(task_id)
}

fn final_fix_inherited_task_verification_commands(
    task_file: &TaskFile,
) -> Vec<VerificationCommand> {
    let mut seen = BTreeSet::new();
    let mut commands = Vec::new();
    for task in &task_file.tasks {
        if task.id.starts_with("final-fix-round-") {
            continue;
        }
        for command in &task.verification_commands {
            let key = (
                command.name.clone(),
                command.command.clone(),
                command.required,
                command.timeout_seconds,
            );
            if seen.insert(key) {
                commands.push(command.clone());
            }
        }
    }
    commands
}

fn record_final_fix_task(
    store: &RunStore,
    run_id: &str,
    round: u64,
    task_id: &str,
) -> std::result::Result<(), AppError> {
    store.update_run_state(run_id, |state| {
        if let Some(round_state) = state
            .final_review
            .rounds
            .iter_mut()
            .rev()
            .find(|candidate| candidate.round == round)
        {
            round_state.final_fix_task_id = Some(task_id.to_string());
        }
        Ok(())
    })
}

fn run_final_fix_task(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_id: &str,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<(), AppError> {
    for _ in 0..20 {
        let outcome = execute_task_until_boundary(
            context,
            store,
            run_id,
            task_id,
            codex_bin.clone(),
            true,
            true,
        )?;
        let state = store.read_run_state(run_id)?;
        let task_state = find_task_state(&state, task_id)?;
        match task_state.status {
            TaskStatus::Done => return Ok(()),
            TaskStatus::Blocked | TaskStatus::ReviewFailed => {
                return Err(AppError::Runtime(format!(
                    "final-fix task {task_id} stopped at status {}: {}",
                    task_state.status.as_str(),
                    task_state.last_error.clone().unwrap_or_default()
                )));
            }
            _ => {}
        }
        if matches!(
            outcome,
            TaskExecutionOutcome::FailedRetryable
                | TaskExecutionOutcome::Blocked
                | TaskExecutionOutcome::ReviewChangesRequested
                | TaskExecutionOutcome::Deferred
        ) {
            return Err(AppError::Runtime(format!(
                "final-fix task {task_id} did not complete; outcome={outcome:?}"
            )));
        }
    }
    Err(AppError::Runtime(format!(
        "final-fix task {task_id} exceeded scheduler step limit"
    )))
}

pub fn inspect_run(
    start: &Path,
    options: InspectOptions,
) -> std::result::Result<RunInspectView, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    inspect_run_in_repo(&repo_root, &home, options)
}

pub fn inspect_run_in_repo(
    repo_root: &Path,
    home: &Path,
    options: InspectOptions,
) -> std::result::Result<RunInspectView, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let active_runs = discover_run_ids(&store.repo_runs_dir)?;
    let archived_runs = discover_archived_runs(&store)?;
    let selected = match options.run_id.as_deref() {
        Some(_) => Some(resolve_existing_run_location(
            &context.repo_root,
            &store,
            options.run_id.as_deref(),
        )?),
        None if active_runs.len() == 1 => Some(resolve_existing_run_location(
            &context.repo_root,
            &store,
            Some(&active_runs[0]),
        )?),
        None => None,
    };

    Ok(RunInspectView {
        repo_root: context.repo_root,
        repo_runs_dir: store.repo_runs_dir,
        active_runs,
        archived_runs,
        selected,
    })
}

pub fn format_inspect_text(view: &RunInspectView) -> String {
    let mut out = String::new();
    out.push_str(&format!("Repo: {}\n", view.repo_root.display()));
    out.push_str(&format!("Run store: {}\n", view.repo_runs_dir.display()));
    if view.active_runs.is_empty() {
        out.push_str("Active runs: (none)\n");
    } else {
        out.push_str(&format!("Active runs: {}\n", view.active_runs.join(", ")));
    }
    if view.archived_runs.is_empty() {
        out.push_str("Archived runs: (none)\n");
    } else {
        out.push_str("Archived runs:\n");
        for archived in &view.archived_runs {
            out.push_str(&format!(
                "- {} ({}) {}\n",
                archived.run_id,
                archived.archive_name,
                archived.run_dir.display()
            ));
        }
    }
    if let Some(selected) = &view.selected {
        out.push_str("\nSelected run\n");
        out.push_str(&format!("Run: {}\n", selected.run_id));
        out.push_str(&format!("Location: {}\n", selected.location));
        if let Some(archive_name) = &selected.archive_name {
            out.push_str(&format!("Archive: {archive_name}\n"));
        }
        out.push_str(&format!("Run dir: {}\n", selected.run_dir.display()));
        out.push_str(&format!(
            "Visible run dir: {}\n",
            selected.visible_run_dir.display()
        ));
        out.push_str(&format!("Tasks: {}\n", selected.tasks_path.display()));
        out.push_str(&format!("State: {}\n", selected.state_path.display()));
        out.push_str(&format!("Metadata: {}\n", selected.metadata_path.display()));
        out.push_str(&format!("Logs: {}\n", selected.logs_dir.display()));
        out.push_str(&format!("Output: {}\n", selected.output_dir.display()));
    }
    out
}

pub fn read_run_logs(
    start: &Path,
    options: LogsOptions,
) -> std::result::Result<LogsView, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    read_run_logs_in_repo(&repo_root, &home, options)
}

pub fn read_run_logs_in_repo(
    repo_root: &Path,
    home: &Path,
    options: LogsOptions,
) -> std::result::Result<LogsView, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let location =
        resolve_existing_run_location(&context.repo_root, &store, options.run_id.as_deref())?;
    let logs_dir = location.logs_dir.clone();
    let mut files = discover_log_files(
        &logs_dir,
        options.task_id.as_deref(),
        options.phase.as_deref(),
    )?;
    if options.latest
        && files.len() > 1
        && let Some(latest) = files
            .iter()
            .max_by_key(|file| log_modified_key(&file.path))
            .cloned()
    {
        files = vec![latest];
    }

    let tails = match options.tail_lines {
        Some(lines) => files
            .iter()
            .map(|file| {
                Ok(LogTailView {
                    name: file.name.clone(),
                    path: file.path.clone(),
                    text: read_last_lines(&file.path, lines)?,
                })
            })
            .collect::<std::result::Result<Vec<_>, AppError>>()?,
        None => Vec::new(),
    };

    Ok(LogsView {
        run_id: location.run_id,
        run_dir: location.run_dir,
        location: location.location,
        archive_name: location.archive_name,
        logs_dir,
        files,
        tails,
    })
}

pub fn format_logs_text(view: &LogsView) -> String {
    let mut out = String::new();
    out.push_str(&format!("Run: {}\n", view.run_id));
    out.push_str(&format!("Location: {}\n", view.location));
    if let Some(archive_name) = &view.archive_name {
        out.push_str(&format!("Archive: {archive_name}\n"));
    }
    out.push_str(&format!("Logs: {}\n", view.logs_dir.display()));

    if view.files.is_empty() {
        out.push_str("No matching logs.\n");
        return out;
    }

    if view.tails.is_empty() {
        for file in &view.files {
            out.push_str(&format!(
                "- {} ({} bytes) {}\n",
                file.name,
                file.bytes,
                file.path.display()
            ));
        }
        return out;
    }

    for tail in &view.tails {
        out.push_str(&format!("\n== {} ==\n", tail.path.display()));
        out.push_str(&tail.text);
        if !tail.text.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

pub fn reset_task(
    start: &Path,
    options: ResetTaskOptions,
) -> std::result::Result<ResetTaskResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    reset_task_in_repo(&repo_root, &home, options)
}

pub fn skip_phase(
    start: &Path,
    options: SkipPhaseOptions,
) -> std::result::Result<SkipPhaseResult, AppError> {
    let repo_root = find_repo_root(start)?;
    let home = home_dir()?;
    skip_phase_in_repo(&repo_root, &home, options)
}

pub fn skip_phase_in_repo(
    repo_root: &Path,
    home: &Path,
    options: SkipPhaseOptions,
) -> std::result::Result<SkipPhaseResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, options.run_id.as_deref())?;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    recover_stale_running_tasks(&store, &run_id)?;
    let now = current_timestamp()?;

    let mut metadata = store.read_metadata(&run_id)?;
    let phase = find_phase_metadata_mut(&mut metadata, &options.phase_id)?;
    phase.decomposed = true;
    phase
        .extra
        .insert("skippedAt".to_string(), Value::String(now.clone()));
    if let Some(reason) = &options.reason {
        phase
            .extra
            .insert("skipReason".to_string(), Value::String(reason.clone()));
    } else {
        phase.extra.remove("skipReason");
    }
    if metadata.active_phase.as_deref() == Some(options.phase_id.as_str()) {
        metadata.active_phase = None;
        metadata.problem_framing = ProblemFramingState::default();
        metadata.requirement_review = RequirementReviewState::default();
        metadata.resolved_problem_file = None;
        metadata.resolved_spec_file = None;
    }
    store.write_metadata(&run_id, &metadata)?;

    let tasks_path = store.tasks_path(&run_id)?;
    let mut ignored_tasks = 0;
    let mut already_done_tasks = 0;
    if tasks_path.exists() {
        let task_file = store.read_task_file(&run_id)?;
        store.update_run_state(&run_id, |state| {
            ensure_state_matches_tasks(&task_file, state)?;
            state.problem_framing = ProblemFramingState::default();
            state.requirement_review = RequirementReviewState::default();
            for task in task_file
                .tasks
                .iter()
                .filter(|task| task_phase_label(task) == options.phase_id)
            {
                let task_state = find_task_state_mut(state, &task.id)?;
                match task_state.status {
                    TaskStatus::Done => {
                        already_done_tasks += 1;
                    }
                    TaskStatus::Ignored => {}
                    TaskStatus::Running => {
                        return Err(AppError::Runtime(format!(
                            "task {} in phase {} is still running",
                            task.id, options.phase_id
                        )));
                    }
                    _ => {
                        task_state.status = TaskStatus::Ignored;
                        task_state.ignored_at = Some(now.clone());
                        task_state.ignore_reason = options.reason.clone();
                        task_state.finished_at = Some(now.clone());
                        task_state.updated_at = Some(now.clone());
                        clear_runner_marker(task_state);
                        ignored_tasks += 1;
                    }
                }
            }
            Ok(())
        })?;
    }

    let result = SkipPhaseResult {
        run_id: run_id.clone(),
        phase_id: options.phase_id.clone(),
        tasks_path: store.tasks_path(&run_id)?,
        state_path: store.state_path(&run_id)?,
        metadata_path: store.metadata_path(&run_id)?,
        ignored_tasks,
        already_done_tasks,
        reason: options.reason.clone(),
        message: format!(
            "Skipped phase {} (ignoredTasks={}, alreadyDoneTasks={})",
            options.phase_id, ignored_tasks, already_done_tasks
        ),
    };
    append_event_log(&store.run_dir(&run_id)?, &result.message)?;
    Ok(result)
}

pub fn reset_task_in_repo(
    repo_root: &Path,
    home: &Path,
    options: ResetTaskOptions,
) -> std::result::Result<ResetTaskResult, AppError> {
    let context = load_config(repo_root, home, true)?;
    let store = RunStore::for_repo(&context.repo_root, &context.home_dir)
        .map_err(|err| AppError::Runtime(format!("failed to resolve run store: {err}")))?;
    let run_id = select_run_id(&store, options.run_id.as_deref())?;
    let _execution_lock = store.try_acquire_execution_lock(&run_id)?;
    recover_stale_running_tasks(&store, &run_id)?;
    let task_file = store.read_task_file(&run_id)?;
    let task = find_task(&task_file, &options.task_id)?.clone();
    if matches!(options.phase, TaskPhase::AnalysisReview | TaskPhase::Done) {
        return Err(AppError::Config(format!(
            "reset phase {} is not runnable",
            options.phase.as_str()
        )));
    }

    let now = current_timestamp()?;
    let reset = store.update_run_state(&run_id, |state| {
        ensure_state_matches_tasks(&task_file, state)?;
        let task_state = state
            .tasks
            .iter_mut()
            .find(|candidate| candidate.id == task.id)
            .expect("state was normalized");
        if task_state.status == TaskStatus::Done {
            return Err(AppError::Runtime(format!(
                "task {} is already done; refusing to reset committed work",
                task.id
            )));
        }
        if task_state.status == TaskStatus::Ignored {
            return Err(AppError::Runtime(format!(
                "task {} is ignored; edit state.json manually if you really want to resurrect it",
                task.id
            )));
        }
        if task_state.status == TaskStatus::Running {
            return Err(AppError::Runtime(format!(
                "task {} is still running; stop the runner or wait for stale recovery first",
                task.id
            )));
        }

        task_state.status = TaskStatus::Pending;
        task_state.phase = Some(options.phase);
        task_state.started_at = None;
        task_state.finished_at = None;
        task_state.updated_at = Some(now.clone());
        task_state.last_error = None;
        task_state.last_exit_code = None;
        task_state.last_log = None;
        task_state.last_verdict = None;
        task_state.last_review_comments = None;
        clear_runner_marker(task_state);
        if options.clear_attempts {
            task_state.attempts = 0;
        }
        if options.clear_review_attempts {
            task_state.review_attempts = 0;
        }

        Ok((
            task_state.attempts,
            task_state.review_attempts,
            task_state.phase.expect("phase was set"),
        ))
    })?;

    let max_attempts = task_max_attempts(&task);
    let max_review_attempts = task_max_review_attempts(&task);
    let mut warnings = Vec::new();
    if !matches!(
        reset.2,
        TaskPhase::Verify | TaskPhase::Review | TaskPhase::Commit
    ) && reset.0 >= max_attempts
    {
        warnings.push(format!(
            "attempts is still {}/{}; increase maxAttempts in tasks.json or rerun reset with --clear-attempts",
            reset.0, max_attempts
        ));
    }
    if reset.2 == TaskPhase::Review && reset.1 >= max_review_attempts {
        warnings.push(format!(
            "reviewAttempts is still {}/{}; increase maxReviewAttempts in tasks.json or rerun reset with --clear-review-attempts",
            reset.1, max_review_attempts
        ));
    }

    Ok(ResetTaskResult {
        run_id: run_id.clone(),
        task_id: task.id.clone(),
        phase: reset.2.as_str().to_string(),
        tasks_path: store.tasks_path(&run_id)?,
        state_path: store.state_path(&run_id)?,
        attempts: reset.0,
        max_attempts,
        review_attempts: reset.1,
        max_review_attempts,
        warnings,
        message: format!(
            "Reset task {} to pending phase {}",
            task.id,
            reset.2.as_str()
        ),
    })
}

pub fn format_reset_text(result: &ResetTaskResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", result.message));
    out.push_str(&format!("Run: {}\n", result.run_id));
    out.push_str(&format!("Tasks: {}\n", result.tasks_path.display()));
    out.push_str(&format!("State: {}\n", result.state_path.display()));
    out.push_str(&format!(
        "Attempts: {}/{}\n",
        result.attempts, result.max_attempts
    ));
    out.push_str(&format!(
        "Review attempts: {}/{}\n",
        result.review_attempts, result.max_review_attempts
    ));
    for warning in &result.warnings {
        out.push_str(&format!("warning: {warning}\n"));
    }
    out.push_str(&format!(
        "Next: codex-task run {} --run-id {} --from {}\n",
        result.task_id, result.run_id, result.phase
    ));
    out
}

pub fn format_skip_phase_text(result: &SkipPhaseResult) -> String {
    let mut out = String::new();
    out.push_str(&format!("{}\n", result.message));
    out.push_str(&format!("Run: {}\n", result.run_id));
    out.push_str(&format!("Phase: {}\n", result.phase_id));
    out.push_str(&format!("Metadata: {}\n", result.metadata_path.display()));
    out.push_str(&format!("Tasks: {}\n", result.tasks_path.display()));
    out.push_str(&format!("State: {}\n", result.state_path.display()));
    if let Some(reason) = &result.reason {
        out.push_str(&format!("Reason: {reason}\n"));
    }
    out.push_str(&format!(
        "Next: codex-task watch --run-id {}\n",
        result.run_id
    ));
    out
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
    let run_state = store.read_run_state(&selected_run_id)?;
    let tasks_path = store.tasks_path(&selected_run_id)?;
    let task_file = if tasks_path.exists() {
        read_task_file(&tasks_path)?
    } else {
        let metadata = store.read_metadata(&selected_run_id)?;
        status_task_file_from_metadata(&metadata)
    };

    Ok(StatusResult::View(Box::new(merge_status_view(
        run_dir, task_file, run_state,
    )?)))
}

fn status_task_file_from_metadata(metadata: &RunMetadata) -> TaskFile {
    let spec_file = metadata
        .resolved_spec_file
        .clone()
        .or_else(|| metadata.resolved_problem_file.clone())
        .unwrap_or_else(|| metadata.spec_file.clone());
    let spec_files =
        if metadata.resolved_spec_file.is_some() || metadata.resolved_problem_file.is_some() {
            vec![spec_file.clone()]
        } else {
            normalize_spec_files(&metadata.spec_file, &metadata.spec_files)
        };
    TaskFile {
        schema_version: 2,
        run_id: metadata.run_id.clone(),
        branch: metadata.branch.clone(),
        spec_file,
        spec_files,
        verification_commands: Vec::new(),
        tasks: Vec::new(),
        extra: Map::new(),
    }
}

fn read_metadata_or_task_file(
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
) -> std::result::Result<RunMetadata, AppError> {
    match store.read_metadata(run_id) {
        Ok(metadata) => Ok(metadata),
        Err(AppError::Io(_)) => Ok(RunMetadata {
            schema_version: 1,
            run_id: task_file.run_id.clone(),
            branch: task_file.branch.clone(),
            spec_file: task_file.spec_file.clone(),
            spec_files: task_file.spec_files.clone(),
            problem_framing: ProblemFramingState::default(),
            resolved_problem_file: None,
            requirement_review: RequirementReviewState::default(),
            resolved_spec_file: None,
            phases: Vec::new(),
            active_phase: None,
            extra: Map::new(),
        }),
        Err(err) => Err(err),
    }
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

fn resolve_existing_run_location(
    repo_root: &Path,
    store: &RunStore,
    requested: Option<&str>,
) -> std::result::Result<RunLocationView, AppError> {
    match requested {
        Some(run_id) => {
            RunId::parse(run_id)?;
            let active = store.run_dir(run_id)?;
            if active.exists() {
                return Ok(run_location_view(
                    repo_root,
                    run_id.to_string(),
                    active,
                    false,
                    None,
                ));
            }
            let archives = discover_archived_runs(store)?
                .into_iter()
                .filter(|archive| archive.run_id == run_id)
                .collect::<Vec<_>>();
            if let Some(archive) = archives.last() {
                return Ok(run_location_view(
                    repo_root,
                    archive.run_id.clone(),
                    archive.run_dir.clone(),
                    true,
                    Some(archive.archive_name.clone()),
                ));
            }
            Err(AppError::Runtime(format!(
                "run {run_id} does not exist under {} or its archive",
                store.repo_runs_dir.display()
            )))
        }
        None => {
            let active_runs = discover_run_ids(&store.repo_runs_dir)?;
            if active_runs.len() == 1 {
                let run_id = active_runs[0].clone();
                return Ok(run_location_view(
                    repo_root,
                    run_id.clone(),
                    store.run_dir(&run_id)?,
                    false,
                    None,
                ));
            }
            if active_runs.len() > 1 {
                return Err(AppError::Config(format!(
                    "Multiple active runs found under {}: {}. Pass --run-id.",
                    store.repo_runs_dir.display(),
                    active_runs.join(", ")
                )));
            }

            let archives = discover_archived_runs(store)?;
            if archives.len() == 1 {
                let archive = &archives[0];
                return Ok(run_location_view(
                    repo_root,
                    archive.run_id.clone(),
                    archive.run_dir.clone(),
                    true,
                    Some(archive.archive_name.clone()),
                ));
            }
            if archives.is_empty() {
                Err(AppError::Runtime(format!(
                    "No runs found under {}",
                    store.repo_runs_dir.display()
                )))
            } else {
                let names = archives
                    .iter()
                    .map(|archive| format!("{} ({})", archive.run_id, archive.archive_name))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(AppError::Config(format!(
                    "Multiple archived runs found under {}: {names}. Pass --run-id.",
                    store.repo_runs_dir.join("archive").display()
                )))
            }
        }
    }
}

fn run_location_view(
    repo_root: &Path,
    run_id: String,
    run_dir: PathBuf,
    archived: bool,
    archive_name: Option<String>,
) -> RunLocationView {
    RunLocationView {
        visible_run_dir: project_task_run_dir(repo_root, &run_id),
        run_id,
        tasks_path: run_dir.join("tasks.json"),
        state_path: run_dir.join("state.json"),
        metadata_path: run_dir.join("metadata.json"),
        logs_dir: run_dir.join("logs"),
        output_dir: run_dir.join("output"),
        run_dir,
        location: if archived { "archive" } else { "active" }.to_string(),
        archive_name,
    }
}

fn discover_archived_runs(store: &RunStore) -> std::result::Result<Vec<ArchivedRunView>, AppError> {
    let archive_root = store.repo_runs_dir.join("archive");
    if !archive_root.exists() {
        return Ok(Vec::new());
    }
    let mut archives = Vec::new();
    for entry in fs::read_dir(&archive_root)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", archive_root.display())))?
    {
        let entry = entry.map_err(|err| {
            AppError::Io(format!(
                "failed to read {} entry: {err}",
                archive_root.display()
            ))
        })?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let archive_name = entry.file_name().to_string_lossy().to_string();
        let run_id = read_task_file(&path.join("tasks.json"))
            .map(|task_file| task_file.run_id)
            .unwrap_or_else(|_| archive_name.clone());
        archives.push(ArchivedRunView {
            run_id,
            archive_name,
            run_dir: path,
        });
    }
    archives.sort_by(|left, right| {
        left.run_id
            .cmp(&right.run_id)
            .then_with(|| left.archive_name.cmp(&right.archive_name))
    });
    Ok(archives)
}

fn discover_log_files(
    logs_dir: &Path,
    task_id: Option<&str>,
    phase: Option<&str>,
) -> std::result::Result<Vec<LogFileView>, AppError> {
    if !logs_dir.exists() {
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(logs_dir)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", logs_dir.display())))?
    {
        let entry = entry.map_err(|err| {
            AppError::Io(format!(
                "failed to read {} entry: {err}",
                logs_dir.display()
            ))
        })?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(task_id) = task_id
            && !name.starts_with(&format!("{task_id}."))
        {
            continue;
        }
        if let Some(phase) = phase
            && !log_name_matches_phase(&name, phase)
        {
            continue;
        }
        let bytes = entry.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        files.push(LogFileView { name, path, bytes });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

fn log_name_matches_phase(name: &str, phase: &str) -> bool {
    name.starts_with(&format!("{phase}.")) || name.contains(&format!(".{phase}."))
}

fn log_modified_key(path: &Path) -> SystemTime {
    path.metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn read_last_lines(path: &Path, max_lines: usize) -> std::result::Result<String, AppError> {
    let text = fs::read_to_string(path)
        .map_err(|err| AppError::Io(format!("failed to read {}: {err}", path.display())))?;
    if max_lines == 0 {
        return Ok(String::new());
    }
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    let mut out = lines[start..].join("\n");
    if text.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    Ok(out)
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
    scope: &WatchScope,
) -> std::result::Result<Option<String>, AppError> {
    let mut tasks = task_file.tasks.iter().collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| left.id.cmp(&right.id))
    });

    for task in tasks {
        if !task_in_watch_scope(task_file, task, scope) {
            continue;
        }
        if runnable_status(task, task_file, state)? == RunnableCheck::Runnable {
            return Ok(Some(task.id.clone()));
        }
    }
    Ok(None)
}

fn count_tasks_with_status_in_scope(
    task_file: &TaskFile,
    state: &RunState,
    status: TaskStatus,
    scope: &WatchScope,
) -> std::result::Result<usize, AppError> {
    let state_by_id = normalized_state_map(task_file, state)?;
    Ok(task_file
        .tasks
        .iter()
        .filter(|task| task_in_watch_scope(task_file, task, scope))
        .filter(|task| {
            state_by_id
                .get(task.id.as_str())
                .map(|task_state| task_state.status == status)
                .unwrap_or(false)
        })
        .count())
}

fn all_tasks_terminal(
    task_file: &TaskFile,
    state: &RunState,
) -> std::result::Result<bool, AppError> {
    let state_by_id = normalized_state_map(task_file, state)?;
    Ok(task_file.tasks.iter().all(|task| {
        state_by_id
            .get(task.id.as_str())
            .map(|task_state| matches!(task_state.status, TaskStatus::Done | TaskStatus::Ignored))
            .unwrap_or(false)
    }))
}

fn prepare_next_phase_for_watch(
    context: &ConfigContext,
    store: &RunStore,
    run_id: &str,
    task_file: &TaskFile,
    scope: &WatchScope,
    codex_bin: Option<PathBuf>,
) -> std::result::Result<Option<PhasePrepareOutcome>, AppError> {
    if scope.group.is_some() {
        return Ok(None);
    }
    let metadata = match store.read_metadata(run_id) {
        Ok(metadata) => metadata,
        Err(AppError::Io(_)) => return Ok(None),
        Err(err) => return Err(err),
    };
    let Some(phase) = metadata.phases.iter().find(|phase| {
        !phase.decomposed && phase_allowed_by_watch_scope(&metadata, &phase.id, scope)
    }) else {
        return Ok(None);
    };
    let phase_id = phase.id.clone();
    let mut warnings = Vec::new();
    let append = !task_file.tasks.is_empty();
    prepare_run_phase(
        context,
        store,
        run_id,
        &phase_id,
        append,
        codex_bin,
        &mut warnings,
    )
    .map(Some)
}

fn phase_allowed_by_watch_scope(
    metadata: &RunMetadata,
    phase_id: &str,
    scope: &WatchScope,
) -> bool {
    if let Some(phase) = &scope.phase {
        return phase == phase_id;
    }
    if let Some(until_phase) = &scope.until_phase {
        let Some(target_rank) = metadata_phase_rank(metadata, until_phase) else {
            return false;
        };
        let Some(phase_rank) = metadata_phase_rank(metadata, phase_id) else {
            return false;
        };
        return phase_rank <= target_rank;
    }
    true
}

fn metadata_phase_rank(metadata: &RunMetadata, phase_id: &str) -> Option<usize> {
    metadata
        .phases
        .iter()
        .position(|phase| phase.id == phase_id)
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
        fallback_required_output_from_last_message: true,
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
        fallback_required_output_from_last_message: false,
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
        fallback_required_output_from_last_message: true,
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
    !is_tool_collaboration_path(path)
        && !path_matches_any(&config.add_exclude, path)
        && (config.add_include.is_empty() || path_matches_any(&config.add_include, path))
}

fn is_tool_collaboration_path(path: &str) -> bool {
    matches!(path, ".codex/task-runner" | ".codex/task-runs")
        || path.starts_with(".codex/task-runner/")
        || path.starts_with(".codex/task-runs/")
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
    let spec_files = task_spec_files(task, &task_file);
    let spec = read_combined_spec_document(context, &spec_files)?;
    let spec_file = task_spec_label(&spec_files);
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
    let spec_files = task_spec_files(task, &task_file);
    let spec = read_combined_spec_document(context, &spec_files)?;
    let spec_file = task_spec_label(&spec_files);
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
    let spec_files = task_spec_files(task, &task_file);
    let spec = read_combined_spec_document(context, &spec_files)?;
    let spec_file = task_spec_label(&spec_files);
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
    Ok(git_status_entries(repo_root)?
        .iter()
        .any(|entry| !is_tool_collaboration_path(&entry.path)))
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
    normalize_task_scopes(&mut task_file);
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
    for spec_file in &task_file.spec_files {
        if Path::new(spec_file).is_absolute() {
            return Err(AppError::Config(format!(
                "specFiles entries must be repo-relative: {spec_file}"
            )));
        }
    }

    let mut ids = BTreeSet::new();
    for task in &task_file.tasks {
        if !ids.insert(task.id.clone()) {
            return Err(AppError::Config(format!("duplicate task id {}", task.id)));
        }
    }

    for task in &task_file.tasks {
        if let Some(spec_file) = &task.spec_file
            && Path::new(spec_file).is_absolute()
        {
            return Err(AppError::Config(format!(
                "task {} specFile must be repo-relative: {spec_file}",
                task.id
            )));
        }
        for spec_file in &task.spec_files {
            if Path::new(spec_file).is_absolute() {
                return Err(AppError::Config(format!(
                    "task {} specFiles entries must be repo-relative: {spec_file}",
                    task.id
                )));
            }
        }
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
    for spec_file in &metadata.spec_files {
        if Path::new(spec_file).is_absolute() {
            return Err(AppError::Config(format!(
                "metadata specFiles entries must be repo-relative: {spec_file}"
            )));
        }
    }
    let mut phase_ids = BTreeSet::new();
    for phase in &metadata.phases {
        if phase.id.trim().is_empty() {
            return Err(AppError::Config(
                "metadata phase id must not be empty".to_string(),
            ));
        }
        if !phase_ids.insert(phase.id.clone()) {
            return Err(AppError::Config(format!(
                "duplicate metadata phase id {}",
                phase.id
            )));
        }
        if Path::new(&phase.spec_file).is_absolute() {
            return Err(AppError::Config(format!(
                "metadata phase {} specFile must be repo-relative: {}",
                phase.id, phase.spec_file
            )));
        }
        for spec_file in &phase.spec_files {
            if Path::new(spec_file).is_absolute() {
                return Err(AppError::Config(format!(
                    "metadata phase {} specFiles entries must be repo-relative: {spec_file}",
                    phase.id
                )));
            }
        }
    }
    if let Some(active_phase) = &metadata.active_phase
        && !phase_ids.contains(active_phase)
    {
        return Err(AppError::Config(format!(
            "activePhase references unknown phase: {active_phase}"
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
        problem_framing_status: run_state.problem_framing.status.as_str().to_string(),
        requirement_review_status: run_state.requirement_review.status.as_str().to_string(),
        feature_review_status: run_state.feature_review_status.as_str().to_string(),
        feature_review_attempts: run_state.feature_review_attempts,
        final_review_status: run_state.final_review.status.as_str().to_string(),
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
        "Problem framing: {}\n",
        view.problem_framing_status
    ));
    out.push_str(&format!(
        "Requirement review: {}\n",
        view.requirement_review_status
    ));
    out.push_str(&format!(
        "Feature review: {} (attempts: {})\n",
        view.feature_review_status, view.feature_review_attempts
    ));
    out.push_str(&format!("Final review: {}\n", view.final_review_status));
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
    fn output_prompt_path_is_repo_relative_even_when_file_is_missing() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let visible_dir = repo.join(".codex/task-runs/readme");
        fs::create_dir_all(&visible_dir).unwrap();

        let output = visible_dir.join("resolved-problem.md");
        let relative = repo_relative_slash_path_for_output(&repo, &output).unwrap();

        assert_eq!(relative, ".codex/task-runs/readme/resolved-problem.md");
        assert!(!output.exists());
    }

    #[test]
    fn output_prompt_path_rejects_paths_outside_repo() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();

        let err = repo_relative_slash_path_for_output(&repo, &outside.join("report.md"))
            .expect_err("outside output path should be rejected");

        assert!(err.to_string().contains("outside repo"));
    }

    #[test]
    fn log_phase_filter_matches_top_level_and_task_phase_names() {
        assert!(log_name_matches_phase(
            "resolve-problem.stderr.log",
            "resolve-problem"
        ));
        assert!(log_name_matches_phase(
            "resolve-problem.last-message.md",
            "resolve-problem"
        ));
        assert!(log_name_matches_phase(
            "p1.implement.stderr.log",
            "implement"
        ));
        assert!(!log_name_matches_phase(
            "requirement-review.stderr.log",
            "resolve-problem"
        ));
    }

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
    fn builtin_prompt_templates_render_all_typed_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::write(repo.join(".codex/task-runner.toml"), "").unwrap();
        let context = load_config(&repo, &home, true).unwrap();

        let problem = load_prompt_template(&context, PromptTemplateKind::ProblemFraming)
            .unwrap()
            .render(&sample_problem_framing_input())
            .unwrap();
        assert!(problem.contains("Problem framing output path"));
        assert!(problem.contains("NEEDS_DECISION"));

        let resolve_problem = load_prompt_template(&context, PromptTemplateKind::ResolveProblem)
            .unwrap()
            .render(&sample_resolve_problem_input())
            .unwrap();
        assert!(resolve_problem.contains("Resolved problem output path"));
        assert!(resolve_problem.contains("User decision"));

        let decompose = load_prompt_template(&context, PromptTemplateKind::DecomposeFeature)
            .unwrap()
            .render(&sample_decompose_input())
            .unwrap();
        assert!(decompose.contains("run-1"));
        assert!(decompose.contains("Feature body"));

        let requirement = load_prompt_template(&context, PromptTemplateKind::RequirementReview)
            .unwrap()
            .render(&sample_requirement_review_input())
            .unwrap();
        assert!(requirement.contains("Requirement review output path"));
        assert!(requirement.contains("NEEDS_CLARIFICATION"));

        let resolve = load_prompt_template(&context, PromptTemplateKind::ResolveRequirement)
            .unwrap()
            .render(&sample_resolve_requirement_input())
            .unwrap();
        assert!(resolve.contains("Resolved spec output path"));
        assert!(resolve.contains("User answers"));

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

        let shard = load_prompt_template(&context, PromptTemplateKind::FinalReviewShard)
            .unwrap()
            .render(&sample_final_review_shard_input())
            .unwrap();
        assert!(shard.contains("Review type: security"));
        assert!(shard.contains("Findings output path"));

        let aggregate = load_prompt_template(&context, PromptTemplateKind::FinalReviewAggregate)
            .unwrap()
            .render(&sample_final_review_aggregate_input())
            .unwrap();
        assert!(aggregate.contains("Aggregate review output path"));
        assert!(aggregate.contains("Shard findings"));
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
    fn codex_executor_can_fallback_to_last_message_for_workspace_write_required_output() {
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
printf '# Resolved\n\nWorkspace-write fallback.\n' > "$last"
"#,
        );
        let mut request = sample_codex_request(temp.path());
        let required_output_path = temp
            .path()
            .join(".codex/task-runs/readme/resolved-problem.md");
        request.required_output_path = Some(required_output_path.clone());
        request.fallback_required_output_from_last_message = true;

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
            "# Resolved\n\nWorkspace-write fallback.\n"
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
            spec_files: vec!["docs/roadmap/sample.md".to_string()],
            verification_commands: Vec::new(),
            tasks: vec![
                Task {
                    id: "p1".to_string(),
                    priority: 1,
                    group: "g".to_string(),
                    phase: "g".to_string(),
                    title: "Pending".to_string(),
                    max_attempts: None,
                    timeout_seconds: None,
                    output: "out.md".to_string(),
                    prompt: "do it".to_string(),
                    spec_file: None,
                    spec_files: Vec::new(),
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
                    phase: "g".to_string(),
                    title: "Done".to_string(),
                    max_attempts: None,
                    timeout_seconds: None,
                    output: "out.md".to_string(),
                    prompt: "do it".to_string(),
                    spec_file: None,
                    spec_files: Vec::new(),
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
            problem_framing: ProblemFramingState::default(),
            requirement_review: RequirementReviewState::default(),
            final_review: FinalReviewState::default(),
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
    fn status_handles_run_before_tasks_are_decomposed() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();

        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.ensure_repo_dir().unwrap();
        let run_dir = store.run_dir("readme").unwrap();
        fs::create_dir_all(&run_dir).unwrap();
        store
            .write_metadata(
                "readme",
                &RunMetadata {
                    schema_version: 1,
                    run_id: "readme".to_string(),
                    branch: "feat/readme".to_string(),
                    spec_file: "docs/roadmap/agent-platform/README.md".to_string(),
                    spec_files: vec!["docs/roadmap/agent-platform/README.md".to_string()],
                    phases: Vec::new(),
                    active_phase: None,
                    problem_framing: ProblemFramingState {
                        status: ProblemFramingStatus::Resolved,
                        ..ProblemFramingState::default()
                    },
                    resolved_problem_file: Some(
                        ".codex/task-runs/readme/resolved-problem.md".to_string(),
                    ),
                    requirement_review: RequirementReviewState {
                        status: RequirementReviewStatus::Clear,
                        ..RequirementReviewState::default()
                    },
                    resolved_spec_file: None,
                    extra: Map::new(),
                },
            )
            .unwrap();
        store
            .write_run_state(
                "readme",
                &RunState {
                    problem_framing: ProblemFramingState {
                        status: ProblemFramingStatus::Resolved,
                        ..ProblemFramingState::default()
                    },
                    requirement_review: RequirementReviewState {
                        status: RequirementReviewStatus::Clear,
                        ..RequirementReviewState::default()
                    },
                    ..RunState::default()
                },
            )
            .unwrap();

        let status = load_status(&repo, &home, Some("readme")).unwrap();
        let StatusResult::View(view) = status else {
            panic!("expected status view");
        };

        assert_eq!(view.problem_framing_status, "resolved");
        assert_eq!(view.requirement_review_status, "clear");
        assert_eq!(
            view.spec_file,
            ".codex/task-runs/readme/resolved-problem.md"
        );
        assert!(view.tasks.is_empty());
        assert_eq!(view.counts["pending"], 0);
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
    fn verification_command_accepts_null_timeout_seconds() {
        let task_file = parse_task_file_json(
            r#"{
  "version": 2,
  "runId": "run",
  "branch": "feat/run",
  "specFile": "spec.md",
  "tasks": [
    {
      "id": "p1",
      "priority": 1,
      "group": "backend",
      "title": "Task",
      "output": "output/p1.md",
      "prompt": "Do it.",
      "dependsOn": [],
      "reviewCriteria": [],
      "verificationCommands": [
        {
          "name": "unit",
          "command": "cargo test",
          "required": true,
          "timeoutSeconds": null
        }
      ]
    }
  ]
}"#,
        )
        .unwrap();

        let verification = &task_file.tasks[0].verification_commands[0];
        assert_eq!(verification.name, "unit");
        assert_eq!(verification.timeout_seconds, None);
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
            problem_framing: ProblemFramingState::default(),
            requirement_review: RequirementReviewState::default(),
            final_review: FinalReviewState::default(),
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
            &requirement_clear_then_last_message_script(&sample_decompose_json(
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
                spec_paths: Vec::new(),
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
            &requirement_clear_then_last_message_script(&sample_decompose_json(
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
                spec_paths: Vec::new(),
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
                group: None,
                phase: None,
                until_phase: None,
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
            &requirement_clear_then_last_message_script(&sample_decompose_json(
                "spec",
                "feat/spec",
                "docs/spec.md",
            )),
        );
        let first = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("docs/spec.md"),
                spec_paths: Vec::new(),
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
                spec_paths: Vec::new(),
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
            &requirement_clear_then_last_message_script(&format!("```json\n{json}\n```\n")),
        );
        let result = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("feature.md"),
                spec_paths: Vec::new(),
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

    #[test]
    fn decompose_output_accepts_description_and_defaults_output() {
        let parsed = parse_decompose_output(
            r#"{
  "version": 2,
  "runId": "readme",
  "branch": "feat/readme",
  "specFile": ".codex/task-runs/readme/resolved-problem.md",
  "tasks": [
    {
      "id": "T001",
      "priority": 1,
      "group": "docs",
      "title": "Write docs",
      "description": "Update the roadmap docs.",
      "dependsOn": [],
      "reviewCriteria": ["Docs are updated."],
      "verificationCommands": ["git diff --check docs/roadmap/agent-platform/README.md"]
    }
  ]
}"#,
        )
        .unwrap();

        let task = &parsed.task_file.tasks[0];
        assert_eq!(task.prompt, "Update the roadmap docs.");
        assert_eq!(task.output, "output/t001.md");
        assert_eq!(
            parsed.task_file.spec_files,
            vec![".codex/task-runs/readme/resolved-problem.md".to_string()]
        );
        assert_eq!(task.phase, "docs");
        assert_eq!(
            task.spec_files,
            vec![".codex/task-runs/readme/resolved-problem.md".to_string()]
        );
        assert_eq!(task.verification_commands.len(), 1);
        assert_eq!(
            task.verification_commands[0].command,
            "git diff --check docs/roadmap/agent-platform/README.md"
        );
    }

    #[test]
    fn decompose_output_preserves_task_phase_and_spec_files() {
        let parsed = parse_decompose_output(
            r#"{
  "version": 2,
  "runId": "roadmap",
  "branch": "feat/roadmap",
  "specFile": "docs/roadmap/ai-platform/01-foundation-data-model.md",
  "specFiles": [
    "docs/roadmap/ai-platform/01-foundation-data-model.md",
    "docs/roadmap/ai-platform/02-domain-event-outbox-runtime.md"
  ],
  "tasks": [
    {
      "id": "agent-01",
      "priority": 1,
      "group": "foundation",
      "phase": "01-foundation-data-model",
      "title": "Add data model",
      "output": "output/agent-01.md",
      "prompt": "Implement only the foundation data model.",
      "specFiles": ["docs/roadmap/ai-platform/01-foundation-data-model.md"],
      "dependsOn": [],
      "reviewCriteria": [],
      "verificationCommands": []
    },
    {
      "id": "agent-02",
      "priority": 2,
      "group": "runtime",
      "phase": "02-domain-event-outbox-runtime",
      "title": "Add outbox runtime",
      "output": "output/agent-02.md",
      "prompt": "Implement only the outbox runtime.",
      "specFiles": ["docs/roadmap/ai-platform/02-domain-event-outbox-runtime.md"],
      "dependsOn": ["agent-01"],
      "reviewCriteria": [],
      "verificationCommands": []
    }
  ]
}"#,
        )
        .unwrap();

        assert_eq!(parsed.task_file.spec_files.len(), 2);
        assert_eq!(parsed.task_file.tasks[0].phase, "01-foundation-data-model");
        assert_eq!(
            parsed.task_file.tasks[0].spec_files,
            vec!["docs/roadmap/ai-platform/01-foundation-data-model.md".to_string()]
        );
        assert_eq!(
            parsed.task_file.tasks[1].phase,
            "02-domain-event-outbox-runtime"
        );
        assert_eq!(
            parsed.task_file.tasks[1].spec_files,
            vec!["docs/roadmap/ai-platform/02-domain-event-outbox-runtime.md".to_string()]
        );
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

        let codex = fake_codex_script(
            temp.path(),
            &requirement_clear_then_last_message_script("not json\n"),
        );
        let err = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("invalid-json.md"),
                spec_paths: Vec::new(),
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
            fs::read_to_string(run_dir.join("logs/invalid-json.decompose.last-message.md"))
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

        let codex = fake_codex_script(temp.path(), &requirement_clear_then_fail_script());
        let err = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("codex-fail.md"),
                spec_paths: Vec::new(),
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
            fs::read_to_string(run_dir.join("logs/codex-fail.decompose.stderr.log"))
                .unwrap()
                .contains("codex failed")
        );
    }

    #[cfg(unix)]
    #[test]
    fn problem_framing_needs_decision_writes_visible_files_and_no_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            repo.join("feature.md"),
            "# Feature\n\nReplace the auth model with a shortcut.\n",
        )
        .unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let codex = fake_codex_script(
            temp.path(),
            &problem_needs_decision_script(
                "- Option A: keep auth boundaries.\n- Option B: shortcut.",
            ),
        );
        let result = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("feature.md"),
                spec_paths: Vec::new(),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.problem_status, "needs_decision");
        assert_eq!(result.requirement_status, "pending");
        assert!(!result.tasks_path.exists());
        let decision = result.decision_path.unwrap();
        assert!(decision.starts_with(repo.join(".codex/task-runs/feature")));
        assert!(!repo.join(".codex/task-runs/feature/options.md").exists());
        let decision_text = fs::read_to_string(&decision).unwrap();
        assert!(decision_text.contains("Option A"));
        assert!(decision_text.contains("TODO"));

        let store = RunStore::for_repo(&repo, &home).unwrap();
        let state = store.read_run_state("feature").unwrap();
        assert_eq!(
            state.problem_framing.status,
            ProblemFramingStatus::NeedsDecision
        );
        let metadata = store.read_metadata("feature").unwrap();
        assert_eq!(
            metadata.problem_framing.status,
            ProblemFramingStatus::NeedsDecision
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_after_problem_decision_writes_resolved_problem_and_decomposes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            repo.join("feature.md"),
            "# Feature\n\nReplace the auth model with a shortcut.\n",
        )
        .unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let start_codex = fake_codex_script(
            temp.path(),
            &problem_needs_decision_script(
                "- Option A: keep auth boundaries.\n- Option B: shortcut.",
            ),
        );
        let start = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("feature.md"),
                spec_paths: Vec::new(),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(start_codex),
            },
        )
        .unwrap();
        let decision_path = start.decision_path.unwrap();
        let decision = fs::read_to_string(&decision_path)
            .unwrap()
            .replace("TODO", "Use Option A and keep auth boundaries.");
        fs::write(&decision_path, decision).unwrap();

        let resolved_problem_file = ".codex/task-runs/feature/phases/feature/resolved-problem.md";
        let resume_codex = fake_codex_script(
            temp.path(),
            &resolve_problem_then_requirement_clear_then_decompose_script(
                "# Resolved Problem\n\nUse Option A and keep auth boundaries.\n",
                &sample_decompose_json("feature", "feat/feature", resolved_problem_file),
            ),
        );
        let resumed = resume_run_in_repo(
            &repo,
            &home,
            ResumeOptions {
                run_id: "feature".to_string(),
                codex_bin: Some(resume_codex),
            },
        )
        .unwrap();

        assert!(resumed.tasks_path.exists());
        assert_eq!(resumed.problem_status, "resolved");
        assert_eq!(resumed.requirement_status, "clear");
        assert_eq!(resumed.spec_file, resolved_problem_file);
        assert!(resumed.resolved_problem_path.unwrap().exists());
        let store = RunStore::for_repo(&repo, &home).unwrap();
        let task_file = store.read_task_file("feature").unwrap();
        assert_eq!(task_file.spec_file, resolved_problem_file);
        let state = store.read_run_state("feature").unwrap();
        assert_eq!(state.problem_framing.status, ProblemFramingStatus::Resolved);
        let metadata = store.read_metadata("feature").unwrap();
        assert_eq!(
            metadata.resolved_problem_file.as_deref(),
            Some(resolved_problem_file)
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_decomposes_after_reviews_clear_when_tasks_missing() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            repo.join("feature.md"),
            "# Feature\n\nReplace the auth model with a shortcut.\n",
        )
        .unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let start_codex = fake_codex_script(
            temp.path(),
            &problem_needs_decision_script(
                "- Option A: keep auth boundaries.\n- Option B: shortcut.",
            ),
        );
        let start = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("feature.md"),
                spec_paths: Vec::new(),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(start_codex),
            },
        )
        .unwrap();
        let decision_path = start.decision_path.unwrap();
        let decision = fs::read_to_string(&decision_path)
            .unwrap()
            .replace("TODO", "Use Option A and keep auth boundaries.");
        fs::write(&decision_path, decision).unwrap();

        let resolved_problem_file = ".codex/task-runs/feature/phases/feature/resolved-problem.md";
        let first_resume_codex = fake_codex_script(
            temp.path(),
            &resolve_problem_then_requirement_clear_then_decompose_script(
                "# Resolved Problem\n\nUse Option A and keep auth boundaries.\n",
                &sample_decompose_json("feature", "feat/feature", resolved_problem_file),
            ),
        );
        resume_run_in_repo(
            &repo,
            &home,
            ResumeOptions {
                run_id: "feature".to_string(),
                codex_bin: Some(first_resume_codex),
            },
        )
        .unwrap();

        let store = RunStore::for_repo(&repo, &home).unwrap();
        fs::remove_file(store.tasks_path("feature").unwrap()).unwrap();

        let decompose_codex = fake_codex_script(
            temp.path(),
            &decompose_success_script(&sample_decompose_json(
                "feature",
                "feat/feature",
                resolved_problem_file,
            )),
        );
        let resumed = resume_run_in_repo(
            &repo,
            &home,
            ResumeOptions {
                run_id: "feature".to_string(),
                codex_bin: Some(decompose_codex),
            },
        )
        .unwrap();

        assert!(resumed.tasks_path.exists());
        assert!(resumed.resumed);
        assert_eq!(resumed.spec_file, resolved_problem_file);
        let task_file = store.read_task_file("feature").unwrap();
        assert_eq!(task_file.spec_file, resolved_problem_file);
        assert!(
            fs::read_to_string(store.run_dir("feature").unwrap().join("logs/events.log"))
                .unwrap()
                .contains("resuming task decomposition")
        );
    }

    #[cfg(unix)]
    #[test]
    fn requirement_review_needs_clarification_writes_visible_files_and_no_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            repo.join("feature.md"),
            "# Feature\n\nDo something vague.\n",
        )
        .unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let codex = fake_codex_script(
            temp.path(),
            &requirement_needs_clarification_script("- What exactly should change?"),
        );
        let result = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("feature.md"),
                spec_paths: Vec::new(),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        assert_eq!(result.requirement_status, "needs_clarification");
        assert!(!result.tasks_path.exists());
        let questions = result.questions_path.unwrap();
        let answers = result.answers_path.unwrap();
        assert!(questions.starts_with(repo.join(".codex/task-runs/feature")));
        assert!(answers.starts_with(repo.join(".codex/task-runs/feature")));
        assert!(
            fs::read_to_string(&questions)
                .unwrap()
                .contains("What exactly")
        );
        assert!(fs::read_to_string(&answers).unwrap().contains("TODO"));

        let store = RunStore::for_repo(&repo, &home).unwrap();
        let state = store.read_run_state("feature").unwrap();
        assert_eq!(
            state.requirement_review.status,
            RequirementReviewStatus::NeedsClarification
        );
        let metadata = store.read_metadata("feature").unwrap();
        assert_eq!(
            metadata.requirement_review.status,
            RequirementReviewStatus::NeedsClarification
        );
    }

    #[cfg(unix)]
    #[test]
    fn resume_after_answers_writes_resolved_spec_and_decomposes() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            repo.join("feature.md"),
            "# Feature\n\nDo something vague.\n",
        )
        .unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let start_codex = fake_codex_script(
            temp.path(),
            &requirement_needs_clarification_script("- What exactly should change?"),
        );
        let start = start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("feature.md"),
                spec_paths: Vec::new(),
                run_id: None,
                branch: None,
                resume: false,
                codex_bin: Some(start_codex),
            },
        )
        .unwrap();
        let answers_path = start.answers_path.unwrap();
        fs::write(
            &answers_path,
            "# Answers for feature\n\n## Answers\n\n<!-- codex-task:answers:start -->\nBuild the explicit scheduler path.\n<!-- codex-task:answers:end -->\n",
        )
        .unwrap();

        let resolved_spec_file = ".codex/task-runs/feature/phases/feature/resolved-spec.md";
        let resume_codex = fake_codex_script(
            temp.path(),
            &resolve_then_decompose_script(
                "# Resolved Spec\n\nBuild the explicit scheduler path.\n",
                &sample_decompose_json("feature", "feat/feature", resolved_spec_file),
            ),
        );
        let resumed = resume_run_in_repo(
            &repo,
            &home,
            ResumeOptions {
                run_id: "feature".to_string(),
                codex_bin: Some(resume_codex),
            },
        )
        .unwrap();

        assert!(resumed.tasks_path.exists());
        assert_eq!(resumed.requirement_status, "resolved");
        assert_eq!(resumed.spec_file, resolved_spec_file);
        assert!(resumed.resolved_spec_path.unwrap().exists());
        let store = RunStore::for_repo(&repo, &home).unwrap();
        let task_file = store.read_task_file("feature").unwrap();
        assert_eq!(task_file.spec_file, resolved_spec_file);
        let state = store.read_run_state("feature").unwrap();
        assert_eq!(
            state.requirement_review.status,
            RequirementReviewStatus::Resolved
        );
        let metadata = store.read_metadata("feature").unwrap();
        assert_eq!(
            metadata.resolved_spec_file.as_deref(),
            Some(resolved_spec_file)
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
                group: None,
                phase: None,
                until_phase: None,
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

    #[test]
    fn scheduler_scope_filters_by_roadmap_phase() {
        let mut p1 = sample_task("p1", 1);
        p1.phase = "01-foundation-data-model".to_string();
        p1.spec_files = vec!["docs/roadmap/ai-platform/01-foundation-data-model.md".to_string()];
        let mut p2 = sample_task("p2", 2);
        p2.phase = "02-domain-event-outbox-runtime".to_string();
        p2.spec_files =
            vec!["docs/roadmap/ai-platform/02-domain-event-outbox-runtime.md".to_string()];
        let task_file = sample_run_task_file(
            "run",
            "docs/roadmap/ai-platform/01-foundation-data-model.md",
            vec![p1, p2],
        );
        let state = initial_run_state(&task_file);

        let phase_scope = WatchScope {
            phase: Some("02-domain-event-outbox-runtime".to_string()),
            ..WatchScope::default()
        };
        assert_eq!(
            select_next_runnable_task(&task_file, &state, &phase_scope).unwrap(),
            Some("p2".to_string())
        );

        let until_scope = WatchScope {
            until_phase: Some("01-foundation-data-model".to_string()),
            ..WatchScope::default()
        };
        assert_eq!(
            select_next_runnable_task(&task_file, &state, &until_scope).unwrap(),
            Some("p1".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn start_directory_creates_phase_index_and_decomposes_only_first_phase() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(repo.join("docs/roadmap")).unwrap();
        fs::write(repo.join("docs/roadmap/02-runtime.md"), "# Runtime\n").unwrap();
        fs::write(repo.join("docs/roadmap/01-foundation.md"), "# Foundation\n").unwrap();
        fs::write(repo.join("docs/roadmap/notes.txt"), "ignore\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "specs"]);

        let codex = fake_codex_script(
            temp.path(),
            &requirement_clear_then_last_message_script(&sample_decompose_json(
                "roadmap",
                "feat/roadmap",
                "docs/roadmap/01-foundation.md",
            )),
        );
        start_run_in_repo(
            &repo,
            &home,
            &repo,
            StartOptions {
                spec_path: PathBuf::from("docs/roadmap"),
                spec_paths: Vec::new(),
                run_id: Some("roadmap".to_string()),
                branch: None,
                resume: false,
                codex_bin: Some(codex),
            },
        )
        .unwrap();

        let store = RunStore::for_repo(&repo, &home).unwrap();
        let metadata = store.read_metadata("roadmap").unwrap();
        assert_eq!(
            metadata
                .phases
                .iter()
                .map(|phase| phase.id.as_str())
                .collect::<Vec<_>>(),
            vec!["01-foundation", "02-runtime"]
        );
        assert!(metadata.phases[0].decomposed);
        assert!(!metadata.phases[1].decomposed);
        let task_file = store.read_task_file("roadmap").unwrap();
        assert_eq!(task_file.spec_file, "docs/roadmap/01-foundation.md");
        assert_eq!(task_file.tasks.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn phase_preparation_appends_next_phase_tasks_without_overwriting_done_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(repo.join("docs/roadmap")).unwrap();
        fs::write(repo.join("docs/roadmap/01-foundation.md"), "# Foundation\n").unwrap();
        fs::write(repo.join("docs/roadmap/02-runtime.md"), "# Runtime\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "specs"]);

        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.ensure_repo_dir().unwrap();
        let mut first_task = sample_task("p1", 1);
        first_task.phase = "01-foundation".to_string();
        first_task.spec_files = vec!["docs/roadmap/01-foundation.md".to_string()];
        let task_file =
            sample_run_task_file("roadmap", "docs/roadmap/01-foundation.md", vec![first_task]);
        store.write_task_file("roadmap", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Done;
        state.tasks[0].phase = Some(TaskPhase::Done);
        store.write_run_state("roadmap", &state).unwrap();
        store
            .write_metadata(
                "roadmap",
                &RunMetadata {
                    schema_version: 1,
                    run_id: "roadmap".to_string(),
                    branch: "feat/roadmap".to_string(),
                    spec_file: "docs/roadmap/01-foundation.md".to_string(),
                    spec_files: vec![
                        "docs/roadmap/01-foundation.md".to_string(),
                        "docs/roadmap/02-runtime.md".to_string(),
                    ],
                    problem_framing: ProblemFramingState::default(),
                    resolved_problem_file: None,
                    requirement_review: RequirementReviewState::default(),
                    resolved_spec_file: None,
                    phases: vec![
                        RunPhaseMetadata {
                            id: "01-foundation".to_string(),
                            spec_file: "docs/roadmap/01-foundation.md".to_string(),
                            spec_files: vec!["docs/roadmap/01-foundation.md".to_string()],
                            problem_framing: ProblemFramingState {
                                status: ProblemFramingStatus::Clear,
                                ..ProblemFramingState::default()
                            },
                            resolved_problem_file: None,
                            requirement_review: RequirementReviewState {
                                status: RequirementReviewStatus::Clear,
                                ..RequirementReviewState::default()
                            },
                            resolved_spec_file: None,
                            decomposed: true,
                            extra: Map::new(),
                        },
                        RunPhaseMetadata {
                            id: "02-runtime".to_string(),
                            spec_file: "docs/roadmap/02-runtime.md".to_string(),
                            spec_files: vec!["docs/roadmap/02-runtime.md".to_string()],
                            problem_framing: ProblemFramingState::default(),
                            resolved_problem_file: None,
                            requirement_review: RequirementReviewState::default(),
                            resolved_spec_file: None,
                            decomposed: false,
                            extra: Map::new(),
                        },
                    ],
                    active_phase: None,
                    extra: Map::new(),
                },
            )
            .unwrap();

        let second_phase_json =
            sample_decompose_json("roadmap", "feat/roadmap", "docs/roadmap/02-runtime.md")
                .replace("\"p1\"", "\"p2\"")
                .replace("output/p1.md", "output/p2.md")
                .replace("Do p1", "Do p2")
                .replace("Implement p1.", "Implement p2.");
        let codex = fake_codex_script(
            temp.path(),
            &requirement_clear_then_last_message_script(&second_phase_json),
        );
        let context = load_config(&repo, &home, true).unwrap();
        let outcome = prepare_next_phase_for_watch(
            &context,
            &store,
            "roadmap",
            &task_file,
            &WatchScope::default(),
            Some(codex),
        )
        .unwrap();
        assert_eq!(outcome, Some(PhasePrepareOutcome::Decomposed));
        let task_file = store.read_task_file("roadmap").unwrap();
        assert_eq!(task_file.tasks.len(), 2);
        assert_eq!(task_file.tasks[0].id, "p1");
        assert_eq!(task_file.tasks[1].id, "p2");
        let metadata = store.read_metadata("roadmap").unwrap();
        assert!(metadata.phases[1].decomposed);
    }

    #[cfg(unix)]
    #[test]
    fn skip_phase_marks_phase_skipped_and_ignores_unfinished_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::create_dir_all(repo.join("docs/roadmap")).unwrap();
        fs::write(repo.join("docs/roadmap/01-foundation.md"), "# Foundation\n").unwrap();
        git(&repo, ["add", "."]);
        git(&repo, ["commit", "-m", "spec"]);

        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.ensure_repo_dir().unwrap();
        let mut p1 = sample_task("p1", 1);
        p1.phase = "01-foundation".to_string();
        let mut p2 = sample_task("p2", 2);
        p2.phase = "01-foundation".to_string();
        let task_file =
            sample_run_task_file("roadmap", "docs/roadmap/01-foundation.md", vec![p1, p2]);
        store.write_task_file("roadmap", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[1].status = TaskStatus::Done;
        state.tasks[1].phase = Some(TaskPhase::Done);
        store.write_run_state("roadmap", &state).unwrap();
        store
            .write_metadata(
                "roadmap",
                &RunMetadata {
                    schema_version: 1,
                    run_id: "roadmap".to_string(),
                    branch: "feat/roadmap".to_string(),
                    spec_file: "docs/roadmap/01-foundation.md".to_string(),
                    spec_files: vec!["docs/roadmap/01-foundation.md".to_string()],
                    problem_framing: ProblemFramingState {
                        status: ProblemFramingStatus::NeedsDecision,
                        ..ProblemFramingState::default()
                    },
                    resolved_problem_file: None,
                    requirement_review: RequirementReviewState::default(),
                    resolved_spec_file: None,
                    phases: vec![RunPhaseMetadata {
                        id: "01-foundation".to_string(),
                        spec_file: "docs/roadmap/01-foundation.md".to_string(),
                        spec_files: vec!["docs/roadmap/01-foundation.md".to_string()],
                        problem_framing: ProblemFramingState::default(),
                        resolved_problem_file: None,
                        requirement_review: RequirementReviewState::default(),
                        resolved_spec_file: None,
                        decomposed: false,
                        extra: Map::new(),
                    }],
                    active_phase: Some("01-foundation".to_string()),
                    extra: Map::new(),
                },
            )
            .unwrap();

        let result = skip_phase_in_repo(
            &repo,
            &home,
            SkipPhaseOptions {
                run_id: Some("roadmap".to_string()),
                phase_id: "01-foundation".to_string(),
                reason: Some("not needed".to_string()),
            },
        )
        .unwrap();

        assert_eq!(result.ignored_tasks, 1);
        assert_eq!(result.already_done_tasks, 1);
        let metadata = store.read_metadata("roadmap").unwrap();
        assert!(metadata.phases[0].decomposed);
        assert_eq!(metadata.active_phase, None);
        assert_eq!(
            metadata.phases[0]
                .extra
                .get("skipReason")
                .and_then(Value::as_str),
            Some("not needed")
        );
        let state = store.read_run_state("roadmap").unwrap();
        assert_eq!(
            find_task_state(&state, "p1").unwrap().status,
            TaskStatus::Ignored
        );
        assert_eq!(
            find_task_state(&state, "p2").unwrap().status,
            TaskStatus::Done
        );
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

        let mut p1 = sample_task("p1", 1);
        p1.timeout_seconds = Some(20);
        let task_file = sample_run_task_file("run", "spec.md", vec![p1, sample_task("p2", 2)]);
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
        for _ in 0..150 {
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
                group: None,
                phase: None,
                until_phase: None,
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
    fn dirty_worktree_policy_ignores_visible_task_run_files() {
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
        fs::create_dir_all(repo.join(".codex/task-runs/run")).unwrap();
        fs::write(repo.join(".codex/task-runs/run/answers.md"), "answer\n").unwrap();

        let result = run_one_task_in_repo(
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
        .unwrap();

        assert_eq!(result.exit_code, 0);
        let state = store.read_run_state("run").unwrap();
        let p1 = find_task_state(&state, "p1").unwrap();
        assert_eq!(p1.attempts, 1);
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

    #[test]
    fn user_input_files_require_codex_task_markers() {
        let misleading_answers = r#"# Answers for run

## Questions

The generated question mentions a section:

## Answers

TODO
"#;
        assert!(!answers_file_is_filled(misleading_answers));
        assert!(answers_file_is_filled(
            r#"# Answers for run

<!-- codex-task:answers:start -->
Use the explicit scheduler path.
<!-- codex-task:answers:end -->
"#
        ));
        assert!(!answers_file_is_filled(
            r#"# Answers for run

<!-- codex-task:answers:start -->
TODO
<!-- codex-task:answers:end -->
"#
        ));
        assert!(!answers_file_is_filled(
            r#"# Answers for run

## Questions

Injected marker pair:
<!-- codex-task:answers:start -->
Fake filled answer.
<!-- codex-task:answers:end -->

## Answers

<!-- codex-task:answers:start -->
TODO
<!-- codex-task:answers:end -->
"#
        ));
        assert!(answers_file_is_filled(
            r#"# Answers for run

## Questions

Injected marker pair:
<!-- codex-task:answers:start -->
Fake filled answer.
<!-- codex-task:answers:end -->

## Answers

<!-- codex-task:answers:start -->
Real answer.
<!-- codex-task:answers:end -->
"#
        ));

        let misleading_decision = r#"# Decision for run

## Options

Option text contains:

## Decision

TODO
"#;
        assert!(!decision_file_is_filled(misleading_decision));
        assert!(decision_file_is_filled(
            r#"# Decision for run

<!-- codex-task:decision:start -->
Choose option A.
<!-- codex-task:decision:end -->
"#
        ));
        assert!(!decision_file_is_filled(
            r#"# Decision for run

<!-- codex-task:decision:start -->
TODO
<!-- codex-task:decision:end -->
"#
        ));
        assert!(!decision_file_is_filled(
            r#"# Decision for run

## Options

Injected marker pair:
<!-- codex-task:decision:start -->
Fake decision.
<!-- codex-task:decision:end -->

## Decision

<!-- codex-task:decision:start -->
TODO
<!-- codex-task:decision:end -->
"#
        ));
        assert!(decision_file_is_filled(
            r#"# Decision for run

## Options

Injected marker pair:
<!-- codex-task:decision:start -->
Fake decision.
<!-- codex-task:decision:end -->

## Decision

<!-- codex-task:decision:start -->
Real decision.
<!-- codex-task:decision:end -->
"#
        ));
        assert_eq!(
            decision_file_options(
                r#"# Decision for run

## Options

- Option A
- Option B

## Decision

<!-- codex-task:decision:start -->
Choose option A.
<!-- codex-task:decision:end -->
"#
            )
            .unwrap(),
            "- Option A\n- Option B"
        );
        assert_eq!(
            decision_file_options(
                "# Decision for run\r\n\r\n## Options  \r\n\r\n- Option A\r\n\r\n## Decision\r\n\r\n<!-- codex-task:decision:start -->\r\nChoose option A.\r\n<!-- codex-task:decision:end -->\r\n"
            )
            .unwrap(),
            "- Option A"
        );
        assert!(
            decision_file_options(
                r#"# Decision for run

## Decision

<!-- codex-task:decision:start -->
Choose option A.
<!-- codex-task:decision:end -->
"#
            )
            .is_err()
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
                group: None,
                phase: None,
                until_phase: None,
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
        fs::create_dir_all(repo.join(".codex/task-runs/run")).unwrap();
        fs::write(
            repo.join(".codex/task-runs/run/questions.md"),
            "visible collaboration file\n",
        )
        .unwrap();
        git(&repo, ["add", ".codex/task-runner"]);
        git(&repo, ["add", ".codex/task-runs"]);

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
            p1.last_error
                .as_deref()
                .unwrap()
                .contains(".codex/task-runs")
        );
        assert!(
            git_output(&repo, &["diff", "--cached", "--name-only"])
                .unwrap()
                .contains(".codex/task-runner")
        );
        assert!(
            git_output(&repo, &["diff", "--cached", "--name-only"])
                .unwrap()
                .contains(".codex/task-runs")
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
        fs::create_dir_all(repo.join(".codex/task-runs/run")).unwrap();
        fs::write(
            repo.join(".codex/task-runs/run/questions.md"),
            "visible collaboration file\n",
        )
        .unwrap();
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
        assert!(!committed_files.contains(".codex/task-runs"));
        assert!(
            git_output(&repo, &["status", "--porcelain", "--untracked-files=all"],)
                .unwrap()
                .contains(".codex/task-runner/runs")
        );
        assert!(
            git_output(&repo, &["status", "--porcelain", "--untracked-files=all"],)
                .unwrap()
                .contains(".codex/task-runs")
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

    #[cfg(unix)]
    #[test]
    fn final_review_change_map_ignores_visible_task_run_files() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        fs::create_dir_all(repo.join(".codex/task-runs/run/final-review/round-1")).unwrap();
        fs::write(
            repo.join(".codex/task-runs/run/final-review/round-1/findings.json"),
            "[]\n",
        )
        .unwrap();

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        store
            .write_run_state("run", &initial_run_state(&task_file))
            .unwrap();
        let context = load_config(&repo, &home, true).unwrap();

        let change_map = build_change_map(&context, &store, "run", &task_file).unwrap();
        let paths = change_map
            .files
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>();

        assert!(paths.contains(&"src/lib.rs"));
        assert!(
            !paths
                .iter()
                .any(|path| path.starts_with(".codex/task-runs/"))
        );
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
    fn final_review_must_fix_blocks_at_max_rounds_without_appending_tasks() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[runner]
max_final_review_rounds = 1
"#,
        )
        .unwrap();
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
            &final_review_must_fix_script("Finish integration."),
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
        assert_eq!(state.feature_review_status, FeatureReviewStatus::Blocked);
        assert_eq!(state.feature_review_attempts, 1);
        assert_eq!(state.final_review.remaining_must_fix.len(), 9);
        let round = &state.final_review.rounds[0];
        assert_eq!(round.shards.len(), FINAL_REVIEW_TYPES.len());
        assert!(Path::new(round.change_map_path.as_deref().unwrap()).exists());
        assert!(Path::new(round.review_plan_path.as_deref().unwrap()).exists());
        assert!(Path::new(round.findings_path.as_deref().unwrap()).exists());
        assert_eq!(store.read_task_file("run").unwrap().tasks.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn final_review_invalid_verdict_is_blocked_not_approved() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        fs::write(
            repo.join(".codex/task-runner.toml"),
            r#"
[runner]
max_final_review_rounds = 1
"#,
        )
        .unwrap();
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Done;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        store.write_run_state("run", &state).unwrap();

        let codex = fake_codex_script(temp.path(), &final_review_invalid_shard_script());
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
        assert_eq!(state.feature_review_status, FeatureReviewStatus::Blocked);
        assert_eq!(state.feature_review_attempts, 1);
        assert!(
            state
                .extra
                .get("featureReviewLastError")
                .and_then(Value::as_str)
                .unwrap()
                .contains("remaining MUST_FIX")
        );
    }

    #[test]
    fn final_fix_without_available_verification_is_marked_degraded() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        fs::create_dir_all(repo.join(".codex")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(repo.join(".codex/task-runner.toml"), "").unwrap();
        fs::write(repo.join("spec.md"), "# Spec\n").unwrap();

        let context = load_config(&repo, &home, true).unwrap();
        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        store
            .write_run_state("run", &initial_run_state(&task_file))
            .unwrap();

        let task_id = append_final_fix_task(
            &context,
            &store,
            "run",
            &task_file,
            1,
            &[FinalReviewFinding {
                id: "must-fix".to_string(),
                review_type: "code-defect".to_string(),
                severity: FindingSeverity::MustFix,
                title: "Blocking issue".to_string(),
                detail: "Fix it.".to_string(),
                source: None,
            }],
        )
        .unwrap();

        assert_eq!(task_id, "final-fix-round-1");
        let updated = store.read_task_file("run").unwrap();
        let final_fix = updated
            .tasks
            .iter()
            .find(|task| task.id == "final-fix-round-1")
            .unwrap();
        assert!(final_fix.verification_commands.is_empty());
        assert_eq!(
            final_fix
                .extra
                .get("verificationDegraded")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            final_fix
                .extra
                .get("verificationDegradedReason")
                .and_then(Value::as_str)
                .unwrap()
                .contains("no global")
        );
        let state = store.read_run_state("run").unwrap();
        let final_fix_state = find_task_state(&state, "final-fix-round-1").unwrap();
        assert_eq!(
            final_fix_state
                .extra
                .get("verificationDegraded")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(
            final_fix_state
                .extra
                .get("verificationDegradedReason")
                .and_then(Value::as_str)
                .unwrap()
                .contains("no global")
        );
        let summary = final_review_verification_summary(&store, "run", &updated).unwrap();
        assert!(summary.contains("final-fix-round-1"));
        assert!(summary.contains("verificationDegraded=true"));
    }

    #[cfg(unix)]
    #[test]
    fn final_review_must_fix_runs_final_fix_then_approves_next_round() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let mut task = sample_task("p1", 1);
        task.verification_commands = vec![verification_command(
            "final fix marker",
            "test -f final-fix.marker",
            true,
            Some(5),
        )];
        let task_file = sample_run_task_file("run", "spec.md", vec![task]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        let mut state = initial_run_state(&task_file);
        state.tasks[0].status = TaskStatus::Done;
        state.tasks[0].phase = Some(TaskPhase::Commit);
        store.write_run_state("run", &state).unwrap();

        let codex = fake_codex_script(temp.path(), &final_review_must_fix_then_approved_script());
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

        assert_eq!(result.exit_code, 0);
        assert!(repo.join("final-fix.marker").exists());
        let state = store.read_run_state("run").unwrap();
        assert_eq!(state.feature_review_status, FeatureReviewStatus::Approved);
        assert_eq!(state.feature_review_attempts, 2);
        assert_eq!(state.final_review.rounds.len(), 2);
        assert_eq!(
            state.final_review.rounds[0].final_fix_task_id.as_deref(),
            Some("final-fix-round-1")
        );
        assert_eq!(
            state.final_review.rounds[1].status,
            FeatureReviewStatus::Approved
        );
        let final_fix_state = find_task_state(&state, "final-fix-round-1").unwrap();
        assert_eq!(final_fix_state.status, TaskStatus::Done);
        assert_eq!(
            final_fix_state
                .extra
                .get("verificationDegraded")
                .and_then(Value::as_bool),
            None
        );
        assert!(
            final_fix_state
                .extra
                .get("verificationLogs")
                .and_then(Value::as_array)
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .any(|path| path.contains("final-fix-marker"))
        );

        let updated_tasks = store.read_task_file("run").unwrap();
        let final_fix_task = updated_tasks
            .tasks
            .iter()
            .find(|task| task.id == "final-fix-round-1")
            .unwrap();
        assert_eq!(final_fix_task.verification_commands.len(), 1);
        assert_eq!(
            final_fix_task.verification_commands[0].name,
            "final fix marker"
        );
    }

    #[cfg(unix)]
    #[test]
    fn final_review_round2_prompt_includes_degraded_final_fix_verification() {
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
            &final_review_must_fix_then_approved_with_prompt_capture_script(),
        );
        let codex_dir = codex.parent().unwrap().to_path_buf();
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

        assert_eq!(result.exit_code, 0);
        let aggregate_prompt =
            fs::read_to_string(codex_dir.join("round2-aggregate-prompt.log")).unwrap();
        assert!(aggregate_prompt.contains("final-fix-round-1"));
        assert!(aggregate_prompt.contains("verificationDegraded=true"));
        assert!(aggregate_prompt.contains("no global"));
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

        let codex = fake_codex_script(temp.path(), &final_review_all_approved_script());
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
        assert_eq!(archived_state.final_review.rounds.len(), 1);
        assert_eq!(
            archived_state.final_review.rounds[0].shards.len(),
            FINAL_REVIEW_TYPES.len()
        );

        let spec_text = fs::read_to_string(repo.join("spec.md")).unwrap();
        assert!(spec_text.contains("status: done"));
        assert!(spec_text.contains("finished_at: 20"));
    }

    #[cfg(unix)]
    #[test]
    fn inspect_and_logs_can_read_archived_run_artifacts() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let home = temp.path().join("home");
        init_test_repo(&repo);
        write_spec_and_commit(&repo, "spec.md");

        let task_file = sample_run_task_file("run", "spec.md", vec![sample_task("p1", 1)]);
        let store = RunStore::for_repo(&repo, &home).unwrap();
        store.write_task_file("run", &task_file).unwrap();
        store
            .write_run_state("run", &initial_run_state(&task_file))
            .unwrap();
        let run_dir = store.run_dir("run").unwrap();
        fs::create_dir_all(run_dir.join("logs")).unwrap();
        fs::write(
            run_dir.join("logs/p1.implement.stderr.log"),
            "line one\nline two\n",
        )
        .unwrap();

        let archive_dir = store.repo_runs_dir.join("archive/run-2026-06-18T00-00-00Z");
        fs::create_dir_all(archive_dir.parent().unwrap()).unwrap();
        fs::rename(&run_dir, &archive_dir).unwrap();

        let inspect = inspect_run_in_repo(
            &repo,
            &home,
            InspectOptions {
                run_id: Some("run".to_string()),
            },
        )
        .unwrap();
        let selected = inspect.selected.unwrap();
        assert_eq!(selected.location, "archive");
        assert_eq!(
            selected.archive_name,
            Some("run-2026-06-18T00-00-00Z".to_string())
        );

        let logs = read_run_logs_in_repo(
            &repo,
            &home,
            LogsOptions {
                run_id: Some("run".to_string()),
                task_id: Some("p1".to_string()),
                phase: Some("implement".to_string()),
                latest: true,
                tail_lines: Some(1),
            },
        )
        .unwrap();
        assert_eq!(logs.location, "archive");
        assert_eq!(logs.files.len(), 1);
        assert_eq!(logs.tails[0].text, "line two\n");
    }

    #[cfg(unix)]
    #[test]
    fn reset_blocked_task_to_runnable_phase_with_attempt_warning() {
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
        state.tasks[0].status = TaskStatus::Blocked;
        state.tasks[0].phase = Some(TaskPhase::Implement);
        state.tasks[0].attempts = 1;
        state.tasks[0].last_error = Some("maxAttempts 1 reached".to_string());
        store.write_run_state("run", &state).unwrap();

        let result = reset_task_in_repo(
            &repo,
            &home,
            ResetTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                phase: TaskPhase::Implement,
                clear_attempts: false,
                clear_review_attempts: false,
            },
        )
        .unwrap();
        assert_eq!(result.phase, "implement");
        assert_eq!(result.attempts, 1);
        assert_eq!(result.warnings.len(), 1);

        let state = store.read_run_state("run").unwrap();
        assert_eq!(state.tasks[0].status, TaskStatus::Pending);
        assert_eq!(state.tasks[0].phase, Some(TaskPhase::Implement));
        assert_eq!(state.tasks[0].attempts, 1);
        assert_eq!(state.tasks[0].last_error, None);

        let result = reset_task_in_repo(
            &repo,
            &home,
            ResetTaskOptions {
                run_id: Some("run".to_string()),
                task_id: "p1".to_string(),
                phase: TaskPhase::Implement,
                clear_attempts: true,
                clear_review_attempts: false,
            },
        )
        .unwrap();
        assert_eq!(result.attempts, 0);
        assert!(result.warnings.is_empty());
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

    fn final_review_all_approved_script() -> String {
        final_review_script(
            r#"{"verdict":"APPROVED","findings":[]}"#,
            "APPROVED",
            "Feature is complete.",
        )
    }

    fn final_review_must_fix_script(detail: &str) -> String {
        final_review_script(
            &format!(
                r#"{{
  "verdict": "CHANGES_REQUESTED",
  "findings": [
    {{
      "id": "must-fix",
      "severity": "MUST_FIX",
      "title": "Blocking issue",
      "detail": "{detail}"
    }}
  ]
}}"#
            ),
            "CHANGES_REQUESTED",
            "Blocking issues remain.",
        )
    }

    fn final_review_invalid_shard_script() -> String {
        final_review_script(
            r#"{"verdict":"PASS","findings":[]}"#,
            "APPROVED",
            "Invalid shard.",
        )
    }

    fn final_review_must_fix_then_approved_script() -> String {
        r#"
last=""
repo=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  elif [ "$1" = "-C" ]; then
    shift
    repo="$1"
  fi
  shift || break
done
if grep -q '^Findings output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Findings output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  if printf '%s\n' "$out" | grep -q '/round-1/'; then
    cat > "$out" <<'CODEX_SHARD'
{
  "verdict": "CHANGES_REQUESTED",
  "findings": [
    {
      "id": "must-fix",
      "severity": "MUST_FIX",
      "title": "Blocking issue",
      "detail": "Write the final-fix marker."
    }
  ]
}
CODEX_SHARD
  else
    printf '{"verdict":"APPROVED","findings":[]}\n' > "$out"
  fi
  printf 'final review shard complete\n' > "$last"
  exit 0
fi
if grep -q '^Aggregate review output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Aggregate review output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  if printf '%s\n' "$out" | grep -q '/round-1/'; then
    verdict="CHANGES_REQUESTED"
    body="Blocking issues remain."
  else
    verdict="APPROVED"
    body="Final fix resolved the blocking issue."
  fi
  cat > "$out" <<CODEX_REVIEW
---
verdict: $verdict
reviewed_at: 2026-06-16T00:00:00Z
---

$body
CODEX_REVIEW
  printf 'final review aggregate complete\n' > "$last"
  exit 0
fi
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
printf 'fixed\n' > "$repo/final-fix.marker"
printf 'implementation summary\n' > "$last"
"#
        .to_string()
    }

    fn final_review_must_fix_then_approved_with_prompt_capture_script() -> String {
        r#"
last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q '^Findings output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Findings output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  if printf '%s\n' "$out" | grep -q '/round-1/'; then
    cat > "$out" <<'CODEX_SHARD'
{
  "verdict": "CHANGES_REQUESTED",
  "findings": [
    {
      "id": "must-fix",
      "severity": "MUST_FIX",
      "title": "Blocking issue",
      "detail": "Fix the final review issue."
    }
  ]
}
CODEX_SHARD
  else
    printf '{"verdict":"APPROVED","findings":[]}\n' > "$out"
  fi
  printf 'final review shard complete\n' > "$last"
  exit 0
fi
if grep -q '^Aggregate review output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Aggregate review output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  if printf '%s\n' "$out" | grep -q '/round-2/'; then
    cp "$script_dir/stdin.log" "$script_dir/round2-aggregate-prompt.log"
    verdict="APPROVED"
    body="Final fix resolved the blocking issue."
  else
    verdict="CHANGES_REQUESTED"
    body="Blocking issues remain."
  fi
  cat > "$out" <<CODEX_REVIEW
---
verdict: $verdict
reviewed_at: 2026-06-16T00:00:00Z
---

$body
CODEX_REVIEW
  printf 'final review aggregate complete\n' > "$last"
  exit 0
fi
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

    fn final_review_script(
        shard_json: &str,
        aggregate_verdict: &str,
        aggregate_body: &str,
    ) -> String {
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
if grep -q '^Findings output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Findings output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_SHARD'
{shard_json}
CODEX_SHARD
  printf 'final review shard complete\n' > "$last"
  exit 0
fi
if grep -q '^Aggregate review output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Aggregate review output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_REVIEW'
---
verdict: {aggregate_verdict}
reviewed_at: 2026-06-16T00:00:00Z
---

{aggregate_body}
CODEX_REVIEW
  printf 'final review aggregate complete\n' > "$last"
  exit 0
fi
printf 'unexpected prompt\n' >&2
exit 9
"#
        )
    }

    fn sample_task(id: &str, priority: u64) -> Task {
        Task {
            id: id.to_string(),
            priority,
            group: "scheduler".to_string(),
            phase: "scheduler".to_string(),
            title: format!("Task {id}"),
            max_attempts: Some(3),
            timeout_seconds: Some(20),
            output: format!("output/{id}.md"),
            prompt: format!("Implement {id}."),
            spec_file: None,
            spec_files: Vec::new(),
            depends_on: Vec::new(),
            review_criteria: Vec::new(),
            analyze_timeout_seconds: Some(20),
            analyze_required: true,
            require_review_approval: false,
            max_review_attempts: 2,
            review_timeout_seconds: Some(20),
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
            spec_files: vec![spec_file.to_string()],
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

    fn sample_problem_framing_input() -> ProblemFramingPromptInput {
        ProblemFramingPromptInput {
            common: sample_common_variables(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            output_review_path: "output/problem-framing.md".to_string(),
        }
    }

    fn sample_resolve_problem_input() -> ResolveProblemPromptInput {
        ResolveProblemPromptInput {
            common: sample_common_variables(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            options: "Options".to_string(),
            decision: "User decision".to_string(),
            output_resolved_problem_path: ".codex/task-runs/run-1/resolved-problem.md".to_string(),
        }
    }

    fn sample_requirement_review_input() -> RequirementReviewPromptInput {
        RequirementReviewPromptInput {
            common: sample_common_variables(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            output_review_path: "output/requirement-review.md".to_string(),
        }
    }

    fn sample_resolve_requirement_input() -> ResolveRequirementPromptInput {
        ResolveRequirementPromptInput {
            common: sample_common_variables(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            feature_spec: "Feature body".to_string(),
            questions: "Question?".to_string(),
            answers: "User answers".to_string(),
            output_resolved_spec_path: ".codex/task-runs/run-1/resolved-spec.md".to_string(),
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

    fn sample_final_review_shard_input() -> FinalReviewShardPromptInput {
        FinalReviewShardPromptInput {
            common: sample_common_variables(),
            run_id: "run-1".to_string(),
            branch: "feat/run-1".to_string(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            resolved_spec: "Resolved spec".to_string(),
            review_type: "security".to_string(),
            change_map: "{}".to_string(),
            relevant_diff: "diff --git a/src/lib.rs b/src/lib.rs".to_string(),
            relevant_logs: "logs".to_string(),
            relevant_files: "files".to_string(),
            output_findings_path: "output/final-review/round-1/security.findings.json".to_string(),
        }
    }

    fn sample_final_review_aggregate_input() -> FinalReviewAggregatePromptInput {
        FinalReviewAggregatePromptInput {
            common: sample_common_variables(),
            run_id: "run-1".to_string(),
            branch: "feat/run-1".to_string(),
            spec_file: "docs/roadmap/feature.md".to_string(),
            resolved_spec: "Resolved spec".to_string(),
            change_map: "{}".to_string(),
            shard_findings: "Shard findings".to_string(),
            public_api_summary: "API summary".to_string(),
            db_summary: "DB summary".to_string(),
            docs_summary: "Docs summary".to_string(),
            verification_summary: "Verification summary".to_string(),
            output_review_path: "output/final-review/round-1/aggregate-review.md".to_string(),
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
            fallback_required_output_from_last_message: false,
            sandbox: "workspace-write".to_string(),
            approval: "never".to_string(),
            model: None,
            reasoning_effort: None,
            search: None,
            timeout_seconds: 20,
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
repo_cwd=$(sed -n '/^-C$/{{n;p;q;}}' "$script_dir/args.log")
if [ -n "$repo_cwd" ]; then
  cd "$repo_cwd"
fi
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
    fn requirement_clear_then_last_message_script(message: &str) -> String {
        format!(
            r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q '^Problem framing output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Problem framing output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_PROBLEM'
---
verdict: CLEAR
reviewed_at: 2026-06-16T00:00:00Z
---

Clear.
CODEX_PROBLEM
  printf 'problem framing clear\n' > "$last"
  exit 0
fi
if grep -q '^Requirement review output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Requirement review output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_REQUIREMENT'
---
verdict: CLEAR
reviewed_at: 2026-06-16T00:00:00Z
---

Clear.
CODEX_REQUIREMENT
  printf 'requirement clear\n' > "$last"
  exit 0
fi
cat > "$last" <<'CODEX_LAST_MESSAGE'
{message}
CODEX_LAST_MESSAGE
"#
        )
    }

    #[cfg(unix)]
    fn requirement_clear_then_fail_script() -> String {
        r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q '^Problem framing output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Problem framing output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_PROBLEM'
---
verdict: CLEAR
reviewed_at: 2026-06-16T00:00:00Z
---

Clear.
CODEX_PROBLEM
  printf 'problem framing clear\n' > "$last"
  exit 0
fi
if grep -q '^Requirement review output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Requirement review output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_REQUIREMENT'
---
verdict: CLEAR
reviewed_at: 2026-06-16T00:00:00Z
---

Clear.
CODEX_REQUIREMENT
  printf 'requirement clear\n' > "$last"
  exit 0
fi
printf 'codex failed\n' >&2
exit 7
"#
        .to_string()
    }

    #[cfg(unix)]
    fn requirement_needs_clarification_script(questions: &str) -> String {
        format!(
            r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q '^Problem framing output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Problem framing output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_PROBLEM'
---
verdict: CLEAR
reviewed_at: 2026-06-16T00:00:00Z
---

Clear.
CODEX_PROBLEM
  printf 'problem framing clear\n' > "$last"
  exit 0
fi
out=$(sed -n 's/^Requirement review output path: //p' "$script_dir/stdin.log" | head -n 1)
mkdir -p "$(dirname "$out")"
cat > "$out" <<'CODEX_REQUIREMENT'
---
verdict: NEEDS_CLARIFICATION
reviewed_at: 2026-06-16T00:00:00Z
---

{questions}
CODEX_REQUIREMENT
printf 'requirement needs clarification\n' > "$last"
"#
        )
    }

    #[cfg(unix)]
    fn problem_needs_decision_script(options: &str) -> String {
        format!(
            r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
out=$(sed -n 's/^Problem framing output path: //p' "$script_dir/stdin.log" | head -n 1)
mkdir -p "$(dirname "$out")"
cat > "$out" <<'CODEX_PROBLEM'
---
verdict: NEEDS_DECISION
reviewed_at: 2026-06-16T00:00:00Z
---

{options}
CODEX_PROBLEM
printf 'problem framing needs decision\n' > "$last"
"#
        )
    }

    #[cfg(unix)]
    fn resolve_problem_then_requirement_clear_then_decompose_script(
        resolved_problem: &str,
        decompose_json: &str,
    ) -> String {
        format!(
            r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q '^Resolved problem output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Resolved problem output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_RESOLVED_PROBLEM'
{resolved_problem}
CODEX_RESOLVED_PROBLEM
  printf 'resolved problem\n' > "$last"
  exit 0
fi
if grep -q '^Requirement review output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Requirement review output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_REQUIREMENT'
---
verdict: CLEAR
reviewed_at: 2026-06-16T00:00:00Z
---

Clear.
CODEX_REQUIREMENT
  printf 'requirement clear\n' > "$last"
  exit 0
fi
cat > "$last" <<'CODEX_DECOMPOSE'
{decompose_json}
CODEX_DECOMPOSE
"#
        )
    }

    #[cfg(unix)]
    fn resolve_then_decompose_script(resolved_spec: &str, decompose_json: &str) -> String {
        format!(
            r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
if grep -q '^Resolved spec output path:' "$script_dir/stdin.log"; then
  out=$(sed -n 's/^Resolved spec output path: //p' "$script_dir/stdin.log" | head -n 1)
  mkdir -p "$(dirname "$out")"
  cat > "$out" <<'CODEX_RESOLVED'
{resolved_spec}
CODEX_RESOLVED
  printf 'resolved spec\n' > "$last"
  exit 0
fi
cat > "$last" <<'CODEX_DECOMPOSE'
{decompose_json}
CODEX_DECOMPOSE
"#
        )
    }

    #[cfg(unix)]
    fn decompose_success_script(decompose_json: &str) -> String {
        format!(
            r#"last=""
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--output-last-message" ]; then
    shift
    last="$1"
  fi
  shift || break
done
cat > "$last" <<'CODEX_DECOMPOSE'
{decompose_json}
CODEX_DECOMPOSE
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
