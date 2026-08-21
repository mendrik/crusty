# Use hybrid semantic retrieval with SQLite embeddings

Crusty combines live exact source search, lexical retrieval, compiler-backed rust-analyzer relationships, and vector similarity instead of treating any one retrieval method as sufficient. Embeddings are stored in the rebuildable SQLite derived index initially: repository-scale measurements do not yet justify a separate vector service, and keeping vectors beside their symbol identities, content hashes, and semantic snapshots makes freshness and invalidation explicit.

## Retrieval contract

Exact identifiers, paths, diagnostics, and dirty-worktree source are resolved from live files and always outrank semantic similarity. Natural-language queries retrieve lexical and vector candidates, then expand and rerank them using definitions, references, implementations, Cargo relationships, tests, and provenance. Vector matches are discovery evidence rather than proof and cannot enter a likely change surface without qualified structural evidence.

The embedding unit is a symbol card rather than an arbitrary fixed-size source chunk. A card may contain the canonical name, signature, documentation, owning module or trait, a bounded source slice, and short summaries of associated tests and relationships. Every embedding is keyed by its card content hash and records the model identity, dimensions, indexed revision, target, feature profile, and semantic snapshot. Changed cards are recomputed incrementally; unchanged vectors are reused.

## Initial implementation

1. Define and version the symbol-card representation independently from the embedding model.
2. Persist vectors and their freshness metadata in SQLite, using bounded brute-force cosine ranking first.
3. Fuse lexical, vector, and graph rankings while preserving exact-match priority and provenance in every result.
4. Evaluate conceptual recall, forbidden hits, context usefulness, update cost, and warm latency on representative Rust queries.
5. Adopt an approximate-nearest-neighbor index or external vector store only when measured repository or multi-repository scale exceeds the SQLite implementation's latency or memory budget.

This architecture helps the attached LLM find code expressed in different vocabulary while preserving the live worktree, compiler behavior, runtime evidence, and human decisions as the authorities.
