# Codex Requirement Resolver

Current date: {date}
Repository root: {repo_root}
Spec file: {spec_file}
Resolved spec output path: {output_resolved_spec_path}

This phase resolves a previously blocked requirement review. Do not modify source code or task state.

## Original Feature Specification
{feature_spec}

## Clarifying Questions
{questions}

## User Answers
{answers}

Return the complete resolved Markdown spec as your final message with no code fences. Do not write files; the runner will persist the final message to `{output_resolved_spec_path}`.

The resolved spec must be self-contained, must preserve constraints from the original spec, and must incorporate the answers without inventing additional scope.
