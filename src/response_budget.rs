//! Byte-based estimates over complete logical JSON responses, including metadata.
use serde_json::{Map, Value, json};

const PRIORITY: &[&str] = &[
    "decisions",
    "steerings",
    "approved_models",
    "live_instructions",
    "learned_quality_constraints",
    "known_work",
    "work_items",
    "governing_documents",
    "lifecycle_risks",
    "runtime_contracts",
    "validation_queue",
    "likely_change_surface",
    "primary_symbols",
    "ranked_symbols",
    "source_slices",
    "semantic_references",
    "documentation",
    "engineering_guidance",
];

pub(crate) fn bound(mut value: Value, tokens: usize) -> Value {
    let tokens = tokens.clamp(250, 20_000);
    let maximum = tokens.saturating_mul(4);
    let wrapped = value.get("result").is_some();
    let path = if wrapped {
        "/result/context_budget"
    } else {
        "/context_budget"
    };
    let original = value.clone();
    let body = if wrapped {
        &original["result"]
    } else {
        &original
    };
    let previous = &body["context_budget"];
    let budget = json!({"tokens":tokens,"estimated_tokens":0,"serialized_bytes":0,"truncated":previous["truncated"] == true,"omitted":previous["omitted"],"estimator":"serialized UTF-8 bytes / 4"});
    body_mut(&mut value, wrapped)["context_budget"] = budget.clone();
    measure(&mut value, path);
    if value.to_string().len() <= maximum {
        return value;
    }

    let mut minimal = Map::new();
    for key in [
        "context_id",
        "query",
        "topic",
        "consulted",
        "guidance_found",
        "generation",
        "guidance_version",
        "engineering_route",
    ] {
        if let Some(item) = body.get(key) {
            let item = if let Some(text) = item.as_str() {
                json!(crate::trim_text(text, 128))
            } else {
                item.clone()
            };
            minimal.insert(key.into(), item);
        }
    }
    if let Some(capture) = body.get("automatic_problem_capture") {
        minimal.insert("automatic_problem_capture".into(), json!({"captured":capture["captured"],"deduplicated":capture["deduplicated"],"source":capture["source"]}));
    }
    if tokens >= 500 && body.get("retrieval").is_some() {
        minimal.insert("retrieval".into(), body["retrieval"].clone());
        minimal.insert("retrieval_provenance".into(), json!({"embedding":{"model":body["retrieval_provenance"]["embedding"]["model"],"card_version":body["retrieval_provenance"]["embedding"]["card_version"]}}));
    }
    let mut kept = if wrapped {
        json!({"freshness":original["freshness"],"result":minimal})
    } else {
        Value::Object(minimal)
    };
    let target = body_mut(&mut kept, wrapped);
    target["context_budget"] = budget;
    target["context_budget"]["truncated"] = json!(true);
    target["context_budget"]["original_bytes"] = json!(original.to_string().len());
    if !target["context_budget"]["omitted"].is_object() {
        target["context_budget"]["omitted"] = json!({});
    }
    for key in PRIORITY {
        if let Some(items) = body.get(key) {
            if let Some(items) = items.as_array() {
                target[key] = json!([]);
                set_omitted(target, key, items.len());
            } else {
                set_omitted(target, key, 1);
            }
        }
    }
    // Large prose in the freshness note is redundant with its backend and stale fields.
    if kept.to_string().len() > maximum && wrapped {
        kept["freshness"].as_object_mut().unwrap().remove("note");
    }

    for key in PRIORITY {
        let Some(items) = body.get(key) else {
            continue;
        };
        let entries = items
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![items.clone()]);
        for mut item in entries {
            if *key == "validation_queue" {
                item.as_object_mut().unwrap().remove("snapshot");
            }
            let mut candidate = kept.clone();
            let target = body_mut(&mut candidate, wrapped);
            if items.is_array() {
                target[key].as_array_mut().unwrap().push(item);
            } else {
                target[key] = item;
            }
            let remaining = target["context_budget"]["omitted"][key]
                .as_u64()
                .unwrap_or(1);
            target["context_budget"]["omitted"][key] = json!(remaining.saturating_sub(1));
            measure(&mut candidate, path);
            if candidate.to_string().len() <= maximum {
                kept = candidate;
            }
        }
    }
    // Add compact discovery provenance after governance and evidence have priority.
    for (key, item) in [
        ("retrieval", body["retrieval"].clone()),
        (
            "retrieval_provenance",
            json!({"embedding":{"model":body["retrieval_provenance"]["embedding"]["model"],"card_version":body["retrieval_provenance"]["embedding"]["card_version"]}}),
        ),
    ] {
        if body.get(key).is_none() {
            continue;
        }
        let mut candidate = kept.clone();
        body_mut(&mut candidate, wrapped)[key] = item;
        measure(&mut candidate, path);
        if candidate.to_string().len() <= maximum {
            kept = candidate;
        }
    }
    // Tiny budgets retain the freshness contract and locators, and report omission.
    if kept.to_string().len() > maximum {
        let body = body_mut(&mut kept, wrapped);
        body.as_object_mut()
            .unwrap()
            .retain(|_, item| !item.as_array().is_some_and(Vec::is_empty));
        for key in ["query", "topic"] {
            if let Some(text) = body[key].as_str() {
                body[key] = json!(crate::trim_text(text, 32));
            }
        }
        body["context_budget"]["omitted"]
            .as_object_mut()
            .unwrap()
            .retain(|_, count| count.as_u64().unwrap_or(0) > 0);
    }
    measure(&mut kept, path);
    kept
}

