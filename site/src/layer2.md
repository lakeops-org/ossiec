# Layer 2: SQL with `AGG()`

Write ordinary SQL over the model's datasets, and read a measure with `AGG(measure)`. You choose the joins; ossiec makes sure each measure is computed over its own rows, so a join never inflates it. This is Ossie's relational interface ([`relational_semantics.md`](https://github.com/apache/ossie/blob/main/core-spec/relational_semantics.md)).

```sql
SELECT c.region, AGG(o.order_count), AGG(p.stock_units)
FROM orders o
LEFT JOIN customers c ON o.customer_id = c.id
LEFT JOIN products p ON o.product_id = p.id
GROUP BY c.region
```

`order_count` counts each order once. `stock_units` sums each product's stock once per region, however many orders repeat it. A plain `SUM(p.stock)` would count it once per order.

Compile from the CLI with `ossiec compile-sql model.yaml query.sql --dialect trino`, or from Rust with `ossiec::compile_sql` for one query. A script with `CREATE VIEW` statements before the query goes to `ossiec::compile_sql_script`; or register views in a `Layer2Catalog` with `define_layer2_view` and compile with `compile_sql_with`.

## The rules ossiec follows

- **The query is one relation (§7.6, §10.2).** Its joins, filters and grouping decide which rows and groups appear, exactly as in standard SQL. Measures never change that.
- **Each measure counts its own source rows once (§8.1, G2).** A join that repeats a row doesn't repeat it in the measure. Rows a join pads with NULLs (an outer join's unmatched side) add nothing, and a group made only of them gets the empty value: NULL, or 0 for `COUNT`.
- **Measures are not values (§4, §7).** A measure is read only through `AGG()`. `AGG(a + b)` is refused; write `AGG(a) + AGG(b)`. A query that aggregates can't return an unevaluated measure (§7.2).
- **Only dataset-scoped metrics are measures (§10.3).** A model-scoped metric has no home relation, so `AGG()` of one is refused.

## What you can write

| Construct | Example | Spec |
|---|---|---|
| `AGG()` and arithmetic over it | `AGG(revenue) / AGG(ticket_count)` | §6, §7.2 |
| Plain aggregates beside measures, and arithmetic on them | `MAX(order_date)`, `SUM(revenue) / COUNT(*)`, `COUNT(*) - AGG(order_count)` | §7.2 |
| Any join kind along a model relationship | `INNER`, `LEFT`, `RIGHT`, `FULL`, `CROSS`, from either side; a comma `FROM` is a `CROSS JOIN`, as in SQL | §7.6, A.5, A.6 |
| `WHERE` as any row predicate | comparisons, `IN`, `LIKE`, `BETWEEN`, `OR`, `NOT`, across datasets, and subqueries (`IN (SELECT …)`, `EXISTS`, a scalar `(SELECT MIN(…) …)`), each a Layer 2 query of its own that can evaluate `AGG()` | §7.3, §7.7 |
| `GROUP BY`, `GROUP BY ALL` | `GROUP BY ALL` groups by the selected non-aggregates | §7.4 |
| `HAVING`, `ORDER BY`, `LIMIT` | `ORDER BY AGG(revenue) DESC LIMIT 10`, `HAVING COUNT(*) > 5`; neither needs to be selected | §7.5 |
| Derived tables and CTEs | a measure passes through any number of levels to the outer `AGG()` | §7.7, A.2, A.3 |
| `DISTINCT` | over fields and evaluated measures | §7.8 |
| Query-local measures | `(AGG(a) / AGG(b)) TO MEASURE AS ratio`, `SUM(x) TO MEASURE AS m` | §5.1, §5.2 |
| Views | `CREATE VIEW v AS SELECT … TO MEASURE …` in a session catalog | A.7 |
| Roles | `departs_from.city`, or the relationship name as the join alias | §11.3 |
| `MEASURE()` | a synonym for `AGG()` | §6 |
| Identifiers | `AGG("ORDER_COUNT")` and `AGG(Order_Count)` both name `order_count`; `AGG("order_count")` doesn't, since a quoted name keeps its case | expression language |

Every row is backed by tests that run the query on DuckDB against a plain-SQL reference; see [Layer 2 test evidence](generated/layer2-tests.md).

## Examples

**Top-N with a derived table (A.2).** Restrict an `AGG` stage to the top two customers:

```sql
SELECT o.status, AGG(o.order_count)
FROM orders o
JOIN (SELECT customer_id, AGG(order_count) AS n
      FROM orders GROUP BY customer_id
      ORDER BY n DESC, customer_id LIMIT 2) t
  ON o.customer_id = t.customer_id
GROUP BY o.status;
```

**Pass-through (§7.7).** The measure stays a measure through the subqueries; the outer `AGG` evaluates it:

```sql
SELECT status, AGG(order_count)
FROM (SELECT status, order_count
      FROM (SELECT status, order_count FROM orders WHERE customer_id IS NOT NULL) a
      WHERE status <> 'cancelled') b
GROUP BY status;
```

**A cross-fact ratio on a conformed dimension (A.7).** Defined once as a view, read like any measure. This and the next example use a model with independent `orders` and `tickets` facts sharing `customers` ([`two_facts_layer2.yaml`](https://github.com/lakeops-org/ossiec/blob/main/tests/fixtures/two_facts_layer2.yaml)):

```sql
CREATE VIEW region_metrics AS
SELECT c.region,
       (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket
FROM customers c
LEFT JOIN orders o ON o.customer_id = c.id
LEFT JOIN tickets t ON t.customer_id = c.id;

SELECT region, AGG(sales_per_ticket) FROM region_metrics GROUP BY region;
```

Every region appears, including one whose customers bought nothing. `ossiec persist-view` (or `persist_layer2_view`) can write `sales_per_ticket` into the model as a metric. The persisted metric is then queried over the model's own paths from each fact to `customers`, not over the view's joins, so its groups can differ: a region with no facts can drop out, and facts with no customer can appear under a NULL region. The other groups both return agree; a NULL region both return can still differ, because only the persisted metric adds those facts to it.

**One relation across facts.** An inner join on one fact narrows the other too, as in plain SQL:

```sql
SELECT c.region, AGG(o.order_amt), AGG(t.ticket_count)
FROM customers c
JOIN orders o ON o.customer_id = c.id
JOIN tickets t ON t.customer_id = c.id
GROUP BY c.region;
```

A region whose customers have tickets but no orders is not in the result, and its tickets aren't counted. A `WHERE` on one fact's columns does the same: `WHERE o.amount > 10` keeps the orders over 10, and only the tickets of customers who placed one. `IN`, `NOT IN`, `BETWEEN` and `LIKE` against literals work the same way.

**Plain aggregates count joined rows; measures don't (§7.2).**

```sql
SELECT c.region, COUNT(*) AS joined_rows, AGG(o.order_count) AS orders
FROM customers c LEFT JOIN orders o ON o.customer_id = c.id
GROUP BY c.region;
```

A customer without orders is one joined row, so `COUNT(*)` counts it, while `orders` doesn't. The two can be combined like any values: `COUNT(*) - AGG(o.order_count)` is the customers in each region with no order, and `HAVING COUNT(*) > AGG(o.order_count)` keeps the regions that have one.

## Options

**`includeUnmatched`** adds the one-side rows no fact reaches (products that never sold) to the outermost stage: set `Options { include_unmatched: true, .. }`, pass `--include-unmatched`, or add the comment `-- ossiec: includeUnmatched`.

## Refused today

ossiec refuses these with a clear message rather than answer differently from the spec. Each is on the [roadmap](roadmap.md).

- `SELECT *`
- Set operators (`UNION`, `INTERSECT`, `EXCEPT`), `ROLLUP`, `CUBE` and `GROUPING SETS`
- Subqueries in the select list, `HAVING` and `ORDER BY`, and a correlated subquery anywhere (one that reads the enclosing query's columns)
- `ASOF`, `SEMI`/`ANTI`, `NATURAL` and `LATERAL` joins
- A `CROSS JOIN` (or comma `FROM`) spanning independent facts, and a comma `FROM` combined with a `RIGHT` or `FULL` join
- A `WHERE` on one of several independent facts' columns that a NULL can pass (`o.amount IS NULL`), a predicate there that isn't a comparison, `IN`, `NOT IN`, `BETWEEN` or `LIKE` against literals (`NOT BETWEEN`, `NOT LIKE`, an `IN` list with `NULL`), one on a field the model defines as an expression (a field `COALESCE(amount, 0)` compared with `= 0`), or one that reads two facts at once. Those forms on a field that is a plain column (`o.amount > 10`, `o.status IN ('a', 'b')`) work
- A fan-out written from the many side (`order_items i LEFT JOIN orders o` reading `AGG(o.order_count)`); `orders o RIGHT JOIN order_items i` works
- A one-side measure on a dataset with no `primary_key` (de-duplication needs one), except where a join only pads with NULLs
