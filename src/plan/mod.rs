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
use serde_json::Value;

use crate::graph::{self, Ctx, SemanticGraph, Term, DIM, TERM};

/// Placeholder for requested dimension `i` inside a metric's outer expression.
pub(crate) const OUT_DIM: &str = "__ossiec_q";
use crate::sql::{self, Tpl};
use crate::wide_table::{self};
use crate::{Error, Result};

pub(crate) use crate::request::{final_part, unquoted};
pub use crate::request::{
    Direction, Filter, MetricFilter, Nulls, Op, OrderBy, Request, Selected, TimeGrain,
};

mod legs;
pub(crate) use legs::grain_of;
use legs::{branches, plan_legs};

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
