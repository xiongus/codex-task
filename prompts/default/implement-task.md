# Autonomous Task Runner

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
