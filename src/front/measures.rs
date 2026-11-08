//! Which select items are measures: `TO MEASURE` classification, base and
//! plain aggregates, bare measures, and what makes a query aggregated (§7.2).

use polyglot_sql::expressions::{Column, Identifier, Select};
use polyglot_sql::{contains_aggregate, get_columns, transform_all, Expression, ExpressionWalk};

use crate::graph::{self, SemanticGraph};
use crate::plan::{self};
use crate::{Error, Result};

use super::{
    resolve_column_dataset, strip_alias, to_measure, unsupported, unwrap_paren, EphemeralMetric,
    QueryScope,
};

/// Whether a query aggregates (§7.2): `GROUP BY`, `HAVING`, or an aggregate in its
/// select list. A measure definition's own aggregate doesn't count: it's cut out of
/// the text as a `TO MEASURE` marker before this runs.
pub(super) fn is_aggregated(sel: &Select) -> bool {
    sel.group_by.is_some() || sel.having.is_some() || select_aggregates(sel)
}

/// The select list holds an aggregate or an `AGG()` read. A `TO MEASURE`
/// definition is a marker column here, so it doesn't count (§4.2). One rule for
/// both §7.2's refusal and the `GROUP BY` requirement.
pub(super) fn select_aggregates(sel: &Select) -> bool {
    sel.expressions.iter().any(|e| {
        let (inner, _) = strip_alias(e);
        contains_aggregate(inner) || inner.contains(is_measure_agg_expr)
    })
}

/// §7.2's refusal of an unevaluated measure in an aggregated query.
/// `what` names the measure, or where it comes from.
pub(super) fn aggregated_measure(what: &str) -> Error {
    Error::Request(format!(
        "Layer 2 query aggregates, so it can't return an unevaluated measure ({what}) (relational_semantics §7.2): read it with AGG(), or carry it through a query without GROUP BY, HAVING or aggregates"
    ))
}

/// `Some` when `e` is one base aggregate to install as a dataset metric.
/// `None` when it is a composition of `AGG(measure)` (the caller parses that).
pub(super) fn classify_to_measure(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
    base_seq: &mut usize,
) -> Result<Option<EphemeralMetric>> {
    let raw = e.contains(is_base_agg_expr);
    let measure = e.contains(is_measure_agg_expr);
    if raw && measure {
        return Err(unsupported(
            "Layer 2 TO MEASURE cannot mix AGG(measure) with a raw SQL aggregate (SUM, COUNT, AVG, MIN, or MAX); use one base aggregate, or a parenthesized composition of AGG()",
        ));
    }
    if raw {
        if !is_single_base_aggregate(e) {
            return Err(unsupported(
                "Layer 2 TO MEASURE accepts one SUM|COUNT|AVG|MIN|MAX(...) or a parenthesized composition of AGG(measure); compound raw aggregates are not supported",
            ));
        }
        let (owner, body) = base_measure_body(graph, scope, e)?;
        let bare = format!("__l2_bm{base_seq}");
        *base_seq += 1;
        let dataset = graph.datasets[owner].name.clone();
        let name = format!("{dataset}.{bare}");
        return Ok(Some(EphemeralMetric {
            name,
            owner,
            body,
            dataset,
            bare,
            raw: false,
        }));
    }
    if !measure {
        return Err(unsupported(
            "Layer 2 TO MEASURE needs one SUM|COUNT|AVG|MIN|MAX(...) or a parenthesized expression over AGG(measure)",
        ));
    }
    Ok(None)
}

pub(super) fn is_base_agg_expr(e: &Expression) -> bool {
    match e {
        Expression::Sum(_)
        | Expression::Avg(_)
        | Expression::Min(_)
        | Expression::Max(_)
        | Expression::Count(_) => true,
        Expression::Function(f) if to_measure::is_base_agg_word(&f.name) => true,
        Expression::AggregateFunction(f) if to_measure::is_base_agg_word(&f.name) => true,
        _ => false,
    }
}

pub(super) fn is_measure_agg_expr(e: &Expression) -> bool {
    match e {
        Expression::Function(f) if is_agg_name(&f.name) => true,
        Expression::AggregateFunction(f) if is_agg_name(&f.name) => true,
        _ => false,
    }
}

/// The whole item is one base aggregate, with no nested aggregate inside it.
pub(super) fn is_single_base_aggregate(e: &Expression) -> bool {
    let e = unwrap_paren(e);
    is_base_agg_expr(e) && e.count(is_base_agg_expr) == 1 && !e.contains(is_measure_agg_expr)
}

/// The dataset that owns the aggregate, and the metric body (field names unqualified).
pub(super) fn base_measure_body(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
) -> Result<(usize, Expression)> {
    let e = unwrap_paren(e);
    let mut owner: Option<usize> = None;
    for c in get_columns(e) {
        let Expression::Column(col) = c else {
            continue;
        };
        let ds = resolve_column_dataset(graph, scope, col)?;
        let _ = dataset_field(graph, ds, col)?;
        match owner {
            None => owner = Some(ds),
            Some(o) if o != ds => {
                return Err(unsupported(
                    "Layer 2 TO MEASURE base aggregate must use fields of a single dataset",
                ));
            }
            _ => {}
        }
    }
    let owner = match owner {
        Some(ds) => ds,
        None => grain_dataset(graph, scope)?,
    };
    let body = transform_all(e.clone(), &|node| {
        let Expression::Column(mut c) = node else {
            return Ok(node);
        };
        let canon = dataset_field(graph, owner, &c)
            .map_err(|err| polyglot_sql::Error::InvalidInput(err.to_string()))?;
        c.table = None;
        c.name = Identifier::new(canon);
        Ok(Expression::Column(c))
    })
    .map_err(|err| Error::Internal(format!("Layer 2 TO MEASURE base aggregate: {err}")))?;
    Ok((owner, body))
}

