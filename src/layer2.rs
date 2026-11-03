//! Ossie Layer 2 frontend: consumer SQL with `AGG(measure)` → Layer 3 request.
//!
//! Spec: `relational_semantics.md` (draft). This first cut covers Appendix A.1 —
//! a single measure relation, no consumer joins — by translating to the existing
//! [`Request`] planner. Joins and subqueries (A.2–A.7) stay [`Error::Unsupported`].
//!
//! Only **dataset-scoped** metrics are measures (spec §10.3). Model-scoped
//! metrics are refused inside `AGG()`.

use polyglot_sql::expressions::{Column, Literal, TableRef};
use polyglot_sql::{parse_one, DialectType, Expression};

use crate::graph::SemanticGraph;
use crate::plan::{Direction, Filter, Nulls, Op, OrderBy, Request};
use crate::{compile, Compiled, Error, Options, Result};

/// Compile a Layer 2 SQL query (`AGG(measure)` over model relations) to SQL.
pub fn compile_layer2(graph: &SemanticGraph, sql: &str, opts: &Options) -> Result<Compiled> {
    let request = to_request(graph, sql)?;
    compile(graph, &request, opts)
}

/// Translate Layer 2 SQL into a Layer 3 [`Request`] (A.1 shape only).
pub fn to_request(graph: &SemanticGraph, sql: &str) -> Result<Request> {
    let ast = parse_one(sql.trim(), DialectType::Generic)
        .map_err(|e| Error::Request(format!("Layer 2 SQL: {e}")))?;
    let Expression::Select(sel) = ast else {
        return Err(unsupported(
            "Layer 2 currently accepts a single SELECT (Appendix A.1); joins and set ops come later",
        ));
    };
    if !sel.joins.is_empty() {
        return Err(unsupported(
            "Layer 2 consumer joins (Appendix A.4–A.6) are not implemented yet; use a single FROM relation",
        ));
    }
    if sel.with.is_some() || sel.distinct || sel.having.is_some() || sel.qualify.is_some() {
        return Err(unsupported(
            "Layer 2 WITH / DISTINCT / HAVING / QUALIFY are not implemented yet",
        ));
    }

    let (dataset, alias) = from_relation(graph, sel.from.as_ref())?;
    let ds_name = graph.datasets[dataset].name.clone();

    let mut metrics = Vec::new();
    let mut dimensions = Vec::new();
    // Select-list label (alias or default) → Layer 3 request field name.
    let mut outputs: Vec<(String, String)> = Vec::new();

    for expr in &sel.expressions {
        let (inner, out_name) = strip_alias(expr);
        match take_agg(inner)? {
            Some(measure) => {
                let metric = resolve_measure(graph, dataset, &ds_name, alias.as_deref(), measure)?;
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
                let dim = resolve_dim(graph, dataset, &ds_name, alias.as_deref(), col)?;
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
                    gb_dims.push(resolve_dim(
                        graph,
                        dataset,
                        &ds_name,
                        alias.as_deref(),
                        col,
                    )?);
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
        Some(w) => where_to_filters(graph, dataset, &ds_name, alias.as_deref(), &w.this)?,
    };

    let order_by = match &sel.order_by {
        None => vec![],
        Some(ob) => ob
            .expressions
            .iter()
            .map(|o| {
                let field = order_field(
                    graph,
                    dataset,
                    &ds_name,
                    alias.as_deref(),
                    &o.this,
                    &outputs,
                    &metrics,
                    &dimensions,
                )?;
                Ok(OrderBy {
                    field,
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

    Ok(Request {
        metrics,
        dimensions,
        filters,
        order_by,
        limit,
        ..Default::default()
    })
}

fn unsupported(msg: impl Into<String>) -> Error {
    Error::Unsupported(msg.into())
}

fn from_relation(
    graph: &SemanticGraph,
    from: Option<&polyglot_sql::expressions::From>,
) -> Result<(usize, Option<String>)> {
    let from = from.ok_or_else(|| Error::Request("Layer 2 query needs a FROM clause".into()))?;
    if from.expressions.len() != 1 {
        return Err(unsupported(
            "Layer 2 currently accepts one FROM relation (no commas / joins)",
        ));
    }
    table_expr(graph, &from.expressions[0])
}

fn table_expr(graph: &SemanticGraph, e: &Expression) -> Result<(usize, Option<String>)> {
    match e {
        Expression::Table(t) => resolve_table(graph, t),
        Expression::Alias(a) => {
            let (ds, _) = table_expr(graph, &a.this)?;
            Ok((ds, Some(a.alias.name.clone())))
        }
        _ => Err(unsupported(
            "Layer 2 FROM must be a model dataset (table or alias), not a subquery",
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

fn resolve_measure(
    graph: &SemanticGraph,
    dataset: usize,
    ds_name: &str,
    table_alias: Option<&str>,
    arg: &Expression,
) -> Result<String> {
    let col = as_column(arg).ok_or_else(|| {
        Error::Request(
            "AGG() argument must be a measure column reference (AGG(m) or AGG(alias.m)); \
             write AGG(a) + AGG(b), not AGG(a + b)"
                .into(),
        )
    })?;
    check_table_qual(col, ds_name, table_alias)?;
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

fn resolve_dim(
    graph: &SemanticGraph,
    dataset: usize,
    ds_name: &str,
    table_alias: Option<&str>,
    col: &Column,
) -> Result<String> {
    check_table_qual(col, ds_name, table_alias)?;
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

fn check_table_qual(col: &Column, ds_name: &str, table_alias: Option<&str>) -> Result<()> {
    let Some(t) = &col.table else {
        return Ok(());
    };
    let ok = t.name.eq_ignore_ascii_case(ds_name)
        || table_alias.is_some_and(|a| t.name.eq_ignore_ascii_case(a));
    if ok {
        Ok(())
    } else {
        Err(Error::Request(format!(
            "column `{}.{}` does not refer to the FROM relation `{ds_name}`",
            t.name, col.name.name
        )))
    }
}

fn where_to_filters(
    graph: &SemanticGraph,
    dataset: usize,
    ds_name: &str,
    table_alias: Option<&str>,
    e: &Expression,
) -> Result<Vec<Filter>> {
    match e {
        Expression::And(a) => {
            let mut out = where_to_filters(graph, dataset, ds_name, table_alias, &a.left)?;
            out.extend(where_to_filters(
                graph,
                dataset,
                ds_name,
                table_alias,
                &a.right,
            )?);
            Ok(out)
        }
        Expression::Eq(b) => {
            cmp_filter(graph, dataset, ds_name, table_alias, &b.left, &b.right, Op::Eq)
        }
        Expression::Neq(b) => {
            cmp_filter(graph, dataset, ds_name, table_alias, &b.left, &b.right, Op::Ne)
        }
        Expression::Lt(b) => {
            cmp_filter(graph, dataset, ds_name, table_alias, &b.left, &b.right, Op::Lt)
        }
        Expression::Lte(b) => {
            cmp_filter(graph, dataset, ds_name, table_alias, &b.left, &b.right, Op::Le)
        }
        Expression::Gt(b) => {
            cmp_filter(graph, dataset, ds_name, table_alias, &b.left, &b.right, Op::Gt)
        }
        Expression::Gte(b) => {
            cmp_filter(graph, dataset, ds_name, table_alias, &b.left, &b.right, Op::Ge)
        }
        Expression::IsNull(i) => {
            let col = as_column(&i.this).ok_or_else(simple_where)?;
            Ok(vec![Filter {
                field: resolve_dim(graph, dataset, ds_name, table_alias, col)?,
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
    dataset: usize,
    ds_name: &str,
    table_alias: Option<&str>,
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
        field: resolve_dim(graph, dataset, ds_name, table_alias, col)?,
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
                Error::Request(format!("Layer 2 LIMIT must be a non-negative integer, got {s}"))
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

fn order_field(
    graph: &SemanticGraph,
    dataset: usize,
    ds_name: &str,
    table_alias: Option<&str>,
    e: &Expression,
    outputs: &[(String, String)],
    metrics: &[String],
    dimensions: &[String],
) -> Result<String> {
    if let Some(arg) = take_agg(e)? {
        return resolve_measure(graph, dataset, ds_name, table_alias, arg);
    }
    if let Some(col) = as_column(e) {
        if col.table.is_none() {
            let n = &col.name.name;
            if let Some((_, req)) = outputs.iter().find(|(label, _)| label == n) {
                return Ok(req.clone());
            }
            if let Some(m) = metrics
                .iter()
                .find(|m| m.as_str() == n || m.ends_with(&format!(".{n}")))
            {
                return Ok(m.clone());
            }
            if let Some(d) = dimensions
                .iter()
                .find(|d| d.as_str() == n || d.ends_with(&format!(".{n}")))
            {
                return Ok(d.clone());
            }
        }
        return resolve_dim(graph, dataset, ds_name, table_alias, col);
    }
    Err(unsupported(
        "Layer 2 ORDER BY must name a selected dimension, measure, or AGG(...)",
    ))
}
