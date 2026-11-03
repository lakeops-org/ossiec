//! Ossie Layer 2 frontend: consumer SQL with `AGG(measure)` → Layer 3 request.
//!
//! Spec: `relational_semantics.md` (draft). Appendix **A.1** (single `FROM` relation)
//! and **A.5–A.6** (consumer joins along model relationships, `INNER` vs `LEFT`)
//! translate to the existing [`Request`] planner; join kinds are patched onto the
//! planned join path before lowering. Appendix **A.4** (measures from independent
//! facts sharing dimensions) maps to the same multi-leg [`Request`] planner.
//! Subqueries and other join forms (A.2–A.3, A.7, `RIGHT`/`FULL`/`CROSS`/`ASOF`,
//! …) stay [`Error::Unsupported`].
//!
//! Only **dataset-scoped** metrics are measures (spec §10.3). Model-scoped
//! metrics are refused inside `AGG()`.

use polyglot_sql::expressions::{Column, Join, JoinKind as SqlJoinKind, Literal, TableRef};
use polyglot_sql::{parse_one, DialectType, Expression};

use crate::graph::SemanticGraph;
use crate::plan::{self, Direction, Filter, JoinKind, Nulls, Op, OrderBy, Request};
use crate::{finish_compile, Compiled, Error, Options, Result};

/// Compile a Layer 2 SQL query (`AGG(measure)` over model relations) to SQL.
pub fn compile_layer2(graph: &SemanticGraph, sql: &str, opts: &Options) -> Result<Compiled> {
    let parsed = parse_layer2(graph, sql)?;
    let mut ctx = crate::graph::Ctx {
        prefer: opts.prefer_dialect.clone(),
        ..Default::default()
    };
    let mut logical = plan::plan(graph, &parsed.request, &mut ctx)?;
    patch_consumer_joins(graph, &mut logical, &parsed.consumer_joins)?;
    finish_compile(graph, &parsed.request, logical, ctx, opts)
}

/// Translate Layer 2 SQL into a Layer 3 [`Request`] (join kinds are not represented).
pub fn to_request(graph: &SemanticGraph, sql: &str) -> Result<Request> {
    Ok(parse_layer2(graph, sql)?.request)
}

struct Layer2Parsed {
    request: Request,
    /// Join kinds from the consumer `FROM` / `JOIN` list (Appendix A.6).
    consumer_joins: Vec<ConsumerJoin>,
}

struct ConsumerJoin {
    rel: usize,
    kind: JoinKind,
}

/// One dataset in the consumer `FROM` / `JOIN` list and its SQL alias.
#[derive(Clone)]
struct TableBinding {
    dataset: usize,
    alias: String,
}

struct QueryScope {
    bindings: Vec<TableBinding>,
}

