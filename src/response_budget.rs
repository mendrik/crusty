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

const PACK_NOTE: &str = "Sections are packed in authority order: every matching item is first placed in compact form, then upgraded to full detail while the budget allows. `compacted` and `omitted` count reduced and missing items; `fetch_more` names how to read the rest.";

/// One budgeted section: items in relevance order, each as `(full, compact)`.
pub(crate) struct Section {
    name: &'static str,
    items: Vec<(Value, Value)>,
    fetch_more: &'static str,
}

impl Section {
    pub(crate) fn new(
        name: &'static str,
        items: Vec<Value>,
        compact: impl Fn(&Value) -> Value,
        fetch_more: &'static str,
    ) -> Self {
        let items = items
            .into_iter()
            .map(|item| {
                let small = compact(&item);
                (item, small)
            })
            .collect();
        Self::with_forms(name, items, fetch_more)
    }

    pub(crate) fn with_forms(
        name: &'static str,
        items: Vec<(Value, Value)>,
        fetch_more: &'static str,
    ) -> Self {
        Self {
            name,
            items,
            fetch_more,
        }
    }

    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Packs `sections` into one response of at most `tokens` estimated tokens,
/// accounting for everything else in `value` (a body, or `{freshness, result}`).
///
/// Every item first gets its compact form in section order, so a section is
/// never dropped wholesale while compact forms fit; items are then upgraded to
/// their full form in the same order. Worst-case budget metadata is reserved
/// before filling, so writing the final counts can only shrink the response.
/// When the envelope alone exceeds half the budget, `shed` steps replace
/// (`Some`) or remove (`None`) envelope keys in order, and the response says so.
pub(crate) fn pack(
    mut value: Value,
    sections: Vec<Section>,
    shed: &[(&str, Option<Value>)],
    tokens: usize,
) -> Value {
    let tokens = tokens.clamp(250, 20_000);
    let maximum = tokens.saturating_mul(4);
    let wrapped = value.get("result").is_some();
    let path = if wrapped {
        "/result/context_budget"
    } else {
        "/context_budget"
    };
    let counts = sections
        .iter()
        .map(|section| (section.name.to_owned(), json!(section.items.len())))
        .collect::<Map<_, _>>();
    let hints = sections
        .iter()
        .filter(|section| !section.is_empty())
        .map(|section| (section.name.to_owned(), json!(section.fetch_more)))
        .collect::<Map<_, _>>();
    {
        let body = body_mut(&mut value, wrapped);
        for section in &sections {
            body[section.name] = json!([]);
        }
        body["context_budget"] = json!({
            "tokens": tokens,
            "estimated_tokens": maximum,
            "serialized_bytes": maximum,
            "estimator": "serialized UTF-8 bytes / 4",
            "truncated": true,
            "omitted": counts,
            "compacted": counts,
            "fetch_more": hints,
            "note": PACK_NOTE,
        });
    }
    let mut envelope = Map::new();
    for (key, replacement) in shed {
        if value.to_string().len() <= maximum / 2 {
            break;
        }
        let body = body_mut(&mut value, wrapped);
        let Some(object) = body.as_object_mut() else {
            break;
        };
        if !object.contains_key(*key) {
            continue;
        }
        match replacement {
            Some(compact) => {
                object.insert((*key).to_owned(), compact.clone());
                envelope.insert((*key).to_owned(), json!("compact"));
            }
            None => {
                object.remove(*key);
                envelope.insert((*key).to_owned(), json!("omitted"));
            }
        }
        body["context_budget"]["envelope"] = json!(envelope);
    }
    if value.to_string().len() > maximum / 2
        && let Some(budget) = body_mut(&mut value, wrapped)["context_budget"].as_object_mut()
    {
        budget.remove("note");
    }

    // Each array element costs its serialization plus at most one comma.
    let mut used = value.to_string().len();
    let sizes = sections
        .iter()
        .map(|section| {
            section
                .items
                .iter()
                .map(|(full, compact)| {
                    let full = full.to_string().len() + 1;
                    (full, (compact.to_string().len() + 1).min(full))
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    // None: omitted; Some(false): compact; Some(true): full.
    let mut chosen = sizes
        .iter()
        .map(|sizes| vec![None; sizes.len()])
        .collect::<Vec<_>>();
    for (section, sizes) in sizes.iter().enumerate() {
        for (item, (_, compact)) in sizes.iter().enumerate() {
            if used + compact <= maximum {
                used += compact;
                chosen[section][item] = Some(false);
            }
        }
    }
    for (section, sizes) in sizes.iter().enumerate() {
        for (item, (full, compact)) in sizes.iter().enumerate() {
            if chosen[section][item] == Some(false) && used + full - compact <= maximum {
                used += full - compact;
                chosen[section][item] = Some(true);
            }
        }
    }

    let mut omitted = Map::new();
    let mut compacted = Map::new();
    let mut fetch_more = Map::new();
    let body = body_mut(&mut value, wrapped);
    for ((section, choices), sizes) in sections.into_iter().zip(chosen).zip(sizes) {
        let mut kept = Vec::new();
        let (mut missing, mut reduced) = (0usize, 0usize);
        for (((full, compact), choice), (full_size, compact_size)) in
            section.items.into_iter().zip(choices).zip(sizes)
        {
            match choice {
                Some(true) => kept.push(full),
                Some(false) if compact_size < full_size => {
                    reduced += 1;
                    kept.push(compact);
                }
                Some(false) => kept.push(full),
                None => missing += 1,
            }
        }
        body[section.name] = json!(kept);
        omitted.insert(section.name.to_owned(), json!(missing));
        if reduced > 0 {
            compacted.insert(section.name.to_owned(), json!(reduced));
        }
        if missing + reduced > 0 {
            fetch_more.insert(section.name.to_owned(), json!(section.fetch_more));
        }
    }
    let budget = &mut body["context_budget"];
    budget["truncated"] = json!(!fetch_more.is_empty() || !envelope.is_empty());
    budget["omitted"] = json!(omitted);
    let object = budget.as_object_mut().expect("context_budget is an object");
    if compacted.is_empty() {
        object.remove("compacted");
    } else {
        object.insert("compacted".into(), json!(compacted));
    }
    if fetch_more.is_empty() {
        object.remove("fetch_more");
    } else {
        object.insert("fetch_more".into(), json!(fetch_more));
    }
    measure(&mut value, path);
    value
}

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
    // Keep what an inner packer reported (compaction, fetch hints, envelope).
    let mut budget = previous.as_object().cloned().unwrap_or_default();
    for (key, item) in [
        ("tokens", json!(tokens)),
        ("estimated_tokens", json!(0)),
        ("serialized_bytes", json!(0)),
        ("truncated", json!(previous["truncated"] == true)),
        ("omitted", previous["omitted"].clone()),
        ("estimator", json!("serialized UTF-8 bytes / 4")),
    ] {
        budget.insert(key.into(), item);
    }
    let budget = Value::Object(budget);
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
    if let Some(budget) = target["context_budget"].as_object_mut() {
        budget.remove("note");
    }
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
