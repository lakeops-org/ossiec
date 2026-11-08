# Reference: CLI and API

## CLI

```text
ossiec validate     <model.yaml>
ossiec describe     <model.yaml>
ossiec compile      <model.yaml> <request.json | -> [--dialect <name>] [--prefer <OSSIE_DIALECT>] [--compact] [--json]
ossiec compile-sql  <model.yaml> <query.sql | ->    [--dialect <name>] [--prefer <OSSIE_DIALECT>] [--include-unmatched] [--compact] [--json]
ossiec persist-view <model.yaml> <view.sql> [--out <model.yaml>]
```

| Command | Does |
|---|---|
| `validate` | Load and check a model; prints its datasets, relationships and metrics, and how many metrics can't be queried yet |
| `describe` | The model's metrics, dimensions and `ai_context`, as JSON |
| `compile` | A [Layer 3](layer3.md) request (JSON) to SQL. `-` reads it from stdin |
| `compile-sql` | A [Layer 2](layer2.md) query to SQL. A script may define `CREATE VIEW` relations first and end with the `SELECT` to compile. Alias: `compile-l2` |
| `persist-view` | Write a `CREATE VIEW`'s `TO MEASURE` items into the model as metrics, and print the updated YAML |

| Option | Meaning |
|---|---|
| `--dialect <name>` | Target engine: `duckdb`, `postgres`, `mysql`, `trino`, `athena`, `starrocks`, `clickhouse`, `snowflake`, `bigquery`, `databricks`, `redshift`, and others polyglot renders. Default: the model's source dialect |
| `--prefer <name>` | Use this Ossie dialect's expressions (`SNOWFLAKE`, `TRINO`, …) where a field or metric declares one |
| `--include-unmatched` | Layer 2: also return the one-side rows no fact reaches |
| `--compact` | One-line SQL |
| `--json` | The full result (SQL, dialects, warnings, hash) as JSON |
| `--out <path>` | `persist-view`: write the YAML here instead of stdout. The model is re-serialized, so comments and unknown keys aren't kept |

Warnings go to stderr, the SQL to stdout.

## Rust

```rust
let graph = ossiec::from_yaml(&yaml)?;
let out = ossiec::compile(&graph, &request, &opts)?;    // Layer 3
let out = ossiec::compile_sql(&graph, sql, &opts)?;     // Layer 2
```

### `Options`

| Field | Meaning |
|---|---|
| `target: Option<DialectType>` | Dialect to emit. `None` emits the statement's own source dialect, for a downstream translator |
| `prefer_dialect: Option<String>` | Prefer this Ossie dialect's expressions where a field or metric declares one |
| `pretty: bool` | Pretty-printed SQL (the default) |
| `include_unmatched: bool` | Layer 2: add one-side rows no fact reaches. Layer 3 ignores it, and a request that asks for it is refused |

### `Compiled`

| Field | Meaning |
|---|---|
| `sql` | The statement |
| `dialect` | The dialect `sql` is written in |
| `source_dialect` | The dialect the model's expressions were read in: `generic`, unless a vendor-only expression was used |
| `model`, `model_version` | Which model it was compiled against |
| `request_hash` | A stable hash of the model's name and version, the request (or the Layer 2 SQL) and the options: the same inputs give the same SQL. It doesn't cover the model's contents |
| `warnings` | For example `FAN_TRAP_RISK` when a Layer 2 measure is de-duplicated, or a bare name accepted in a model-scoped metric |

### `Error`

| Variant | When |
|---|---|
| `Load` | The document isn't an Ossie model ossiec can read |
| `InvalidModel` | The model breaks Ossie rules; every problem is listed |
| `Request` | The request or query doesn't fit the model: an unknown name, a bad filter, … |
| `Query { code, message }` | A Layer 3 request the spec rejects, with its code (`E_NO_PATH`, …); `error.code()` gives it |
| `Unsupported` | Valid Ossie that this version can't compile yet; see [Known gaps](roadmap.md) |
| `Internal` | A bug in ossiec |

The full API is on docs.rs once ossiec is published to crates.io; until then, `cargo doc --open`.
