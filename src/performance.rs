//! Comparable release-process measurements on isolated immutable revisions.

use crate::{
    coordination::Coordinator,
    execution::MAX_CAPTURE_BYTES,
    verification::{BuildProfile, CheckKind, VerificationPlanRequest},
};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rand::random;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{fs, process::Command, time::Instant};

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerformanceContractRequest {
    pub workload: String,
    /// Budget for the metric measured by performance.measure's selected adapter.
    pub wall_time_budget_ms: f64,
    /// Operations completed by each invocation, explicitly defined by the owner.
    pub operations_per_invocation: u64,
    pub rationale: String,
}

fn default_samples() -> usize {
    7
}
fn default_warmup() -> usize {
    1
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MetricSource {
    /// Emit a JSON object with elapsed_ns and stable result/checksum fields.
    #[default]
    StdoutJson,
    /// Includes startup and up to 25 ms exit-poll observation overhead.
    ProcessWallTime,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerformanceMeasureRequest {
    pub contract_id: String,
    pub baseline_ref: String,
    pub candidate_ref: String,
    pub package: String,
    pub binary: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub profile: BuildProfile,
    #[serde(default = "default_samples")]
    pub samples: usize,
    #[serde(default = "default_warmup")]
    pub warmup: usize,
    #[serde(default)]
    pub metric_source: MetricSource,
    /// Exact stdout equivalence is the default behavioral oracle. Disabling it
    /// requires separate correctness evidence before accepting an optimization.
    #[serde(default)]
    pub allow_output_difference: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PerformanceContract {
    id: String,
    workload: String,
    wall_time_budget_ms: f64,
    operations_per_invocation: u64,
    rationale: String,
    created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Sample {
    elapsed_ns: u128,
    process_wall_ns: u128,
    stdout_digest: String,
    stderr_digest: String,
    stdout_path: String,
    stderr_path: String,
}

impl Coordinator {
    fn performance_db(&self) -> Result<rusqlite::Connection> {
        let db = self.db()?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS performance_contracts(id TEXT PRIMARY KEY,payload TEXT NOT NULL);CREATE TABLE IF NOT EXISTS performance_measurements(id TEXT PRIMARY KEY,payload TEXT NOT NULL)")?;
        Ok(db)
    }
    pub(crate) fn performance_contract(
        &self,
        request: PerformanceContractRequest,
    ) -> Result<Value> {
        ensure!(
            !request.workload.trim().is_empty()
                && request.workload.len() <= 8000
                && !request.rationale.trim().is_empty()
                && request.rationale.len() <= 8000,
            "workload and rationale are required"
        );
        ensure!(
            request.wall_time_budget_ms.is_finite()
                && request.wall_time_budget_ms > 0.0
                && request.operations_per_invocation > 0,
            "positive finite budget and operation count are required"
        );
        let contract = PerformanceContract {
            id: format!("perf_{:032x}", random::<u128>()),
            workload: request.workload,
            wall_time_budget_ms: request.wall_time_budget_ms,
            operations_per_invocation: request.operations_per_invocation,
            rationale: request.rationale,
            created_at: Utc::now().timestamp(),
        };
        self.performance_db()?.execute(
            "INSERT INTO performance_contracts(id,payload) VALUES (?1,?2)",
            params![contract.id, serde_json::to_string(&contract)?],
        )?;
        Ok(serde_json::to_value(contract)?)
    }
    pub(crate) fn performance_get(&self, id: &str) -> Result<Value> {
        let db = self.performance_db()?;
        for table in ["performance_contracts", "performance_measurements"] {
            let query = format!("SELECT payload FROM {table} WHERE id=?1");
            if let Some(raw) = db
                .query_row(&query, [id], |row| row.get::<_, String>(0))
                .optional()?
            {
                return Ok(serde_json::from_str(&raw)?);
            }
        }
        anyhow::bail!("unknown performance contract or measurement")
    }
    fn save_measurement(&self, record: &Value) -> Result<()> {
        self.performance_db()?.execute("INSERT INTO performance_measurements(id,payload) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET payload=excluded.payload",params![record["id"].as_str(),record.to_string()])?;
        Ok(())
    }

    pub(crate) fn performance_measure(&self, request: PerformanceMeasureRequest) -> Result<Value> {
        ensure!(
            (3..=100).contains(&request.samples)
                && request.warmup <= 20
                && request.arguments.len() <= 100,
            "invalid measurement sample/warmup/argument bounds"
        );
        ensure!(
            request
                .arguments
                .iter()
                .all(|arg| arg.len() <= 8000 && !arg.contains('\0')),
            "invalid executable arguments"
        );
        let contract: PerformanceContract =
            serde_json::from_value(self.performance_get(&request.contract_id)?)?;
        let baseline = self.resolve_commit(&request.baseline_ref)?;
        let candidate = self.resolve_commit(&request.candidate_ref)?;
        let _measurement_guard = self.lock("performance.lock")?;
        let id = format!("measure_{:032x}", random::<u128>());
        let mut record = json!({"id":id,"contract":contract,"baseline_head":baseline,"candidate_head":candidate,"profile":request.profile,
            "package":request.package,"binary":request.binary,"arguments":request.arguments,"samples":request.samples,"warmup":request.warmup,
            "metric_source":request.metric_source,"state":"running","created_at":Utc::now().timestamp(),"platform":{"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"available_parallelism":std::thread::available_parallelism().ok().map(|n|n.get())},
            "protocol":"Sequential isolated release builds, warmup, then measured invocations. Build time is excluded. StdoutJson reads executable-instrumented elapsed_ns and compares other JSON result fields. ProcessWallTime includes startup and up to 25 ms exit observation overhead, so small differences are unresolved. No profiler/allocation/soundness evidence is inferred; external load is uncontrolled."});
        self.save_measurement(&record)?;
        let outcome = (|| -> Result<()> {
            let mut expected_stdout = None;
            for (label, head) in [("baseline", &baseline), ("candidate", &candidate)] {
                self.control.check()?;
                let worktree = self.state.join("worktrees").join(format!("{id}-{label}"));
                fs::create_dir_all(worktree.parent().context("missing worktree parent")?)?;
                {
                    let _git_guard = self.lock("git-mutation.lock")?;
                    self.git(&[
                        "worktree",
                        "add",
                        "--detach",
                        worktree.to_str().context("non-UTF-8 worktree")?,
                        head,
                    ])?;
                }
                let coord = Coordinator::open(&worktree, self.control.clone())?;
                let metadata = coord.project_contract()?;
                let package = metadata["members"]
                    .as_array()
                    .context("metadata members missing")?
                    .iter()
                    .find(|p| p["name"] == request.package)
                    .context("benchmark package is not a workspace member")?;
                ensure!(
                    package["targets"]
                        .as_array()
                        .is_some_and(|targets| targets.iter().any(|t| t["name"] == request.binary
                            && t["kind"]
                                .as_array()
                                .is_some_and(|k| k.contains(&json!("bin"))))),
                    "benchmark binary is not a declared package target"
                );
                let mut profile = request.profile.clone();
                profile.packages = vec![request.package.clone()];
                let mut build: Value = coord.verification_plan(VerificationPlanRequest {
                    profile,
                    checks: vec![CheckKind::Release],
                    offline: true,
                    test_filter: None,
                    deny_warnings: None,
                })?;
                let command = build["commands"][0].clone();
                let mut args = command["args"]
                    .as_array()
                    .context("build arguments missing")?
                    .iter()
                    .map(|a| {
                        a.as_str()
                            .context("invalid build argument")
                            .map(str::to_owned)
                    })
                    .collect::<Result<Vec<_>>>()?;
                args.extend(["--bin".into(), request.binary.clone()]);
                let output = self
                    .control
                    .output(Command::new("cargo").current_dir(&worktree).args(&args))?;
                ensure!(
                    output.status.success()
                        && output.stdout.len() < MAX_CAPTURE_BYTES
                        && output.stderr.len() < MAX_CAPTURE_BYTES,
                    "release build failed or was truncated: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let executable = output
                    .stdout
                    .split(|b| *b == b'\n')
                    .filter_map(|line| serde_json::from_slice::<Value>(line).ok())
                    .find(|message| {
                        message["reason"] == "compiler-artifact"
                            && message["target"]["name"] == request.binary
                            && message["executable"].is_string()
                    })
                    .context("Cargo did not report the built benchmark executable")?["executable"]
                    .as_str()
                    .context("executable missing")?
                    .to_owned();
                let executable_path = fs::canonicalize(&executable)?;
                let target_directory=fs::canonicalize(worktree.join("target")).context("benchmark requires its isolated default target directory; external target directories are unsupported")?;
                ensure!(
                    executable_path.starts_with(&target_directory),
                    "benchmark executable lies outside the isolated target directory"
                );
                let before = coord.workspace_fingerprint()?;
                let mut samples = Vec::new();
                for index in 0..request.warmup + request.samples {
                    let start = Instant::now();
                    let output = self.control.output(
                        Command::new(&executable_path)
                            .current_dir(&worktree)
                            .args(&request.arguments),
                    )?;
                    let process_wall_ns = start.elapsed().as_nanos();
                    ensure!(
                        output.status.success()
                            && output.stdout.len() < MAX_CAPTURE_BYTES
                            && output.stderr.len() < MAX_CAPTURE_BYTES,
                        "benchmark process failed or output was truncated: {}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                    let stdout_digest = format!("b3:{}", blake3::hash(&output.stdout).to_hex());
                    let (elapsed_ns, oracle_digest) = match request.metric_source {
                        MetricSource::ProcessWallTime => (process_wall_ns, stdout_digest.clone()),
                        MetricSource::StdoutJson => {
                            let mut result:Value=serde_json::from_slice(&output.stdout).context("benchmark must emit JSON with positive elapsed_ns and stable result fields")?;
                            let elapsed = result["elapsed_ns"]
                                .as_u64()
                                .filter(|value| *value > 0)
                                .context("benchmark elapsed_ns must be a positive u64")?;
                            let object = result
                                .as_object_mut()
                                .context("benchmark JSON must be an object")?;
                            object.remove("elapsed_ns");
                            ensure!(
                                !object.is_empty() || request.allow_output_difference,
                                "stdout correctness oracle needs stable result fields besides elapsed_ns"
                            );
                            (
                                elapsed as u128,
                                format!(
                                    "b3:{}",
                                    blake3::hash(result.to_string().as_bytes()).to_hex()
                                ),
                            )
                        }
                    };
                    if !request.allow_output_difference {
                        if let Some(expected) = &expected_stdout {
                            ensure!(
                                &oracle_digest == expected,
                                "benchmark result differs between runs/revisions; correctness oracle failed"
                            );
                        } else {
                            expected_stdout = Some(oracle_digest);
                        }
                    }
                    if index < request.warmup {
                        continue;
                    }
                    let stdout = self.state.join(format!("{id}-{label}-{index}.stdout"));
                    let stderr = self.state.join(format!("{id}-{label}-{index}.stderr"));
                    ensure!(
                        output.stdout.len() + output.stderr.len() <= 100_000,
                        "benchmark artifact exceeds 100000 bytes per sample"
                    );
                    fs::write(&stdout, &output.stdout)?;
                    fs::write(&stderr, &output.stderr)?;
                    samples.push(Sample {
                        elapsed_ns,
                        process_wall_ns,
                        stdout_digest,
                        stderr_digest: format!("b3:{}", blake3::hash(&output.stderr).to_hex()),
                        stdout_path: stdout.to_string_lossy().into_owned(),
                        stderr_path: stderr.to_string_lossy().into_owned(),
                    });
                }
                ensure!(
                    before == coord.workspace_fingerprint()?,
                    "benchmark modified visible source inputs; comparison is invalid"
                );
                build["samples"] = json!(samples);
                build["statistics"] = statistics(&samples, contract.operations_per_invocation);
                build["worktree"] = json!(worktree);
                record[label] = build;
                self.save_measurement(&record)?;
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            record["state"] = json!("failed");
            record["error"] = json!(format!("{error:#}"));
            self.save_measurement(&record)?;
            return Err(error);
        }
        let base = record["baseline"]["statistics"]["median_ms"]
            .as_f64()
            .context("baseline median missing")?;
        let candidate = record["candidate"]["statistics"]["median_ms"]
            .as_f64()
            .context("candidate median missing")?;
        record["state"] = json!("completed");
        record["median_improvement_percent"] = json!((base - candidate) / base * 100.0);
        record["candidate_within_wall_time_budget"] =
            json!(candidate <= contract.wall_time_budget_ms);
        record["stdout_equivalence_checked"] = json!(!request.allow_output_difference);
        record["claim_limit"] = json!(
            "Observed samples only; this does not prove statistical significance, allocation efficiency, semantic equivalence beyond the selected stdout oracle, or universally optimal code."
        );
        self.save_measurement(&record)?;
        Ok(record)
    }
}

fn statistics(samples: &[Sample], operations: u64) -> Value {
    let mut ns = samples
        .iter()
        .map(|sample| sample.elapsed_ns)
        .collect::<Vec<_>>();
    ns.sort();
    let median = (ns[(ns.len() - 1) / 2] as f64 + ns[ns.len() / 2] as f64) / 2_000_000.0;
    let p95 = ns[(ns.len() as f64 * 0.95).ceil() as usize - 1] as f64 / 1_000_000.0;
    json!({"median_ms":median,"p95_ms":p95,"min_ms":ns[0] as f64/1_000_000.0,"max_ms":ns[ns.len()-1] as f64/1_000_000.0,
        "mean_ms":ns.iter().map(|value|*value as f64).sum::<f64>()/ns.len() as f64/1_000_000.0,"median_operations_per_second":operations as f64/(median/1000.0),"sample_count":samples.len()})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measurement_statistics_report_distribution_and_unit_conversion() {
        let samples = [1, 3, 2, 4, 100]
            .into_iter()
            .map(|ms| Sample {
                elapsed_ns: ms * 1_000_000,
                process_wall_ns: ms * 1_000_000,
                stdout_digest: String::new(),
                stderr_digest: String::new(),
                stdout_path: String::new(),
                stderr_path: String::new(),
            })
            .collect::<Vec<_>>();
        let stats = statistics(&samples, 6);
        assert_eq!(stats["median_ms"], 3.0);
        assert_eq!(stats["p95_ms"], 100.0);
        assert_eq!(stats["median_operations_per_second"], 2000.0);
    }

    #[test]
    fn real_release_comparison_retains_revision_bound_samples_and_checksums() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname='measured_fixture'\nversion='0.1.0'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"),"fn main() {\n    let start = std::time::Instant::now();\n    let mut sum = 0u64;\n    for i in 0..100_000u64 {\n        sum = sum.wrapping_add(std::hint::black_box(i));\n    }\n    println!(\"{{\\\"elapsed_ns\\\":{},\\\"checksum\\\":{}}}\", start.elapsed().as_nanos(), sum);\n}\n").unwrap();
        let run = |program: &str, args: &[&str]| {
            let output = Command::new(program)
                .current_dir(root)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        run("cargo", &["generate-lockfile", "--offline"]);
        run("git", &["init", "-q"]);
        run("git", &["config", "user.name", "Test"]);
        run("git", &["config", "user.email", "test@example.test"]);
        run("git", &["add", "Cargo.toml", "Cargo.lock", "src/main.rs"]);
        run("git", &["commit", "-qm", "baseline"]);
        let baseline = run("git", &["rev-parse", "HEAD"]);
        fs::write(
            root.join("README.md"),
            "Candidate with identical behavior\n",
        )
        .unwrap();
        run("git", &["add", "README.md"]);
        run("git", &["commit", "-qm", "candidate"]);
        let candidate = run("git", &["rev-parse", "HEAD"]);
        let coord = Coordinator::open(root, crate::execution::ExecutionControl::default()).unwrap();
        let contract = coord
            .performance_contract(PerformanceContractRequest {
                workload: "Sum 100000 integers using black_box".into(),
                wall_time_budget_ms: 100.0,
                operations_per_invocation: 100_000,
                rationale: "Measure a specific workload, excluding build/launch overhead".into(),
            })
            .unwrap();
        let result = coord
            .performance_measure(PerformanceMeasureRequest {
                contract_id: contract["id"].as_str().unwrap().into(),
                baseline_ref: baseline.trim().into(),
                candidate_ref: candidate.trim().into(),
                package: "measured_fixture".into(),
                binary: "measured_fixture".into(),
                arguments: vec![],
                profile: BuildProfile::default(),
                samples: 3,
                warmup: 1,
                metric_source: MetricSource::StdoutJson,
                allow_output_difference: false,
            })
            .unwrap();
        assert_eq!(result["state"], "completed");
        assert_eq!(result["stdout_equivalence_checked"], true);
        assert_eq!(result["baseline"]["samples"].as_array().unwrap().len(), 3);
        assert_eq!(result["candidate"]["samples"].as_array().unwrap().len(), 3);
        assert!(
            result["candidate"]["statistics"]["median_ms"]
                .as_f64()
                .unwrap()
                > 0.0
        );
        assert_eq!(run("git", &["rev-parse", "HEAD"]), candidate);
        assert_eq!(
            coord
                .performance_get(result["id"].as_str().unwrap())
                .unwrap(),
            result
        );
    }
}
