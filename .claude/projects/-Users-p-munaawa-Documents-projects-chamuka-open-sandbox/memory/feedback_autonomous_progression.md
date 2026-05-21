---
name: feedback-autonomous-progression
description: Proceed through TDD phases without stopping to ask — only pause on actual blockers
metadata:
  type: feedback
---

Proceed through TDD phases (red → green → refactor → e2e-mock → live-e2e) autonomously without asking for confirmation between phases. Only stop when hitting an actual blocker.

**Why:** The user considers "Should I continue?" prompts wasted round-trips. They want momentum.

**How to apply:** After completing any TDD phase, immediately proceed to the next. Tag, commit, keep going. Only pause for ambiguous contracts, unclear design decisions, missing dependencies, or genuinely unclear test failures.