fn parse_layer2(graph: &SemanticGraph, sql: &str) -> Result<Layer2Parsed> {
    let ast = parse_one(sql.trim(), DialectType::Generic)
        .map_err(|e| Error::Request(format!("Layer 2 SQL: {e}")))?;
    let Expression::Select(sel) = ast else {
        return Err(unsupported(
            "Layer 2 currently accepts a single SELECT (Appendix A.1); joins and set ops come later",
        ));
    };
    if sel.with.is_some() || sel.distinct || sel.having.is_some() || sel.qualify.is_some() {
        return Err(unsupported(
            "Layer 2 WITH / DISTINCT / HAVING / QUALIFY are not implemented yet",
        ));
    }

    let parsed_from = parse_from_clause(graph, sel.from.as_ref(), &sel.joins)?;
    let scope = &parsed_from.scope;

    let mut metrics = Vec::new();
    let mut dimensions = Vec::new();
    // Select-list label (alias or default) → Layer 3 request field name.
    let mut outputs: Vec<(String, String)> = Vec::new();

    for expr in &sel.expressions {
        let (inner, out_name) = strip_alias(expr);
        match take_agg(inner)? {
            Some(measure) => {
                let metric = resolve_measure(graph, scope, measure)?;
                let label = out_name.clone().unwrap_or_else(|| metric.clone());
                if !metrics.iter().any(|m| m == &metric) {
                    metrics.push(metric.clone());
                }
                outputs.push((label, metric));
            }
            None => {
                let col = as_column(inner).ok_or_else(|| {
                    unsupported(
                        "Layer 2 select list may only contain AGG(measure) and dimension columns",
                    )
                })?;
                let dim = resolve_dim(graph, scope, col)?;
                let label = out_name.unwrap_or_else(|| dim.clone());
                if !dimensions.iter().any(|d| d == &dim) {
                    dimensions.push(dim.clone());
                }
                outputs.push((label, dim));
            }
        }
    }

    if metrics.is_empty() && dimensions.is_empty() {
        return Err(Error::Request(
            "Layer 2 query must select at least one AGG(measure) or dimension".into(),
        ));
    }

    if !metrics.is_empty() {
        match &sel.group_by {
            None if dimensions.is_empty() => {}
            None => {
                return Err(Error::Request(
                    "Layer 2 aggregated query with dimensions needs GROUP BY".into(),
                ));
            }
            Some(gb) => {
                if gb.all.is_some() {
                    return Err(unsupported("GROUP BY ALL is not supported in Layer 2"));
                }
                let mut gb_dims = Vec::new();
                for e in &gb.expressions {
                    let col = as_column(e).ok_or_else(|| {
                        unsupported("Layer 2 GROUP BY must name dimension columns")
                    })?;
                    gb_dims.push(resolve_dim(graph, scope, col)?);
                }
                let mut want = dimensions.clone();
                want.sort();
                gb_dims.sort();
                if gb_dims != want {
                    return Err(Error::Request(format!(
                        "Layer 2 GROUP BY must match the selected dimensions (got [{}], selected [{}])",
                        gb_dims.join(", "),
                        dimensions.join(", ")
                    )));
                }
            }
        }
    } else if sel.group_by.is_some() {
        return Err(Error::Request(
            "Layer 2 GROUP BY without AGG(measure) is not used; omit GROUP BY for dimension listing"
                .into(),
        ));
    }

    let filters = match &sel.where_clause {
        None => vec![],
        Some(w) => where_to_filters(graph, scope, &w.this)?,
    };

    let order_ctx = OrderCtx {
        graph,
        scope,
        outputs: &outputs,
        metrics: &metrics,
        dimensions: &dimensions,
    };
    let order_by = match &sel.order_by {
        None => vec![],
        Some(ob) => ob
            .expressions
            .iter()
            .map(|o| {
                Ok(OrderBy {
                    field: order_field(&order_ctx, &o.this)?,
                    direction: if o.desc {
                        Direction::Desc
                    } else {
                        Direction::Asc
                    },
                    nulls: match o.nulls_first {
                        Some(true) => Some(Nulls::First),
                        Some(false) => Some(Nulls::Last),
                        None => None,
                    },
                })
            })
            .collect::<Result<_>>()?,
    };

    let limit = match &sel.limit {
        None => None,
        Some(l) => {
            if l.percent {
                return Err(unsupported("Layer 2 LIMIT PERCENT is not supported"));
            }
            Some(limit_u64(&l.this)?)
        }
    };

    Ok(Layer2Parsed {
        request: Request {
            metrics,
            dimensions,
            filters,
            order_by,
            limit,
            ..Default::default()
        },
        consumer_joins: parsed_from.consumer_joins,
    })
}

