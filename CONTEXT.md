# Crusty Repository Observatory

Crusty helps an attached coding agent find, organize, research, and track improvement potential in a Rust repository while preserving evidence freshness and human ownership.

## Language

### Coordinated delivery

**Coding session**:
A bounded period of work with an identified owner, intent, worktree, and renewable right to participate in coordinated changes.
_Avoid_: Server session, task, work item

**Change surface claim**:
A coding session's exclusive declaration of the repository paths it intends to modify while its lease remains active.
_Avoid_: File lock, deletion proof, merge conflict

**Commit plan**:
A proposed grouping of a session's owned changes into cohesive commits, bound to the observed branch, head, and file contents.
_Avoid_: Staging area, completed commit, work chunk

**Implicit commit session**:
The ephemeral coding session Crusty registers for a single agent's commit plan when no other session is active; it owns exactly the planned paths and ends with the delivery.
_Avoid_: Anonymous session, bypassed ownership

**Work chunk**:
A coherent deliverable connecting accepted intent, participating sessions, commits, verification evidence, and its eventual pull request.
_Avoid_: Finding, single commit, arbitrary batch

**Integration conflict**:
A disagreement Git observes when combining identified branch revisions, requiring a validated resolution before incorporation.
_Avoid_: Claim overlap, permission conflict, harmless concurrent work

**Reviewed revision**:
The identified pull-request head and base for which review findings and verification evidence were collected.
_Avoid_: Latest PR, permanent approval, green branch

**Crusty**:
The local repository observatory for Rust projects.
_Avoid_: rust-repo-intelligence when referring to the product

### Repository evidence

**Engineering guidance**:
Versioned conditional advice with rationale, exceptions and evidence requirements for the actual Rust change.
_Avoid_: Blocking policy, unconditional checklist, proof

**Approved domain model**:
A human-reviewed contract for concept owners, invariants, mutation rights and dependency directions.
_Avoid_: Inferred architecture, automatic source conformance, proposed model

**Verification profile**:
An explicit supported selection of workspace members, Cargo features, target and toolchain for a check run.
_Avoid_: All possible configurations, project matrix, green build

**Verification run**:
Executed checks and artifacts bound to source, head, profile, environment and tool versions.
_Avoid_: Task completion, validation reference, permanent permission to deliver

**Delivery policy**:
Explicit human authorization bounded by repository, base, allowed actions, expiry and mutation budget.
_Avoid_: Product implementation consent, remote protection bypass, agent-created authority

**Delivery action**:
A durable remote mutation intent and its observed outcome, recoverable independently of task retention.
_Avoid_: Actual merge, proof of review, unconditional retry

**Measured workload**:
Comparable baseline/candidate execution samples for a declared operation and correctness oracle.
_Avoid_: Universal optimization, profiler evidence, statistical significance

**Live worktree**:
The current Rust source read directly from disk for exact navigation.
_Avoid_: Current index, cache

**Derived index**:
The rebuildable symbol, relationship, Cargo, history, and document projection.
_Avoid_: Source of truth, memory database

**Published generation**:
The last atomically committed derived-index generation available to readers.
_Avoid_: Latest source, refresh result

**Freshness envelope**:
The live and indexed revisions, generation, backend, staleness with its reason, refresh activity, and confidence attached to an answer. Staleness compares live index inputs with those the published generation indexed, not whether the worktree is clean.
_Avoid_: Cache status, version

**Publisher lease**:
The filesystem lock granting one Crusty process exclusive authority to publish an index generation.
_Avoid_: Database lock, leader election

**Symbol card**:
A versioned, bounded description of one repository symbol and its local structural context, used as the stable unit of similarity retrieval.
_Avoid_: Source chunk, embedding

**Hybrid retrieval**:
Concept discovery that combines exact identity, lexical relevance, symbol-card similarity, and typed relationships while preserving the evidence channel of every candidate.
_Avoid_: Semantic proof, vector search

**Retrieval provenance**:
The model, card version, snapshot, ranking channel, and structural evidence that explain why a repository candidate was returned.
_Avoid_: Confidence score, proof

**Semantic companion**:
The one rust-analyzer process a Crusty server keeps for live position queries and diagnostics; it is off until `semantic.enable`, is then warmed in the background and restarted with backoff when it dies, and `semantic.disable` stops it.
_Avoid_: Language server session, index backend

**Analyzer profile**:
The inputs whose change restarts the semantic companion: analyzer version, initialization options, toolchain, and Cargo configuration (manifests, lockfile, `.cargo/config`, toolchain files). Source contents are not part of it; edits are synchronised into the running companion.
_Avoid_: Semantic snapshot, workspace fingerprint

**Captured diagnostics**:
Diagnostics the semantic companion published, kept per file with the document version and content hash they describe, so each is labelled fresh, stale, unconfirmed, or pending against the file on disk.
_Avoid_: Compiler result, verification

**Repository consultation**:
A bounded, read-only briefing of global and topical repository guidance requested before an attached agent plans, answers, or acts.
_Avoid_: Change preparation, repository search

### Human governance

**Decision**:
A human architectural decision recorded in the ledger; only an `accepted` decision governs consultation, prepared changes, and validation.
_Avoid_: Inference, finding, suggestion

**Supersession**:
The human act of recording a new decision or steering that replaces one or more governing records of the same kind, closing each with its actor and history.
_Avoid_: Compaction, automatic sweep, contradiction detection

**Retirement**:
The human act of closing an accepted decision or active steering without a replacement; the record stays in the ledger as history.
_Avoid_: Deletion, archive, cleanup

**Steering**:
A durable human instruction with a scope, priority, and optional expiry that shapes later changes; only an active, unexpired steering governs.
_Avoid_: Prompt, hint, decision

**Steering scope**:
The paths, symbols, or concepts a steering governs. A path scope applies to its ancestors and descendants, never to sibling paths that merely share directory names; an empty scope applies everywhere.
_Avoid_: Tag, keyword

**Stale reference**:
A repository path named by a steering or decision that no longer exists; it is a prompt to ask a human to retire or supersede the record, never a reason for an agent to do so itself.
_Avoid_: Broken link, automatic cleanup

### Research and decisions

**Research run**:
A budgeted investigation combining bounded local evidence with primary-first web search performed by the attached agent.
_Avoid_: Background crawler, backlog grooming

**Research packet**:
The local signals, research questions, web-search queries, evidence policy, and submission contract handed to the attached agent.
_Avoid_: Prompt, sampling request

**Finding**:
An evidence-backed technical, product, or design improvement proposal awaiting human judgment.
_Avoid_: Task, issue, requirement

**Evidence reference**:
A provenance record pointing to local repository state or a qualified web source.
_Avoid_: Citation blob, proof

**Finding review**:
A human decision to accept, dismiss, or request more evidence for a finding.
_Avoid_: Automatic triage, work acceptance

**Promotion**:
The explicit human act that turns an accepted finding into project work.
_Avoid_: Discovery, recommendation, auto-create

**Project work**:
Human-owned, durable intent tracked by Crusty without replacing an external issue tracker or product backlog.
_Avoid_: Finding, autonomous task

### Quality learning

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
