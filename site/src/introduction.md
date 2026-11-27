# ossiec

<p class="hero"><strong>The Apache Ossie compiler.</strong> Define a metric once. Get correct SQL for any engine.</p>

[Apache Ossie](https://ossie.apache.org/) is the vendor-neutral standard for semantic models: datasets, fields, relationships and metrics, written in YAML. Ossie defines what a metric *means*. It doesn't say how to turn one into SQL.

**ossiec does that part.** Give it an Ossie model and a question, and it returns one SQL statement, in the dialect you need, that can't double-count.

```console
$ echo '{"metrics": ["total_revenue", "total_stock"], "dimensions": ["products.category"]}' \
    | ossiec compile shop.yaml - --dialect snowflake
```

## Why a compiler?

Ask a person or an LLM for "units in stock by customer region" over a star schema, and you'll usually get this:

```sql
SELECT c.region, SUM(p.stock)
FROM orders o
LEFT JOIN customers c ON o.customer_id = c.customer_id
LEFT JOIN products  p ON o.product_id  = p.product_id
GROUP BY c.region
```

It runs, it looks right, and it's wrong. Stock lives on `products`, but the join repeats each product once per order:

| region | naive SQL | ossiec | truth |
|---|---:|---:|---|
| EU | **250** | **150** | EU bought products 10 and 11: 100 + 50 units |
| US | **60** | **30** | US bought product 12, twice: 30 units |

ossiec computes every measure over its own rows, never over rows a join repeated. That guarantee comes from the Ossie spec itself (Layer 2's G2), and ossiec holds it on every engine it targets.

## The three layers

Ossie organizes its spec in layers ([`spec_layers.md`](https://github.com/apache/ossie/blob/main/core-spec/spec_layers.md)): Layer 1 defines the model, and Layers 2 and 3 are two ways to query it. ossiec implements all three. A Layer 3 request means the same as an equivalent Layer 2 query, so Layer 2's no-duplication guarantee carries through. Layer 4, ontologies, is out of scope; ossiec rejects those documents with a clear error.

| Layer | You write | ossiec returns | Spec |
|---|---|---|---|
| **3 · Semantic requests** | Metrics, dimensions and filters, as JSON. ossiec picks the joins. | One SQL statement, or the spec's error code when no answer is safe | [`wide_table_semantics.md`](https://github.com/apache/ossie/blob/main/core-spec/wide_table_semantics.md) |
| **2 · SQL with `AGG()`** | Your own SQL, with `AGG(measure)` where you read a measure. You choose the joins. | That query, with every measure computed over its own rows | [`relational_semantics.md`](https://github.com/apache/ossie/blob/main/core-spec/relational_semantics.md) |
| **1 · Model definition** | The model: datasets, fields, relationships and metrics, and the SQL expressions inside them | A validated model. When a query compiles, each function in it is rewritten for the target engine, or refused if the engine can't express it faithfully | [`spec.md`](https://github.com/apache/ossie/blob/main/core-spec/spec.md), [`expression_language.md`](https://github.com/apache/ossie/blob/main/core-spec/expression_language.md) |

## At a glance

- **Six engines verified by execution:** DuckDB, Postgres, Trino, StarRocks, ClickHouse and MySQL. Every correctness case runs on each and must return the same rows. Apache Spark is run too, and Athena and Databricks are written as the engines they're built on: Athena's SQL is Trino's and runs on Trino, Databricks' is Spark's and runs on Spark. Snowflake, BigQuery, Redshift and others are compile-checked. See [Engines](engines.md).
- **Refuses rather than guesses.** A question with no safe answer gets the spec's error code (`E_NO_PATH`, `E_AMBIGUOUS_PATH`, …), and a function an engine can't express gets a clear refusal rather than a different answer. The known exceptions: on StarRocks, `UPPER` of non-ASCII text changes ASCII letters only, and one `NTH_VALUE` form fails with an engine error ([gaps](generated/functions.md#gaps)).
- **Spec coverage, section by section:** see [Spec coverage](spec-coverage.md). The function and Layer 2 tables are generated from the test suite, so they can't drift from what ossiec does.
- **A small Rust library and CLI.** No server, no runtime, no Python. Deterministic: the same model, request and options give byte-identical SQL.

Next: [Getting started](getting-started.md).