fn patch_consumer_joins(
    graph: &SemanticGraph,
    logical: &mut plan::LogicalPlan,
    consumer: &[ConsumerJoin],
) -> Result<()> {
    let planned: std::collections::BTreeSet<usize> = logical
        .legs
        .iter()
        .flat_map(|leg| leg.joins.iter().map(|s| s.rel))
        .collect();
    for cj in consumer {
        if !planned.contains(&cj.rel) {
            return Err(unsupported(format!(
                "Layer 2 join on relationship `{}` is not on the resolved join path; \
                 Appendix A.5 fan-out or joins the planner does not use need more work",
                graph.rels[cj.rel].name
            )));
        }
    }
    for leg in &mut logical.legs {
        for step in &mut leg.joins {
            if let Some(cj) = consumer.iter().find(|c| c.rel == step.rel) {
                step.kind = cj.kind;
            }
        }
    }
    Ok(())
}

fn unsupported(msg: impl Into<String>) -> Error {
    Error::Unsupported(msg.into())
}

struct ParsedFrom {
    scope: QueryScope,
    consumer_joins: Vec<ConsumerJoin>,
}

fn parse_from_clause(
    graph: &SemanticGraph,
    from: Option<&polyglot_sql::expressions::From>,
    joins: &[Join],
) -> Result<ParsedFrom> {
    let from = from.ok_or_else(|| Error::Request("Layer 2 query needs a FROM clause".into()))?;
    if from.expressions.is_empty() {
        return Err(Error::Request("Layer 2 query needs a FROM clause".into()));
    }
    let mut scope = QueryScope { bindings: vec![] };
    for e in &from.expressions {
        let (dataset, alias) = table_expr(graph, e)?;
        let alias = alias.unwrap_or_else(|| graph.datasets[dataset].name.clone());
        if scope
            .bindings
            .iter()
            .any(|b| b.alias.eq_ignore_ascii_case(&alias))
        {
            return Err(Error::Request(format!(
                "Layer 2 duplicate relation alias `{alias}` in FROM"
            )));
        }
        scope.bindings.push(TableBinding { dataset, alias });
    }
    let mut consumer_joins = Vec::new();
    for j in joins {
        let kind = consumer_join_kind(j)?;
        let on = j.on.as_ref().ok_or_else(|| {
            unsupported("Layer 2 JOIN requires an ON condition matching a model relationship")
        })?;
        if !j.using.is_empty() {
            return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
        }
        let (right_ds, right_alias) = table_expr(graph, &j.this)?;
        let right_binding = TableBinding {
            dataset: right_ds,
            alias: right_alias.unwrap_or_else(|| graph.datasets[right_ds].name.clone()),
        };
        let pairs = on_equality_pairs(on)?;
        let join_scope = QueryScope {
            bindings: scope
                .bindings
                .iter()
                .chain(std::iter::once(&right_binding))
                .cloned()
                .collect(),
        };
        let rel = match_relationship(graph, &join_scope, &pairs)?;
        if consumer_joins.iter().any(|c: &ConsumerJoin| c.rel == rel) {
            return Err(unsupported(format!(
                "Layer 2 duplicate join on relationship `{}`",
                graph.rels[rel].name
            )));
        }
        consumer_joins.push(ConsumerJoin { rel, kind });
        scope.bindings.push(right_binding);
    }
    Ok(ParsedFrom {
        scope,
        consumer_joins,
    })
}

fn consumer_join_kind(j: &Join) -> Result<JoinKind> {
    match j.kind {
        SqlJoinKind::Inner | SqlJoinKind::Implicit => Ok(JoinKind::Inner),
        SqlJoinKind::Left | SqlJoinKind::NaturalLeft => Ok(JoinKind::Left),
        SqlJoinKind::Right
        | SqlJoinKind::Full
        | SqlJoinKind::Cross
        | SqlJoinKind::AsOf
        | SqlJoinKind::AsOfLeft
        | SqlJoinKind::AsOfRight
        | SqlJoinKind::Outer
        | SqlJoinKind::Natural
        | SqlJoinKind::NaturalRight
        | SqlJoinKind::NaturalFull
        | SqlJoinKind::Semi
        | SqlJoinKind::Anti
        | SqlJoinKind::LeftSemi
        | SqlJoinKind::LeftAnti
        | SqlJoinKind::RightSemi
        | SqlJoinKind::RightAnti
        | SqlJoinKind::CrossApply
        | SqlJoinKind::OuterApply
        | SqlJoinKind::Lateral
        | SqlJoinKind::LeftLateral
        | SqlJoinKind::Straight
        | SqlJoinKind::Array
        | SqlJoinKind::LeftArray
        | SqlJoinKind::Paste
        | SqlJoinKind::Positional => Err(unsupported(
            "Layer 2 only supports INNER JOIN, JOIN, and LEFT JOIN (Appendix A.6)",
        )),
    }
}

