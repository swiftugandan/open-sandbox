# Engineering Discipline

This document is loaded into every Claude session in this project via `@import` from `CLAUDE.md`. It is the authoritative reference for how engineering work is done here. It is decoupled from any specific project plan so it remains stable as the project evolves.

## TDD cycle (non-negotiable order)

Every unit of work follows this sequence:

1. **Red** — write a failing test that encodes the desired behavior. Commit before any production code exists. The failing test is the specification.
2. **Green** — write the minimum production code that makes the test pass. Resist adding functionality not demanded by a test.
3. **Refactor** — improve structure without changing behavior. Run the smells checklist below. Tests stay green throughout.
4. **E2E against mocks** — exercise the unit against mocked implementations of its contract peers. This catches contract violations before live integration.
5. **Live E2E** — exercise the unit against the real implementations of its contract peers. This is the final acceptance gate for the unit.

No production code is written before its failing test exists in the working tree. This is the discipline that makes the other invariants possible — without it, "testable" becomes aspirational rather than mechanical.

## Forbidden code smells

Implementations containing any of these are rejected. The list is not exhaustive but covers the failure modes that recur most:

**Structure:**
- God objects and god modules (one type or file doing too much)
- Deep nesting beyond two levels (extract a function or invert the condition)
- Functions longer than what fits on a screen without scrolling
- Long parameter lists (group into a struct, or the function is doing too much)
- Duplicated logic (DRY, but not prematurely — three occurrences is the trigger)
- Dead code and commented-out code (delete it; git remembers)

**Types:**
- Primitive obsession (`UserId(Uuid)` not bare `Uuid`; `Email(String)` not bare `String`)
- Stringly-typed APIs where an enum would do
- Boolean parameters that should be enums for readability at call sites
- Leaky abstractions across the contract boundary (internal types in public APIs)

**Error handling:**
- `unwrap()` and `expect()` in non-test paths
- Panics in library code (return a `Result` instead)
- Ignored errors (`let _ = ...` on a fallible call without a comment justifying it)
- `unsafe` without a documented invariant explaining why it is sound

**Naming and clarity:**
- Magic numbers and magic strings (named constants instead)
- Function names that lie about what the function does
- Abbreviations that save typing but cost reading time
- Comments explaining what the code does (the code should do that); comments explaining *why* are welcome

**Process:**
- Suppressed lints without a comment explaining the suppression
- `TODO` and `FIXME` left in committed code without a tracking issue or ticket reference
- Premature abstraction (extract when the duplication appears, not before)

When in doubt, optimize for the next reader. The next reader is often you in three months with no context loaded.

## Source control discipline

**Trunk-based development.** `main` is always releasable. Short-lived feature branches off `main`, merged back via PR or self-review. Long-lived branches accumulate merge debt and should not exist.

**Branch naming:**
- `module/<name>` — implementation of a binary or module from `PLAN.md`
- `contracts/amendment-<short-desc>` — changes to frozen contracts
- `fix/<short-desc>` — bug fixes
- `spike/<short-desc>` — explicitly discardable exploratory work

**Conventional commits** for the subject line: `feat:`, `fix:`, `refactor:`, `test:`, `docs:`, `chore:`. One logical change per commit. Every commit compiles and passes the test suite for the module it touches. No `WIP`, no `fix typo` chains — squash before merge.

**Commit trailers** for implementation work:
```
feat(auth): implement JWT validation

Module: auth
Contract: auth-token@0.1.0
Phase: green
```

**No force-push on shared branches.** `main` and any branch other people are reading from are append-only. Force-push is fine on your own unmerged feature branch.

**PR (or self-review) discipline.** Every merge to `main` references the contract(s) it satisfies. If a change touches the contracts crate, that goes in a separate PR ahead of the consuming changes.

## Git as the status tracker

The state of the project is queryable from git, not from an external tracker:

- **Phase completion:** `git tag --list` shows which phase artifacts are tagged (`spec/`, `sad/`, `contracts/`, `plan/`).
- **Contract freeze:** `git tag --list "contracts/*-frozen"` shows what is locked in.
- **Per-module progress:** `git tag --list "module/<name>/*"` shows where a module is in its TDD cycle (`red`, `green`, `refactored`, `e2e-mock`, `live-verified`, `done`).
- **Module history:** `git log --grep="Module: <name>"` shows everything done for one module across all branches.
- **What's live:** `git tag --list "module/*/live-verified"` shows which modules are verified against real peers.

This means status reports are generated from `git`, not maintained by hand. If a question about project state cannot be answered from git, the git conventions are incomplete and should be extended rather than worked around.

## Confidence self-assessments

At every phase gate, produce an explicit block:

```
Confidence: [low | medium | high]
Residual risks:
  - <specific, named risk>
Known gaps:
  - <specific, named gap>
```

Vague confidence ("seems fine", "should work") is not a confidence assessment — it is the absence of one. If confidence is medium or low, name the gaps concretely and resolve them in the current phase before proceeding. The point is to make uncertainty legible so it can be acted on, not to ritually declare readiness.

## Autonomous progression

Proceed through TDD phases (red → green → refactor → e2e-mock → live-e2e) without waiting for confirmation between phases. Only stop and ask when hitting an actual blocker: an ambiguous contract, a design decision with no clear answer, a dependency that isn't available, or a failing test whose root cause is unclear. "Should I continue?" is not a blocker — it is wasted round-trips.

## What this discipline is for

These rules are not aesthetics. Each one prevents a specific failure mode that compounds over time:

- TDD prevents untestable code from being written in the first place. Adding tests after the fact is harder and the resulting tests are worse.
- The smells list prevents the slow accretion of complexity that makes a codebase eventually unmodifiable.
- Source control discipline makes the project's history a useful artifact rather than a noisy log.
- Git-as-tracker prevents the drift between what the tracker says and what the code says.
- Confidence gates prevent the failure mode of compounding unknown unknowns to the end of the project, where they are most expensive to address.

When a rule feels like it is getting in the way, the rule is usually right and the situation is the thing to reconsider. Departures from the discipline should be documented inline with a reason, not silently elided.
