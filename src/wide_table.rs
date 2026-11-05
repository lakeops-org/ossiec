//! Layer 3 rejections: `wide_table_semantics.md` Section 8, row by row.
//!
//! [`check`] runs before planning, in the table's order, because the first row
//! whose condition holds decides the code. A request it accepts has one path from
//! every home dataset to every dataset it needs, so planning never has to guess.
//!
//! A few request properties are outside the spec, each of them asked for
//! explicitly, which Section 9 allows:
//!
//! - `relationship.field` names a role, the path through that relationship.
//! - `filters` entries can read several datasets, unlike the one `filter`.
//! - `deferredMetrics` answers the model-scoped metrics the spec defers.
//! - `timeGrain`, `metricFilters`, `orderBy` and `limit`.

use std::collections::BTreeSet;
use std::fmt;

use polyglot_sql::expressions::Column;
use polyglot_sql::{
    contains_aggregate, contains_window_function, get_columns, is_query, is_subquery, DialectType,
    Expression, ExpressionWalk,
};
use serde::Serialize;

use crate::graph::{Ctx, SemanticGraph};
use crate::plan::{self, Node, Request};
use crate::sql;
use crate::{Error, Result};

/// The codes of Section 8. They sort in the table's order, so the smallest one
/// that applies is the one to raise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub enum ErrorCode {
    InvalidQuery,
    EmptyAggregationQuery,
    NameNotFound,
    MetricInDimensions,
    FieldInMetrics,
    AggregateInWhere,
    WindowInWhere,
    CrossDatasetFilter,
    NameCollision,
    UnaggregatedFinerGrainReference,
    DeferredWindowedMetric,
    AmbiguousMeasureGrain,
    NoPath,
    PartialDimensionReachability,
    NoAnchor,
    UnsupportedFilterPath,
    AmbiguousPath,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        use ErrorCode::*;
        match self {
            InvalidQuery => "E_INVALID_QUERY",
            EmptyAggregationQuery => "E_EMPTY_AGGREGATION_QUERY",
            NameNotFound => "E_NAME_NOT_FOUND",
            MetricInDimensions => "E_METRIC_IN_DIMENSIONS",
            FieldInMetrics => "E_FIELD_IN_METRICS",
            AggregateInWhere => "E_AGGREGATE_IN_WHERE",
            WindowInWhere => "E_WINDOW_IN_WHERE",
            CrossDatasetFilter => "E_CROSS_DATASET_FILTER",
            NameCollision => "E_NAME_COLLISION",
            UnaggregatedFinerGrainReference => "E_UNAGGREGATED_FINER_GRAIN_REFERENCE",
            DeferredWindowedMetric => "E_DEFERRED_WINDOWED_METRIC",
            AmbiguousMeasureGrain => "E_AMBIGUOUS_MEASURE_GRAIN",
            NoPath => "E_NO_PATH",
            PartialDimensionReachability => "E_PARTIAL_DIMENSION_REACHABILITY",
            NoAnchor => "E_NO_ANCHOR",
            UnsupportedFilterPath => "E_UNSUPPORTED_FILTER_PATH",
            AmbiguousPath => "E_AMBIGUOUS_PATH",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

fn reject<T>(code: ErrorCode, message: impl Into<String>) -> Result<T> {
    Err(Error::Query {
        code,
        message: message.into(),
    })
}

/// What a name in `filter` refers to.
enum FilterRef {
    Field(usize, usize, Option<usize>),
    Metric,
    Unknown(String),
}

fn column_text(c: &Column) -> String {
    match &c.table {
        Some(t) => format!("{}.{}", t.name, c.name.name),
        None => c.name.name.clone(),
    }
}

fn filter_ref(g: &SemanticGraph, c: &Column) -> FilterRef {
    if c.table.is_some() {
        if let Ok((ds, f, role)) = g.field_of_column(c) {
            return FilterRef::Field(ds, f, role);
        }
    }
    let name = column_text(c);
    match g.metric_by_name(&name) {
        Ok(_) => FilterRef::Metric,
        Err(_) => FilterRef::Unknown(name),
    }
}

/// The `filter` expression, or `E_INVALID_QUERY` if it isn't one. It is read as a
/// `WHERE` clause, which also takes `((a)) AND b`, and nothing but that clause may
/// come out: `a = 1 ORDER BY 2` and a second statement are not expressions.
pub(crate) fn parse_filter(text: &str) -> Result<Expression> {
    let not_one = |why: String| {
        reject::<Expression>(
            ErrorCode::InvalidQuery,
            format!("`filter` is not an expression: {why}"),
        )
    };
    let statement = match sql::parse(&format!("SELECT 1 WHERE {text}"), DialectType::Generic) {
        Ok(statement) => statement,
        Err(e) => return not_one(e),
    };
    let Expression::Select(mut select) = statement else {
        return not_one(format!("`{text}`"));
    };
    let Some(clause) = select.where_clause.take() else {
        return not_one(format!("`{text}`"));
    };
    let ast = clause.this;
    let rest = polyglot_sql::generate(&Expression::Select(select), DialectType::Generic);
    if rest.as_deref().ok() != Some("SELECT 1") {
        return not_one(format!("`{text}`"));
    }
    if ast.contains(|e| is_query(e) || is_subquery(e)) {
        return reject(
            ErrorCode::InvalidQuery,
            format!("`filter` cannot contain a query: `{text}`"),
        );
    }
    Ok(ast)
}

/// Every field `filter` reads. [`check`] has refused any other name.
pub(crate) fn filter_fields(g: &SemanticGraph, ast: &Expression) -> Vec<(usize, usize, Node)> {
    get_columns(ast)
        .into_iter()
        .filter_map(|e| match e {
            Expression::Column(c) => match filter_ref(g, c) {
                FilterRef::Field(ds, f, role) => Some((ds, f, (ds, role))),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// The dataset a node's path has to reach. A role's path ends at its relationship's source.
fn target(g: &SemanticGraph, (ds, role): Node) -> usize {
    role.map_or(ds, |r| g.rels[r].from)
}

fn reaches(g: &SemanticGraph, from: usize, to: usize) -> bool {
    from == to || g.paths_from(from).contains_key(&to)
}

/// One dimension: its text in the request, and what it resolved to.
struct Dim<'a> {
    text: &'a str,
    field: (usize, usize, Option<usize>),
    alias: Option<&'a str>,
}

impl Dim<'_> {
    fn node(&self) -> Node {
        (self.field.0, self.field.2)
    }
}

/// One metric: its name, its home datasets, and what each home's aggregates read.
struct Metric {
    name: String,
    homes: BTreeSet<usize>,
    reads: BTreeSet<(usize, usize)>,
}

impl Metric {
    /// The first home dataset that can't reach `to`. A metric reaches a dataset
    /// only if every home does (Section 5.3).
    fn stuck_home(&self, g: &SemanticGraph, to: usize) -> Option<usize> {
        self.homes.iter().copied().find(|&h| !reaches(g, h, to))
    }
}

/// Section 8, in order.
pub(crate) fn check(g: &SemanticGraph, req: &Request, ctx: &mut Ctx) -> Result<()> {
    // 1. Malformed, or asking for something this interface doesn't do.
    if req.include_unmatched {
        return reject(
            ErrorCode::InvalidQuery,
            "`includeUnmatched` has nothing to add in Layer 3: every metric already reads all the rows of its home dataset. For a Layer 2 query, set it there",
        );
    }
    let filter = req.filter.as_deref().map(parse_filter).transpose()?;
    for f in &req.filters {
        // The field isn't resolved yet, and the shape of a value doesn't depend on it.
        plan::predicate(sql::col("x"), f.op, f.value.as_ref(), &f.values, None).or_else(|e| {
            reject(
                ErrorCode::InvalidQuery,
                format!("filters `{}`: {}", f.field, plain(e)),
            )
        })?;
    }

    // 2.
    if req.metrics.is_empty() && req.dimensions.is_empty() {
        return reject(
            ErrorCode::EmptyAggregationQuery,
            "ask for at least one dimension or metric",
        );
    }

    // 3. A reference to the wrong kind of thing does resolve: rows 4 to 6 report it.
    let is_field = |name: &str| g.field_by_name(name).is_ok();
    let is_metric = |name: &str| g.metric_by_name(name).is_ok();
    let not_found = |e: Error| reject::<()>(ErrorCode::NameNotFound, plain(e));
    let dimension_names = req.dimensions.iter().map(|d| d.name.as_str());
    for name in dimension_names.chain(req.filters.iter().map(|f| f.field.as_str())) {
        if let (Err(e), false) = (g.field_by_name(name), is_metric(name)) {
            not_found(e)?;
        }
    }
    for name in &req.metrics {
        if let (Err(e), false) = (g.metric_by_name(name), is_field(name)) {
            not_found(e)?;
        }
    }
    let filter_columns: Vec<&Column> = filter
        .iter()
        .flat_map(get_columns)
        .filter_map(|e| match e {
            Expression::Column(c) => Some(c.as_ref()),
            _ => None,
        })
        .collect();
    for c in &filter_columns {
        if let FilterRef::Unknown(name) = filter_ref(g, c) {
            return reject(
                ErrorCode::NameNotFound,
                format!("`filter` reads `{name}`, which is not a field of this model; use `dataset.field`"),
            );
        }
    }

    // 4 and 5.
    if let Some(d) = req.dimensions.iter().find(|d| !is_field(d)) {
        return reject(
            ErrorCode::MetricInDimensions,
            format!(
                "dimension `{}` is a metric; list it under `metrics`",
                d.name
            ),
        );
    }
    if let Some(m) = req.metrics.iter().find(|m| !is_metric(m)) {
        return reject(
            ErrorCode::FieldInMetrics,
            format!("metric `{}` is a field; list it under `dimensions`", m.name),
        );
    }

    // 6 and 7.
    if let Some(f) = req.filters.iter().find(|f| !is_field(&f.field)) {
        return reject(
            ErrorCode::AggregateInWhere,
            format!(
                "filter `{}` is a metric; a filter applies before aggregation",
                f.field
            ),
        );
    }
    if let Some(ast) = &filter {
        let names_metric = filter_columns
            .iter()
            .any(|c| matches!(filter_ref(g, c), FilterRef::Metric));
        if names_metric || contains_aggregate(ast) {
            return reject(
                ErrorCode::AggregateInWhere,
                "`filter` reads an aggregate or a metric; a filter applies before aggregation",
            );
        }
        if contains_window_function(ast) {
            return reject(
                ErrorCode::WindowInWhere,
                "`filter` contains a window function",
            );
        }
    }

    // 8. `filter` has one dataset. A role is its own: two roles are two datasets.
    let filter_nodes: Vec<Node> = filter
        .as_ref()
        .map(|ast| filter_fields(g, ast).into_iter().map(|(.., n)| n).collect())
        .unwrap_or_default();
    let distinct: BTreeSet<Node> = filter_nodes.iter().copied().collect();
    if distinct.len() > 1 {
        let names: Vec<String> = distinct.iter().map(|&n| plan::alias(g, n)).collect();
        return reject(
            ErrorCode::CrossDatasetFilter,
            format!(
                "`filter` reads {}; it can read one dataset. A Layer 2 query can filter across datasets",
                names.join(" and ")
            ),
        );
    }

    // 9.
    let dims: Vec<Dim> = req
        .dimensions
        .iter()
        .map(|d| {
            Ok(Dim {
                text: d,
                field: g.field_by_name(d)?,
                alias: d.alias.as_deref(),
            })
        })
        .collect::<Result<_>>()?;
    let metric_indexes: Vec<usize> = req
        .metrics
        .iter()
        .map(|m| g.metric_by_name(m))
        .collect::<Result<_>>()?;
    let mut seen = BTreeSet::new();
    // Section 3.3: the alias, or the final part of the reference. Identifiers
    // compare case-insensitively.
    let outputs = dims
        .iter()
        .map(|d| match d.alias {
            Some(alias) => alias.to_string(),
            None => g.datasets[d.field.0].fields[d.field.1].name.clone(),
        })
        .chain(
            metric_indexes
                .iter()
                .zip(&req.metrics)
                .map(|(&i, m)| match &m.alias {
                    Some(alias) => alias.clone(),
                    None => plan::final_part(&g.metrics[i].name).to_string(),
                }),
        );
    for name in outputs {
        if !seen.insert(name.to_ascii_lowercase()) {
            return reject(
                ErrorCode::NameCollision,
                format!("two entries have the output name `{name}`; give one an `alias`"),
            );
        }
    }

    // 10, 11 and 12, across every metric.
    let mut rejections = vec![];
    for &i in &metric_indexes {
        rejections.extend(g.metric_rejections(i, ctx)?);
    }
    if req.deferred_metrics {
        rejections.retain(|r| !r.deferred);
    }
    if let Some(r) = rejections.into_iter().min_by_key(|r| r.code) {
        return reject(r.code, r.message);
    }

    let metrics: Vec<Metric> = metric_indexes
        .iter()
        .map(|&i| {
            let rm = g.resolve_metric(i, ctx)?;
            Ok(Metric {
                name: g.metrics[i].name.clone(),
                homes: rm
                    .terms
                    .iter()
                    .map(|t| plan::grain_of(g, &t.datasets))
                    .collect::<Result<_>>()?,
                reads: rm
                    .terms
                    .iter()
                    .map(|t| {
                        let home = plan::grain_of(g, &t.datasets)?;
                        Ok(t.datasets.iter().map(move |&d| (home, d)))
                    })
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect(),
            })
        })
        .collect::<Result<_>>()?;

    // Everything the filters read, with the text to report it by.
    let mut filtered: Vec<(String, Node)> = req
        .filters
        .iter()
        .map(|f| {
            let (ds, _, role) = g.field_by_name(&f.field)?;
            Ok((f.field.clone(), (ds, role)))
        })
        .collect::<Result<_>>()?;
    if let Some(&node) = distinct.iter().next() {
        filtered.push((req.filter.clone().unwrap_or_default(), node));
    }

    // 13 and 14: a dimension none of the metrics reaches goes first.
    if !metrics.is_empty() {
        let stuck = |d: &Dim, m: &Metric| m.stuck_home(g, target(g, d.node()));
        for d in &dims {
            if metrics.iter().all(|m| stuck(d, m).is_some()) {
                let m = &metrics[0];
                let home = stuck(d, m).expect("checked above");
                return reject(
                    ErrorCode::NoPath,
                    format!(
                        "dimension `{}`: no metric reaches it. `{}` can't reach `{}` from `{}` along many-to-one relationships; a Layer 2 query can join in the other direction",
                        d.text, m.name, g.datasets[target(g, d.node())].name, g.datasets[home].name
                    ),
                );
            }
        }
        for d in &dims {
            if let Some((m, home)) = metrics
                .iter()
                .find_map(|m| stuck(d, m).map(|home| (m, home)))
            {
                return reject(
                    ErrorCode::PartialDimensionReachability,
                    format!(
                        "dimension `{}`: metric `{}` can't reach `{}` from `{}`, though another metric can; query them apart, or use Layer 2",
                        d.text, m.name, g.datasets[target(g, d.node())].name, g.datasets[home].name
                    ),
                );
            }
        }
    }

    // 15.
    let anchors: Vec<usize> = if metrics.is_empty() {
        let targets: BTreeSet<usize> = dims.iter().map(|d| target(g, d.node())).collect();
        let anchors: Vec<usize> = targets
            .iter()
            .copied()
            .filter(|&c| targets.iter().all(|&t| reaches(g, c, t)))
            .collect();
        if anchors.is_empty() {
            let names: Vec<&str> = targets
                .iter()
                .map(|&t| g.datasets[t].name.as_str())
                .collect();
            return reject(
                ErrorCode::NoAnchor,
                format!(
                    "no dataset among {} reaches the others; add a metric whose dataset reaches them all",
                    names.join(", ")
                ),
            );
        }
        anchors
    } else {
        vec![]
    };

    // 16.
    for (text, node) in &filtered {
        let to = target(g, *node);
        let dataset = &g.datasets[to].name;
        for m in &metrics {
            if let Some(home) = m.stuck_home(g, to) {
                return reject(
                    ErrorCode::UnsupportedFilterPath,
                    format!(
                        "filter `{text}`: metric `{}` can't reach `{dataset}` from `{}`; a Layer 2 query can apply a filter to one metric",
                        m.name, g.datasets[home].name
                    ),
                );
            }
        }
        if let [anchor] = anchors[..] {
            if !reaches(g, anchor, to) {
                return reject(
                    ErrorCode::UnsupportedFilterPath,
                    format!(
                        "filter `{text}`: the anchor `{}` can't reach `{dataset}`",
                        g.datasets[anchor].name
                    ),
                );
            }
        }
    }

    // 17.
    if let [_, _, ..] = anchors[..] {
        let names: Vec<&str> = anchors
            .iter()
            .map(|&a| g.datasets[a].name.as_str())
            .collect();
        return reject(
            ErrorCode::AmbiguousPath,
            format!(
                "{} all qualify as the anchor, since they reach each other",
                names.join(" and ")
            ),
        );
    }
    let roots: BTreeSet<usize> = metrics
        .iter()
        .flat_map(|m| m.homes.iter().copied())
        .chain(anchors)
        .collect();
    // Every leg joins every dimension and filter, but only its own terms' reads.
    let everywhere = dims
        .iter()
        .map(|d| (d.text, target(g, d.node())))
        .chain(filtered.iter().map(|(t, n)| (t.as_str(), target(g, *n))));
    let needed: Vec<(&str, usize, usize)> = everywhere
        .flat_map(|(text, to)| roots.iter().map(move |&root| (text, root, to)))
        .chain(metrics.iter().flat_map(|m| {
            m.reads
                .iter()
                .map(|&(root, to)| (m.name.as_str(), root, to))
        }))
        .collect();
    for (text, root, to) in needed {
        let all = g.paths_from(root);
        if let Some(paths) = all.get(&to).filter(|p| p.len() > 1) {
            let shown: Vec<String> = paths
                .iter()
                .map(|p| {
                    p.iter()
                        .map(|&r| g.rels[r].name.as_str())
                        .collect::<Vec<_>>()
                        .join(" → ")
                })
                .collect();
            return reject(
                ErrorCode::AmbiguousPath,
                format!(
                    "`{text}`: `{}` reaches `{}` in more than one way ({}); name the one you mean as `<relationship>.<field>`, or use Layer 2",
                    g.datasets[root].name,
                    g.datasets[to].name,
                    shown.join(" | ")
                ),
            );
        }
    }
    Ok(())
}

/// The message of a request error, without its prefix.
fn plain(e: Error) -> String {
    match e {
        Error::Request(m) => m,
        other => other.to_string(),
    }
}
