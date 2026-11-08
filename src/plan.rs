//! Planning: validate a request against the graph and decide how to compute it.
//!
//! 1. Resolve every name in the request.
//! 2. Pick the **join root**: the dataset from which every referenced dataset
//!    is reachable along many-to-one relationships. Joins only ever go from
//!    the many side to the one side, so they never multiply root rows. Layer 3
//!    has one root per home dataset, after [`wide_table::check`].
//! 3. Classify each aggregate term:
//!    - `Inline`: it reads the root, or it is duplicate-insensitive (`MIN`,
//!      `MAX`, `COUNT(DISTINCT ...)`). Correct over the joined rows as-is.
//!    - `PkDedup` (Layer 2): it only reads one-side datasets (`SUM(store.employees)`
//!      over a `store_sales` root). Joined rows repeat those values, so the
//!      term is computed over rows de-duplicated on its dataset's primary key.

use std::collections::BTreeSet;

use crate::sql::transform;
use polyglot_sql::Expression;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::graph::{self, Ctx, SemanticGraph, Term, DIM, TERM};

/// Placeholder for requested dimension `i` inside a metric's outer expression.
pub(crate) const OUT_DIM: &str = "__ossiec_q";
use crate::sql::{self, Tpl};
use crate::wide_table::{self, ErrorCode};
use crate::{Error, Result};

/// A semantic query: which metrics, sliced by which dimensions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    /// Ignored by the compiler; lets callers keep the target model in the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `metric` (model-scoped) or `dataset.metric` (dataset-scoped), or an object
    /// `{"metric": ..., "alias": ...}`. Empty to list the distinct values of
    /// `dimensions` instead.
    #[serde(
        default,
        deserialize_with = "metrics_from_json",
        serialize_with = "metrics_to_json"
    )]
    pub metrics: Vec<Selected>,
    /// `dataset.field`, or `relationship.field` when that dataset plays the role, or
    /// an object `{"field": ..., "alias": ...}`.
    #[serde(
        default,
        deserialize_with = "dimensions_from_json",
        serialize_with = "dimensions_to_json"
    )]
    pub dimensions: Vec<Selected>,
    /// One row predicate over one dataset's fields (wide table §3.4), applied in
    /// every branch: `customers.region <> 'WEST'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Structured predicates, all applied in every branch. Unlike `filter` they
    /// can read several datasets: each must be reachable from every metric.
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
    /// Answer the model-scoped metrics the spec defers (wide table §5.2): windows,
    /// computed over the grouped result, and aggregates reading several datasets,
    /// computed from the one that reaches the others. Without this they raise
    /// `E_DEFERRED_WINDOWED_METRIC` and `E_AMBIGUOUS_MEASURE_GRAIN`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred_metrics: bool,
    /// Layer 2 only: also count one-side rows no fact row reaches (products never
    /// sold), as a group of their own where the fact's dimensions are NULL (relational
    /// semantics §10.2). [`crate::compile`] raises `E_INVALID_QUERY`: in Layer 3 a
    /// metric already reads all the rows of its home dataset.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_unmatched: bool,
    /// Datasets that must appear in the join tree even when no selected
    /// dimension, filter or metric names them. Used by the AGG SQL frontend so a
    /// derived-table JOIN can attach to `FROM customers …` while the measures
    /// live on another fact. Not part of the JSON request surface (`serde(skip)`).
    #[serde(skip)]
    #[doc(hidden)]
    pub ensure_datasets: Vec<usize>,
}

/// One entry of `dimensions` or `metrics`. In Layer 3 its output name is the alias
/// if given, or the final part of the reference (wide table §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub name: String,
    pub alias: Option<String>,
}

impl From<&str> for Selected {
    fn from(name: &str) -> Self {
        name.to_string().into()
    }
}

impl From<String> for Selected {
    fn from(name: String) -> Self {
        Selected { name, alias: None }
    }
}

