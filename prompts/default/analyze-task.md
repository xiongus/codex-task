# Codex Task Analyzer

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

Return the complete Markdown analysis report as your final message with no code fences. Do not write files; the runner will persist the final message to `{output_analysis_path}`. The report must cover current state, gaps, implementation plan, risks, and acceptance criteria.