fn on_equality_pairs(on: &Expression) -> Result<Vec<(Column, Column)>> {
    match on {
        Expression::Eq(b) => {
            let l = as_column(&b.left).ok_or_else(on_must_be_equalities)?;
            let r = as_column(&b.right).ok_or_else(on_must_be_equalities)?;
            Ok(vec![(l.to_owned(), r.to_owned())])
        }
        Expression::And(a) => {
            let mut out = on_equality_pairs(&a.left)?;
            out.extend(on_equality_pairs(&a.right)?);
            Ok(out)
        }
        _ => Err(on_must_be_equalities()),
    }
}

fn on_must_be_equalities() -> Error {
    unsupported("Layer 2 JOIN ON must be column equalities (and AND of equalities)")
}

/// Resolved endpoints of one ON equality: `(dataset, field_name)`.
type FieldRef = (usize, String);

fn match_relationship(
    graph: &SemanticGraph,
    scope: &QueryScope,
    pairs: &[(Column, Column)],
) -> Result<usize> {
    if pairs.is_empty() {
        return Err(on_must_be_equalities());
    }
    let resolved: Result<Vec<(FieldRef, FieldRef)>> = pairs
        .iter()
        .map(|(l, r)| {
            Ok((
                resolve_field_ref(graph, scope, l)?,
                resolve_field_ref(graph, scope, r)?,
            ))
        })
        .collect();
    let resolved = resolved?;
    for (ri, rel) in graph.rels.iter().enumerate() {
        if rel_matches_pairs(graph, rel, &resolved) {
            return Ok(ri);
        }
    }
    Err(unsupported(
        "Layer 2 JOIN ON must match a model relationship (equality on its from_columns and to_columns, in either direction)",
    ))
}

fn rel_matches_pairs(
    graph: &SemanticGraph,
    rel: &crate::graph::RelInfo,
    pairs: &[(FieldRef, FieldRef)],
) -> bool {
    if pairs.len() != rel.from_fields.len() {
        return false;
    }
    let forward: Vec<(FieldRef, FieldRef)> = rel
        .from_fields
        .iter()
        .zip(rel.to_fields.iter())
        .map(|(&ff, &tf)| {
            (
                (rel.from, graph.datasets[rel.from].fields[ff].name.clone()),
                (rel.to, graph.datasets[rel.to].fields[tf].name.clone()),
            )
        })
        .collect();
    pairs_match_set(pairs, &forward) || pairs_match_set(pairs, &reverse_pairs(&forward))
}

fn reverse_pairs(pairs: &[(FieldRef, FieldRef)]) -> Vec<(FieldRef, FieldRef)> {
    pairs.iter().map(|(a, b)| (b.clone(), a.clone())).collect()
}

fn pairs_match_set(got: &[(FieldRef, FieldRef)], want: &[(FieldRef, FieldRef)]) -> bool {
    want.iter().all(|w| {
        got.iter()
            .any(|g| field_pair_eq(g, w) || field_pair_eq(g, &(w.1.clone(), w.0.clone())))
    }) && got.iter().all(|g| {
        want.iter()
            .any(|w| field_pair_eq(g, w) || field_pair_eq(g, &(w.1.clone(), w.0.clone())))
    })
}