impl PartialEq<&str> for Selected {
    fn eq(&self, other: &&str) -> bool {
        self.alias.is_none() && self.name == *other
    }
}

impl std::ops::Deref for Selected {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

fn selected<'de, D: serde::Deserializer<'de>>(
    d: D,
    key: &str,
) -> std::result::Result<Vec<Selected>, D::Error> {
    use serde::de::Error as _;
    let entries = Vec::<Value>::deserialize(d)?;
    entries
        .into_iter()
        .map(|entry| match entry {
            Value::String(name) => Ok(name.into()),
            Value::Object(o) => {
                if let Some(k) = o.keys().find(|k| *k != key && *k != "alias") {
                    return Err(D::Error::custom(format!("unknown property `{k}`")));
                }
                let text = |k: &str| match o.get(k) {
                    None => Ok(None),
                    Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                    Some(_) => Err(D::Error::custom(format!(
                        "`{k}` must be a non-empty string"
                    ))),
                };
                Ok(Selected {
                    name: text(key)?.ok_or_else(|| D::Error::custom(format!("missing `{key}`")))?,
                    alias: text("alias")?,
                })
            }
            other => Err(D::Error::custom(format!(
                "expected a reference or an object with `{key}`, got {other}"
            ))),
        })
        .collect()
}

fn dimensions_from_json<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Selected>, D::Error> {
    selected(d, "field")
}

fn metrics_from_json<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Selected>, D::Error> {
    selected(d, "metric")
}

/// A plain string without an alias, so a request's JSON and hash don't change.
fn selected_to_json<S: serde::Serializer>(
    entries: &[Selected],
    key: &str,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    let json: Vec<Value> = entries
        .iter()
        .map(|e| match &e.alias {
            None => Value::String(e.name.clone()),
            Some(alias) => serde_json::json!({ key: e.name, "alias": alias }),
        })
        .collect();
    json.serialize(s)
}

fn dimensions_to_json<S: serde::Serializer>(
    entries: &[Selected],
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    selected_to_json(entries, "field", s)
}

fn metrics_to_json<S: serde::Serializer>(
    entries: &[Selected],
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    selected_to_json(entries, "metric", s)
}

/// The final part of a reference.
pub(crate) fn final_part(name: &str) -> &str {
    unquoted(name.rsplit('.').next().unwrap_or(name))
}

/// A name without the quotes the model or a request wrote it in.
pub(crate) fn unquoted(name: &str) -> &str {
    name.strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .filter(|n| !n.is_empty())
        .unwrap_or(name)
}

impl Request {
    /// Read a request from JSON. A malformed one, or one with an unknown or `null`
    /// property, raises `E_INVALID_QUERY` (wide table §3.7).
    pub fn from_json(text: &str) -> Result<Request> {
        let invalid = |message: String| Error::Query {
            code: ErrorCode::InvalidQuery,
            message,
        };
        let value: Value = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if let Some((key, _)) = value
            .as_object()
            .and_then(|o| o.iter().find(|(_, v)| v.is_null()))
        {
            return Err(invalid(format!("`{key}` is null")));
        }
        serde_json::from_value(value).map_err(|e| invalid(e.to_string()))
    }
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
    /// Where NULLs go. Default: first when ascending, last when descending,
    /// on every engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nulls: Option<Nulls>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Nulls {
    First,
    Last,
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
    /// Index of the leg (join tree) this term is evaluated in.
    pub leg: usize,
}

/// A dataset in a join tree: itself, or playing a role (reached through a
/// named relationship), each with its own alias.
pub(crate) type Node = (usize, Option<usize>);

/// Join type for a step in a leg. Layer 3 requests always use [`JoinKind::Left`];
/// Layer 2 consumer SQL may ask for inner, left, right, full, or cross
/// (relational_semantics A.6 and §7.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum JoinKind {
    #[default]
    Left,
    Inner,
    Right,
    Full,
    Cross,
}

