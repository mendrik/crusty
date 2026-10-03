//! Relevance rules for topic-driven retrieval, consultation above all.
//!
//! A consultation topic is a whole user intent. Turning every word into an
//! OR-ed prefix term let stopwords and incidental vocabulary match unrelated
//! work items, documents, and decisions, and nothing ranked or thresholded the
//! result. These helpers decide which query terms carry meaning, whether a
//! candidate matched enough of them, and how repository paths relate.
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Common English function words. Sorted for binary search; a test keeps it so.
const STOPWORDS: &[&str] = &[
    "a", "about", "after", "again", "also", "am", "an", "and", "any", "are", "as", "at", "be",
    "because", "been", "before", "being", "both", "but", "by", "can", "could", "did", "do", "does",
    "doing", "during", "each", "for", "from", "had", "has", "have", "having", "he", "her", "here",
    "him", "his", "how", "i", "if", "in", "into", "is", "it", "its", "itself", "just", "let",
    "may", "me", "might", "more", "most", "must", "my", "no", "nor", "not", "of", "off", "on",
    "once", "only", "or", "other", "our", "ours", "out", "over", "please", "she", "should", "so",
    "some", "such", "than", "that", "the", "their", "them", "then", "there", "these", "they",
    "this", "those", "through", "to", "too", "under", "until", "up", "us", "very", "was", "we",
    "were", "what", "when", "where", "which", "while", "who", "whom", "why", "will", "with",
    "would", "you", "your",
];

/// Topic words that make CI and workflow configuration relevant.
const CI_TOPIC_WORDS: &[&str] = &[
    "actions",
    "ci",
    "config",
    "configuration",
    "configure",
    "continuous",
    "deploy",
    "deployment",
    "github",
    "gitlab",
    "jenkins",
    "pipeline",
    "pipelines",
    "release",
    "workflow",
    "workflows",
    "yaml",
    "yml",
];

/// Extensions that make a dot-separated word a file path rather than, say, a
/// tool name such as `repo.consult`.
const FILE_EXTENSIONS: &[&str] = &[
    "c", "cfg", "css", "h", "html", "ini", "js", "json", "lock", "md", "proto", "py", "rs", "sh",
    "sql", "toml", "ts", "txt", "ui", "xml", "yaml", "yml",
];

/// File stems too generic to say what a path-scoped work item is about.
const GENERIC_STEMS: &[&str] = &[
    "build", "cargo", "index", "lib", "main", "mod", "readme", "src", "test", "tests",
];

/// Work statuses that no longer describe open intent. Unknown legacy statuses
/// stay visible: hiding an unrecognised status would make the item vanish.
const CLOSED_WORK_STATUSES: &[&str] = &[
    "archived",
    "canceled",
    "cancelled",
    "closed",
    "complete",
    "completed",
    "dismissed",
    "done",
    "duplicate",
    "obsolete",
    "rejected",
    "resolved",
    "retired",
    "superseded",
    "wontfix",
];

/// A full-text hit whose BM25 rank is weaker than this fraction of the best
/// relevant hit is noise from an incidental term, not a topical match.
pub(crate) const RELATIVE_BM25_CUTOFF: f64 = 0.3;

/// Drops stopwords and one-character terms. When nothing survives, the
/// original terms are kept so a query such as `a` or `the` still searches.
pub(crate) fn meaningful_terms(values: Vec<String>) -> Vec<String> {
    let kept = values
        .iter()
        .filter(|term| {
            term.chars().count() > 1
                && STOPWORDS
                    .binary_search(&term.to_lowercase().as_str())
                    .is_err()
        })
        .cloned()
        .collect::<Vec<_>>();
    if kept.is_empty() { values } else { kept }
}

/// Matches a candidate text against meaningful query terms the way the FTS
/// index does: lowercase, `_` inside tokens, and prefix matching per token.
pub(crate) struct TermMatcher {
    terms: Vec<String>,
    strong: Vec<bool>,
    verifiable: bool,
}

