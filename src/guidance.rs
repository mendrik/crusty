//! Versioned engineering judgment. Advice is conditional, never implicit policy.

use anyhow::{Result, ensure};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EngineeringArea {
    Core,
    Ownership,
    Api,
    Architecture,
    Cleanup,
    Concurrency,
    Unsafe,
    Performance,
    Verification,
    Cargo,
    Specialized,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GuidanceRequest {
    pub intent: String,
    /// Explicit areas augment routing from intent; evidence still determines applicability.
    #[serde(default)]
    pub areas: Vec<EngineeringArea>,
    pub budget: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
struct Rule {
    id: &'static str,
    area: EngineeringArea,
    action: &'static str,
    rationale: &'static str,
    exception: &'static str,
    evidence: &'static str,
}

const RULES: &[Rule] = &[
    Rule {
        id: "core.authority",
        area: EngineeringArea::Core,
        action: "Read current instructions, accepted decisions, Cargo/CI contracts and dirty changes. Name the behavior and observable acceptance criterion before editing.",
        rationale: "A globally elegant change can violate the actual product contract.",
        exception: "Focused, obvious changes need only relevant local evidence; do not require a repository-wide audit.",
        evidence: "repo.consult, project.contract, current source, exact searches and compiler/runtime behavior.",
    },
    Rule {
        id: "core.boundaries",
        area: EngineeringArea::Core,
        action: "Parse and validate boundary data once. Keep a canonical typed internal representation; use enums/private constructors for consequential states and transitions.",
        rationale: "Strings, loosely structured JSON and repeated validation let inconsistent states cross modules.",
        exception: "serde_json::Value is appropriate for opaque external evidence, extensible payloads and final transport serialization; do not invent DTO layers that have no independent evolution.",
        evidence: "Trace every construction, mutation, deserialization and persistence path, including retries and recovery.",
    },
    Rule {
        id: "core.complete",
        area: EngineeringArea::Core,
        action: "Make the smallest complete change across legitimate callers, tests, configuration, documentation and dependencies. Preserve user edits and explicit compatibility contracts.",
        rationale: "A local patch can leave two competing owners or an unfinished migration.",
        exception: "Split a migration only when atomic change is genuinely impossible; record its remaining callers and retirement criterion.",
        evidence: "Current references, exact identifier sweep, full resulting diff and supported checks.",
    },
    Rule {
        id: "ownership.lifecycle",
        area: EngineeringArea::Ownership,
        action: "Assign one lifecycle owner. Borrow for temporary access; transfer or clone for independent retention. Explain mutation and Send/Sync rights before adding pointers or locks.",
        rationale: "Ownership errors usually reveal an unclear retention or mutation contract.",
        exception: "Clone when independent ownership is correct and measured cost is acceptable; avoid lifetime ceremony solely to eliminate a cheap clone.",
        evidence: "All retain/spawn/await/drop paths, precise compiler notes, allocation evidence when relevant.",
    },
    Rule {
        id: "api.contracts",
        area: EngineeringArea::Api,
        action: "Make public invariants, error identity, serialization, feature behavior, semver/MSRV and ABI commitments explicit. Keep trait bounds cohesive and minimal.",
        rationale: "Consumers rely on more than signatures; broad bounds and hidden fallbacks constrain future changes.",
        exception: "Application errors may aggregate with anyhow when the repository accepts it. Panic only for an enforced, obvious impossible state or clear test failure.",
        evidence: "Consumer call sites, doctests, error recovery, cargo metadata, compatibility checks and declared target matrix.",
    },
    Rule {
        id: "architecture.owners",
        area: EngineeringArea::Architecture,
        action: "Map concepts, lifecycle/state owners, mutation rights, adapters and dependency direction. Trace cross-module assumptions and enforce important invariants at their canonical owner.",
        rationale: "Local correctness misses inconsistent validation, duplicated state and forbidden transitions across entry points.",
        exception: "A warning pattern is a lead, never a finding by itself. Avoid new layers or traits without a real reason to vary.",
        evidence: "Approved domain.model, live architecture facts, implementations/references, runtime failure paths and counter-evidence.",
    },
    Rule {
        id: "architecture.failure",
        area: EngineeringArea::Architecture,
        action: "Design partial failure, idempotency, crash recovery, resource cleanup and cache invalidation together with the successful path.",
        rationale: "Retries and stale state can violate invariants even when every individual function looks correct.",
        exception: "A deliberately non-retryable operation should return an inspectable recovery state, not conceal uncertain completion.",
        evidence: "Timeout-after-effect, cancellation, process restart, concurrent caller and stale revision fixtures.",
    },
    Rule {
        id: "cleanup.canonical",
        area: EngineeringArea::Cleanup,
        action: "Select the current canonical owner from requirements. Delete unnecessary behavior, reuse it, move it to its owner, then generalize only actual shared semantics.",
        rationale: "Keeping obsolete production paths or adding a utils layer preserves accumulated architecture debt.",
        exception: "Retain compatibility only for a concrete external contract, isolated at its boundary with an explicit lifetime.",
        evidence: "Lifecycle evidence with provenance, legitimate consumers, Cargo features/dependencies, generated conventions and accepted decisions.",
    },
    Rule {
        id: "cleanup.slice",
        area: EngineeringArea::Cleanup,
        action: "Trace the complete migration slice: definitions, callers, impls, tests, fixtures, flags, configs, manifests, dependencies and docs. Remove replaced paths and sweep for orphaned names.",
        rationale: "Search matches alone neither prove deadness nor cover all dynamic/configured use.",
        exception: "Keep reflection, ABI, plugin or generated consumers until their contract is established; a missing semantic match is not proof of absence.",
        evidence: "semantic.query, symbol.relations provenance, repo.search exact, live config, external contracts, check results and final sweep.",
    },
    Rule {
        id: "concurrency.lifecycle",
        area: EngineeringArea::Concurrency,
        action: "Bound tasks/channels/work and define cancellation, deadlines, backpressure, lock ordering and join/drop ownership. Keep blocking I/O and CPU work off async workers.",
        rationale: "Resource exhaustion and shutdown races are correctness failures, not merely style issues.",
        exception: "Hold a synchronous lock only for short non-await work; longer critical work needs explicit scheduling and contention limits.",
        evidence: "Deterministic races, slow consumers, dropped futures, subprocess-tree cleanup, timeout and shutdown tests.",
    },
    Rule {
        id: "unsafe.soundness",
        area: EngineeringArea::Unsafe,
        action: "State and enforce every unsafe invariant: validity, initialization, aliasing, alignment, lifetimes, provenance, ownership, Pin and thread transfer. Hide minimal unsafe operations behind a safe boundary.",
        rationale: "Type checking and green ordinary tests cannot prove unsafe soundness.",
        exception: "Use unsafe only for an established requirement; do not introduce it for speculative speed. Miri is supporting evidence with target/coverage limitations.",
        evidence: "Every caller and mutation path, SAFETY rationale, Miri/fuzz/sanitizer/ABI checks as applicable, Rust Reference and Nomicon.",
    },
    Rule {
        id: "performance.measure",
        area: EngineeringArea::Performance,
        action: "Name workload, budget, baseline, profile, hardware/toolchain and metric. Measure representative release behavior before changing algorithms, allocation, batching or locality.",
        rationale: "Clippy cleanliness, fewer clones and iterator syntax do not establish faster code.",
        exception: "An obvious complexity defect can justify an algorithm change; still verify semantic equivalence and quantify representative impact.",
        evidence: "Reproducible before/after samples, distributions and measurement protocol; profiler evidence tied to exact revisions/configuration.",
    },
    Rule {
        id: "performance.cost",
        area: EngineeringArea::Performance,
        action: "Inspect repeated parsing/I/O, N+1 queries, full scans, allocation, hashing, lock contention and unbounded work. State relevant time/space complexity and cache freshness contracts.",
        rationale: "Structural costs often dominate small syntax-level optimizations.",
        exception: "Prefer clarity where measurements show negligible cost. A cache is worthwhile only with clear ownership, bounds and invalidation.",
        evidence: "Workload scale, operation counts, allocation profiles, latency tails and failure/correctness checks.",
    },
    Rule {
        id: "verification.layers",
        area: EngineeringArea::Verification,
        action: "Reproduce failures and read full compiler notes first. Run focused checks, then applicable workspace tests, doctests, lint policy and supported feature/target/MSRV profiles.",
        rationale: "A default host build cannot establish promised matrix coverage.",
        exception: "Do not mirror implementation in tests or add low-value tests for trivial reversible edits. Test behavioral boundaries and regressions with independent oracles.",
        evidence: "verification.plan/run with exact revision, profile, full diagnostic children and explicit unrun/unsupported coverage.",
    },
    Rule {
        id: "cargo.matrix",
        area: EngineeringArea::Cargo,
        action: "Use Cargo metadata plus current CI/config/instructions to establish package topology, feature policy, toolchain/MSRV, targets, dependencies and lint policy. Test supported feature combinations explicitly.",
        rationale: "Feature lists are possible names, not a contract that all features can coexist.",
        exception: "Use all-features only when the project supports it. Missing MSRV/target promises remain unknown, not invented defaults.",
        evidence: "Live project.contract; cargo tree duplicates/features; declared matrix; installation availability and primary Cargo documentation.",
    },
    Rule {
        id: "specialized.proofs",
        area: EngineeringArea::Specialized,
        action: "Choose evidence for the actual domain: concurrency scheduling, unsafe/FFI ABI, no_std/embedded target, WASM runtime, database transactions, MCP/process lifecycle, public API and dependency policy.",
        rationale: "Generic checks cannot substitute for the specialized failure mode.",
        exception: "Report unavailable runners/toolchains and test scope honestly. Do not add dependencies merely to satisfy a generic checklist.",
        evidence: "Domain-specific deterministic fixtures, Miri/Loom/fuzz/sanitizers, release measurements, cross-target or ABI/API/advisory tools where supported.",
    },
];

pub fn guidance(request: GuidanceRequest) -> Result<Value> {
    ensure!(
        !request.intent.trim().is_empty() && request.intent.len() <= 16_000,
        "intent must be 1..=16000 bytes"
    );
    let areas = route(&request.intent, &request.areas);
    let rules = RULES
        .iter()
        .filter(|r| areas.contains(&r.area))
        .collect::<Vec<_>>();
    Ok(crate::response_budget::bound(
        json!({"query":request.intent,"guidance_version":"rust-engineering-v1","areas":areas,
        "engineering_guidance":rules,"authority":"Conditional engineering judgment. Current human instructions, accepted policy and measured compiler/runtime evidence take precedence; these cards never activate a constraint or create work.",
        "sources":["Rust engineering workflow, ownership/API, concurrency, unsafe, verification and performance expertise","Canonical-owner cleanup workflow","Rustitect semantic boundaries, invariants and cross-entry-point review"],
        "next":"Read project.contract and pertinent live source; obtain semantic evidence and an applicable verification plan. Rule exceptions matter."}),
        request.budget.unwrap_or(3000),
    ))
}

pub(crate) fn guidance_hint(intent: &str) -> Value {
    let areas = route(intent, &[]);
    json!({"version":"rust-engineering-v1","areas":areas,"tool":"engineering.guidance","authority":"conditional advice, not human policy"})
}

fn route(intent: &str, explicit: &[EngineeringArea]) -> Vec<EngineeringArea> {
    let text = intent.to_lowercase();
    let mut areas = vec![EngineeringArea::Core];
    for (area, terms) in [
        (
            EngineeringArea::Ownership,
            &["borrow", "lifetime", "clone", "ownership", "type-safe"][..],
        ),
        (
            EngineeringArea::Api,
            &["api", "error", "public", "trait", "type-safe"][..],
        ),
        (
            EngineeringArea::Architecture,
            &[
                "architecture",
                "structure",
                "boundary",
                "domain",
                "refactor",
                "state",
                "review",
            ][..],
        ),
        (
            EngineeringArea::Cleanup,
            &[
                "cleanup",
                "clean",
                "consolidat",
                "migrat",
                "legacy",
                "dead code",
                "duplicate",
            ][..],
        ),
        (
            EngineeringArea::Concurrency,
            &[
                "async",
                "tokio",
                "parallel",
                "concurren",
                "cancel",
                "mutex",
                "channel",
                "session",
            ][..],
        ),
        (
            EngineeringArea::Unsafe,
            &[
                "unsafe",
                "ffi",
                "raw pointer",
                "abi",
                "pin",
                "allocator",
                "soundness",
            ][..],
        ),
        (
            EngineeringArea::Performance,
            &[
                "perform",
                "optimiz",
                "latency",
                "slow",
                "benchmark",
                "allocation",
                "throughput",
            ][..],
        ),
        (
            EngineeringArea::Verification,
            &[
                "test", "check", "debug", "diagnos", "review", "verify", "bug",
            ][..],
        ),
        (
            EngineeringArea::Cargo,
            &[
                "cargo",
                "feature",
                "msrv",
                "toolchain",
                "dependenc",
                "workspace",
                "target",
            ][..],
        ),
        (
            EngineeringArea::Specialized,
            &[
                "wasm",
                "embedded",
                "no_std",
                "mcp",
                "database",
                "sqlite",
                "sanitizer",
                "fuzz",
            ][..],
        ),
    ] {
        if (explicit.contains(&area) || terms.iter().any(|term| text.contains(term)))
            && !areas.contains(&area)
        {
            areas.push(area);
        }
    }
    areas
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn different_intents_receive_specific_advice_and_exceptions() {
        let cleanup = guidance(GuidanceRequest {
            intent: "clean duplicate implementations".into(),
            areas: vec![],
            budget: Some(8000),
        })
        .unwrap();
        let unsafe_advice = guidance(GuidanceRequest {
            intent: "review unsafe FFI soundness".into(),
            areas: vec![],
            budget: Some(8000),
        })
        .unwrap();
        assert!(
            cleanup["engineering_guidance"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == "cleanup.slice")
        );
        assert!(
            !cleanup["engineering_guidance"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == "unsafe.soundness")
        );
        assert!(
            unsafe_advice["engineering_guidance"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["id"] == "unsafe.soundness"
                    && r["exception"].as_str().is_some_and(|s| !s.is_empty()))
        );
        let tiny = guidance(GuidanceRequest {
            intent: "optimize concurrent unsafe Rust".into(),
            areas: vec![],
            budget: Some(250),
        })
        .unwrap();
        assert!(tiny.to_string().len() <= 1000);
        assert_eq!(tiny["context_budget"]["truncated"], true);
    }
}