impl JoinKind {
    pub(crate) fn sql(self) -> &'static str {
        match self {
            JoinKind::Left => "LEFT JOIN",
            JoinKind::Inner => "INNER JOIN",
            JoinKind::Right => "RIGHT JOIN",
            JoinKind::Full => "FULL JOIN",
            JoinKind::Cross => "CROSS JOIN",
        }
    }

    /// The same join read from the other side: `a LEFT JOIN b` is `b RIGHT JOIN a`.
    pub(crate) fn mirrored(self) -> JoinKind {
        match self {
            JoinKind::Left => JoinKind::Right,
            JoinKind::Right => JoinKind::Left,
            other => other,
        }
    }

    /// RIGHT/FULL null-extend one side and CROSS multiplies rows. An inline
    /// aggregate would then see rows that are not the measure's source rows
    /// (relational_semantics §7.6, G2).
    pub(crate) fn inflates_inline(self) -> bool {
        matches!(self, JoinKind::Right | JoinKind::Full | JoinKind::Cross)
    }
}

/// One join: `<kind> JOIN <to> AS to_alias ON <rel's columns from from_alias>`.
/// [`JoinKind::Cross`] has no `ON` and no relationship (`rel` is `None`).
pub(crate) struct JoinStep {
    pub rel: Option<usize>,
    pub from_alias: String,
    pub to: usize,
    pub to_alias: String,
    pub kind: JoinKind,
    /// Consumer joined along `rel.from` while the attach side is `rel.to` (fan-out).
    pub reverse: bool,
}

/// A join tree rooted at one fact. Requests over independent facts have one
/// leg per fact; their parts are stacked like any others.
pub(crate) struct Leg {
    pub root: usize,
    pub joins: Vec<JoinStep>,
    /// Derived tables the consumer joined in (Layer 2 A.2), after `joins`.
    pub derived: Vec<DerivedJoin>,
    /// Layer 2: the consumer's joins on this leg, and on no other, can drop rows
    /// of the datasets it shares with the other legs (an `INNER` join, a `LEFT`
    /// join from the fact, a narrowing derived table). The query is one relation
    /// (§7.6, §10.2), so every other leg keeps only the shared rows this leg's
    /// tree still has, matched on these aliases: the datasets the consumer bound,
    /// as this leg names them. `None` when the leg narrows nothing.
    pub narrows_on: Option<Vec<String>>,
}

/// A derived table (an already-compiled inner query) joined onto a leg:
/// `<kind> JOIN <rel_placeholder> AS rel_alias ON <from_fields> = <rel_cols>`.
///
/// Stored as field indexes and column names, never as prebuilt ASTs, because
/// `tree_sql` renames aliases per part (base, stacked, `PkDedup`, unmatched).
/// `rel_placeholder` is a table name that Layer 2 replaces by the compiled
/// inner query once every stage has been lowered and dialect-adapted.
#[derive(Debug, Clone)]
pub(crate) struct DerivedJoin {
    /// Alias (on the leg) of the dataset the derived table attaches to.
    pub from_alias: String,
    /// `(dataset, field)` of each attach-side join column.
    pub from_fields: Vec<(usize, usize)>,
    /// Placeholder table name, e.g. `__ossiec_deriv0`.
    pub rel_placeholder: String,
    /// Alias the derived table takes in this query.
    pub rel_alias: String,
    /// Output column names on the derived table, one per `from_fields` entry.
    pub rel_cols: Vec<String>,
    /// `IS NOT DISTINCT FROM` instead of `=` (NULL keys match).
    pub null_safe: bool,
    pub kind: JoinKind,
}

/// What a statement sorts by.
pub(crate) enum OrderKey {
    /// An output column, by name.
    Output(String),
    /// An expression over term and dimension placeholders, which needn't be selected
    /// (Layer 2, relational semantics §7.5).
    Expr(Box<Expression>),
}

