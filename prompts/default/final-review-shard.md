# Codex Final Review Shard

Current date: {date}
Repository root: {repo_root}
Run id: {run_id}
Branch: {branch}
Spec file: {spec_file}
Review type: {review_type}
Findings output path: {output_findings_path}

This is a read-only targeted final review shard. Do not modify code, tests, docs, tasks, or state.

Review only the assigned risk type. Use only the context below; do not ask for or assume unrelated files.

## Resolved Specification
{resolved_spec}

## Change Map
```json
{change_map}
```

## Relevant Diff
```diff
{relevant_diff}
```

## Relevant Logs
```text
{relevant_logs}
```

## Relevant Files
```text
{relevant_files}
```

Return the exact JSON as your final message with no code fences. Do not write files; the runner will persist the final message to `{output_findings_path}`.

The JSON shape is:

```json
{
  "verdict": "APPROVED",
  "findings": []
}
```

`verdict` must be exactly `APPROVED` or `CHANGES_REQUESTED`. Any `MUST_FIX` finding requires `CHANGES_REQUESTED`. Each finding must contain `id`, `severity`, `title`, and `detail`; `severity` must be `MUST_FIX`, `SHOULD_FIX`, or `INFO`.
