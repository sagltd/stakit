---
name: feature
description: >
  Implement a feature end-to-end to a verified, reviewed, "actually done" state. Coding anything.
metadata:
  author: samuelgja
  version: '1.1.0'
---

# Feature

Drive a feature from a goal (the user's `/feature …` text and/or a referenced
spec/plan) to a state that is **actually done**: implemented in tested slices,
all edge cases covered, and signed off by adversarial review agents.

The goal is already in the prompt — e.g. `/feature add composite upsert` or
`/feature implement the agent spec`. Do not re-ask what to build unless it is
genuinely ambiguous; clarify only blocking unknowns.

This is the **stakit** repo: a Rust **cargo workspace** (edition 2024), members in
`crates/*`. Tests run with **`cargo-nextest`**; the full gate is **`./code-check.sh`**
(`cargo fmt --check` + `cargo clippy -D warnings` + `cargo build` + `cargo nextest run`
+ doctests). `unsafe` is forbidden workspace-wide; public items need docs.

## Hard rules (non-negotiable)

1. **Load the best-practices skill FIRST**, before writing any code (step 1 below).
2. **Re-read the spec/plan before EVERY slice** if the feature has one — never drift
   from the plan.
3. **Every slice is tested**, including edge cases and error paths, and **verified
   green** (`./code-check.sh`, or the per-slice `cargo nextest` + `cargo clippy`)
   before moving to the next slice. Don't defer tests.
4. The feature is **NOT done** until all review agents report clean AND the full gate
   passes. "I think it's done" is not done.
5. **Do not run `git` commands** unless the user explicitly asks. Leave changes
   uncommitted; the user commits. (No branches, no worktrees, no commits.)

Create a TodoWrite/TaskCreate item for each checklist step and work them in order.

## Checklist

1. Load the matching best-practices skill(s) for the work.
2. Read the spec/plan (or write a short one with testable slices).
3. Implement slice-by-slice — re-read the plan, build, test (edge cases), verify.
4. Finish check — full gate green + it actually runs.
5. Spawn the review agents (security, performance, code-review).
6. Master fix loop — fix returned findings, re-verify, repeat until all clean.

---

## Step 1 — Load the best-practices skill (always first)

This repo is Rust-only. Invoke the matching skill(s) with the Skill tool **before
any implementation**:

| Work | Skill(s) to load |
|---|---|
| Any Rust in `crates/*` | `rust-best-practices` |
| Async / Tokio code | `+ rust-async-patterns` |
| Correctness / over-engineering / comments | `+ code-quality` |
| Anything (always) | `any-code` (discovery/reuse-first), and read `CLAUDE.md` |

State which you loaded and why. These skills define the quality bar for both the
implementation and the review agents.

## Step 2 — Spec / plan

- If the feature references a spec or plan (e.g. under `docs/superpowers/specs/` or
  `docs/superpowers/plans/`), **read it fully now**.
- If the feature is non-trivial and has no plan, write a brief plan: a short list of
  **independently testable slices** in dependency order, with the success/verify
  criterion for each. Keep it in the spec/plan dir.
- Gate per slice: `cargo nextest run -p <crate>` + `cargo clippy -p <crate> --all-targets --all-features -- -D warnings` + `cargo fmt -p <crate> -- --check`. Full gate: **`./code-check.sh`**.

## Step 3 — Implement in tested slices

For **each** slice, in order:

1. **Re-read the spec/plan** — confirm this slice and that you're still on plan.
2. Implement it, following the loaded best-practices skill + repo conventions
   (discovery-first, guard clauses, explicit names, focused modules — see `CLAUDE.md`).
3. Write tests for the slice, **including edge cases and error paths** (prefer
   test-first; the `test-driven-development` skill applies). Unit tests in
   `#[cfg(test)] mod tests`; integration tests in the crate's `tests/`.
4. **Verify green**: run the slice's gate (fmt + clippy + nextest). A failure here is
   a bug to fix now (`systematic-debugging` / `bug-fix`), not later.
5. Only then move to the next slice.

Never batch-implement multiple slices without verifying each.

## Step 4 — Finish check (you think it's done)

Before review, confirm yourself:
- All slices implemented; every slice's tests pass, edge cases included.
- **Full gate green**: `./code-check.sh`.
- It **actually works**: build + run the relevant example/binary as applicable (don't
  claim done off a clippy pass alone — `verification-before-completion`).

## Step 5 — Spawn the review agents

Only once you believe it's done + tested. Because this repo is **no-git**, the review
agents are **read-only auditors** — they do NOT edit files concurrently (no worktrees).
Spawn **three agents in one message** with `run_in_background: true` so they run
concurrently; each reads the changed code and returns findings.

1. **security/safety** — unsafe input handling, panics/`unwrap`/`expect` on
   untrusted paths, secret/data leaks, auth gaps, injection, integer/overflow issues.
   Loads `rust-best-practices` + a `security-review` lens.
2. **performance** — hot paths, needless allocations/clones, blocking work in async,
   N+1 queries, unbounded growth. Loads `rust-best-practices` + `rust-async-patterns`.
3. **code-review** — correctness, edge cases, maintainability, API ergonomics, and
   adherence to the loaded best-practices skills + `CLAUDE.md`.

Brief each agent to:
- **Load and follow** the relevant best-practices skill(s).
- Be **read-only**: do not edit files (no-git repo, concurrent edits would clobber).
- For every issue: give `file:line`, severity, the concrete problem, and a **failing
  test or repro** that would prove it, plus the suggested fix.
- Return a structured report: `{ findings: [{file, line, severity, problem, repro, fix}], assessment }`.

## Step 6 — Master fix loop (until clean)

1. Collect the agents' reports.
2. For each finding, **master (you) fixes it sequentially**: write the failing test
   the agent described, implement the fix, verify the test goes green + `./code-check.sh`
   stays green. Use `bug-fix` / `systematic-debugging` as needed.
3. **Re-run** the relevant agent(s) on the changed code to confirm the finding is
   resolved and nothing regressed.
4. Repeat 2–3 until **every agent reports clean** and the full gate is green with all
   tests (including every test added for findings).

The feature is **done only when**: all slices implemented, all edge cases tested,
full gate green, and all review agents report clean. Until then, it is not done —
keep iterating.

## Reminders

- Re-read the spec/plan before each slice — every time.
- Agents are adversarial: they hunt bugs and prove them with a failing test/repro. A
  clean report with zero findings is suspicious on a non-trivial feature — push for a
  real audit.
- Don't widen scope beyond the feature; fix what the agents surface, not unrelated code.
- No `git` commands unless the user asks.
