# Embedding ossiec

ossiec is a library first, built to sit inside query engines, SQL proxies and agent tool servers.

- **Compile for the engine you run on.** Resolve the target first, then call `compile` or `compile_sql` with `Options { target: Some(..), .. }`. You get engine-ready SQL with ossiec's dialect rules applied.
- **Or translate downstream.** With `target: None` you get SQL in the model's source dialect (`Compiled::source_dialect`), for your own translator. ossiec's dialect rules don't apply on that path: a translator that maps `LAG` to ClickHouse's `lag`, for example, gets `0` instead of NULL for the first row.
- **Apply your own governance.** Row filters, column masks and permissions work on the generated SQL as on any other query: it references the model's physical tables.
- **For agents.** `describe()` returns metrics, dimensions and `ai_context` as JSON. A request without metrics lists dimension values, so an agent can filter with real ones. Errors name the problem and the valid choices (`unknown metric 'revenue'; available: total_revenue, …`), so an agent can correct itself.
- **Deterministic.** The same model, request and options give byte-identical SQL and the same `request_hash`, so results can be cached by hash.
- **Safe construction.** Model and request content become AST nodes, never spliced SQL text.

## The API in brief

| Call | Does |
|---|---|
| `from_yaml(text)` | Load and validate a model, once |
| `compile(&graph, &request, &opts)` | Layer 3: a semantic request to SQL |
| `compile_sql(&graph, sql, &opts)` | Layer 2: SQL with `AGG()` to SQL |
| `define_layer2_view` / `compile_sql_with` | A.7 views in a session `Layer2Catalog` |
| `persist_layer2_view` | Write a view's `TO MEASURE` items into the model |
| `graph.describe()` | Metrics, dimensions and `ai_context`, as JSON |
| `to_yaml(graph.model())` | The model back as YAML (comments and unknown keys aren't kept) |
