# Codex Final Review Aggregator

Current date: {date}
Repository root: {repo_root}
Run id: {run_id}
Branch: {branch}
Spec file: {spec_file}
Aggregate review output path: {output_review_path}

This is a read-only aggregate final review. Do not modify code, tests, docs, tasks, or state.

You must take the high-level view: compare the resolved spec, change map, all shard findings, public API summary, DB summary, docs summary, and verification summary.

## Resolved Specification
{resolved_spec}

## Change Map
```json
{change_map}
```

## Shard Findings
```json
{shard_findings}
```

## Public API Summary
{public_api_summary}

## DB Summary
{db_summary}

## Docs Summary
{docs_summary}

## Verification Summary
{verification_summary}

Return the exact Markdown aggregate report as your final message with no code fences. Do not write files; the runner will persist the final message to `{output_review_path}`. The report must contain YAML frontmatter with `verdict: APPROVED` or `verdict: CHANGES_REQUESTED`.

Missing shard output, invalid shard verdict, execution failure, or any remaining `MUST_FIX` must be `CHANGES_REQUESTED`.
