//! One `AGG` stage: its `FROM`, select list, `WHERE`, `GROUP BY`, `HAVING`,
//! `ORDER BY` and `LIMIT`, parsed into a request for the planner.

use polyglot_sql::expressions::Select;
use polyglot_sql::{contains_aggregate, DialectType, Expression, ExpressionWalk};

use crate::graph::SemanticGraph;
use crate::plan::{Direction, Nulls, OrderBy, Request};
use crate::{Error, Result};

use super::{
    aggregated_measure, as_column, bare_field, bare_measure, classify_to_measure,
    default_measure_name, formula_leaves, is_aggregated, is_base_agg_expr, is_measure_agg_expr,
    is_model_metric, limit_u64, model_scoped, order_field, order_key, parse_formula_or_agg,
    parse_formula_with_plain, parse_from_clause, parse_having, parse_measure_formula, resolve_agg,
    resolve_dim, resolve_measure, strip_alias, take_agg, to_measure, unsupported, unwrap_paren,
    where_to_filters, Conjunct, EphemeralMetric, L2Formula, Layer2Parsed, OrderCtx, OrderExpr,
    OutCol, ParsedFrom, RawAggs, StageCfg, UnevaluatedSpec,
};

/// `GROUP BY ALL` (§7.4) as the list it stands for: every selected column that isn't
/// an aggregate. A bare measure stays in the list, and is refused as `GROUP BY m` is.
pub(super) fn expand_group_by_all(sel: &mut Select) -> Result<()> {
    let is_all = sel
        .group_by
        .as_ref()
        .is_some_and(|gb| gb.all == Some(true) && gb.expressions.is_empty());
    if !is_all {
        return Ok(());
    }
    let mut columns = vec![];
    for item in &sel.expressions {
        let (inner, _) = strip_alias(item);
        if matches!(inner, Expression::Star(_)) {
            return Err(unsupported(
                "Layer 2 GROUP BY ALL needs its columns named, not `*`",
            ));
        }
        let is_measure_definition =
            as_column(inner).is_some_and(|c| to_measure::marker_index(c).is_some());
        let aggregated = contains_aggregate(inner) || inner.contains(is_measure_agg_expr);
        if !is_measure_definition && !aggregated {
            columns.push(inner.clone());
        }
    }
    match (&mut sel.group_by, columns.is_empty()) {
        (group_by, true) => *group_by = None,
        (Some(gb), false) => {
            gb.all = None;
            gb.expressions = columns;
        }
        (None, false) => unreachable!("checked above"),
    }
    Ok(())
}

/// Clause checks and the `FROM` / `JOIN` list of one `AGG` stage.
pub(super) fn parse_stage_prep(
    graph: &SemanticGraph,
    sel: &Select,
    cfg: &StageCfg<'_>,
) -> Result<ParsedFrom> {
    if sel.distinct_on.is_some() || sel.qualify.is_some() {
        return Err(unsupported(
            "Layer 2 DISTINCT ON / QUALIFY are not implemented yet",
        ));
    }
    parse_from_clause(graph, sel.from.as_ref(), &sel.joins, cfg.outer)
}

/// Aliases a stage's `FROM` introduces: what a nested query must not reference.
pub(super) fn stage_aliases(graph: &SemanticGraph, from: &ParsedFrom) -> Vec<String> {
    let scope = &from.scope;
    scope
        .bindings
        .iter()
        .flat_map(|b| [b.alias.clone(), graph.datasets[b.dataset].name.clone()])
        .chain(scope.derived.iter().cloned())
        .collect()
}