fn field_pair_eq(a: &(FieldRef, FieldRef), b: &(FieldRef, FieldRef)) -> bool {
    a.0 .0 == b.0 .0
        && a.1 .0 == b.1 .0
        && a.0 .1.eq_ignore_ascii_case(&b.0 .1)
        && a.1 .1.eq_ignore_ascii_case(&b.1 .1)
}

fn resolve_field_ref(graph: &SemanticGraph, scope: &QueryScope, col: &Column) -> Result<FieldRef> {
    let ds = resolve_column_dataset(graph, scope, col)?;
    let field = &col.name.name;
    let f = graph.datasets[ds]
        .fields
        .iter()
        .position(|f| f.name.eq_ignore_ascii_case(field))
        .ok_or_else(|| {
            Error::Request(format!(
                "unknown field `{field}` on dataset `{}`",
                graph.datasets[ds].name
            ))
        })?;
    Ok((ds, graph.datasets[ds].fields[f].name.clone()))
}

fn resolve_column_dataset(
    graph: &SemanticGraph,
    scope: &QueryScope,
    col: &Column,
) -> Result<usize> {
    if let Some(t) = &col.table {
        for b in &scope.bindings {
            let ds_name = &graph.datasets[b.dataset].name;
            if t.name.eq_ignore_ascii_case(&b.alias) || t.name.eq_ignore_ascii_case(ds_name) {
                return Ok(b.dataset);
            }
        }
        return Err(Error::Request(format!(
            "column `{}.{}` does not refer to a relation in FROM/JOIN",
            t.name, col.name.name
        )));
    }
    if scope.bindings.len() == 1 {
        return Ok(scope.bindings[0].dataset);
    }
    let field = &col.name.name;
    let hits: Vec<usize> = scope
        .bindings
        .iter()
        .filter(|b| {
            graph.datasets[b.dataset]
                .fields
                .iter()
                .any(|f| f.name.eq_ignore_ascii_case(field))
        })
        .map(|b| b.dataset)
        .collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        _ => Err(Error::Request(format!(
            "column `{field}` is ambiguous; qualify it with a FROM/JOIN alias"
        ))),
    }
}

fn table_expr(graph: &SemanticGraph, e: &Expression) -> Result<(usize, Option<String>)> {
    match e {
        Expression::Table(t) => resolve_table(graph, t),
        Expression::Alias(a) => {
            let (ds, _) = table_expr(graph, &a.this)?;
            Ok((ds, Some(a.alias.name.clone())))
        }
        _ => Err(unsupported(
            "Layer 2 FROM must be a model dataset (table or alias); \
             derived tables / subqueries (Appendix A.2–A.3) are not supported yet",
        )),
    }
}

