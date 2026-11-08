# Layer 3: semantic requests

A request names metrics and dimensions. ossiec finds the joins, computes each metric over its own home dataset, and stacks the results. This is Ossie's wide-table interface ([`wide_table_semantics.md`](https://github.com/apache/ossie/blob/main/core-spec/wide_table_semantics.md)).

## The request

```jsonc
{
  "metrics": ["total_revenue", {"metric": "orders.order_count", "alias": "orders"}],
  "dimensions": ["customers.region", {"field": "orders.order_date", "alias": "day"}],
  "filter": "orders.status IN ('completed', 'shipped')", // one expression over one dataset
  "filters": [                                            // structured, and may read several datasets
    {"field": "orders.order_date", "op": "BETWEEN", "values": ["2026-01-01", "2026-03-31"]}
  ],
  "metricFilters": [{"metric": "total_revenue", "op": ">", "value": 1000}], // HAVING
  "timeGrain": "month",                    // day | week | month | quarter | year
  "timeDimension": "orders.order_date",    // only needed when several time dimensions are requested
  "orderBy": [{"field": "total_revenue", "direction": "desc", "nulls": "last"}],
  "limit": 100,
  "deferredMetrics": false
}
```

The spec's query is `metrics`, `dimensions` and `filter`. The rest are ossiec's own properties, which the spec allows when a query asks for them explicitly (§9). Unknown keys are rejected, so a typo like `dimension` fails loudly instead of being ignored.

| Filter `op` | Value |
|---|---|
| `=` `!=` `<` `<=` `>` `>=` `LIKE` | `value` |
| `IN` `NOT IN` | `values` (non-empty) |
| `BETWEEN` | `values`: exactly two |
| `IS NULL` `IS NOT NULL` | none |

## What ossiec does with it

- **Output columns** are named by the alias, or by the reference's final part: `customers.region` comes back as `region`. Two with the same name raise `E_NAME_COLLISION`.
- **Home datasets.** Each metric reads all of its home dataset, in its own branch, joined many→one to every dimension and filter. Metrics from datasets that don't join each other (store sales and web sales by item category) are stacked, so the groups are the union: a category sold only online still appears (§6.1).
- **Roles.** When a dataset is reachable more than one way (a sale's date and its store's opening date), a plain `dates.year` raises `E_AMBIGUOUS_PATH`. Name the relationship to choose: `sale_date.year`.
- **Dimension values.** Leave out `metrics` to list values instead: `{"dimensions": ["customers.region"]}` returns every region. Agents use this to filter with real values.
- **`deferredMetrics`.** The spec defers two kinds of model-scoped metric: windows, and aggregates that read several datasets. With `"deferredMetrics": true` ossiec answers them; without it they raise `E_DEFERRED_WINDOWED_METRIC` and `E_AMBIGUOUS_MEASURE_GRAIN`.
- **Window metrics** (running totals, ranks, period-over-period) run over the grouped result. The fields they order or partition by must be requested dimensions.
- **Strict engines.** A string compared with a `Date` field is cast (`CAST('2026-01-01' AS DATE)`), so filters work on Trino.

## Refusals

When the spec rejects a request, ossiec raises its code (§8). If several apply, the first in this order wins:

| Code | Condition |
|---|---|
| `E_INVALID_QUERY` | Malformed, or a property Layer 3 doesn't support (`includeUnmatched`) |
| `E_EMPTY_AGGREGATION_QUERY` | No dimensions or metrics |
| `E_NAME_NOT_FOUND` | A reference doesn't resolve |
| `E_METRIC_IN_DIMENSIONS`, `E_FIELD_IN_METRICS` | An entry names the wrong kind of thing |
| `E_AGGREGATE_IN_WHERE`, `E_WINDOW_IN_WHERE` | A filter reads an aggregate, a metric or a window |
| `E_CROSS_DATASET_FILTER` | `filter` reads more than one dataset |
| `E_NAME_COLLISION` | Two entries share an output name |
| `E_UNAGGREGATED_FINER_GRAIN_REFERENCE` | A model-scoped metric reads a field outside every aggregate |
| `E_DEFERRED_WINDOWED_METRIC` | A model-scoped metric has a window, without `deferredMetrics` |
| `E_AMBIGUOUS_MEASURE_GRAIN` | An aggregate reads several datasets, has a `FILTER` on another dataset, or reads none |
| `E_NO_PATH` | No metric reaches a dimension's dataset |
| `E_PARTIAL_DIMENSION_REACHABILITY` | Some metrics reach a dimension's dataset, others don't |
| `E_NO_ANCHOR` | No metrics, and no dimension's dataset reaches the others |
| `E_UNSUPPORTED_FILTER_PATH` | A metric or the anchor can't reach a filter's dataset |
| `E_AMBIGUOUS_PATH` | Two paths lead to a dataset the query needs, or two datasets qualify as the anchor |

A metric the spec rejects doesn't stop the model loading: only a request that names it is refused, and `describe` marks it.

## When Layer 3 can't say it

Layer 3 picks one join path per question. When you need a different join, a Top-N, or a ratio across facts on a bridge you choose, write it in [Layer 2](layer2.md): the spec points there itself (§7).
