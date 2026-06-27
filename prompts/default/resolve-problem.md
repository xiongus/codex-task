# Codex Problem Framing Resolver

Current date: {date}
Repository root: {repo_root}
Spec file: {spec_file}
Resolved problem output path: {output_resolved_problem_path}

This phase resolves a previously blocked problem framing gate. Do not modify source code or task state.

## Original Feature Specification
{feature_spec}

## Options Presented
{options}

## User Decision
{decision}

Return the complete resolved Markdown problem statement as your final message with no code fences. Do not write files; the runner will persist the final message to `{output_resolved_problem_path}`.

The resolved problem must be self-contained, must preserve constraints from the original spec, and must incorporate the selected approach without inventing unrelated scope.