fn resolve_table(graph: &SemanticGraph, t: &TableRef) -> Result<(usize, Option<String>)> {
    if t.schema.is_some() || t.catalog.is_some() {
        return Err(unsupported(
            "Layer 2 FROM should name a model dataset, not a catalog.schema.table",
        ));
    }
    let name = &t.name.name;
    let ds = graph
        .datasets
        .iter()
        .position(|d| d.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            Error::Request(format!(
                "unknown Layer 2 relation `{name}`; datasets: {}",
                graph
                    .datasets
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;
    Ok((ds, t.alias.as_ref().map(|a| a.name.clone())))
}

fn strip_alias(e: &Expression) -> (&Expression, Option<String>) {
    match e {
        Expression::Alias(a) => (&a.this, Some(a.alias.name.clone())),
        other => (other, None),
    }
}

fn take_agg(e: &Expression) -> Result<Option<&Expression>> {
    match e {
        Expression::Function(f) if is_agg_name(&f.name) => {
            if f.args.len() != 1 {
                return Err(Error::Request(format!(
                    "`{}` takes exactly one measure argument",
                    f.name
                )));
            }
            Ok(Some(&f.args[0]))
        }
        Expression::AggregateFunction(f) if is_agg_name(&f.name) => {
            if f.args.len() != 1 {
                return Err(Error::Request(format!(
                    "`{}` takes exactly one measure argument",
                    f.name
                )));
            }
            if f.filter.is_some() {
                return Err(unsupported(
                    "FILTER on AGG() itself is not defined; put filters in WHERE or in the measure",
                ));
            }
            Ok(Some(&f.args[0]))
        }
        _ => Ok(None),
    }
}

fn is_agg_name(name: &str) -> bool {
    // Spec §6: AGG is normative; MEASURE is a non-normative synonym.
    matches!(name.to_ascii_uppercase().as_str(), "AGG" | "MEASURE")
}

fn as_column(e: &Expression) -> Option<&Column> {
    match e {
        Expression::Column(c) => Some(c),
        _ => None,
    }
}

fn resolve_measure(graph: &SemanticGraph, scope: &QueryScope, arg: &Expression) -> Result<String> {
    let col = as_column(arg).ok_or_else(|| {
        Error::Request(
            "AGG() argument must be a measure column reference (AGG(m) or AGG(alias.m)); \
             write AGG(a) + AGG(b), not AGG(a + b)"
                .into(),
        )
    })?;
    let dataset = resolve_column_dataset(graph, scope, col)?;
    let ds_name = &graph.datasets[dataset].name;
    let measure = &col.name.name;

    let scoped = format!("{ds_name}.{measure}");
    if let Ok(i) = graph.metric_by_name(&scoped) {
        if graph.metrics[i].owner == Some(dataset) {
            return Ok(graph.metrics[i].name.clone());
        }
    }
    if let Ok(i) = graph.metric_by_name(measure) {
        if graph.metrics[i].owner == Some(dataset) {
            return Ok(graph.metrics[i].name.clone());
        }
        if graph.metrics[i].owner.is_none() {
            return Err(Error::Request(format!(
                "`{measure}` is a model-scoped metric; Layer 2 only exposes dataset-scoped metrics as measures (relational_semantics §10.3)"
            )));
        }
        return Err(Error::Request(format!(
            "`{measure}` is not a measure on dataset `{ds_name}`"
        )));
    }
    if graph
        .metrics
        .iter()
        .any(|m| m.name.eq_ignore_ascii_case(measure) && m.owner.is_none())
    {
        return Err(Error::Request(format!(
            "`{measure}` is a model-scoped metric; Layer 2 only exposes dataset-scoped metrics as measures (relational_semantics §10.3)"
        )));
    }
    Err(Error::Request(format!(
        "unknown measure `{measure}` on dataset `{ds_name}`; dataset metrics: {}",
        graph
            .metrics
            .iter()
            .filter(|m| m.owner == Some(dataset))
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

fn resolve_dim(graph: &SemanticGraph, scope: &QueryScope, col: &Column) -> Result<String> {
    let dataset = resolve_column_dataset(graph, scope, col)?;
    let ds_name = &graph.datasets[dataset].name;
    let field = &col.name.name;
    let f = graph.datasets[dataset]
        .fields
        .iter()
        .position(|f| f.name.eq_ignore_ascii_case(field))
        .ok_or_else(|| Error::Request(format!("unknown field `{field}` on dataset `{ds_name}`")))?;
    Ok(format!(
        "{ds_name}.{}",
        graph.datasets[dataset].fields[f].name
    ))
}

fn where_to_filters(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
) -> Result<Vec<Filter>> {
    match e {
        Expression::And(a) => {
            let mut out = where_to_filters(graph, scope, &a.left)?;
            out.extend(where_to_filters(graph, scope, &a.right)?);
            Ok(out)
        }
        Expression::Eq(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Eq),
        Expression::Neq(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Ne),
        Expression::Lt(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Lt),
        Expression::Lte(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Le),
        Expression::Gt(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Gt),
        Expression::Gte(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Ge),
        Expression::IsNull(i) => {
            let col = as_column(&i.this).ok_or_else(simple_where)?;
            Ok(vec![Filter {
                field: resolve_dim(graph, scope, col)?,
                op: if i.not { Op::IsNotNull } else { Op::IsNull },
                value: None,
                values: vec![],
            }])
        }
        _ => Err(simple_where()),
    }
}

fn simple_where() -> Error {
    unsupported(
        "Layer 2 WHERE currently accepts column comparisons and AND (e.g. status = 'completed')",
    )
}

fn cmp_filter(
    graph: &SemanticGraph,
    scope: &QueryScope,
    left: &Expression,
    right: &Expression,
    op: Op,
) -> Result<Vec<Filter>> {
    let (col_e, lit_e) = match (as_column(left), as_column(right)) {
        (Some(_), None) => (left, right),
        (None, Some(_)) if matches!(op, Op::Eq | Op::Ne) => (right, left),
        _ => return Err(simple_where()),
    };
    let col = as_column(col_e).unwrap();
    let value = literal_json(lit_e).ok_or_else(simple_where)?;
    Ok(vec![Filter {
        field: resolve_dim(graph, scope, col)?,
        op,
        value: Some(value),
        values: vec![],
    }])
}

fn literal_json(e: &Expression) -> Option<serde_json::Value> {
    match e {
        Expression::Literal(l) => match l.as_ref() {
            Literal::String(s)
            | Literal::NationalString(s)
            | Literal::Date(s)
            | Literal::Timestamp(s)
            | Literal::Datetime(s) => Some(serde_json::Value::String(s.clone())),
            Literal::Number(s) => {
                if let Ok(i) = s.parse::<i64>() {
                    Some(serde_json::json!(i))
                } else if let Ok(f) = s.parse::<f64>() {
                    serde_json::Number::from_f64(f).map(serde_json::Value::Number)
                } else {
                    Some(serde_json::Value::String(s.clone()))
                }
            }
            _ => None,
        },
        Expression::Boolean(b) => Some(serde_json::Value::Bool(b.value)),
        Expression::Null(_) => Some(serde_json::Value::Null),
        _ => None,
    }
}

fn limit_u64(e: &Expression) -> Result<u64> {
    match e {
        Expression::Literal(l) => match l.as_ref() {
            Literal::Number(s) => s.parse::<u64>().map_err(|_| {
                Error::Request(format!(
                    "Layer 2 LIMIT must be a non-negative integer, got {s}"
                ))
            }),
            _ => Err(unsupported(
                "Layer 2 LIMIT must be a non-negative integer literal",
            )),
        },
        _ => Err(unsupported(
            "Layer 2 LIMIT must be a non-negative integer literal",
        )),
    }
}

struct OrderCtx<'a> {
    graph: &'a SemanticGraph,
    scope: &'a QueryScope,
    outputs: &'a [(String, String)],
    metrics: &'a [String],
    dimensions: &'a [String],
}

fn order_field(ctx: &OrderCtx<'_>, e: &Expression) -> Result<String> {
    if let Some(arg) = take_agg(e)? {
        return resolve_measure(ctx.graph, ctx.scope, arg);
    }
    if let Some(col) = as_column(e) {
        if col.table.is_none() {
            let n = &col.name.name;
            if let Some((_, req)) = ctx.outputs.iter().find(|(label, _)| label == n) {
                return Ok(req.clone());
            }
            if let Some(m) = ctx
                .metrics
                .iter()
                .find(|m| m.as_str() == n || m.ends_with(&format!(".{n}")))
            {
                return Ok(m.clone());
            }
            if let Some(d) = ctx
                .dimensions
                .iter()
                .find(|d| d.as_str() == n || d.ends_with(&format!(".{n}")))
            {
                return Ok(d.clone());
            }
        }
        return resolve_dim(ctx.graph, ctx.scope, col);
    }
    Err(unsupported(
        "Layer 2 ORDER BY must name a selected dimension, measure, or AGG(...)",
    ))
}
