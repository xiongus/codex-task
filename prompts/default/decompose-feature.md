# Codex Task Decomposer

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
- Every task object must contain these fields: `id`, `priority`, `group`, `title`, `output`, `prompt`, `dependsOn`, `reviewCriteria`, and `verificationCommands`.
- Use `prompt` for the executable task instructions. Do not emit `description` instead of `prompt`.
- Use a stable Markdown path such as `output/<task-id>.md` for each task `output`.
- Each task must be narrowly scoped, actionable, and include explicit negative constraints from the spec.
- Each task's `reviewCriteria`, `dependsOn`, and `verificationCommands` fields must be JSON arrays. Do not emit a single string for array fields.
- Preserve existing project boundaries. Do not invent unrelated APIs, tables, or features.
