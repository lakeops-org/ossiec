# How it works

```
YAML ──load──► Model ──bind──► SemanticGraph ──plan──► LogicalPlan ──lower──► AST ──render──► SQL
```

1. **Bind**, once per model. Parse every expression and resolve every name with ANSI identifier rules. Split each metric into aggregate *terms* and an outer formula: `SUM(a.x) / NULLIF(SUM(b.y), 0)` becomes two terms and `t0 / NULLIF(t1, 0)`. All problems are reported together.
2. **Pick the join roots.** Each aggregate reads its home dataset, in a branch that joins many→one from there to every dimension and filter. Many→one joins never multiply the home's rows. In Layer 2, the consumer's own joins decide the tree.
3. **Classify each term.**
   - A term that reads the root, or that duplicates can't change (`MIN`, `MAX`, `COUNT(DISTINCT …)`), is computed **inline** over the joined rows.
   - In Layer 2, a term that reads only one-side data (`SUM(products.stock)` over an `orders` root) is computed over rows **de-duplicated on that dataset's primary key**: the reference strategy of the relational spec (§9).
   - A join that only pads rows with NULLs (`customers LEFT JOIN orders`, planned from `orders`) needs no key: each aggregate skips the padded rows with a `FILTER`.
4. **Stack, don't join.** Each branch and strategy becomes a part grouped by the same dimensions. Parts are combined with `UNION ALL` and `MAX` per term, so there's no join between parts to get wrong.
5. **One relation in Layer 2.** When independent facts are planned as separate branches, a join that narrows one (`INNER`, a derived table) narrows the others too, through `EXISTS` on the rows they share.
6. **Windows last.** Window functions run in the final `SELECT`, over one row per group.
7. **Render.** Build one polyglot AST, implement each Ossie function for the target, apply the target's dialect rules, and transpile.

## Source layout

| Module | Concern |
|---|---|
| `model.rs` | The Ossie YAML as types |
| `graph/` | A bound model: binding, metrics into terms, the catalog |
| `request.rs` | The Layer 3 request and its JSON |
| `plan/` | Decisions: names, terms, strategies, join trees |
| `protect.rs` | Keeping measures duplicate-free over joins that add rows |
| `wide_table.rs` | Layer 3 rejections |
| `lower.rs` | Plan to SQL syntax tree |
| `functions.rs` | Ossie's functions, engine by engine |
| `dialect/` | Per-engine rules |
| `front/` | Layer 2: parsing, joins, derived tables, views, persisting |

The full rationale is in [DESIGN.md](https://github.com/lakeops-org/ossiec/blob/main/docs/DESIGN.md).
