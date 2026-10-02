//! Live Cargo contracts and revision-bound, explicitly scoped verification.

use crate::{coordination::Coordinator, execution::MAX_CAPTURE_BYTES};
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use rand::random;
use rusqlite::{OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, io::Read, process::Command, time::Instant};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    Format,
    Check,
    Test,
    Clippy,
    DocTests,
    Documentation,
    Release,
    Bench,
    Miri,
}

fn default_checks() -> Vec<CheckKind> {
    vec![
        CheckKind::Format,
        CheckKind::Check,
        CheckKind::Test,
        CheckKind::Clippy,
    ]
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BuildProfile {
    /// Empty means the complete workspace. Otherwise explicit member names.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Explicit supported feature combination; no inferred powerset.
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub no_default_features: bool,
    #[serde(default)]
    pub all_features: bool,
    pub target: Option<String>,
    /// Installed rustup toolchain, e.g. stable, nightly, or declared MSRV.
    pub toolchain: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerificationPlanRequest {
    #[serde(default)]
    pub profile: BuildProfile,
    #[serde(default = "default_checks")]
    pub checks: Vec<CheckKind>,
    /// Keep network and dependency resolution explicit. Plans always use --locked.
    #[serde(default)]
    pub offline: bool,
    /// Optional named libtest filter. Focused results do not authorize delivery.
    pub test_filter: Option<String>,
    /// Omission follows current repository lint policy rather than inventing it.
    pub deny_warnings: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckCommand {
    kind: CheckKind,
    program: String,
    args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerificationPlan {
    id: String,
    head: Option<String>,
    workspace_digest: String,
    environment_digest: String,
    tool_versions: Value,
    profile: BuildProfile,
    commands: Vec<CheckCommand>,
    test_filter: Option<String>,
    created_at: i64,
    scope: String,
}

impl Coordinator {
    fn verification_tool_versions(&self, profile: &BuildProfile) -> Result<Value> {
        let mut versions = serde_json::Map::new();
        for (program, flag) in [("rustc", "-vV"), ("cargo", "--version")] {
            let mut command = Command::new(program);
            command.current_dir(&self.root);
            if let Some(toolchain) = &profile.toolchain {
                command.arg(format!("+{toolchain}"));
            }
            let output = self.control.output(command.arg(flag))?;
            ensure!(
                output.status.success(),
                "cannot identify verification {program}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            versions.insert(
                program.into(),
                json!(String::from_utf8(output.stdout)?.trim()),
            );
        }
        Ok(Value::Object(versions))
    }
    fn verification_db(&self) -> Result<rusqlite::Connection> {
        let db = self.db()?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS verification_plans(id TEXT PRIMARY KEY,payload TEXT NOT NULL); CREATE TABLE IF NOT EXISTS verification_runs(id TEXT PRIMARY KEY,plan_id TEXT NOT NULL,payload TEXT NOT NULL)")?;
        Ok(db)
    }

    pub(crate) fn project_contract(&self) -> Result<Value> {
        let output = self
            .control
            .output(Command::new("cargo").current_dir(&self.root).args([
                "metadata",
                "--format-version=1",
                "--no-deps",
                "--offline",
            ]))?;
        ensure!(
            output.status.success(),
            "Cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        ensure!(
            output.stdout.len() < MAX_CAPTURE_BYTES,
            "Cargo metadata exceeds capture budget"
        );
        let metadata: Value = serde_json::from_slice(&output.stdout)?;
        let workspace = fs::canonicalize(
            metadata["workspace_root"]
                .as_str()
                .context("Cargo workspace root missing")?,
        )?;
        ensure!(
            workspace == self.root,
            "verification requires the Cargo workspace root: {}",
            workspace.display()
        );
        let members = metadata["workspace_members"]
            .as_array()
            .context("Cargo workspace members missing")?;
        let packages = metadata["packages"]
            .as_array()
            .context("Cargo packages missing")?
            .iter()
            .filter(|package| members.contains(&package["id"]))
            .map(|p| {
                json!({"name":p["name"],"id":p["id"],"manifest_path":p["manifest_path"],
                "edition":p["edition"],"rust_version":p["rust_version"],"features":p["features"],
                "targets":p["targets"],"metadata":p["metadata"],"dependencies":p["dependencies"]})
            })
            .collect::<Vec<_>>();
        let mut evidence = Vec::new();
        let mut omitted = 0usize;
        for path in self.input_paths()? {
            let name = path.file_name().and_then(|p| p.to_str()).unwrap_or("");
            let relative = path
                .strip_prefix(&self.root)?
                .to_string_lossy()
                .into_owned();
            let relevant = matches!(
                name,
                "AGENTS.md"
                    | "CLAUDE.md"
                    | "CONTEXT.md"
                    | "Cargo.toml"
                    | "rust-toolchain"
                    | "rust-toolchain.toml"
                    | "rustfmt.toml"
                    | ".rustfmt.toml"
                    | "clippy.toml"
                    | ".clippy.toml"
                    | "config"
                    | "config.toml"
            ) || relative.starts_with(".github/workflows/")
                || relative.starts_with("docs/adr/");
            if !relevant {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink()
                || !metadata.is_file()
                || metadata.len() > 64_000
                || evidence.len() >= 100
            {
                omitted += 1;
                continue;
            }
            evidence.push(json!({"path":relative,"content":fs::read_to_string(&path)?,"provenance":"live_file"}));
        }
        Ok(
            json!({"workspace_root":workspace,"members":packages,"default_members":metadata["workspace_default_members"],
            "evidence":evidence,"evidence_omitted":omitted,"head":self.git_optional(&["rev-parse","--verify","HEAD"])? ,
            "workspace_digest":self.workspace_fingerprint()?,"environment_digest":environment_digest(),
            "matrix_authority":"Cargo declares possible features and targets, not supported feature combinations. Read live CI and instructions; supply each supported combination explicitly. Missing MSRV is not inferred.",
            "source":"cargo metadata --no-deps --offline plus live governing/configuration files"}),
        )
    }

    pub(crate) fn input_paths(&self) -> Result<Vec<std::path::PathBuf>> {
        let output = self
            .control
            .output(Command::new("git").current_dir(&self.root).args([
                "ls-files",
                "--cached",
                "--others",
                "--exclude-standard",
                "-z",
            ]))?;
        let mut paths = if output.status.success() {
            ensure!(
                output.stdout.len() < MAX_CAPTURE_BYTES,
                "workspace path list exceeds capture budget"
            );
            output
                .stdout
                .split(|b| *b == 0)
                .filter(|p| !p.is_empty())
                .map(|p| Ok(self.root.join(std::str::from_utf8(p)?)))
                .collect::<Result<Vec<_>>>()?
        } else {
            walkdir::WalkDir::new(&self.root)
                .follow_links(false)
                .into_iter()
                .filter_entry(|entry| {
                    entry.depth() == 0
                        || !(excluded_component(entry.file_name().to_string_lossy().as_ref())
                            || entry.depth() == 1 && entry.file_name() == "target")
                })
                .filter_map(|entry| match entry {
                    Ok(e) if e.file_type().is_file() || e.file_type().is_symlink() => {
                        Some(Ok(e.path().to_path_buf()))
                    }
                    Ok(_) => None,
                    Err(e) => Some(Err(e.into())),
                })
                .collect::<Result<Vec<_>>>()?
        };
        paths.retain(|p| {
            if p.starts_with(self.root.join("target")) {
                return false;
            }
            !p.strip_prefix(&self.root)
                .unwrap_or(p)
                .components()
                .any(|c| excluded_component(c.as_os_str().to_string_lossy().as_ref()))
        });
        paths.sort();
        paths.dedup();
        ensure!(
            paths.len() <= 100_000,
            "workspace exceeds 100000 visible input files"
        );
        Ok(paths)
    }

    pub(crate) fn workspace_fingerprint(&self) -> Result<String> {
        let mut hasher = blake3::Hasher::new();
        for path in self.input_paths()? {
            self.control.check()?;
            let relative = path
                .strip_prefix(&self.root)?
                .as_os_str()
                .as_encoded_bytes();
            hasher.update(&(relative.len() as u64).to_le_bytes());
            hasher.update(relative);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    hasher.update(b"deleted");
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if metadata.file_type().is_symlink() {
                hasher.update(b"symlink");
                hasher.update(fs::read_link(&path)?.as_os_str().as_encoded_bytes());
                continue;
            }
            ensure!(
                metadata.is_file(),
                "non-file workspace input {}",
                path.display()
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hasher.update(&(metadata.permissions().mode() & 0o111).to_le_bytes());
            }
            hasher.update(&metadata.len().to_le_bytes());
            let mut file = fs::File::open(&path)?;
            let mut buffer = [0u8; 64 * 1024];
            loop {
                self.control.check()?;
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
        }
        Ok(format!("b3:{}", hasher.finalize().to_hex()))
    }

    pub(crate) fn verification_plan(&self, mut request: VerificationPlanRequest) -> Result<Value> {
        let contract = self.project_contract()?;
        if request.deny_warnings.is_none() {
            request.deny_warnings = Some(
                contract["evidence"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|doc| {
                        let content = doc["content"].as_str().unwrap_or("");
                        content.contains("-D warnings") || content.contains("--deny warnings")
                    }),
            );
        }
        validate_profile(&request.profile, &contract)?;
        ensure!(
            !request.checks.is_empty() && request.checks.len() <= 20,
            "provide 1..=20 checks"
        );
        if let Some(filter) = &request.test_filter {
            ensure!(
                !filter.starts_with('-') && filter.len() <= 200 && !filter.contains('\0'),
                "invalid test filter"
            );
        }
        let commands = request
            .checks
            .iter()
            .map(|kind| command(kind, &request))
            .collect::<Vec<_>>();
        let tool_versions = self.verification_tool_versions(&request.profile)?;
        let plan = VerificationPlan {id:format!("checks_{:032x}",random::<u128>()),head:contract["head"].as_str().map(str::to_owned),
            workspace_digest:contract["workspace_digest"].as_str().context("missing digest")?.into(),environment_digest:environment_digest(),
            tool_versions,profile:request.profile,commands,test_filter:request.test_filter,created_at:Utc::now().timestamp(),
            scope:"Git-visible workspace files excluding generated targets and Crusty state; ignored inputs, external services and other feature/target/toolchain profiles require separate evidence".into()};
        self.verification_db()?.execute(
            "INSERT INTO verification_plans(id,payload) VALUES (?1,?2)",
            params![plan.id, serde_json::to_string(&plan)?],
        )?;
        Ok(serde_json::to_value(plan)?)
    }

    pub(crate) fn verification_run(&self, id: &str) -> Result<Value> {
        let db = self.verification_db()?;
        let raw: String = db
            .query_row(
                "SELECT payload FROM verification_plans WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .context("unknown verification plan")?;
        let plan: VerificationPlan = serde_json::from_str(&raw)?;
        ensure!(
            self.verification_tool_versions(&plan.profile)? == plan.tool_versions
                && self.workspace_fingerprint()? == plan.workspace_digest
                && environment_digest() == plan.environment_digest
                && self.git_optional(&["rev-parse", "--verify", "HEAD"])? == plan.head,
            "verification plan is stale; create a new plan for current source/environment"
        );
        let run_id = format!("checkrun_{:032x}", random::<u128>());
        let mut checks = Vec::new();
        let mut passed = true;
        for check in &plan.commands {
            self.control.check()?;
            let start = Instant::now();
            let output = self.control.output(
                Command::new(&check.program)
                    .current_dir(&self.root)
                    .args(&check.args),
            )?;
            let complete =
                output.stdout.len() < MAX_CAPTURE_BYTES && output.stderr.len() < MAX_CAPTURE_BYTES;
            let success = output.status.success() && complete;
            passed &= success;
            let diagnostics = compiler_diagnostics(&output.stdout);
            let artifact = self.state.join(format!("{run_id}-{}.log", checks.len()));
            let mut log = output.stdout.clone();
            log.extend_from_slice(b"\n--- stderr ---\n");
            log.extend_from_slice(&output.stderr);
            fs::write(&artifact, &log)?;
            checks.push(json!({"kind":check.kind,"program":check.program,"args":check.args,"passed":success,
                "exit_code":output.status.code(),"complete":complete,"elapsed_ms":start.elapsed().as_millis(),
                "diagnostics":diagnostics,"output_path":artifact,"output_tail":tail(&log,6000)}));
            if !success {
                break;
            }
        }
        let unchanged = self.verification_tool_versions(&plan.profile)? == plan.tool_versions
            && self.workspace_fingerprint()? == plan.workspace_digest
            && environment_digest() == plan.environment_digest
            && self.git_optional(&["rev-parse", "--verify", "HEAD"])? == plan.head;
        let covered = checks.len() == plan.commands.len();
        let delivery_checks = [
            CheckKind::Format,
            CheckKind::Check,
            CheckKind::Test,
            CheckKind::Clippy,
        ]
        .iter()
        .all(|kind| plan.commands.iter().any(|command| &command.kind == kind));
        let report = json!({"id":run_id,"plan_id":plan.id,"head":plan.head,"workspace_digest":plan.workspace_digest,
            "environment_digest":plan.environment_digest,"tool_versions":plan.tool_versions,"profile":plan.profile,"scope":plan.scope,"created_at":Utc::now().timestamp(),
            "checks":checks,"passed":passed && unchanged && covered,"source_unchanged":unchanged,"complete":covered,
            "delivery_eligible":passed && unchanged && covered && delivery_checks && plan.profile.packages.is_empty() && plan.test_filter.is_none(),
            "coverage":"Only the named configuration and requested checks; supported CI matrix coverage must be assessed separately."});
        db.execute(
            "INSERT INTO verification_runs(id,plan_id,payload) VALUES (?1,?2,?3)",
            params![run_id, plan.id, report.to_string()],
        )?;
        Ok(report)
    }

    pub(crate) fn verification_get(&self, id: &str) -> Result<Value> {
        let db = self.verification_db()?;
        let payload: Option<String> = db
            .query_row(
                "SELECT payload FROM verification_runs WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(payload) = payload {
            return Ok(serde_json::from_str(&payload)?);
        }
        let payload: String = db
            .query_row(
                "SELECT payload FROM verification_plans WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .context("unknown verification plan or run")?;
        Ok(serde_json::from_str(&payload)?)
    }

    pub(crate) fn require_delivery_verification(
        &self,
        run_id: &str,
        expected_head: &str,
    ) -> Result<Value> {
        let report = self.verification_get(run_id)?;
        ensure!(
            report["delivery_eligible"] == true,
            "verification does not cover required delivery checks"
        );
        ensure!(
            report["head"] == expected_head && self.resolve_commit("HEAD")? == expected_head,
            "verification and current HEAD differ from delivered revision"
        );
        ensure!(
            report["workspace_digest"] == self.workspace_fingerprint()?
                && report["environment_digest"] == environment_digest(),
            "verification source or environment changed; rerun checks"
        );
        let profile: BuildProfile = serde_json::from_value(report["profile"].clone())?;
        ensure!(
            report["tool_versions"] == self.verification_tool_versions(&profile)?,
            "verification toolchain changed; rerun checks"
        );
        ensure!(
            self.changed_paths()?.is_empty(),
            "delivery requires a clean source worktree"
        );
        Ok(report)
    }
}

fn excluded_component(name: &str) -> bool {
    matches!(name, ".git" | ".rust-repo-intelligence")
}

fn environment_digest() -> String {
    let env = std::env::vars_os()
        .filter(|(key, _)| {
            let key = key.to_string_lossy();
            key.starts_with("CARGO_")
                || key.starts_with("RUST")
                || key.starts_with("PKG_CONFIG")
                || matches!(
                    key.as_ref(),
                    "CC" | "CXX" | "CFLAGS" | "CXXFLAGS" | "LDFLAGS"
                )
        })
        .collect::<BTreeMap<_, _>>();
    let mut hash = blake3::Hasher::new();
    for (key, value) in env {
        for bytes in [key.as_encoded_bytes(), value.as_encoded_bytes()] {
            hash.update(&(bytes.len() as u64).to_le_bytes());
            hash.update(bytes);
        }
    }
    format!("b3:{}", hash.finalize().to_hex())
}

fn validate_profile(profile: &BuildProfile, contract: &Value) -> Result<()> {
    ensure!(
        profile.packages.len() <= 100 && profile.features.len() <= 200,
        "profile exceeds selection budget"
    );
    ensure!(
        !profile.all_features || (profile.features.is_empty() && !profile.no_default_features),
        "all_features cannot combine with explicit features or no_default_features"
    );
    let members = contract["members"].as_array().context("missing members")?;
    for package in &profile.packages {
        ensure!(
            members.iter().any(|p| p["name"] == *package),
            "unknown workspace package `{package}`"
        );
    }
    for feature in &profile.features {
        ensure!(
            !feature.starts_with('-')
                && !feature.contains(',')
                && !feature.contains(char::is_whitespace),
            "invalid feature `{feature}`"
        );
        let (package, name) = feature
            .split_once('/')
            .map_or((None, feature.as_str()), |(p, f)| (Some(p), f));
        ensure!(
            members
                .iter()
                .filter(|p| package.is_none_or(|name| p["name"] == name))
                .any(|p| p["features"].get(name).is_some()),
            "unknown feature `{feature}`"
        );
    }
    for (name, value) in [
        ("target", &profile.target),
        ("toolchain", &profile.toolchain),
    ] {
        if let Some(value) = value {
            ensure!(
                !value.is_empty()
                    && value.len() <= 200
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
                "invalid {name}"
            );
        }
    }
    Ok(())
}

fn command(kind: &CheckKind, request: &VerificationPlanRequest) -> CheckCommand {
    let mut args = Vec::new();
    if let Some(toolchain) = &request.profile.toolchain {
        args.push(format!("+{toolchain}"));
    }
    let subcommand = match kind {
        CheckKind::Format => "fmt",
        CheckKind::Check => "check",
        CheckKind::Test | CheckKind::DocTests => "test",
        CheckKind::Clippy => "clippy",
        CheckKind::Documentation => "doc",
        CheckKind::Release => "build",
        CheckKind::Bench => "bench",
        CheckKind::Miri => "miri",
    };
    args.push(subcommand.into());
    if *kind == CheckKind::Miri {
        args.push("test".into());
    }
    if *kind == CheckKind::Format {
        args.extend(["--all".into(), "--".into(), "--check".into()]);
        return CheckCommand {
            kind: kind.clone(),
            program: "cargo".into(),
            args,
        };
    }
    args.push("--locked".into());
    if request.offline {
        args.push("--offline".into());
    }
    if request.profile.packages.is_empty() {
        args.push("--workspace".into());
    } else {
        for package in &request.profile.packages {
            args.extend(["-p".into(), package.clone()]);
        }
    }
    if matches!(kind, CheckKind::Check | CheckKind::Clippy) {
        args.push("--all-targets".into());
    }
    if *kind == CheckKind::DocTests {
        args.push("--doc".into());
    }
    if *kind == CheckKind::Documentation {
        args.push("--no-deps".into());
    }
    if *kind == CheckKind::Release {
        args.push("--release".into());
    }
    if request.profile.no_default_features {
        args.push("--no-default-features".into());
    }
    if request.profile.all_features {
        args.push("--all-features".into());
    }
    if !request.profile.features.is_empty() {
        args.extend(["--features".into(), request.profile.features.join(",")]);
    }
    if let Some(target) = &request.profile.target {
        args.extend(["--target".into(), target.clone()]);
    }
    if matches!(
        kind,
        CheckKind::Check
            | CheckKind::Clippy
            | CheckKind::Test
            | CheckKind::Release
            | CheckKind::Documentation
    ) {
        args.push("--message-format=json".into());
    }
    if *kind == CheckKind::Clippy && request.deny_warnings == Some(true) {
        args.extend(["--".into(), "-D".into(), "warnings".into()]);
    }
    if matches!(
        kind,
        CheckKind::Test | CheckKind::DocTests | CheckKind::Miri
    ) && let Some(filter) = &request.test_filter
    {
        args.push(filter.clone());
    }
    CheckCommand {
        kind: kind.clone(),
        program: "cargo".into(),
        args,
    }
}

fn compiler_diagnostics(bytes: &[u8]) -> Value {
    let mut diagnostics = Vec::new();
    let mut omitted = 0;
    let mut retained_bytes = 0;
    for line in bytes.split(|b| *b == b'\n') {
        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-message" {
            continue;
        }
        // Retain the original compiler message, including children, all spans,
        // suggested replacements/applicability, rendered explanation and code.
        if retained_bytes + line.len() > 160_000 {
            omitted += 1;
            continue;
        }
        retained_bytes += line.len();
        diagnostics.push(message);
    }
    json!({"messages":diagnostics,"omitted":omitted,"complete":omitted==0})
}

fn tail(bytes: &[u8], max: usize) -> String {
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(max)..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_and_doctest_profiles_do_not_silently_drop_documentation_tests() {
        let request = VerificationPlanRequest {
            profile: BuildProfile::default(),
            checks: default_checks(),
            offline: true,
            test_filter: None,
            deny_warnings: None,
        };
        let test = command(&CheckKind::Test, &request);
        assert!(!test.args.contains(&"--all-targets".into()));
        assert!(test.args.contains(&"--workspace".into()));
        assert!(
            command(&CheckKind::DocTests, &request)
                .args
                .contains(&"--doc".into())
        );
        assert!(
            command(&CheckKind::Check, &request)
                .args
                .contains(&"--all-targets".into())
        );
    }
    #[test]
    fn compiler_children_secondary_spans_and_suggestions_survive() {
        let raw = json!({"reason":"compiler-message","message":{"message":"error","children":[{"message":"help","spans":[{"is_primary":false,"suggested_replacement":"fixed","suggestion_applicability":"MachineApplicable"}]}],"spans":[{"is_primary":true},{"is_primary":false}]}});
        let result = compiler_diagnostics(raw.to_string().as_bytes());
        assert_eq!(result["messages"][0], raw);
        assert_eq!(result["complete"], true);
    }
    #[test]
    fn mutually_exclusive_feature_sets_are_explicit_not_power_sets() {
        let contract = json!({"members":[{"name":"one","features":{"left":[],"right":[]}}]});
        let profile = BuildProfile {
            features: vec!["one/left".into()],
            ..Default::default()
        };
        assert!(validate_profile(&profile, &contract).is_ok());
        assert!(
            validate_profile(
                &BuildProfile {
                    features: vec!["missing".into()],
                    ..Default::default()
                },
                &contract
            )
            .is_err()
        );
        assert!(
            validate_profile(
                &BuildProfile {
                    all_features: true,
                    features: vec!["left".into()],
                    ..Default::default()
                },
                &contract
            )
            .is_err()
        );
    }

    #[test]
    fn real_virtual_workspace_profiles_preserve_doctests_and_reject_stale_inputs() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers=['one','two']\nresolver='3'\n",
        )
        .unwrap();
        for package in ["one", "two"] {
            fs::create_dir_all(temp.path().join(package).join("src")).unwrap();
            fs::write(temp.path().join(package).join("Cargo.toml"),format!("[package]\nname='{package}'\nversion='0.1.0'\nedition='2024'\nrust-version='1.85'\n[features]\nleft=[]\nright=[]\n")).unwrap();
            fs::write(temp.path().join(package).join("src/lib.rs"),format!("#[cfg(all(feature = \"left\", feature = \"right\"))]\ncompile_error!(\"mutually exclusive\");\n/// ```\n/// assert_eq!({package}::answer(), 42);\n/// ```\npub fn answer() -> u32 {{\n    42\n}}\n")).unwrap();
        }
        let output = Command::new("cargo")
            .current_dir(temp.path())
            .args(["generate-lockfile", "--offline"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let coord =
            Coordinator::open(temp.path(), crate::execution::ExecutionControl::default()).unwrap();
        let contract = coord.project_contract().unwrap();
        assert_eq!(contract["members"].as_array().unwrap().len(), 2);
        assert_eq!(contract["members"][0]["rust_version"], "1.85");
        let request = || VerificationPlanRequest {
            profile: BuildProfile {
                features: vec!["one/left".into(), "two/right".into()],
                ..Default::default()
            },
            checks: default_checks(),
            offline: true,
            test_filter: None,
            deny_warnings: None,
        };
        let stale = coord.verification_plan(request()).unwrap();
        let path = temp.path().join("one/src/lib.rs");
        let original = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("{original}\n// later edit\n")).unwrap();
        assert!(
            coord
                .verification_run(stale["id"].as_str().unwrap())
                .is_err()
        );
        fs::write(&path, &original).unwrap();
        let plan = coord.verification_plan(request()).unwrap();
        assert!(
            !plan["commands"][3]["args"]
                .as_array()
                .unwrap()
                .contains(&json!("-D"))
        );
        let result = coord
            .verification_run(plan["id"].as_str().unwrap())
            .unwrap();
        assert_eq!(result["passed"], true, "{result}");
        let clippy = coord
            .verification_plan(VerificationPlanRequest {
                profile: BuildProfile {
                    features: vec!["one/left".into()],
                    ..Default::default()
                },
                checks: vec![CheckKind::Clippy],
                offline: true,
                test_filter: None,
                deny_warnings: Some(true),
            })
            .unwrap();
        assert_eq!(
            coord
                .verification_run(clippy["id"].as_str().unwrap())
                .unwrap()["passed"],
            true
        );
        fs::write(
            &path,
            original.replace(
                "assert_eq!(one::answer(), 42)",
                "assert_eq!(one::answer(), 0)",
            ),
        )
        .unwrap();
        let plan = coord
            .verification_plan(VerificationPlanRequest {
                profile: BuildProfile::default(),
                checks: vec![CheckKind::DocTests],
                offline: true,
                test_filter: None,
                deny_warnings: None,
            })
            .unwrap();
        let result = coord
            .verification_run(plan["id"].as_str().unwrap())
            .unwrap();
        assert_eq!(result["passed"], false);
        assert_eq!(result["delivery_eligible"], false);
    }
}
