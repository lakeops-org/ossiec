//! Planning: validate a request against the graph and decide how to compute it.
//!
//! 1. Resolve every name in the request.
//! 2. Pick the **join root**: the dataset from which every referenced dataset
//!    is reachable along many-to-one relationships. Joins only ever go from
//!    the many side to the one side, so they never multiply root rows.
//! 3. Classify each aggregate term:
//!    - `Inline`: it reads the root, or it is duplicate-insensitive (`MIN`,
//!      `MAX`, `COUNT(DISTINCT ...)`). Correct over the joined rows as-is.
//!    - `PkDedup`: it only reads one-side datasets (`SUM(store.employees)`
//!      over a `store_sales` root). Joined rows repeat those values, so the
//!      term is computed over rows de-duplicated on its dataset's primary key.

use std::collections::BTreeSet;

use polyglot_sql::{transform, Expression};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::{Ctx, SemanticGraph, Term, DIM, TERM};

/// Placeholder for requested dimension `i` inside a metric's outer expression.
pub(crate) const OUT_DIM: &str = "__ossiec_q";
use crate::sql::{self, Tpl};
use crate::{Error, Result};

/// A semantic query: which metrics, sliced by which dimensions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    /// Ignored by the compiler; lets callers keep the target model in the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `metric` (model-scoped) or `dataset.metric` (dataset-scoped). Empty to
    /// list the distinct values of `dimensions` instead.
    #[serde(default)]
    pub metrics: Vec<String>,
    /// `dataset.field`.
    #[serde(default)]
    pub dimensions: Vec<String>,
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Conditions on requested metrics (SQL `HAVING`).
    #[serde(default)]
    pub metric_filters: Vec<MetricFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_grain: Option<TimeGrain>,
    /// Which requested dimension `timeGrain` applies to, when several are time dimensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_dimension: Option<String>,
    #[serde(default)]
    pub order_by: Vec<OrderBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub field: String,
    pub op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricFilter {
    pub metric: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderBy {
    pub field: String,
    #[serde(default)]
    pub direction: Direction,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    #[serde(alias = "ASC")]
    Asc,
    #[serde(alias = "DESC")]
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    #[serde(rename = "=")]
    Eq,
    #[serde(rename = "!=", alias = "<>")]
    Ne,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "IN", alias = "in")]
    In,
    #[serde(rename = "NOT IN", alias = "not in")]
    NotIn,
    #[serde(rename = "LIKE", alias = "like")]
    Like,
    #[serde(rename = "BETWEEN", alias = "between")]
    Between,
    #[serde(rename = "IS NULL", alias = "is null")]
    IsNull,
    #[serde(rename = "IS NOT NULL", alias = "is not null")]
    IsNotNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeGrain {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl TimeGrain {
    fn as_str(self) -> &'static str {
        match self {
            TimeGrain::Day => "day",
            TimeGrain::Week => "week",
            TimeGrain::Month => "month",
            TimeGrain::Quarter => "quarter",
            TimeGrain::Year => "year",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strategy {
    Inline,
    PkDedup { grain: usize },
}

pub(crate) struct PlannedTerm {
    pub term: Term,
    pub strategy: Strategy,
}

/// Everything `lower` needs; no SQL decisions are left.
pub(crate) struct LogicalPlan {
    pub root: usize,
    /// Relationship indices, in join order.
    pub joins: Vec<usize>,
    /// (output name, expression with time grain applied).
    pub dims: Vec<(String, Expression)>,
    pub filters: Vec<Expression>,
    pub terms: Vec<PlannedTerm>,
    /// (output name, expression over `__ossiec_t{term}` placeholders).
    pub outputs: Vec<(String, Expression)>,
    /// Metric predicates, over the metric expression and over the output column.
    pub metric_filters: Vec<(Expression, Expression)>,
    /// Some output uses a window function. Metric filters then apply to the
    /// finished rows (after windows), not to groups.
    pub windowed: bool,
    pub order: Vec<(String, Direction)>,
    pub limit: Option<u64>,
}

fn req_err(m: impl Into<String>) -> Error {
    Error::Request(m.into())
}

pub(crate) fn plan(g: &SemanticGraph, req: &Request, ctx: &mut Ctx) -> Result<LogicalPlan> {
    // No metrics: the request lists dimension values (distinct combinations).
    if req.metrics.is_empty() && req.dimensions.is_empty() {
        return Err(req_err("ask for at least one metric or dimension"));
    }

    // Dimensions.
    let mut dim_refs: Vec<(String, usize, usize)> = vec![];
    for d in &req.dimensions {
        let (ds, f) = g.field_by_name(d)?;
        let name = format!("{}.{}", g.datasets[ds].name, g.datasets[ds].fields[f].name);
        if !dim_refs.iter().any(|(n, ..)| *n == name) {
            dim_refs.push((name, ds, f));
        }
    }
    let grained = match (req.time_grain, &req.time_dimension) {
        (None, None) => None,
        (None, Some(_)) => return Err(req_err("timeDimension needs a timeGrain")),
        (Some(grain), Some(td)) => {
            let (ds, f) = g.field_by_name(td)?;
            let i = dim_refs
                .iter()
                .position(|(_, d, ff)| (*d, *ff) == (ds, f))
                .ok_or_else(|| {
                    req_err(format!(
                        "timeDimension `{td}` must also be a requested dimension"
                    ))
                })?;
            Some((i, grain))
        }
        (Some(grain), None) => {
            let times: Vec<usize> = (0..dim_refs.len())
                .filter(|&i| g.datasets[dim_refs[i].1].fields[dim_refs[i].2].is_time)
                .collect();
            match times.as_slice() {
                [one] => Some((*one, grain)),
                [] => return Err(req_err("timeGrain needs a requested time dimension (a field with a temporal datatype or dimension.is_time)")),
                _ => return Err(req_err("several time dimensions are requested; set timeDimension to pick the one timeGrain applies to")),
            }
        }
    };
    let mut dims = vec![];
    for (i, (name, ds, f)) in dim_refs.iter().enumerate() {
        let mut e = g.field_sql(*ds, *f, ctx);
        if let Some((gi, grain)) = grained {
            if gi == i {
                let mut t = Tpl::new();
                let x = t.e(e);
                // A Date field stays a Date: Postgres would return timestamptz and
                // StarRocks DATETIME (engine tests: month_grain_with_date_range).
                let is_date = g.datasets[*ds].fields[*f]
                    .datatype
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case("date"));
                let trunc = format!("DATE_TRUNC('{}', {x})", grain.as_str());
                t.push(&if is_date {
                    format!("CAST({trunc} AS DATE)")
                } else {
                    trunc
                });
                e = t.build()?;
            }
        }
        dims.push((name.clone(), e));
    }

    // Metrics: lift aggregate terms, sharing identical ones.
    let mut terms: Vec<Term> = vec![];
    let mut outputs: Vec<(String, Expression)> = vec![];
    let mut windowed = false;
    for m in &req.metrics {
        let mi = g.metric_by_name(m)?;
        let name = g.metrics[mi].name.clone();
        if outputs.iter().any(|(n, _)| *n == name) {
            continue;
        }
        let rm = g.resolve_metric(mi, ctx)?;
        windowed |= rm.windowed;
        // A window partitions or orders by fields; each must be an output row key.
        let dim_index: Vec<usize> = rm
            .dims
            .iter()
            .map(|&(ds, f)| {
                dim_refs
                    .iter()
                    .position(|(_, d, ff)| (*d, *ff) == (ds, f))
                    .ok_or_else(|| {
                        req_err(format!(
                            "metric `{name}` is a window over `{}.{}`; add it to `dimensions`",
                            g.datasets[ds].name, g.datasets[ds].fields[f].name
                        ))
                    })
            })
            .collect::<Result<_>>()?;
        let global: Vec<usize> = rm
            .terms
            .into_iter()
            .map(|t| match terms.iter().position(|x| x.key == t.key) {
                Some(i) => i,
                None => {
                    terms.push(t);
                    terms.len() - 1
                }
            })
            .collect();
        outputs.push((name, renumber(rm.outer, &global, &dim_index)));
    }

    // Filters.
    let mut required: BTreeSet<usize> = dim_refs.iter().map(|(_, ds, _)| *ds).collect();
    let mut filters = vec![];
    for f in &req.filters {
        let (ds, fi) = g.field_by_name(&f.field)?;
        required.insert(ds);
        let lhs = g.field_sql(ds, fi, ctx);
        let datatype = g.datasets[ds].fields[fi].datatype.as_deref();
        filters.push(predicate(lhs, f.op, f.value.as_ref(), &f.values, datatype)?);
    }
    let mut metric_filters = vec![];
    for mf in &req.metric_filters {
        let mi = g.metric_by_name(&mf.metric)?;
        let (_, outer) = outputs
            .iter()
            .find(|(n, _)| *n == g.metrics[mi].name)
            .ok_or_else(|| {
                req_err(format!(
                    "metricFilters: `{}` must also be a requested metric",
                    mf.metric
                ))
            })?;
        if !matches!(mf.value, Value::Number(_)) {
            return Err(req_err(format!(
                "metricFilters: `{}` compares to a number",
                mf.metric
            )));
        }
        metric_filters.push((
            predicate(outer.clone(), mf.op, Some(&mf.value), &[], None)?,
            predicate(
                sql::quoted_col(&g.metrics[mi].name),
                mf.op,
                Some(&mf.value),
                &[],
                None,
            )?,
        ));
    }

    // Order and limit refer to output names.
    let known: Vec<&str> = dims
        .iter()
        .map(|(n, _)| n.as_str())
        .chain(outputs.iter().map(|(n, _)| n.as_str()))
        .collect();
    let mut order = vec![];
    for o in &req.order_by {
        let name = match crate::graph::lookup(known.iter().copied(), &o.field, false) {
            Ok(Some(i)) => known[i].to_string(),
            _ => {
                return Err(req_err(format!(
                    "orderBy `{}` must be a requested dimension or metric ({})",
                    o.field,
                    known.join(", ")
                )))
            }
        };
        order.push((name, o.direction));
    }

    // Join tree and term strategies.
    for t in &terms {
        required.extend(t.datasets.iter().copied());
    }
    let (root, joins) = join_tree(g, &required)?;
    let mut planned = vec![];
    for term in terms {
        let strategy = if term.dup_insensitive || term.datasets.contains(&root) {
            Strategy::Inline
        } else {
            let grain = grain_of(g, &term.datasets)?;
            if g.datasets[grain].primary_key.is_empty() {
                return Err(Error::Unsupported(format!(
                    "`{}` sums values from `{}`, which repeat once per joined `{}` row; declare a primary_key on `{}` so they can be counted once",
                    term.key, g.datasets[grain].name, g.datasets[root].name, g.datasets[grain].name
                )));
            }
            ctx.warn(format!(
                "FAN_TRAP_RISK: `{}` is computed over `{}` rows de-duplicated by primary key; a `{}` row that belongs to several groups counts in each, so groups do not add up to a grand total",
                term.key, g.datasets[grain].name, g.datasets[grain].name
            ));
            Strategy::PkDedup { grain }
        };
        planned.push(PlannedTerm { term, strategy });
    }

    Ok(LogicalPlan {
        root,
        joins,
        dims,
        filters,
        terms: planned,
        outputs,
        metric_filters,
        windowed,
        order,
        limit: req.limit,
    })
}

/// Rename local term placeholders to global term numbers.
fn renumber(e: Expression, global: &[usize], dim_index: &[usize]) -> Expression {
    transform(e, &|e| {
        Ok(Some(match e {
            Expression::Column(c) if c.table.is_none() && c.name.name.starts_with(TERM) => {
                let local: usize = c.name.name[TERM.len()..].parse().expect("term placeholder");
                sql::col(&format!("{TERM}{}", global[local]))
            }
            Expression::Column(c) if c.table.is_none() && c.name.name.starts_with(DIM) => {
                let local: usize = c.name.name[DIM.len()..]
                    .parse()
                    .expect("dimension placeholder");
                sql::col(&format!("{OUT_DIM}{}", dim_index[local]))
            }
            other => other,
        }))
    })
    .expect("renumbering cannot fail")
}

fn literal(v: &Value, datatype: Option<&str>) -> Result<Expression> {
    use polyglot_sql::builder::{cast, lit};
    let e = match v {
        Value::String(s) => {
            let l = lit(s.as_str());
            match datatype.map(str::to_ascii_lowercase).as_deref() {
                Some("date") => cast(l, "DATE"),
                Some("datetime") | Some("datetimetz") => cast(l, "TIMESTAMP"),
                _ => l,
            }
        }
        Value::Number(n) => match n.as_i64() {
            Some(i) => lit(i),
            None => lit(n.as_f64().unwrap_or_default()),
        },
        Value::Bool(b) => lit(*b),
        Value::Null => return Err(req_err("compare to null with `IS NULL` / `IS NOT NULL`")),
        other => return Err(req_err(format!("unsupported filter value {other}"))),
    };
    Ok(e.into_inner())
}

fn predicate(
    lhs: Expression,
    op: Op,
    value: Option<&Value>,
    values: &[Value],
    datatype: Option<&str>,
) -> Result<Expression> {
    let list: Vec<&Value> = match value {
        Some(Value::Array(a)) => a.iter().collect(),
        _ => values.iter().collect(),
    };
    let mut t = Tpl::new();
    let l = t.e(lhs);
    let bin = |sym: &str, t: &mut Tpl| -> Result<String> {
        // A JSON `null` arrives as "no value".
        let v = value.filter(|v| !v.is_array()).ok_or_else(|| {
            req_err(format!(
                "`{sym}` needs a single non-null `value` (compare with null using `IS NULL` / `IS NOT NULL`)"
            ))
        })?;
        Ok(format!("{l} {sym} {}", t.e(literal(v, datatype)?)))
    };
    let sql = match op {
        Op::Eq => bin("=", &mut t)?,
        Op::Ne => bin("<>", &mut t)?,
        Op::Lt => bin("<", &mut t)?,
        Op::Le => bin("<=", &mut t)?,
        Op::Gt => bin(">", &mut t)?,
        Op::Ge => bin(">=", &mut t)?,
        Op::Like => bin("LIKE", &mut t)?,
        Op::In | Op::NotIn => {
            if list.is_empty() {
                return Err(req_err("IN / NOT IN need a non-empty `values` list"));
            }
            let items: Vec<String> = list
                .iter()
                .map(|v| Ok(t.e(literal(v, datatype)?)))
                .collect::<Result<_>>()?;
            let kw = if op == Op::In { "IN" } else { "NOT IN" };
            format!("{l} {kw} ({})", items.join(", "))
        }
        Op::Between => {
            let [lo, hi] = list.as_slice() else {
                return Err(req_err("BETWEEN needs exactly two `values`"));
            };
            let (lo, hi) = (t.e(literal(lo, datatype)?), t.e(literal(hi, datatype)?));
            format!("{l} BETWEEN {lo} AND {hi}")
        }
        Op::IsNull => format!("{l} IS NULL"),
        Op::IsNotNull => format!("{l} IS NOT NULL"),
    };
    t.push(&sql);
    t.build()
}

/// The dataset every required dataset hangs off, and the joins to reach them.
fn join_tree(g: &SemanticGraph, required: &BTreeSet<usize>) -> Result<(usize, Vec<usize>)> {
    let names = |s: &BTreeSet<usize>| {
        s.iter()
            .map(|&d| format!("`{}`", g.datasets[d].name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let reaches = |r: usize, paths: &std::collections::BTreeMap<usize, Vec<Vec<usize>>>| {
        required.iter().all(|t| *t == r || paths.contains_key(t))
    };
    let all_paths: Vec<_> = (0..g.datasets.len()).map(|r| g.paths_from(r)).collect();
    let candidates: BTreeSet<usize> = (0..g.datasets.len())
        .filter(|&r| reaches(r, &all_paths[r]))
        .collect();
    if candidates.is_empty() {
        return Err(Error::Unsupported(format!(
            "no dataset joins to all of {} along many-to-one relationships; queries across independent facts are not supported yet",
            names(required)
        )));
    }
    let inside: Vec<usize> = candidates.intersection(required).copied().collect();
    let root = match inside.as_slice() {
        [one] => *one,
        [] => {
            // A common root outside the request, e.g. a fact joining two requested dimensions.
            let deepest: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&c| {
                    candidates
                        .iter()
                        .all(|&o| o == c || all_paths[o].contains_key(&c))
                })
                .collect();
            match deepest.as_slice() {
                [one] => *one,
                _ => {
                    return Err(Error::Unsupported(format!(
                        "{} are joined by several facts ({}) and the request doesn't say which; queries across independent facts are not supported yet",
                        names(required),
                        names(&candidates)
                    )))
                }
            }
        }
        _ => {
            return Err(Error::Unsupported(format!(
                "relationships between {} form a cycle",
                names(required)
            )))
        }
    };

    let mut edges: Vec<(usize, usize)> = vec![]; // (depth, relationship)
    for t in required.iter().filter(|&&t| t != root) {
        let paths = &all_paths[root][t];
        if paths.len() > 1 {
            let show = |p: &Vec<usize>| {
                p.iter()
                    .map(|&r| g.rels[r].name.as_str())
                    .collect::<Vec<_>>()
                    .join(" → ")
            };
            return Err(req_err(format!(
                "`{}` is reachable from `{}` in more than one way ({}); role-playing dimensions are not supported yet",
                g.datasets[*t].name,
                g.datasets[root].name,
                paths.iter().map(show).collect::<Vec<_>>().join(" | ")
            )));
        }
        edges.extend(paths[0].iter().enumerate().map(|(depth, &r)| (depth, r)));
    }
    edges.sort();
    let mut joins = vec![];
    for (_, r) in edges {
        if !joins.contains(&r) {
            joins.push(r);
        }
    }
    Ok((root, joins))
}

/// The dataset among `datasets` that reaches all the others: the term's row grain.
fn grain_of(g: &SemanticGraph, datasets: &BTreeSet<usize>) -> Result<usize> {
    datasets
        .iter()
        .copied()
        .find(|&d| {
            let p = g.paths_from(d);
            datasets.iter().all(|o| *o == d || p.contains_key(o))
        })
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "an aggregate reads {} but none of them joins to the others",
                datasets
                    .iter()
                    .map(|&d| format!("`{}`", g.datasets[d].name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}