impl TermMatcher {
    pub(crate) fn new(query: &[String]) -> Self {
        let mut seen = BTreeSet::new();
        let mut terms = Vec::new();
        let mut strong = Vec::new();
        for term in query.iter().filter(|term| !term.is_empty()) {
            let lower = term.to_lowercase();
            if seen.insert(lower.clone()) {
                strong.push(is_identifier_like(term));
                terms.push(lower);
            }
        }
        // FTS folds diacritics; this check does not, so a non-ASCII query
        // keeps only the rank cutoff instead of a gate it cannot evaluate.
        let verifiable = terms.iter().all(|term| term.is_ascii());
        Self {
            terms,
            strong,
            verifiable,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// Distinct terms a long topic must match. A three-word query may match
    /// on one term; a whole paragraph must agree on more than one word.
    fn required(&self) -> usize {
        match self.terms.len() {
            0..=3 => 1,
            4..=10 => 2,
            _ => 3,
        }
    }

    /// Indices of the query terms that prefix-match a token of `text`.
    pub(crate) fn matched(&self, text: &str) -> BTreeSet<usize> {
        let tokens = tokens(text);
        self.terms
            .iter()
            .enumerate()
            .filter(|(_, term)| {
                tokens
                    .range::<String, _>((*term).clone()..)
                    .next()
                    .is_some_and(|token| token.starts_with(term.as_str()))
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// Whether the matched term set is meaningful: enough distinct terms, or
    /// one identifier-like term such as `audit_target` or `SessionLease`.
    pub(crate) fn sufficient(&self, matched: &BTreeSet<usize>) -> bool {
        if matched.is_empty() {
            return false;
        }
        !self.verifiable
            || matched.len() >= self.required()
            || matched.iter().any(|index| self.strong[*index])
    }

    pub(crate) fn accepts(&self, text: &str) -> bool {
        self.sufficient(&self.matched(text))
    }
}

fn is_identifier_like(term: &str) -> bool {
    term.chars().count() >= 4
        && (term.contains('_')
            || term.chars().skip(1).any(char::is_uppercase)
            || term.chars().any(|character| character.is_ascii_digit()))
}

fn tokens(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !(character.is_alphanumeric() || character == '_'))
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Keeps full-text candidates `(hit, bm25, text)` that match enough distinct
/// terms and rank within [`RELATIVE_BM25_CUTOFF`] of the best such hit.
/// Candidates arrive best first, as FTS5 orders by ascending (negative) BM25.
pub(crate) fn retain_relevant(
    candidates: Vec<(Value, f64, String)>,
    matcher: &TermMatcher,
    limit: usize,
) -> Vec<Value> {
    let relevant = candidates
        .into_iter()
        .filter(|(_, _, text)| matcher.accepts(text))
        .collect::<Vec<_>>();
    let best = relevant
        .iter()
        .map(|(_, score, _)| *score)
        .fold(f64::INFINITY, f64::min);
    relevant
        .into_iter()
        .filter(|(_, score, _)| best >= 0.0 || *score <= best * RELATIVE_BM25_CUTOFF)
        .take(limit)
        .map(|(hit, _, _)| hit)
        .collect()
}

/// A path-looking word normalised for ancestry checks: it contains `/` or ends
/// in a known file extension. URLs, tool names, and version numbers are not paths.
pub(crate) fn path_like(word: &str) -> Option<String> {
    let word = word.trim_matches(|character: char| {
        matches!(
            character,
            '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';'
        )
    });
    if word.contains("://") {
        return None;
    }
    // `src/lib.rs:42` and `src/lib.rs:42:7` name the same file.
    let word = word.split(':').next().unwrap_or("");
    let word = word.trim_end_matches(['.', '/']);
    let word = word.strip_prefix("./").unwrap_or(word);
    if word.is_empty() || !word.chars().any(char::is_alphanumeric) {
        return None;
    }
    if word.contains('/') {
        return Some(word.to_owned());
    }
    let (stem, extension) = word.rsplit_once('.')?;
    (!stem.is_empty()
        && FILE_EXTENSIONS
            .binary_search(&extension.to_ascii_lowercase().as_str())
            .is_ok())
    .then(|| word.to_owned())
}

/// Every path-looking word in `topic`, deduplicated in order of appearance.
pub(crate) fn topic_paths(topic: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    topic
        .split_whitespace()
        .filter_map(path_like)
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

/// `ancestor` is `path` itself or one of its directories. The root (empty)
/// scope contains every path.
pub(crate) fn path_contains(ancestor: &str, path: &str) -> bool {
    ancestor.is_empty()
        || path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Two paths are related when one contains the other.
pub(crate) fn paths_related(left: &str, right: &str) -> bool {
    path_contains(left, right) || path_contains(right, left)
}

/// CI and workflow configuration rather than prose policy.
pub(crate) fn is_ci_configuration(path: &str) -> bool {
    let lower = path.to_lowercase();
    let file = lower.rsplit('/').next().unwrap_or(&lower);
    let yaml = file.ends_with(".yml") || file.ends_with(".yaml");
    (yaml
        && (lower.starts_with("workflows/")
            || lower.contains("/workflows/")
            || lower.starts_with(".circleci/")
            || lower.contains("/.circleci/")
            || lower.starts_with(".buildkite/")
            || lower.starts_with(".woodpecker")
            || file.contains("pipeline")
            || file == ".gitlab-ci.yml"
            || file == ".travis.yml"))
        || file == "jenkinsfile"
}

/// Whether a topic is about CI, workflows, or configuration.
pub(crate) fn topic_concerns_ci(topic: &str) -> bool {
    tokens(topic)
        .iter()
        .any(|token| CI_TOPIC_WORDS.binary_search(&token.as_str()).is_ok())
        || topic_paths(topic)
            .iter()
            .any(|path| is_ci_configuration(path))
}

pub(crate) fn work_status_is_open(status: &str) -> bool {
    let status = status
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' ', '\''], "");
    CLOSED_WORK_STATUSES
        .binary_search(&status.as_str())
        .is_err()
}

/// A topic prepared once for scoring many work items.
pub(crate) struct WorkQuery {
    phrase: String,
    matcher: TermMatcher,
    paths: Vec<String>,
}

impl WorkQuery {
    pub(crate) fn new(query: &str) -> Self {
        let phrase = query.trim().to_lowercase();
        let terms = if phrase.is_empty() {
            Vec::new()
        } else {
            crate::terms(query)
        };
        Self {
            matcher: TermMatcher::new(&terms),
            paths: topic_paths(query),
            phrase,
        }
    }

    /// Relevance of one work row, or `None` when it does not match. Higher is
    /// better; an empty query matches everything with score zero.
    ///
    /// A work item scoped to a path is relevant to a topic that names paths
    /// only when one contains the other, so an item for another crate cannot
    /// ride in on shared vocabulary. Path components are not matched as words
    /// except for a distinctive file stem.
    pub(crate) fn score(&self, item: &Value) -> Option<f64> {
        if self.phrase.is_empty() {
            return Some(0.0);
        }
        let id = item["id"].as_str().unwrap_or("").to_lowercase();
        if !id.is_empty() && self.phrase.contains(&id) {
            return Some(1_000.0);
        }
        let scope = item["scope"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        let mut scope_paths = Vec::new();
        let mut scope_words = Vec::new();
        for entry in scope {
            match path_like(entry) {
                Some(path) => scope_paths.push(path),
                None => scope_words.push(entry.to_owned()),
            }
        }
        let mut score = 0.0;
        let mut path_match = false;
        if !scope_paths.is_empty() && !self.paths.is_empty() {
            path_match = scope_paths.iter().any(|scope_path| {
                self.paths
                    .iter()
                    .any(|topic_path| paths_related(scope_path, topic_path))
            });
            if !path_match {
                return None;
            }
            score += 50.0;
        }
        for path in &scope_paths {
            let file = path.rsplit('/').next().unwrap_or(path);
            let stem = file.split('.').next().unwrap_or(file).to_lowercase();
            if !stem.is_empty() && GENERIC_STEMS.binary_search(&stem.as_str()).is_err() {
                scope_words.push(stem);
            }
        }
        let title = item["title"].as_str().unwrap_or("");
        let title_hits = self.matcher.matched(title);
        let scope_hits = self.matcher.matched(&scope_words.join(" "));
        let all = title_hits.union(&scope_hits).copied().collect();
        score += 3.0 * title_hits.len() as f64 + 2.0 * scope_hits.len() as f64;
        if title.to_lowercase().contains(&self.phrase) {
            score += 20.0;
        }
        (path_match || self.matcher.sufficient(&all)).then_some(score)
    }
}

/// The consultation view of a work item: enough to recognise it, with the
/// evidence, acceptance criteria, and verification left to `work.get`.
pub(crate) fn work_brief(item: &Value) -> Value {
    let scope = item["scope"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| entry.as_str().map(|entry| crate::trim_text(entry, 120)))
        .collect::<Vec<_>>();
    let summary = ["acceptance_criteria", "evidence"]
        .iter()
        .filter_map(|field| item[*field].as_array().and_then(|values| values.first()))
        .find_map(|first| match first {
            Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
            Value::Object(object) => ["excerpt", "title", "uri"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str))
                .map(str::to_owned),
            _ => None,
        })
        .map(|text| crate::trim_text(&text, 160));
    let mut brief = json!({
        "id": item["id"],
        "title": crate::trim_text(item["title"].as_str().unwrap_or(""), 160),
        "status": item["status"],
        "priority": item["priority"],
        "scope": scope.iter().take(4).collect::<Vec<_>>(),
        "detail": "work.get",
    });
    if scope.len() > 4 {
        brief["scope_more"] = json!(scope.len() - 4);
    }
    if let Some(summary) = summary {
        brief["summary"] = json!(summary);
    }
    if let Some(ready) = item.get("ready") {
        brief["ready"] = ready.clone();
    }
    brief
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn vocabularies_stay_sorted_for_binary_search() {
        for list in [
            STOPWORDS,
            CI_TOPIC_WORDS,
            FILE_EXTENSIONS,
            GENERIC_STEMS,
            CLOSED_WORK_STATUSES,
        ] {
            assert!(list.windows(2).all(|pair| pair[0] < pair[1]), "{list:?}");
        }
    }

    #[test]
    fn stopwords_and_single_characters_are_not_query_terms() {
        assert_eq!(
            meaningful_terms(strings(&["Please", "fix", "a", "the", "Store", "x"])),
            ["fix", "Store"]
        );
        assert_eq!(meaningful_terms(strings(&["the", "a"])), ["the", "a"]);
        assert_eq!(meaningful_terms(strings(&["日本語"])), ["日本語"]);
    }

    #[test]
    fn long_topics_need_several_distinct_matches_or_an_identifier() {
        let matcher = TermMatcher::new(&strings(&[
            "improve",
            "consult",
            "budget",
            "packing",
            "relevance",
        ]));
        assert!(!matcher.accepts("Improve the logo"));
        assert!(matcher.accepts("consultation budget"));
        let identifier = TermMatcher::new(&strings(&[
            "audit",
            "audit_target",
            "implementation",
            "path",
        ]));
        assert!(identifier.accepts("audit_target"));
        assert!(TermMatcher::new(&strings(&["日本語", "設計"])).accepts("日本語"));
    }

    #[test]
    fn relative_bm25_cutoff_drops_incidental_hits() {
        let matcher = TermMatcher::new(&strings(&["budget"]));
        let hits = retain_relevant(
            vec![
                (json!("best"), -20.0, "budget".into()),
                (json!("close"), -9.0, "budget".into()),
                (json!("incidental"), -2.0, "budget".into()),
                (json!("unmatched"), -19.0, "other".into()),
            ],
            &matcher,
            10,
        );
        assert_eq!(hits, [json!("best"), json!("close")]);
    }

    #[test]
    fn paths_are_recognised_and_related_by_ancestry() {
        assert_eq!(
            topic_paths("Fix `src/lib.rs:42`, crates/foo/ and Cargo.toml; not repo.consult or 0.3"),
            ["src/lib.rs", "crates/foo", "Cargo.toml"]
        );
        assert!(paths_related("crates/foo", "crates/foo/src/lib.rs"));
        assert!(paths_related("crates/foo/src/lib.rs", "crates/foo"));
        assert!(!paths_related("crates/foo", "crates/foobar"));
        assert!(path_contains("", "anything"));
    }

    #[test]
    fn ci_configuration_is_recognised_by_path_and_topic() {
        assert!(is_ci_configuration(".github/workflows/ci.yml"));
        assert!(!is_ci_configuration("docs/workflows/release.md"));
        assert!(topic_concerns_ci("Fix the CI workflow"));
        assert!(!topic_concerns_ci("Design a settings feature"));
    }

    #[test]
    fn work_scoring_respects_status_paths_and_vocabulary() {
        assert!(!work_status_is_open("done"));
        assert!(!work_status_is_open("Won't-Fix"));
        assert!(work_status_is_open("proposed"));
        assert!(work_status_is_open("blocked"));
        let item = |title: &str, scope: &[&str]| json!({"id":"work_1","title":title,"scope":scope});
        let query = WorkQuery::new("Improve consult relevance in crates/core/src/consult.rs");
        assert!(
            query
                .score(&item("Consult relevance", &["crates/core"]))
                .is_some()
        );
        assert!(
            query
                .score(&item("Consult relevance", &["crates/other"]))
                .is_none()
        );
        let unrelated =
            WorkQuery::new("Please improve the error handling for the settings page and its tests");
        assert!(
            unrelated
                .score(&item(
                    "Refresh the logo on the marketing page",
                    &["web/site"]
                ))
                .is_none()
        );
        assert_eq!(
            WorkQuery::new("about work_1 please").score(&item("x", &[])),
            Some(1_000.0)
        );
    }
}