pub(super) fn dataset_field(graph: &SemanticGraph, ds: usize, col: &Column) -> Result<String> {
    let ds_name = &graph.datasets[ds].name;
    let label = match &col.table {
        Some(t) => format!("{}.{}", t.name, col.name.name),
        None => col.name.name.clone(),
    };
    match graph::lookup(
        graph.datasets[ds].fields.iter().map(|f| f.name.as_str()),
        &col.name.name,
        col.name.quoted,
    )
    .map_err(Error::Request)?
    {
        Some(i) => Ok(graph.datasets[ds].fields[i].name.clone()),
        None => {
            let is_measure = graph.metrics.iter().any(|m| {
                m.owner == Some(ds)
                    && (m.name.eq_ignore_ascii_case(&col.name.name)
                        || m.name
                            .eq_ignore_ascii_case(&format!("{ds_name}.{}", col.name.name)))
            });
            if is_measure {
                Err(unsupported(format!(
                    "Layer 2 aggregate argument `{label}` is a measure on `{ds_name}`, not a field; name a field (SUM(amount)) or read the measure with AGG()"
                )))
            } else {
                Err(Error::Request(format!(
                    "Layer 2 aggregate argument `{label}` is not a field of `{ds_name}`"
                )))
            }
        }
    }
}

/// `COUNT(*)` (and any aggregate with no field) counts one dataset's grain.
pub(super) fn grain_dataset(graph: &SemanticGraph, scope: &QueryScope) -> Result<usize> {
    match scope.bindings.as_slice() {
        [one] => Ok(one.dataset),
        [] => Err(Error::Request(
            "Layer 2 COUNT(*) TO MEASURE needs a FROM dataset whose grain it counts".into(),
        )),
        many => {
            let names = many
                .iter()
                .map(|b| graph.datasets[b.dataset].name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(unsupported(format!(
                "Layer 2 COUNT(*) TO MEASURE counts one dataset's grain, but FROM joins several datasets ({names}); name a column of that dataset (for example COUNT(orders.id))"
            )))
        }
    }
}

/// `dataset.field` → `field`.
pub(super) fn bare_field(dim: &str) -> &str {
    plan::unquoted(dim.split_once('.').map_or(dim, |(_, f)| f))
}

/// A bare column that names a dataset-scoped measure, not a field: the dataset
/// it is a measure of (the unevaluated-measure pass-through of spec §7.7).
pub(super) fn bare_measure(
    graph: &SemanticGraph,
    scope: &QueryScope,
    col: &Column,
) -> Option<usize> {
    let hit = scope.bindings.iter().find(|b| {
        let ds = &graph.datasets[b.dataset].name;
        let qualified_ok = col.table.as_ref().is_none_or(|t| {
            t.name.eq_ignore_ascii_case(&b.alias) || t.name.eq_ignore_ascii_case(ds)
        });
        qualified_ok
            && super::resolve::dataset_measure(graph, b.dataset, &col.name.name, col.name.quoted)
                .is_ok_and(|m| m.is_some())
    })?;
    Some(hit.dataset)
}

pub(super) fn is_model_metric(graph: &SemanticGraph, name: &str) -> bool {
    graph
        .metrics
        .iter()
        .any(|m| m.owner.is_none() && m.name.eq_ignore_ascii_case(name))
}

pub(super) fn model_scoped(measure: &str) -> Error {
    Error::Request(format!(
        "`{measure}` is a model-scoped metric; Layer 2 only exposes dataset-scoped metrics as measures (relational_semantics §10.3)"
    ))
}

pub(super) fn take_agg(e: &Expression) -> Result<Option<&Expression>> {
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

pub(super) fn is_agg_name(name: &str) -> bool {
    // Spec §6: AGG is normative; MEASURE is a non-normative synonym.
    matches!(name.to_ascii_uppercase().as_str(), "AGG" | "MEASURE")
}

/// The plain aggregates (§7.2) of one stage, wherever they appear: the select
/// list, `HAVING` or `ORDER BY`. Each becomes an ephemeral metric marked raw,
/// evaluated over the query's joined rows as written, so a repeated or
/// NULL-extended row counts, as in plain SQL.
pub(super) struct RawAggs<'a> {
    pub seq: &'a mut usize,
    pub made: &'a mut Vec<EphemeralMetric>,
}

impl RawAggs<'_> {
    /// The raw metric for one `SUM|COUNT|AVG|MIN|MAX(...)`.
    pub(super) fn metric(
        &mut self,
        graph: &SemanticGraph,
        scope: &QueryScope,
        e: &Expression,
    ) -> Result<String> {
        let e = unwrap_paren(e);
        if e.count(is_base_agg_expr) != 1 || e.contains(is_measure_agg_expr) {
            return Err(unsupported(
                "Layer 2 plain aggregates can't nest: an aggregate's argument is a row expression, with no aggregate or AGG() inside it",
            ));
        }
        // `COUNT(*)` reads no field: it counts the joined rows whichever
        // dataset holds it, so it sits on the first one in FROM.
        let (owner, body) = match scope.bindings.first() {
            Some(b) if get_columns(e).is_empty() => (b.dataset, e.clone()),
            _ => base_measure_body(graph, scope, e)?,
        };
        let bare = format!("__l2_bm{}", *self.seq);
        *self.seq += 1;
        let dataset = graph.datasets[owner].name.clone();
        let metric = format!("{dataset}.{bare}");
        self.made.push(EphemeralMetric {
            name: metric.clone(),
            owner,
            body,
            dataset,
            bare,
            raw: true,
        });
        Ok(metric)
    }
}