/// Everything `lower` needs; no SQL decisions are left.
pub(crate) struct LogicalPlan {
    pub legs: Vec<Leg>,
    /// (output name, expression with time grain applied).
    pub dims: Vec<(String, Expression)>,
    pub filters: Vec<Expression>,
    pub terms: Vec<PlannedTerm>,
    /// (output name, expression over `__ossiec_t{term}` placeholders).
    pub outputs: Vec<(String, Expression)>,
    /// Metric predicates, over the metric expression and over the output column.
    pub metric_filters: Vec<(Expression, Expression)>,
    /// Each dimension's SQL type, from its field's datatype, when declared. Used
    /// to type the NULL a dimension takes in unmatched rows.
    pub dim_types: Vec<Option<&'static str>>,
    /// Output names of String-typed dimensions (for engines whose default
    /// collation doesn't sort by byte).
    pub string_dims: Vec<String>,
    /// String-typed field expressions used as dimensions or filtered on (for
    /// engines whose default collation compares case-insensitively).
    pub string_exprs: Vec<Expression>,
    /// Some output uses a window function. Metric filters then apply to the
    /// finished rows (after windows), not to groups.
    pub windowed: bool,
    pub include_unmatched: bool,
    pub order: Vec<(OrderKey, Direction, Option<Nulls>)>,
    pub limit: Option<u64>,
}

/// The alias a node gets: the dataset's name, or `dataset__relationship` for a role.
pub(crate) fn alias(g: &SemanticGraph, (ds, role): Node) -> String {
    match role {
        None => g.datasets[ds].name.clone(),
        Some(r) => format!(
            "{}__{}",
            unquoted(&g.datasets[ds].name),
            unquoted(&g.rels[r].name)
        ),
    }
}

fn req_err(m: impl Into<String>) -> Error {
    Error::Request(m.into())
}

/// Which query interface a plan answers. They differ in where measures come
/// from: the two specs' results differ on purpose (wide table §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interface {
    /// Layer 3: the engine chooses the joins, one branch per home dataset.
    WideTable,
    /// Layer 2: the consumer's joins decide the rows every measure sees.
    Relational,
}

