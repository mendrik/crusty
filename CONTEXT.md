# Repository Quality Learning

Crusty preserves repository evidence and turns resolved defect categories into reviewable validation knowledge without expanding the implementation scope of later changes.

## Language

**Crusty**:
The repository intelligence and durable quality-memory system for Rust projects.
_Avoid_: rust-repo-intelligence when referring to the product

**Problem record**:
A durable, repository-scoped account of a reported defect, its evidence, diagnosis, resolution, and lifecycle.
_Avoid_: Bug memory, issue task

**Automatic problem capture**:
The intake of an explicit user-reported defect into repository memory without requiring a separate bookkeeping request.
_Avoid_: Conversation scraping, automatic constraint approval

**Learned quality constraint**:
A reusable invariant proposed from one or more problem records and governed independently through review, enforcement, and retirement.
_Avoid_: Remembered bug, automatic feature work

**Validation obligation**:
A snapshot-bound activation of a learned quality constraint for a particular change, including its match explanation, recipe, result, and blocking effect.
_Avoid_: Constraint, task

**Validation queue**:
The ordered checks for one change, separated by repository policy, learned quality constraints, and change-specific inference.
_Avoid_: Roadmap, backlog
