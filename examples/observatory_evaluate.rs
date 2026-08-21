use anyhow::{Context, Result, ensure};
use rust_repo_intelligence::observatory::{ContextRequest, Observatory, SearchRequest};
use serde_json::json;
use std::{env, path::PathBuf, time::Instant};

#[tokio::main]
async fn main() -> Result<()> {
    let root = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(env::current_dir()?);
    let observatory = Observatory::open(root).context("opening observatory")?;

    observatory.search(exact_request()).await?;
    observatory.context(context_request()).await?;

    let exact = sample(60, || observatory.search(exact_request())).await?;
    let context = sample(30, || observatory.context(context_request())).await?;
    let exact_p95 = percentile(&exact, 95);
    let context_p95 = percentile(&context, 95);

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "live_exact_ms":{"p50":percentile(&exact,50),"p95":exact_p95,"slo_p95":150.0},
            "warm_intelligence_ms":{"p50":percentile(&context,50),"p95":context_p95,"slo_p95":750.0},
            "slow_operations":"Refresh, checks, and research return task ids instead of occupying the MCP request path."
        }))?
    );
    ensure!(
        exact_p95 < 150.0,
        "live exact p95 {exact_p95:.2}ms exceeded 150ms SLO"
    );
    ensure!(
        context_p95 < 750.0,
        "warm intelligence p95 {context_p95:.2}ms exceeded 750ms SLO"
    );
    Ok(())
}

fn exact_request() -> SearchRequest {
    SearchRequest {
        query: "Service".into(),
        mode: "exact".into(),
        limit: 20,
        include_source: true,
    }
}

fn context_request() -> ContextRequest {
    ContextRequest {
        query: "index refresh work".into(),
        budget: 1_500,
        limit: 16,
    }
}

async fn sample<F, Fut>(count: usize, mut operation: F) -> Result<Vec<f64>>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value>>,
{
    let mut durations = Vec::with_capacity(count);
    for _ in 0..count {
        let started = Instant::now();
        operation().await?;
        durations.push(started.elapsed().as_secs_f64() * 1_000.0);
    }
    durations.sort_by(f64::total_cmp);
    Ok(durations)
}

fn percentile(values: &[f64], percentile: usize) -> f64 {
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values[index]
}