pub(crate) fn plan(
    g: &SemanticGraph,
    req: &Request,
    interface: Interface,
    ctx: &mut Ctx,
) -> Result<LogicalPlan> {
    if interface == Interface::WideTable {
        wide_table::check(g, req, ctx)?;
    }
    // No metrics: the request lists dimension values (distinct combinations).
    if req.metrics.is_empty() && req.dimensions.is_empty() {
        return Err(req_err("ask for at least one metric or dimension"));
    }

    // Dimensions.
    // (output name, dataset, field, role)
    let by_output_name = interface == Interface::WideTable;
    let mut dim_refs: Vec<(String, usize, usize, Option<usize>)> = vec![];
    // Each name `orderBy` can use: (reference, output name).
    let mut references: Vec<(String, String)> = vec![];
    for d in &req.dimensions {
        let (ds, f, role) = g.field_by_name(d)?;
        let prefix = match role {
            Some(r) => g.rels[r].name.clone(),
            None => g.datasets[ds].name.clone(),
        };
        let field = &g.datasets[ds].fields[f].name;
        let reference = format!("{prefix}.{field}");
        let name = match &d.alias {
            Some(alias) if by_output_name => alias.clone(),
            _ if by_output_name => field.clone(),
            _ => reference.clone(),
        };
        if !dim_refs.iter().any(|(n, ..)| *n == name) {
            references.push((reference, name.clone()));
            dim_refs.push((name, ds, f, role));
        }
    }
    let grained = match (req.time_grain, &req.time_dimension) {
        (None, None) => None,
        (None, Some(_)) => return Err(req_err("timeDimension needs a timeGrain")),
        (Some(grain), Some(td)) => {
            let (ds, f, role) = g.field_by_name(td)?;
            // A field selected under two aliases: the grain applies to the first.
            let i = dim_refs
                .iter()
                .position(|(_, d, ff, r)| (*d, *ff, *r) == (ds, f, role))
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
    for (i, (name, ds, f, role)) in dim_refs.iter().enumerate() {
        let mut e = g.field_sql_as(*ds, *f, &alias(g, (*ds, *role)), ctx);
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
    // Which metric each output is.
    let mut output_metrics: Vec<usize> = vec![];
    let mut windowed = false;
    for m in &req.metrics {
        let mi = g.metric_by_name(m)?;
        let reference = &g.metrics[mi].name;
        let name = match &m.alias {
            Some(alias) if by_output_name => alias.clone(),
            _ if by_output_name => final_part(reference).to_string(),
            _ => reference.clone(),
        };
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
                    .position(|(_, d, ff, r)| (*d, *ff, *r) == (ds, f, None))
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
            // The same SQL over other rows is another term: `COUNT(*)` names no
            // column, so only its datasets tell `orders` from `tickets`.
            .map(|t| {
                match terms
                    .iter()
                    .position(|x| x.key == t.key && x.datasets == t.datasets && x.raw == t.raw)
                {
                    Some(i) => i,
                    None => {
                        terms.push(t);
                        terms.len() - 1
                    }
                }
            })
            .collect();
        references.push((g.metrics[mi].name.clone(), name.clone()));
        output_metrics.push(mi);
        outputs.push((name, renumber(rm.outer, &global, &dim_index)));
    }

    // Filters.
    let mut required: BTreeSet<Node> = dim_refs
        .iter()
        .map(|(_, ds, _, role)| (*ds, *role))
        .collect();
    for &ds in &req.ensure_datasets {
        required.insert((ds, None));
    }
    let mut filters = vec![];
    let is_string = |ds: usize, f: usize| {
        g.datasets[ds].fields[f]
            .datatype
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case("string"))
    };
    let mut string_exprs: Vec<Expression> = dim_refs
        .iter()
        .zip(&dims)
        .filter(|((_, ds, f, _), _)| is_string(*ds, *f))
        .map(|(_, (_, e))| e.clone())
        .collect();
    for f in &req.filters {
        let (ds, fi, role) = g.field_by_name(&f.field)?;
        required.insert((ds, role));
        let lhs = g.field_sql_as(ds, fi, &alias(g, (ds, role)), ctx);
        if is_string(ds, fi) && !string_exprs.contains(&lhs) {
            string_exprs.push(lhs.clone());
        }
        let datatype = g.datasets[ds].fields[fi].datatype.as_deref();
        filters.push(predicate(lhs, f.op, f.value.as_ref(), &f.values, datatype)?);
    }
    if let Some(text) = &req.filter {
        let ast = wide_table::parse_filter(text)?;
        for (ds, f, node) in wide_table::filter_fields(g, &ast) {
            required.insert(node);
            if is_string(ds, f) {
                let e = g.field_sql_as(ds, f, &alias(g, node), ctx);
                if !string_exprs.contains(&e) {
                    string_exprs.push(e);
                }
            }
        }
        filters.push(filter_sql(g, ast, ctx)?);
    }
    let mut metric_filters = vec![];
    for mf in &req.metric_filters {
        let mi = g.metric_by_name(&mf.metric)?;
        let (output, outer) = output_metrics
            .iter()
            .position(|&o| o == mi)
            .map(|i| &outputs[i])
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
            predicate(sql::quoted_col(output), mf.op, Some(&mf.value), &[], None)?,
        ));
    }

    // Order refers to an entry by its output name or its reference.
    let mut order = vec![];
    for o in &req.order_by {
        let find = |names: Vec<&str>| crate::graph::lookup(names, &o.field, false).ok().flatten();
        let name = find(references.iter().map(|(_, n)| n.as_str()).collect())
            .or_else(|| find(references.iter().map(|(r, _)| r.as_str()).collect()))
            .map(|i| references[i].1.clone());
        let Some(name) = name else {
            let known: Vec<&str> = references.iter().map(|(r, _)| r.as_str()).collect();
            return Err(req_err(format!(
                "orderBy `{}` must be a requested dimension or metric ({})",
                o.field,
                known.join(", ")
            )));
        };
        order.push((OrderKey::Output(name), o.direction, o.nulls));
    }

    // Join trees (one per fact) and term strategies.
    let (legs, term_legs) = if interface == Interface::WideTable && !terms.is_empty() {
        branches(g, &required, &terms)?
    } else {
        plan_legs(g, &required, &terms)?
    };
    let mut planned = vec![];
    for (term, leg) in terms.into_iter().zip(term_legs) {
        let root = legs[leg].root;
        // With includeUnmatched, every one-side term is de-duplicated, so its
        // unmatched rows can be added (an inline COUNT DISTINCT can't take them).
        let inline = term.raw
            || term.datasets.contains(&root)
            || (term.dup_insensitive && !req.include_unmatched);
        let strategy = if inline {
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
        planned.push(PlannedTerm {
            term,
            strategy,
            leg,
        });
    }

    Ok(LogicalPlan {
        legs,
        dims,
        filters,
        terms: planned,
        outputs,
        metric_filters,
        string_dims: dim_refs
            .iter()
            .filter(|(_, ds, f, _)| {
                g.datasets[*ds].fields[*f]
                    .datatype
                    .as_deref()
                    .is_some_and(|d| d.eq_ignore_ascii_case("string"))
            })
            .map(|(n, ..)| n.clone())
            .collect(),
        dim_types: dim_refs
            .iter()
            .map(|(_, ds, f, _)| sql_type(g.datasets[*ds].fields[*f].datatype.as_deref()))
            .collect(),
        string_exprs,
        windowed,
        include_unmatched: req.include_unmatched,
        order,
        limit: req.limit,
    })
}

/// SQL type for an Ossie datatype.
fn sql_type(datatype: Option<&str>) -> Option<&'static str> {
    match datatype?.to_ascii_lowercase().as_str() {
        "integer" => Some("BIGINT"),
        "decimal" => Some("DECIMAL(38, 9)"),
        "float" => Some("DOUBLE"),
        "string" => Some("VARCHAR"),
        "date" => Some("DATE"),
        "datetime" | "datetimetz" => Some("TIMESTAMP"),
        "boolean" => Some("BOOLEAN"),
        _ => None,
    }
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

/// `filter` over the query's aliases, each field in its SQL.
fn filter_sql(g: &SemanticGraph, ast: Expression, ctx: &mut Ctx) -> Result<Expression> {
    let ctx = std::cell::RefCell::new(ctx);
    transform(graph::true_division(ast), &|e| {
        let Expression::Column(c) = e else {
            return Ok(Some(e));
        };
        Ok(Some(match g.field_of_column(&c) {
            Ok((ds, f, role)) => {
                g.field_sql_as(ds, f, &alias(g, (ds, role)), &mut ctx.borrow_mut())
            }
            Err(_) => Expression::Column(c),
        }))
    })
    .map_err(|e| Error::Internal(e.to_string()))
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

pub(crate) fn predicate(
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

/// Wide table §5.4: one leg per home dataset, joined to every dimension and
/// filter. [`wide_table::check`] has refused a request where a home has no single
/// path to something it needs.
fn branches(
    g: &SemanticGraph,
    required: &BTreeSet<Node>,
    terms: &[Term],
) -> Result<(Vec<Leg>, Vec<usize>)> {
    let mut homes: Vec<(usize, BTreeSet<Node>)> = vec![];
    let mut term_legs = vec![];
    for t in terms {
        let home = grain_of(g, &t.datasets)?;
        let leg = homes
            .iter()
            .position(|(h, _)| *h == home)
            .unwrap_or_else(|| {
                homes.push((home, required.clone()));
                homes.len() - 1
            });
        // A deferred aggregate also reads the datasets its home joins to.
        homes[leg].1.extend(t.datasets.iter().map(|&d| (d, None)));
        term_legs.push(leg);
    }
    let legs = homes
        .iter()
        .map(|(home, nodes)| tree_from(g, *home, nodes))
        .collect::<Result<_>>()?;
    Ok((legs, term_legs))
}

/// One join tree for everything, or, when no dataset reaches all of `required`
/// (independent facts sharing dimensions), one per root: each term gets the tree
/// for its own datasets plus every dimension and filter. Returns the legs and each
/// term's leg.
fn plan_legs(
    g: &SemanticGraph,
    required: &BTreeSet<Node>,
    terms: &[Term],
) -> Result<(Vec<Leg>, Vec<usize>)> {
    let mut all = required.clone();
    for t in terms {
        all.extend(t.datasets.iter().map(|&d| (d, None)));
    }
    let single = join_tree(g, &all);
    let independent =
        matches!(&single, Err(Error::Unsupported(m)) if m.contains("independent facts"));
    if !independent || terms.is_empty() {
        return Ok((vec![single?], vec![0; terms.len()]));
    }
    // One leg per root: terms whose own tree roots at the same fact share it.
    let mut by_root: Vec<(usize, BTreeSet<Node>)> = vec![];
    let mut term_roots = vec![];
    for t in terms {
        let mut req = required.clone();
        req.extend(t.datasets.iter().map(|&d| (d, None)));
        let root = join_tree(g, &req)?.root;
        match by_root.iter_mut().find(|(r, _)| *r == root) {
            Some((_, nodes)) => nodes.extend(req),
            None => by_root.push((root, req)),
        }
        term_roots.push(root);
    }
    let legs = by_root
        .iter()
        .map(|(_, nodes)| join_tree(g, nodes))
        .collect::<Result<Vec<_>>>()?;
    let term_legs = term_roots
        .iter()
        .map(|r| by_root.iter().position(|(x, _)| x == r).expect("leg"))
        .collect();
    Ok((legs, term_legs))
}

/// The dataset every required node hangs off, and the joins to reach them.
fn join_tree(g: &SemanticGraph, required: &BTreeSet<Node>) -> Result<Leg> {
    let plain: BTreeSet<usize> = required
        .iter()
        .map(|&(ds, role)| match role {
            None => ds,
            Some(r) => g.rels[r].from, // a role needs its relationship's source
        })
        .collect();
    let names = |s: &BTreeSet<usize>| {
        s.iter()
            .map(|&d| format!("`{}`", g.datasets[d].name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let all_paths: Vec<_> = (0..g.datasets.len()).map(|r| g.paths_from(r)).collect();
    let reaches = |r: usize| {
        plain
            .iter()
            .all(|t| *t == r || all_paths[r].contains_key(t))
    };
    let candidates: BTreeSet<usize> = (0..g.datasets.len()).filter(|&r| reaches(r)).collect();
    if candidates.is_empty() {
        return Err(Error::Unsupported(format!(
            "no dataset joins to all of {} along many-to-one relationships, and they don't split into independent facts sharing the requested dimensions",
            names(&plain)
        )));
    }
    let inside: Vec<usize> = candidates.intersection(&plain).copied().collect();
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
                        "{} are joined by several facts ({}); with no measure to say which, the request is ambiguous (independent facts need a measure from each)",
                        names(&plain),
                        names(&candidates)
                    )))
                }
            }
        }
        _ => {
            return Err(Error::Unsupported(format!(
                "relationships between {} form a cycle",
                names(&plain)
            )))
        }
    };
    tree_from(g, root, required)
}

