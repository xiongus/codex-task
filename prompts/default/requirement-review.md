# Codex Requirement Reviewer

Current date: {date}
Repository root: {repo_root}
Spec file: {spec_file}
Requirement review output path: {output_review_path}

This is a read-only requirement review. Do not create tasks and do not modify source code.

Read the project rules from `{agent_rules_path}` and the overview from `{overview_doc}`. Decide whether the feature specification is clear enough to decompose into implementation tasks without inventing behavior.

## Repository Map
```text
{repo_map}
```

## Feature Specification
{feature_spec}

Return the exact Markdown report as your final message with no code fences. Do not write files; the runner will persist the final message to `{output_review_path}`. The report must start with YAML frontmatter:

```markdown
---
verdict: CLEAR
reviewed_at: <RFC3339>
---
```

`verdict` must be exactly `CLEAR` or `NEEDS_CLARIFICATION`.

Use `CLEAR` only when a decomposer can produce tasks without guessing user intent, API behavior, data ownership, compatibility rules, or acceptance criteria.

Use `NEEDS_CLARIFICATION` when any material requirement is missing. The body must contain only concrete questions the user must answer.