/// One `AGG` stage: select list, `FROM`/`JOIN`, `WHERE`, `GROUP BY`, `ORDER BY`, `LIMIT`.
/// `derived_outs` are the compiled columns of the derived tables in `FROM`.
pub(super) fn parse_stage_from(
    graph: &SemanticGraph,
    sel: &Select,
    cfg: &StageCfg<'_>,
    mut parsed_from: ParsedFrom,
    derived_outs: &[(String, Vec<OutCol>)],
    base_seq: &mut usize,
) -> Result<Layer2Parsed> {
    parsed_from.scope.derived_outs = derived_outs.to_vec();
    let scope = &parsed_from.scope;

    let mut metrics: Vec<String> = Vec::new();
    let mut dimensions = Vec::new();
    let mut measure_outputs = Vec::new();
    let mut dim_labels: Vec<(String, String)> = Vec::new();
    let mut unevaluated: Vec<UnevaluatedSpec> = Vec::new();
    let mut ephemeral: Vec<EphemeralMetric> = Vec::new();
    // Select-list label (alias or default) → order-by / output resolution name.
    let mut outputs: Vec<(String, String)> = Vec::new();

    for (idx, expr) in sel.expressions.iter().enumerate() {
        let (inner, alias) = strip_alias(expr);
        if let Some(col) = as_column(inner) {
            if let Some(n) = to_measure::marker_index(col) {
                let name = alias
                    .ok_or_else(|| Error::Request("Layer 2 TO MEASURE needs `AS <name>`".into()))?;
                if !cfg.derived {
                    return Err(unsupported(
                        "Layer 2 TO MEASURE defines a measure for an outer AGG(): use it in a derived table's select list",
                    ));
                }
                let e = cfg
                    .to_measures
                    .get(n)
                    .ok_or_else(|| Error::Internal("unknown TO MEASURE marker".into()))?;
                if let Some(ep) = classify_to_measure(graph, scope, e, base_seq)? {
                    let metric = ep.name.clone();
                    ephemeral.push(ep);
                    // Pass-through: nothing is added to this stage's request. The
                    // outer AGG evaluates the ephemeral metric at its own grain.
                    unevaluated.push(UnevaluatedSpec {
                        name: name.clone(),
                        metric: metric.clone(),
                        formula: None,
                    });
                    outputs.push((name, metric));
                    continue;
                }
                let formula = parse_formula_or_agg(graph, scope, e)?;
                let leaves = formula_leaves(&formula);
                if leaves.is_empty() {
                    return Err(unsupported(
                        "Layer 2 TO MEASURE needs an expression over AGG(measure) leaves",
                    ));
                }
                for leaf in leaves {
                    if !metrics.contains(&leaf) {
                        metrics.push(leaf);
                    }
                }
                // Evaluated physically at this grain (so the stage keeps a column),
                // but exposed to the outer query as an unevaluated measure.
                measure_outputs.push((name.clone(), formula.clone()));
                outputs.push((name.clone(), name.clone()));
                unevaluated.push(UnevaluatedSpec {
                    metric: name.clone(),
                    name,
                    formula: Some(formula),
                });
                continue;
            }
        }
        if inner.contains(is_base_agg_expr) {
            // §7.2: ordinary aggregates over fields, beside the measures, alone or
            // in arithmetic with each other and with AGG(). Each is a raw leaf:
            // evaluated over the joined rows as written.
            if cfg.derived && alias.is_none() {
                return Err(unsupported(
                    "Layer 2 derived table: every aggregate needs an explicit alias so the outer query can name it (e.g. MAX(order_date) AS last_order)",
                ));
            }
            let mut raw = RawAggs {
                seq: &mut *base_seq,
                made: &mut ephemeral,
            };
            let formula = parse_formula_with_plain(graph, scope, inner, &mut raw)?;
            for leaf in formula_leaves(&formula) {
                if !metrics.contains(&leaf) {
                    metrics.push(leaf);
                }
            }
            let out_name = match alias.clone() {
                Some(a) => a,
                None => polyglot_sql::generate(inner, DialectType::Generic)
                    .map_err(|e| Error::Internal(e.to_string()))?,
            };
            measure_outputs.push((out_name.clone(), formula));
            outputs.push((out_name.clone(), out_name));
            continue;
        }
        if let Some(formula) = parse_measure_formula(graph, scope, inner)? {
            for leaf in formula_leaves(&formula) {
                if !metrics.contains(&leaf) {
                    metrics.push(leaf.clone());
                }
            }
            if cfg.derived && alias.is_none() {
                return Err(unsupported(
                    "Layer 2 derived table: every AGG(...) item needs an explicit alias so the outer query can name it (e.g. AGG(order_count) AS n)",
                ));
            }
            // An unaliased `AGG(t.m)` is named by its final part, `m`, as any
            // column is (§3.1), even when `m` is a view's ratio.
            let agg_column = take_agg(unwrap_paren(inner))
                .ok()
                .flatten()
                .and_then(as_column)
                .map(|c| c.name.name.clone());
            let out_name = alias
                .clone()
                .or(agg_column)
                .unwrap_or_else(|| default_measure_name(&formula, idx));
            let order_ref = match &formula {
                L2Formula::Agg(m) => m.clone(),
                _ => out_name.clone(),
            };
            measure_outputs.push((out_name.clone(), formula));
            outputs.push((out_name.clone(), order_ref));
            continue;
        }
        let col = as_column(inner).ok_or_else(|| {
            unsupported(
                "Layer 2 select list may only contain AGG(measure) expressions, dimension columns, and + - * / arithmetic on AGG(...)",
            )
        })?;
        let dim = match resolve_dim(graph, scope, col) {
            Ok(d) => d,
            Err(e) => {
                // A bare measure column: §7.7 pass-through (derived tables only).
                if let Some(ds) = bare_measure(graph, scope, col) {
                    let ds_name = &graph.datasets[ds].name;
                    if !cfg.derived {
                        return Err(unsupported(format!(
                            "Layer 2 bare measure `{}` on `{ds_name}` at the top level: only a derived table's select list may pass a measure through for an outer AGG() (relational_semantics §7.7); evaluate it with AGG({0}) AS <alias>",
                            col.name.name
                        )));
                    }
                    let metric = resolve_measure(graph, scope, inner)?;
                    let name = alias.unwrap_or_else(|| col.name.name.clone());
                    unevaluated.push(UnevaluatedSpec {
                        name: name.clone(),
                        metric: metric.clone(),
                        formula: None,
                    });
                    outputs.push((name, metric));
                    continue;
                }
                if col.table.is_none() && is_model_metric(graph, &col.name.name) {
                    return Err(model_scoped(&col.name.name));
                }
                return Err(e);
            }
        };
        // As in standard SQL (§3.1), a column is named by its final part: `status`.
        let label = alias.unwrap_or_else(|| bare_field(&dim).to_string());
        match dim_labels.iter().find(|(d, _)| d == &dim) {
            Some((_, first)) if first != &label => {
                return Err(unsupported(format!(
                    "Layer 2 selects `{dim}` twice under different names (`{first}`, `{label}`); select it once"
                )));
            }
            Some(_) => {}
            None => dim_labels.push((dim.clone(), label.clone())),
        }
        if !dimensions.iter().any(|d| d == &dim) {
            dimensions.push(dim.clone());
        }
        outputs.push((label, dim));
    }

    if metrics.is_empty() && dimensions.is_empty() {
        return Err(Error::Request(
            "Layer 2 query must select at least one AGG(measure) or dimension".into(),
        ));
    }
    // Column names within a relation MUST be unique (§3.1), and not every engine
    // takes two output columns with one name.
    for (i, (label, _)) in outputs.iter().enumerate() {
        if outputs[..i]
            .iter()
            .any(|(p, _)| p.eq_ignore_ascii_case(label))
        {
            return Err(Error::Request(format!(
                "Layer 2 query has two output columns named `{label}`; give one an alias"
            )));
        }
    }

    // `GROUP BY` must name exactly the selected dimensions. With `AGG` it is
    // required whenever dimensions are selected; without `AGG` it is optional
    // (a dimension listing is one row per combination anyway: Appendix A.3's `d`).
    // A measure definition reads `AGG` without aggregating the query (§4.2), so a
    // relation of fields and definitions needs none either: Appendix A.7's view.
    let aggregates = super::measures::select_aggregates(sel);
    match &sel.group_by {
        None if aggregates && !dimensions.is_empty() => {
            return Err(Error::Request(
                "Layer 2 aggregated query with dimensions needs GROUP BY".into(),
            ));
        }
        None => {}
        Some(gb) => {
            if gb.all.is_some() {
                return Err(unsupported(
                    "Layer 2 GROUP BY takes ALL on its own, and not DISTINCT",
                ));
            }
            let mut gb_dims = Vec::new();
            for e in &gb.expressions {
                let col = as_column(e)
                    .ok_or_else(|| unsupported("Layer 2 GROUP BY must name dimension columns"))?;
                gb_dims.push(resolve_dim(graph, scope, col).map_err(|e| {
                    if bare_measure(graph, scope, col).is_some() {
                        Error::Request(format!(
                            "Layer 2 GROUP BY cannot name measure `{}`; group by dimensions only",
                            col.name.name
                        ))
                    } else {
                        e
                    }
                })?);
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

    let mut filters = vec![];
    let mut predicates = vec![];
    if let Some(w) = &sel.where_clause {
        for conjunct in where_to_filters(graph, scope, &w.this)? {
            match conjunct {
                Conjunct::Filter(f) => filters.push(f),
                Conjunct::Predicate(p) => predicates.push(format!("({p})")),
            }
        }
    }
    let having = match &sel.having {
        None => None,
        Some(h) => {
            if sel.group_by.is_none() && !dimensions.is_empty() {
                return Err(Error::Request(
                    "Layer 2 HAVING makes a query aggregated, so its selected columns need GROUP BY".into(),
                ));
            }
            let mut raw = RawAggs {
                seq: &mut *base_seq,
                made: &mut ephemeral,
            };
            let (pushed, parsed) = parse_having(graph, scope, &h.this, &dimensions, &mut raw)?;
            for conjunct in pushed {
                match conjunct {
                    Conjunct::Filter(f) => filters.push(f),
                    Conjunct::Predicate(p) => predicates.push(format!("({p})")),
                }
            }
            if let Some(p) = &parsed {
                for leaf in p.measures.iter().flat_map(formula_leaves) {
                    if !metrics.contains(&leaf) {
                        metrics.push(leaf);
                    }
                }
            }
            parsed
        }
    };
    let filter = (!predicates.is_empty()).then(|| predicates.join(" AND "));

    let selected_measure = |m: &str| {
        measure_outputs
            .iter()
            .any(|(_, f)| matches!(f, L2Formula::Agg(x) if x == m))
    };
    let order_ctx = OrderCtx {
        graph,
        scope,
        outputs: &outputs,
        metrics: &metrics,
        dimensions: &dimensions,
    };
    let mut order_by = vec![];
    let mut order_exprs: Vec<OrderExpr> = vec![];
    for (position, o) in sel
        .order_by
        .iter()
        .flat_map(|ob| ob.expressions.iter())
        .enumerate()
    {
        let direction = if o.desc {
            Direction::Desc
        } else {
            Direction::Asc
        };
        let nulls = match o.nulls_first {
            Some(true) => Some(Nulls::First),
            Some(false) => Some(Nulls::Last),
            None => None,
        };
        // A selected measure or a column sorts by name; anything else by value.
        let arg = take_agg(&o.this)?;
        let by_name = match arg {
            Some(arg) => matches!(
                resolve_agg(graph, scope, arg)?,
                L2Formula::Agg(m) if selected_measure(&m)
            ),
            None => as_column(&o.this).is_some(),
        };
        if by_name {
            order_by.push(OrderBy {
                field: order_field(&order_ctx, &o.this)?,
                direction,
                nulls,
            });
        } else {
            order_exprs.push(OrderExpr {
                position,
                key: order_key(
                    graph,
                    scope,
                    &o.this,
                    &dimensions,
                    &mut RawAggs {
                        seq: &mut *base_seq,
                        made: &mut ephemeral,
                    },
                )?,
                direction,
                nulls,
            });
        }
    }
    // §7.2: an aggregated query reports values, and an unevaluated measure isn't one.
    // A measure passes through, or is defined, only in a query that doesn't aggregate.
    if let Some(m) = unevaluated.first().filter(|_| is_aggregated(sel)) {
        return Err(aggregated_measure(&format!("`{}`", m.name)));
    }
    // §7.8: `DISTINCT` compares whole rows, and an unevaluated measure has no value.
    // Otherwise it changes nothing: `GROUP BY` must name exactly the selected
    // dimensions, so a stage's rows are already distinct.
    if sel.distinct {
        if let Some(m) = unevaluated.first() {
            return Err(Error::Request(format!(
                "Layer 2 DISTINCT can't compare unevaluated measure `{}`; evaluate it with AGG() or leave it out",
                m.name
            )));
        }
        if !order_exprs.is_empty() {
            return Err(Error::Request(
                "Layer 2 DISTINCT can only sort by columns it selects".into(),
            ));
        }
    }
    if !order_exprs.is_empty() {
        let reads_measure = order_exprs.iter().any(|o| !o.key.measures.is_empty());
        if reads_measure && sel.group_by.is_none() && !dimensions.is_empty() {
            return Err(Error::Request(
                "Layer 2 ORDER BY AGG() makes a query aggregated, so its selected columns need GROUP BY".into(),
            ));
        }
        for leaf in order_exprs
            .iter()
            .flat_map(|o| o.key.measures.iter())
            .flat_map(formula_leaves)
        {
            if !metrics.contains(&leaf) {
                metrics.push(leaf);
            }
        }
    }

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
            metrics: metrics.into_iter().map(Into::into).collect(),
            dimensions: dimensions.into_iter().map(Into::into).collect(),
            filter,
            filters,
            order_by,
            limit,
            ..Default::default()
        },
        consumer_joins: parsed_from.consumer_joins,
        measure_outputs,
        dim_labels,
        having,
        order_exprs,
        derived: parsed_from.derived,
        unevaluated,
        ephemeral,
    })
}