/// The joins from `root` to every required node.
///
/// A plain dataset reachable more than one way takes the unique shortest path
/// (Layer 2: `date_dim` from `store_sales` is the sale date, not the customer's
/// first-sale date two hops away). A tie is an error that suggests naming the role.
/// Layer 3 has already refused both. A role node `(dataset, rel)` is reached through
/// `rel`, under its own alias.
fn tree_from(g: &SemanticGraph, root: usize, required: &BTreeSet<Node>) -> Result<Leg> {
    let root_paths = g.paths_from(root);
    let show = |p: &Vec<usize>| {
        p.iter()
            .map(|&r| g.rels[r].name.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    };
    // The default path to `t`: the only one, or the unique shortest.
    let path_to = |t: usize| -> Result<Vec<usize>> {
        let paths = &root_paths[&t];
        let shortest = paths.iter().map(Vec::len).min().unwrap_or(0);
        let best: Vec<&Vec<usize>> = paths.iter().filter(|p| p.len() == shortest).collect();
        match best.as_slice() {
            [one] => Ok((*one).clone()),
            _ => Err(req_err(format!(
                "`{}` is reachable from `{}` in more than one way ({}); name the one you mean as `<relationship>.<field>`, e.g. `{}.<field>`",
                g.datasets[t].name,
                g.datasets[root].name,
                best.iter().map(|p| show(p)).collect::<Vec<_>>().join(" | "),
                g.rels[*best[0].last().expect("non-empty path")].name
            ))),
        }
    };

    // (depth, step)
    let mut steps: Vec<(usize, JoinStep)> = vec![];
    let add_path = |path: &[usize], steps: &mut Vec<(usize, JoinStep)>| -> Result<()> {
        for (depth, &r) in path.iter().enumerate() {
            let rel = &g.rels[r];
            match steps.iter().find(|(_, s)| s.to_alias == g.datasets[rel.to].name) {
                Some((_, s)) if s.rel == Some(r) => {}
                Some((_, s)) => {
                    return Err(req_err(format!(
                        "`{}` is reached both through `{}` and `{}`; name the role you mean as `<relationship>.<field>`",
                        g.datasets[rel.to].name,
                        g.rels[s.rel.expect("planned relationship join")].name,
                        rel.name
                    )))
                }
                None => steps.push((
                    depth,
                    JoinStep {
                        rel: Some(r),
                        from_alias: g.datasets[rel.from].name.clone(),
                        to: rel.to,
                        to_alias: g.datasets[rel.to].name.clone(),
                        kind: JoinKind::Left,
                        reverse: false,
                    },
                )),
            }
        }
        Ok(())
    };
    for &(ds, role) in required {
        let target = match role {
            None => ds,
            Some(r) => g.rels[r].from,
        };
        let path = if target == root {
            vec![]
        } else {
            path_to(target)?
        };
        add_path(&path, &mut steps)?;
        if let Some(r) = role {
            let to_alias = alias(g, (ds, role));
            if !steps.iter().any(|(_, s)| s.to_alias == to_alias) {
                let from_alias = g.datasets[g.rels[r].from].name.clone();
                steps.push((
                    path.len(),
                    JoinStep {
                        rel: Some(r),
                        from_alias,
                        to: ds,
                        to_alias,
                        kind: JoinKind::Left,
                        reverse: false,
                    },
                ));
            }
        }
    }
    steps.sort_by(|(da, a), (db, b)| (da, a.rel, &a.to_alias).cmp(&(db, b.rel, &b.to_alias)));
    Ok(Leg {
        root,
        joins: steps.into_iter().map(|(_, s)| s).collect(),
        derived: vec![],
        narrows_on: None,
    })
}

/// The dataset among `datasets` that reaches all the others: the term's row grain.
pub(crate) fn grain_of(g: &SemanticGraph, datasets: &BTreeSet<usize>) -> Result<usize> {
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