fn body_mut(value: &mut Value, wrapped: bool) -> &mut Value {
    if wrapped { &mut value["result"] } else { value }
}
fn set_omitted(target: &mut Value, key: &str, added: usize) {
    let old = target["context_budget"]["omitted"][key]
        .as_u64()
        .unwrap_or(0);
    target["context_budget"]["omitted"][key] = json!(old + added as u64);
}
fn measure(value: &mut Value, path: &str) {
    // The decimal lengths of these metrics affect their own serialized size.
    for _ in 0..4 {
        let bytes = value.to_string().len();
        if let Some(budget) = value.pointer_mut(path) {
            budget["estimated_tokens"] = json!(bytes.div_ceil(4));
            budget["serialized_bytes"] = json!(bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_responses_include_escaping_metadata_and_repeated_budgeting() {
        let freshness = json!({
            "backend":"published_snapshot","confidence":0.65,"stale":true,
            "indexed":{"generation":48,"head":"a".repeat(40),"workspace_digest":"b".repeat(67),"published_at":"2026-09-30T12:00:00+00:00"},
            "live":{"head":"c".repeat(40),"dirty":true},"note":"Published guidance, no implicit refresh"});
        for tokens in [250, 251, 500, 600, 1000, 20_000] {
            let value = json!({"freshness":freshness,"result":{
                "query":"\"\n😀".repeat(6000),"generation":48,"context_id":"ctx_test",
                "decisions":[{"id":"DEC-1","title":"Keep human guidance","rationale":"large".repeat(2000)}],
                "steerings":[],"known_work":[],"work_items":[],"governing_documents":[],
                "lifecycle_risks":[],"runtime_contracts":[],"likely_change_surface":[],
                "ranked_symbols":[{"symbol":"x".repeat(2000)}],
                "source_slices":[{"source":"\"\n😀".repeat(2000)}],"documentation":[],
                "validation_queue":{"learned":[],"snapshot":{"metadata":"x".repeat(20000)}},
                "retrieval_provenance":{"metadata":"x".repeat(20000)},"context_budget":{"tokens":tokens}
            }});
            let once = bound(value, tokens);
            for response in [once.clone(), bound(once, tokens)] {
                let bytes = response.to_string().len();
                assert!(
                    bytes <= tokens * 4,
                    "{tokens} tokens: {bytes} bytes {response}"
                );
                assert_eq!(
                    response["result"]["context_budget"]["serialized_bytes"],
                    bytes
                );
                assert_eq!(
                    response["result"]["context_budget"]["estimated_tokens"],
                    bytes.div_ceil(4)
                );
                assert_eq!(response["freshness"]["stale"], true);
                assert_eq!(response["result"]["context_id"], "ctx_test");
            }
        }
    }
}
