use anyhow::{Context, Result};
use rust_repo_intelligence::Service;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{env, fs, path::PathBuf, time::Instant};

#[derive(Deserialize)]
struct QueryCase {
    query: String,
    expected: String,
    #[serde(default)]
    forbidden: Vec<String>,
}

fn main() -> Result<()> {
    let workspace = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);
    let corpus = env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("evaluation/queries.json"));
    let cases: Vec<QueryCase> = serde_json::from_slice(
        &fs::read(&corpus).with_context(|| format!("reading {}", corpus.display()))?,
    )?;
    let mut service = Service::open(&workspace)?;

    let refresh_started = Instant::now();
    service.refresh_if_stale()?;
    let refresh_us = refresh_started.elapsed().as_micros();
    let embedding_index = service.index_status()["embedding"].clone();

    let mut samples = Vec::new();
    let mut context_samples = Vec::new();
    let mut recalled = 0usize;
    let mut missed = Vec::new();
    let mut forbidden_checks = 0usize;
    let mut false_positives = Vec::new();
    for round in 0..20 {
        for case in &cases {
            let started = Instant::now();
            let result = service.locate(&case.query, 10, false)?;
            samples.push(started.elapsed().as_micros());
            let context_started = Instant::now();
            let context = service.context_pack(&case.query, 1_500, 10)?;
            context_samples.push(context_started.elapsed().as_micros());
            if round == 0 {
                if contains_expected(&result, &case.expected)
                    || contains_expected(&context, &case.expected)
                {
                    recalled += 1;
                } else {
                    missed.push(json!({"query":case.query,"expected":case.expected}));
                }
                for forbidden in &case.forbidden {
                    forbidden_checks += 1;
                    if contains_expected(&result, forbidden)
                        || contains_expected(&context, forbidden)
                    {
                        false_positives.push(json!({"query":case.query,"forbidden":forbidden}));
                    }
                }
            }
        }
    }
    samples.sort_unstable();
    context_samples.sort_unstable();
    let output = json!({
        "workspace": workspace,
        "corpus": corpus,
        "queries": cases.len(),
        "samples": samples.len(),
        "incremental_refresh_us": refresh_us,
        "embedding_index": embedding_index,
        "warm_query_p50_us": percentile(&samples, 50),
        "warm_query_p95_us": percentile(&samples, 95),
        "context_pack_p50_us": percentile(&context_samples, 50),
        "context_pack_p95_us": percentile(&context_samples, 95),
        "recall_at_10": if cases.is_empty() { 1.0 } else { recalled as f64 / cases.len() as f64 },
        "forbidden_hit_rate_at_10": if forbidden_checks == 0 { 0.0 } else { false_positives.len() as f64 / forbidden_checks as f64 },
        "recalled": recalled,
        "missed": missed,
        "false_positives": false_positives,
    });
    println!("{}", serde_json::to_string_pretty(&output)?);
    Ok(())
}

fn contains_expected(result: &Value, expected: &str) -> bool {
    let expected = expected.to_lowercase();
    [
        "canonical_candidates",
        "implementation_candidates",
        "tests",
        "full_text_hits",
        "documentation",
        "runtime_contracts",
        "ranked_symbols",
    ]
    .into_iter()
    .filter_map(|key| result[key].as_array())
    .flatten()
    .any(|candidate| {
        ["symbol", "title", "path"]
            .into_iter()
            .filter_map(|key| candidate[key].as_str())
            .any(|value| value.to_lowercase().contains(&expected))
    })
}

fn percentile(samples: &[u128], percentile: usize) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    let index = (samples.len() - 1) * percentile / 100;
    samples[index]
}
