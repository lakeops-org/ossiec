//! SQL frontend for consumer queries with `AGG(measure)`.
//!
//! This is the **compiler front end** for Ossie's relational (Layer 2) *surface*:
//! parse AGG SQL, then hand a [`Request`] (or nested stages) to the same
//! `plan → lower → render` pipeline structured requests use. The module is named
//! for that role, not for the Ossie layer number.
//!
//! Spec: `relational_semantics.md` (draft). Appendix **A.1** (single `FROM` relation),
//! **A.4** (multi-fact measures sharing dimensions, plus `AGG` arithmetic in the
//! select list), and **A.5–A.6** (consumer joins along model relationships,
//! including unused many→one and reverse fan-out, with `INNER` / `LEFT` /
//! `RIGHT` / `FULL` / `CROSS`, §7.6) translate
//! to the existing [`Request`] planner. [`to_request`] expose
//! only **leaf** measures referenced inside `AGG(...)`, not composed output names,
//! and only for flat queries.
//!
//! **A.2–A.3** (derived tables) compile recursively in [`nested`]: every `AGG`
//! stage is an ordinary [`Request`] → plan → lower pass, derived tables join in
//! through the planner's `DerivedJoin` on each leg that hosts the attach dataset,
//! and an outer SQL aggregate over derived
//! tables (A.3) is a thin shell outside plan/lower.
//!
//! **§7.7** (unevaluated measures): a derived table may pass a dataset-scoped
//! measure through its select list; an outer `AGG(t.m)` evaluates it. A query-local
//! `(AGG(a) / AGG(b)) TO MEASURE AS m` item (Appendix A.7 / §5.2 surface) defers a
//! composed measure the same way. `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE AS m`
//! (§5.1) defers a base measure: an ephemeral dataset-scoped metric on a clone of
//! the graph, passed through for the outer `AGG`. [`define_layer2_view`] keeps a
//! named measure relation in a session [`Layer2Catalog`] (`CREATE VIEW`).
//! [`persist_layer2_view`] writes that view's `TO MEASURE` items into the model
//! as permanent metrics (and re-binds). Other join forms (`ASOF`, `SEMI`/`ANTI`,
//! `NATURAL`, `LATERAL`, …) stay [`Error::Unsupported`].
//!
//! **Roles.** A column qualified by a relationship name (`departs_from.city`) is a
//! Layer 3 role: the request dimension is `relationship.field`, and the planner
//! joins that dataset under `dataset__relationship`. The qualifier may be the
//! JOIN alias when that alias is the relationship name. A plain `dataset.field`
//! still takes the unique shortest path and is refused when two paths tie.
//!
//! **`includeUnmatched`.** [`Options::include_unmatched`] or a comment
//! `-- ossiec: includeUnmatched` sets [`Request::include_unmatched`] on the
//! outermost `AGG` stage. Derived joins that attach inside the unmatched grain's
//! subtree are renamed with it (`…__unmatched`); the fact-side probe still sees
//! every derived join.
//!
//! Only **dataset-scoped** metrics are measures (spec §10.3). Model-scoped
//! metrics are refused inside `AGG()`.

use std::collections::HashMap;

use polyglot_sql::builder::{func, lit, Expr};
use polyglot_sql::expressions::{
    BinaryOp, Column, DotAccess, Identifier, Join, JoinKind as SqlJoinKind, Literal, Select,
    TableRef,
};
use polyglot_sql::{
    get_columns, parse_one, transform_all, DialectType, Expression, ExpressionWalk,
};

use crate::graph::{self, SemanticGraph};
use crate::plan::{
    self, Direction, Filter, JoinKind, JoinStep, LogicalPlan, Nulls, Op, OrderBy, Request, Strategy,
};
use crate::{Compiled, Error, Options, Result};

mod catalog;
mod fold;
mod nested;
mod persist;
mod shell;
mod to_measure;

pub use catalog::{
    compile_script, define_layer2_view, Layer2Catalog, Layer2Column, Layer2ColumnKind,
};
pub use persist::persist_layer2_view;

use nested::{OutCol, OutKind};

/// Compile consumer SQL with `AGG(measure)` over model relations.
/// Derived tables (Appendix A.2–A.3) compile recursively; see [`nested`].
///
/// Views are not in scope. Register them with [`define_layer2_view`] and compile
/// through [`compile_with`], or pass a whole script to [`compile_script`].
pub fn compile(graph: &SemanticGraph, sql: &str, opts: &Options) -> Result<Compiled> {
    compile_with(graph, &Layer2Catalog::new(), sql, opts)
}

/// [`compile`] against a session [`Layer2Catalog`] of `CREATE VIEW` relations.
///
/// A view name in `FROM` / `JOIN` is that view's defining `SELECT`: fields and
/// deferred measures (`TO MEASURE`, §7.7 pass-through) are columns of the
/// relation, and `AGG(view.measure)` evaluates the deferred formula at this
/// query's grain.
pub fn compile_with(
    graph: &SemanticGraph,
    catalog: &Layer2Catalog,
    sql: &str,
    opts: &Options,
) -> Result<Compiled> {
    nested::compile(graph, catalog, sql, opts)
}

/// After output names change (aliases, composed metrics, dimension labels),
/// align `ORDER BY`, dimensions and `string_dims` with the consumer's select labels.
fn rename_consumer_outputs(
    logical: &mut LogicalPlan,
    measure_outputs: &[(String, L2Formula)],
    dim_labels: &[(String, String)],
) {
    let mut rename: HashMap<String, String> = HashMap::new();
    for (out, formula) in measure_outputs {
        if let L2Formula::Agg(m) = formula {
            if out != m {
                rename.insert(m.clone(), out.clone());
            }
        }
    }
    let mut dim_rename: HashMap<String, String> = HashMap::new();
    for (dim, label) in dim_labels {
        if dim != label {
            dim_rename.insert(dim.clone(), label.clone());
        }
    }
    for (name, _) in &mut logical.dims {
        if let Some(label) = dim_rename.get(name) {
            *name = label.clone();
        }
    }
    for name in &mut logical.string_dims {
        if let Some(label) = dim_rename.get(name) {
            *name = label.clone();
        }
    }
    for (name, _, _) in &mut logical.order {
        if let Some(alias) = rename.get(name).or_else(|| dim_rename.get(name)) {
            *name = alias.clone();
        }
    }
}

/// Translate Layer 2 SQL into a Layer 3 [`Request`] (join kinds are not
/// represented). Flat queries only: derived tables (A.2–A.3) are not one request.
pub fn to_request(graph: &SemanticGraph, sql: &str) -> Result<Request> {
    Ok(parse_layer2(graph, sql)?.request)
}

pub(super) struct Layer2Parsed {
    pub request: Request,
    /// Join kinds from the consumer `FROM` / `JOIN` list (Appendix A.5–A.6).
    pub consumer_joins: Vec<ConsumerJoin>,
    /// Composed select-list measures: (output column name, formula tree).
    pub measure_outputs: Vec<(String, L2Formula)>,
    /// Selected dimension (`dataset.field`) and the label the consumer sees it under.
    pub dim_labels: Vec<(String, String)>,
    /// Derived tables joined to the stage (Appendix A.2).
    pub derived: Vec<DerivedSpec>,
    /// Columns of this stage that are measures awaiting a later `AGG()` (§7.7):
    /// pass-through measures and query-local `TO MEASURE` items.
    pub unevaluated: Vec<UnevaluatedSpec>,
    /// Base `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE` items to install on the
    /// compile's graph clone before the stage is planned.
    pub ephemeral: Vec<EphemeralMetric>,
}

/// A query-local base measure: one SQL aggregate, owned by a single dataset.
#[derive(Debug, Clone)]
pub(super) struct EphemeralMetric {
    /// Canonical `dataset.metric` name.
    pub name: String,
    pub owner: usize,
    pub body: Expression,
    pub dataset: String,
    pub bare: String,
}

/// A select-list column that stays a measure for an outer `AGG()`.
pub(super) struct UnevaluatedSpec {
    pub name: String,
    /// Canonical metric for a pass-through; the alias for a `TO MEASURE` item.
    pub metric: String,
    /// `TO MEASURE`: the composed formula over leaf metrics.
    pub formula: Option<L2Formula>,
}

/// How a stage's select list is read.
pub(super) struct StageCfg<'a> {
    /// A derived table's columns are addressed by name from outside: dimensions
    /// default to their bare field name and every `AGG(...)` needs an alias.
    pub derived: bool,
    /// Aliases of enclosing queries (to name correlated references).
    pub outer: &'a [String],
    /// Expressions of the query's `TO MEASURE` items, by marker index.
    pub to_measures: &'a [Expression],
}

/// A derived table joined to an `AGG` stage: `JOIN (<select>) alias ON ...`.
pub(super) struct DerivedSpec {
    pub alias: String,
    pub select: Select,
    pub kind: JoinKind,
    /// Dataset the derived table attaches to (the dataset side of ON).
    pub attach_ds: usize,
    /// `(dataset, field)` per ON equality, attach side.
    pub from_fields: Vec<(usize, usize)>,
    /// Derived-table column per ON equality.
    pub cols: Vec<String>,
    pub null_safe: bool,
    /// Relationship the attach columns name (`relationship.field`), if any.
    pub attach_role: Option<usize>,
}

pub(super) struct ConsumerJoin {
    /// Model relationship. `None` for `CROSS JOIN` (no `ON`).
    rel: Option<usize>,
    kind: JoinKind,
    reverse: bool,
    attach_ds: usize,
    attach_alias: String,
    other_ds: usize,
    other_alias: String,
    /// Relationship the attach columns name, when they are `relationship.field`.
    attach_role: Option<usize>,
}

/// Select-list expression whose leaves are bare `AGG(measure)` references.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum L2Formula {
    Agg(String),
    Add(Box<L2Formula>, Box<L2Formula>),
    Sub(Box<L2Formula>, Box<L2Formula>),
    Mul(Box<L2Formula>, Box<L2Formula>),
    Div(Box<L2Formula>, Box<L2Formula>),
    NullIf(Box<L2Formula>, Box<L2Formula>),
    Lit(serde_json::Value),
}

/// One dataset in the consumer `FROM` / `JOIN` list and its SQL alias.
#[derive(Clone)]
struct TableBinding {
    dataset: usize,
    alias: String,
}

pub(super) struct QueryScope {
    bindings: Vec<TableBinding>,
    /// Aliases of derived tables in this stage's `FROM` (usable only in their ON,
    /// or as `AGG(alias.measure)` for their unevaluated measures, §7.7).
    derived: Vec<String>,
    /// Output columns of those derived tables (filled once they are compiled).
    derived_outs: Vec<(String, Vec<OutCol>)>,
    /// Aliases of enclosing queries: a reference to one is correlated.
    outer: Vec<String>,
}

/// Parse the consumer's single top-level `SELECT`. `TO MEASURE` items (which the
/// SQL parser cannot read) are cut out first; the returned expressions are theirs,
/// indexed by the marker column left in the select list (see [`to_measure`]).
pub(super) fn parse_top_select(
    graph: &SemanticGraph,
    sql: &str,
) -> Result<(Select, Vec<Expression>)> {
    let (text, to_measures) = to_measure::extract(sql.trim())?;
    let ast = parse_one(&text, DialectType::Generic)
        .map_err(|e| Error::Request(format!("Layer 2 SQL: {e}")))?;
    let (ast, to_measures) = strip_model_from_columns(graph, ast, to_measures)?;
    match ast {
        Expression::Select(sel) => Ok((*sel, to_measures)),
        Expression::CreateView(_) => Err(unsupported(
            "Layer 2 CREATE VIEW is not a query; register it with define_layer2_view (or pass CREATE VIEW statements and a final SELECT to compile_script / compile-l2)",
        )),
        Expression::DropView(_) => Err(unsupported(
            "Layer 2 DROP VIEW is not supported; views live in a session Layer2Catalog",
        )),
        _ => Err(unsupported(
            "Layer 2 accepts a single SELECT (set operators are not supported)",
        )),
    }
}

/// `model.dataset.field` is `dataset.field` (§11.2). The parser reads it as a dot
/// access on the column `model.dataset`, so turn that form into a column here,
/// before any clause looks at it.
pub(super) fn strip_model_from_columns(
    graph: &SemanticGraph,
    ast: Expression,
    to_measures: Vec<Expression>,
) -> Result<(Expression, Vec<Expression>)> {
    let model = &graph.model().name;
    let strip = |e: Expression| {
        transform_all(e, &|node| {
            Ok(match node {
                Expression::Dot(d) => match *d {
                    DotAccess {
                        this: Expression::Column(mut c),
                        field,
                        ..
                    } if c
                        .table
                        .as_ref()
                        .is_some_and(|t| t.name.eq_ignore_ascii_case(model)) =>
                    {
                        c.table = Some(c.name);
                        c.name = field;
                        Expression::Column(c)
                    }
                    other => Expression::Dot(Box::new(other)),
                },
                other => other,
            })
        })
        .map_err(|e| Error::Internal(format!("Layer 2 model-qualified column: {e}")))
    };
    let to_measures = to_measures.into_iter().map(strip).collect::<Result<_>>()?;
    Ok((strip(ast)?, to_measures))
}

fn parse_layer2(graph: &SemanticGraph, sql: &str) -> Result<Layer2Parsed> {
    let (sel, to_measures) = parse_top_select(graph, sql)?;
    let cfg = StageCfg {
        derived: false,
        outer: &[],
        to_measures: &to_measures,
    };
    let parsed_from = parse_stage_prep(graph, &sel, &cfg)?;
    if !parsed_from.derived.is_empty() {
        return Err(unsupported(
            "Layer 2 derived tables (Appendix A.2–A.3) are not one Layer 3 request; use compile_layer2",
        ));
    }
    let mut base_seq = 0usize;
    let mut parsed = parse_stage_from(graph, &sel, &cfg, parsed_from, &[], &mut base_seq)?;
    if !parsed.ephemeral.is_empty() {
        return Err(Error::Internal(
            "flat Layer 2 query cannot install a query-local base measure".into(),
        ));
    }
    if pragma_include_unmatched(sql) {
        parsed.request.include_unmatched = true;
    }
    Ok(parsed)
}

/// `-- ossiec: includeUnmatched` as its own `--` comment anywhere in the SQL.
pub(super) fn pragma_include_unmatched(sql: &str) -> bool {
    sql.lines().any(|line| {
        let Some((_, comment)) = line.split_once("--") else {
            return false;
        };
        comment
            .trim()
            .eq_ignore_ascii_case("ossiec: includeUnmatched")
    })
}

/// Clause checks and the `FROM` / `JOIN` list of one `AGG` stage.
pub(super) fn parse_stage_prep(
    graph: &SemanticGraph,
    sel: &Select,
    cfg: &StageCfg<'_>,
) -> Result<ParsedFrom> {
    if sel.with.is_some() || sel.distinct || sel.having.is_some() || sel.qualify.is_some() {
        return Err(unsupported(
            "Layer 2 WITH / DISTINCT / HAVING / QUALIFY are not implemented yet",
        ));
    }
    parse_from_clause(graph, sel.from.as_ref(), &sel.joins, cfg.outer)
}

/// Aliases a stage's `FROM` introduces: what a nested query must not reference.
fn stage_aliases(graph: &SemanticGraph, from: &ParsedFrom) -> Vec<String> {
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
            let out_name = alias
                .clone()
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
        // A derived table is read by column name: dimensions default to the bare
        // field name (`customer_id`), not `orders.customer_id`.
        let default_label = if cfg.derived {
            bare_field(&dim).to_string()
        } else {
            dim.clone()
        };
        let label = alias.unwrap_or(default_label);
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

    // `GROUP BY` must name exactly the selected dimensions. With `AGG` it is
    // required whenever dimensions are selected; without `AGG` it is optional
    // (a dimension listing is one row per combination anyway: Appendix A.3's `d`).
    match &sel.group_by {
        None if !metrics.is_empty() && !dimensions.is_empty() => {
            return Err(Error::Request(
                "Layer 2 aggregated query with dimensions needs GROUP BY".into(),
            ));
        }
        None => {}
        Some(gb) => {
            if gb.all.is_some() {
                return Err(unsupported("GROUP BY ALL is not supported in Layer 2"));
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
        measure_outputs,
        dim_labels,
        derived: parsed_from.derived,
        unevaluated,
        ephemeral,
    })
}

/// `Some` when `e` is one base aggregate to install as a dataset metric.
/// `None` when it is a composition of `AGG(measure)` (the caller parses that).
fn classify_to_measure(
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
        }));
    }
    if !measure {
        return Err(unsupported(
            "Layer 2 TO MEASURE needs one SUM|COUNT|AVG|MIN|MAX(...) or a parenthesized expression over AGG(measure)",
        ));
    }
    Ok(None)
}

fn is_base_agg_expr(e: &Expression) -> bool {
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

fn is_measure_agg_expr(e: &Expression) -> bool {
    match e {
        Expression::Function(f) if is_agg_name(&f.name) => true,
        Expression::AggregateFunction(f) if is_agg_name(&f.name) => true,
        _ => false,
    }
}

/// The whole item is one base aggregate, with no nested aggregate inside it.
fn is_single_base_aggregate(e: &Expression) -> bool {
    let e = unwrap_paren(e);
    is_base_agg_expr(e) && e.count(is_base_agg_expr) == 1 && !e.contains(is_measure_agg_expr)
}

/// The dataset that owns the aggregate, and the metric body (field names unqualified).
fn base_measure_body(
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

fn dataset_field(graph: &SemanticGraph, ds: usize, col: &Column) -> Result<String> {
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
                    "Layer 2 TO MEASURE base aggregate `{label}` is a measure on `{ds_name}`, not a field; name a field (SUM(amount)) or read the measure with AGG()"
                )))
            } else {
                Err(Error::Request(format!(
                    "Layer 2 TO MEASURE base aggregate `{label}` is not a field of `{ds_name}`"
                )))
            }
        }
    }
}

/// `COUNT(*)` (and any aggregate with no field) counts one dataset's grain.
fn grain_dataset(graph: &SemanticGraph, scope: &QueryScope) -> Result<usize> {
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
fn bare_field(dim: &str) -> &str {
    dim.split_once('.').map_or(dim, |(_, f)| f)
}

/// A bare column that names a dataset-scoped measure, not a field: the dataset
/// it is a measure of (the unevaluated-measure pass-through of spec §7.7).
pub(super) fn bare_measure(
    graph: &SemanticGraph,
    scope: &QueryScope,
    col: &Column,
) -> Option<usize> {
    let name = &col.name.name;
    let hit = scope.bindings.iter().find(|b| {
        let ds = &graph.datasets[b.dataset].name;
        let qualified_ok = col.table.as_ref().is_none_or(|t| {
            t.name.eq_ignore_ascii_case(&b.alias) || t.name.eq_ignore_ascii_case(ds)
        });
        qualified_ok
            && graph.metrics.iter().any(|m| {
                m.owner == Some(b.dataset)
                    && (m.name.eq_ignore_ascii_case(name)
                        || m.name.eq_ignore_ascii_case(&format!("{ds}.{name}")))
            })
    })?;
    Some(hit.dataset)
}

fn is_model_metric(graph: &SemanticGraph, name: &str) -> bool {
    graph
        .metrics
        .iter()
        .any(|m| m.owner.is_none() && m.name.eq_ignore_ascii_case(name))
}

fn model_scoped(measure: &str) -> Error {
    Error::Request(format!(
        "`{measure}` is a model-scoped metric; Layer 2 only exposes dataset-scoped metrics as measures (relational_semantics §10.3)"
    ))
}

fn apply_consumer_joins(
    graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    consumer: &[ConsumerJoin],
    ctx: &mut graph::Ctx,
) -> Result<()> {
    let mut added_reverse: Vec<usize> = vec![];
    for cj in consumer {
        if cj.kind == JoinKind::Cross {
            apply_cross_join(graph, logical, cj)?;
            continue;
        }
        let rel = cj.rel.expect("relationship join");
        let mut patched = false;
        for leg in &mut logical.legs {
            for step in &mut leg.joins {
                if step.rel == Some(rel) {
                    step.kind = cj.kind;
                    patched = true;
                }
            }
        }
        if patched {
            continue;
        }
        let leg_i = consumer_join_leg(graph, logical, cj, rel)?;
        // ON must use the same aliases `tree_sql` uses for the leg (dataset
        // name for the root, `to_alias` for prior joins)—not the consumer SQL
        // alias (`o` vs `orders`).
        let from_alias =
            planned_alias_on_leg(graph, &logical.legs[leg_i], cj.attach_ds, cj.attach_role)
                .ok_or_else(|| {
                    unsupported(format!(
                        "Layer 2 join on `{}`: attach dataset `{}` has no planned alias on its leg",
                        graph.rels[rel].name, graph.datasets[cj.attach_ds].name
                    ))
                })?;
        let leg = &mut logical.legs[leg_i];
        leg.joins.push(JoinStep {
            rel: Some(rel),
            from_alias,
            to: cj.other_ds,
            to_alias: cj.other_alias.clone(),
            kind: cj.kind,
            reverse: cj.reverse,
        });
        if cj.reverse {
            added_reverse.push(leg_i);
        }
    }
    for leg_i in added_reverse {
        let root = logical.legs[leg_i].root;
        // G2 protects measures. A stage that only passes a measure through (or
        // lists dimensions) has nothing to duplicate; the join still filters.
        let dedup: Vec<usize> = logical
            .terms
            .iter()
            .enumerate()
            .filter(|(_, term)| {
                term.leg == leg_i && term.strategy == Strategy::Inline && !term.term.dup_insensitive
            })
            .map(|(i, _)| i)
            .collect();
        if dedup.is_empty() {
            continue;
        }
        if graph.datasets[root].primary_key.is_empty() {
            return Err(unsupported(format!(
                "Layer 2 fan-out join requires a primary_key on `{}` so measures stay duplicate-free (G2)",
                graph.datasets[root].name
            )));
        }
        for i in dedup {
            let term = &mut logical.terms[i];
            ctx.warn(format!(
                "FAN_TRAP_RISK: `{}` is computed over `{}` rows de-duplicated by primary key; a `{}` row that belongs to several groups counts in each, so groups do not add up to a grand total",
                term.term.key,
                graph.datasets[root].name,
                graph.datasets[root].name
            ));
            term.strategy = Strategy::PkDedup { grain: root };
        }
    }
    Ok(())
}

/// `CROSS JOIN` has no relationship. One measure leg: either replace the planned
/// equijoin of that dataset with `CROSS JOIN`, or append one when the dataset
/// was not already required. Several legs have no single join tree to put a
/// Cartesian product on.
fn apply_cross_join(
    graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    cj: &ConsumerJoin,
) -> Result<()> {
    if logical.legs.len() != 1 {
        return Err(unsupported(
            "Layer 2 CROSS JOIN cannot span independent facts (relational_semantics §7.6); a Cartesian product has no single measure leg to attach to",
        ));
    }
    let root = logical.legs[0].root;
    let on_leg = |ds: usize| planned_alias_on_leg(graph, &logical.legs[0], ds, None).is_some();
    if !on_leg(cj.attach_ds) && !on_leg(cj.other_ds) {
        return Err(unsupported(
            "Layer 2 CROSS JOIN cannot attach to the planned query; neither side is on the measure's join tree (relational_semantics §7.6)",
        ));
    }
    // The fact stays the root. Cross the other dataset onto it.
    let target = if cj.other_ds == root {
        cj.attach_ds
    } else {
        cj.other_ds
    };
    if target == root {
        return Err(unsupported(
            "Layer 2 CROSS JOIN needs two datasets; both sides resolved to the same fact",
        ));
    }
    let alias = if target == cj.other_ds {
        cj.other_alias.clone()
    } else {
        cj.attach_alias.clone()
    };
    let name = graph.datasets[target].name.clone();
    let hits: Vec<usize> = logical.legs[0]
        .joins
        .iter()
        .enumerate()
        .filter(|(_, s)| s.to == target)
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [i] => {
            let step = &mut logical.legs[0].joins[*i];
            if step.kind == JoinKind::Cross {
                return Err(unsupported(format!(
                    "Layer 2 duplicate CROSS JOIN of `{name}`"
                )));
            }
            step.kind = JoinKind::Cross;
        }
        [] => {
            logical.legs[0].joins.push(JoinStep {
                rel: None,
                from_alias: graph.datasets[root].name.clone(),
                to: target,
                to_alias: alias,
                kind: JoinKind::Cross,
                reverse: false,
            });
        }
        _ => {
            return Err(unsupported(format!(
                "Layer 2 CROSS JOIN of `{name}` is ambiguous; the planned query joins it more than once"
            )));
        }
    }
    Ok(())
}

/// RIGHT/FULL add null-extended rows and CROSS multiplies them. Inline
/// `COUNT(*)` / `SUM` would count those rows. De-duplicate the fact by its
/// primary key (the same path as fan-out) so each source row enters a group
/// once, while the base part still emits every group the join produced (§7.6).
pub(super) fn protect_inflating_joins(
    graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    ctx: &mut graph::Ctx,
) -> Result<()> {
    for leg_i in 0..logical.legs.len() {
        let inflates = logical.legs[leg_i]
            .joins
            .iter()
            .any(|s| s.kind.inflates_inline())
            || logical.legs[leg_i]
                .derived
                .iter()
                .any(|d| d.kind.inflates_inline());
        if !inflates {
            continue;
        }
        let root = logical.legs[leg_i].root;
        if graph.datasets[root].primary_key.is_empty() {
            return Err(unsupported(format!(
                "Layer 2 RIGHT, FULL, or CROSS JOIN requires a primary_key on `{}` so measures stay duplicate-free (relational_semantics §7.6 G2)",
                graph.datasets[root].name
            )));
        }
        for term in &mut logical.terms {
            if term.leg != leg_i || term.strategy != Strategy::Inline || term.term.dup_insensitive {
                continue;
            }
            ctx.warn(format!(
                "FAN_TRAP_RISK: `{}` is computed over `{}` rows de-duplicated by primary key; a `{}` row that belongs to several groups counts in each, so groups do not add up to a grand total",
                term.term.key,
                graph.datasets[root].name,
                graph.datasets[root].name
            ));
            term.strategy = Strategy::PkDedup { grain: root };
        }
    }
    Ok(())
}

fn consumer_join_leg(
    graph: &SemanticGraph,
    logical: &LogicalPlan,
    cj: &ConsumerJoin,
    rel: usize,
) -> Result<usize> {
    for (i, leg) in logical.legs.iter().enumerate() {
        if leg_hosts_attach(graph, leg, cj.attach_ds, cj.attach_role) {
            return Ok(i);
        }
    }
    Err(unsupported(format!(
        "Layer 2 join on relationship `{}` cannot attach to the planned query; no leg includes `{}` as `{}`",
        graph.rels[rel].name,
        graph.datasets[cj.attach_ds].name,
        cj.attach_alias
    )))
}

fn leg_hosts_attach(
    graph: &SemanticGraph,
    leg: &plan::Leg,
    attach_ds: usize,
    attach_role: Option<usize>,
) -> bool {
    planned_alias_on_leg(graph, leg, attach_ds, attach_role).is_some()
}

/// Alias `lower` will use for `ds` on this leg (`tree_sql` / `leg_sql`).
///
/// `role` selects the join that plays that relationship (`dataset__relationship`,
/// or the consumer alias of a join that was added for it). Without a role, a
/// plain dataset alias wins; a single join to `ds` is used otherwise. Several
/// role aliases and no role is ambiguous — this returns `None` rather than the
/// first one.
pub(super) fn planned_alias_on_leg(
    graph: &SemanticGraph,
    leg: &plan::Leg,
    ds: usize,
    role: Option<usize>,
) -> Option<String> {
    if role.is_none() && leg.root == ds {
        return Some(graph.datasets[ds].name.clone());
    }
    let mut hits = Vec::new();
    for s in &leg.joins {
        let joined = if s.reverse {
            graph.rels[s.rel.expect("reverse relationship join")].from
        } else {
            s.to
        };
        if joined != ds {
            continue;
        }
        if let Some(r) = role {
            if s.rel == Some(r) {
                return Some(s.to_alias.clone());
            }
            continue;
        }
        hits.push(s.to_alias.clone());
    }
    if role.is_some() {
        return None;
    }
    let plain = graph.datasets[ds].name.as_str();
    if let Some(alias) = hits.iter().find(|a| a.as_str() == plain) {
        return Some(alias.clone());
    }
    if hits.len() == 1 {
        return Some(hits[0].clone());
    }
    None
}

fn apply_select_formulas(
    _graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    measure_outputs: &[(String, L2Formula)],
) -> Result<()> {
    if measure_outputs.is_empty() {
        return Ok(());
    }
    let metric_outers: HashMap<String, Expression> = logical.outputs.iter().cloned().collect();
    let mut outputs = Vec::with_capacity(measure_outputs.len());
    for (name, formula) in measure_outputs {
        let expr = formula_to_expr(formula, &metric_outers)?;
        outputs.push((name.clone(), graph::true_division(expr)));
    }
    logical.outputs = outputs;
    Ok(())
}

fn formula_to_expr(
    formula: &L2Formula,
    metrics: &HashMap<String, Expression>,
) -> Result<Expression> {
    Ok(match formula {
        L2Formula::Agg(m) => metrics.get(m).cloned().ok_or_else(|| {
            Error::Request(format!("internal: missing planned output for metric `{m}`"))
        })?,
        L2Formula::Add(l, r) => Expression::Add(Box::new(BinaryOp::new(
            formula_to_expr(l, metrics)?,
            formula_to_expr(r, metrics)?,
        ))),
        L2Formula::Sub(l, r) => Expression::Sub(Box::new(BinaryOp::new(
            formula_to_expr(l, metrics)?,
            formula_to_expr(r, metrics)?,
        ))),
        L2Formula::Mul(l, r) => Expression::Mul(Box::new(BinaryOp::new(
            formula_to_expr(l, metrics)?,
            formula_to_expr(r, metrics)?,
        ))),
        L2Formula::Div(l, r) => Expression::Div(Box::new(BinaryOp::new(
            formula_to_expr(l, metrics)?,
            formula_to_expr(r, metrics)?,
        ))),
        L2Formula::NullIf(l, r) => func(
            "NULLIF",
            [
                Expr(formula_to_expr(l, metrics)?),
                Expr(formula_to_expr(r, metrics)?),
            ],
        )
        .into_inner(),
        L2Formula::Lit(v) => literal_expr(v)?,
    })
}

fn literal_expr(v: &serde_json::Value) -> Result<Expression> {
    match v {
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Expression::Literal(Literal::Number(i.to_string()).into()))
            } else {
                Ok(Expression::Literal(Literal::Number(n.to_string()).into()))
            }
        }
        serde_json::Value::String(s) => Ok(Expression::Literal(Literal::String(s.clone()).into())),
        serde_json::Value::Bool(b) => Ok(lit(*b).into_inner()),
        serde_json::Value::Null => Ok(Expression::Null(polyglot_sql::expressions::Null)),
        _ => Err(unsupported(
            "Layer 2 only supports numeric and string literals in AGG expressions",
        )),
    }
}

fn parse_measure_formula(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
) -> Result<Option<L2Formula>> {
    let e = unwrap_paren(e);
    if let Some(measure) = take_agg(e)? {
        return Ok(Some(resolve_agg(graph, scope, measure)?));
    }
    if as_column(e).is_some() {
        return Ok(None);
    }
    Ok(Some(parse_formula(graph, scope, e)?))
}

fn parse_formula(graph: &SemanticGraph, scope: &QueryScope, e: &Expression) -> Result<L2Formula> {
    let e = unwrap_paren(e);
    match e {
        Expression::Add(b) => Ok(L2Formula::Add(
            Box::new(parse_formula_or_agg(graph, scope, &b.left)?),
            Box::new(parse_formula_or_agg(graph, scope, &b.right)?),
        )),
        Expression::Sub(b) => Ok(L2Formula::Sub(
            Box::new(parse_formula_or_agg(graph, scope, &b.left)?),
            Box::new(parse_formula_or_agg(graph, scope, &b.right)?),
        )),
        Expression::Mul(b) => Ok(L2Formula::Mul(
            Box::new(parse_formula_or_agg(graph, scope, &b.left)?),
            Box::new(parse_formula_or_agg(graph, scope, &b.right)?),
        )),
        Expression::Div(b) => Ok(L2Formula::Div(
            Box::new(parse_formula_or_agg(graph, scope, &b.left)?),
            Box::new(parse_formula_or_agg(graph, scope, &b.right)?),
        )),
        Expression::Function(f)
            if f.name.eq_ignore_ascii_case("NULLIF") && f.args.len() == 2 =>
        {
            Ok(L2Formula::NullIf(
                Box::new(parse_formula_or_agg(graph, scope, &f.args[0])?),
                Box::new(parse_formula_or_agg(graph, scope, &f.args[1])?),
            ))
        }
        Expression::Literal(l) => {
            let v = literal_json(&Expression::Literal(l.clone())).ok_or_else(|| {
                unsupported("Layer 2 AGG expressions only support numeric literals")
            })?;
            Ok(L2Formula::Lit(v))
        }
        Expression::Column(_) => Err(unsupported(
            "Layer 2 measure expressions must use AGG(measure) leaves, not bare columns",
        )),
        _ => Err(unsupported(
            "Layer 2 AGG expressions allow + - * /, parentheses, numeric literals, and NULLIF(...); other functions are not supported",
        )),
    }
}

fn parse_formula_or_agg(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
) -> Result<L2Formula> {
    let e = unwrap_paren(e);
    if let Some(measure) = take_agg(e)? {
        return resolve_agg(graph, scope, measure);
    }
    parse_formula(graph, scope, e)
}

fn unwrap_paren(e: &Expression) -> &Expression {
    match e {
        Expression::Paren(p) => unwrap_paren(&p.this),
        other => other,
    }
}

fn formula_leaves(f: &L2Formula) -> Vec<String> {
    match f {
        L2Formula::Agg(m) => vec![m.clone()],
        L2Formula::Add(l, r)
        | L2Formula::Sub(l, r)
        | L2Formula::Mul(l, r)
        | L2Formula::Div(l, r)
        | L2Formula::NullIf(l, r) => {
            let mut v = formula_leaves(l);
            v.extend(formula_leaves(r));
            v.sort();
            v.dedup();
            v
        }
        L2Formula::Lit(_) => vec![],
    }
}

fn default_measure_name(f: &L2Formula, idx: usize) -> String {
    match f {
        L2Formula::Agg(m) => m.clone(),
        _ => format!("__l2_{idx}"),
    }
}

fn unsupported(msg: impl Into<String>) -> Error {
    Error::Unsupported(msg.into())
}

pub(super) struct ParsedFrom {
    pub(super) scope: QueryScope,
    consumer_joins: Vec<ConsumerJoin>,
    pub derived: Vec<DerivedSpec>,
}

/// `(SELECT ...) alias` as a `FROM` / `JOIN` item: the inner `SELECT` and its alias.
pub(super) fn as_derived(e: &Expression) -> Result<Option<(&Select, String)>> {
    let (sub, outer_alias) = match e {
        Expression::Subquery(s) => (s.as_ref(), None),
        Expression::Alias(a) => match &a.this {
            Expression::Subquery(s) => (s.as_ref(), Some(a.alias.name.clone())),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let alias = sub
        .alias
        .as_ref()
        .map(|a| a.name.clone())
        .or(outer_alias)
        .ok_or_else(|| Error::Request("Layer 2 derived table needs an alias".into()))?;
    if sub.lateral {
        return Err(unsupported(
            "Layer 2 correlated (LATERAL) derived tables are not supported",
        ));
    }
    if !sub.column_aliases.is_empty()
        || sub.order_by.is_some()
        || sub.limit.is_some()
        || sub.offset.is_some()
    {
        return Err(unsupported(
            "Layer 2 derived table: name output columns with AS in its select list, and put ORDER BY / LIMIT inside the subquery's SELECT",
        ));
    }
    match &sub.this {
        Expression::Select(s) => Ok(Some((s.as_ref(), alias))),
        _ => Err(unsupported(
            "Layer 2 derived table must be a plain SELECT (set operators and nested parentheses are not supported)",
        )),
    }
}

pub(super) fn parse_from_clause(
    graph: &SemanticGraph,
    from: Option<&polyglot_sql::expressions::From>,
    joins: &[Join],
    outer: &[String],
) -> Result<ParsedFrom> {
    let from = from.ok_or_else(|| Error::Request("Layer 2 query needs a FROM clause".into()))?;
    if from.expressions.is_empty() {
        return Err(Error::Request("Layer 2 query needs a FROM clause".into()));
    }
    let mut scope = QueryScope {
        bindings: vec![],
        derived: vec![],
        derived_outs: vec![],
        outer: outer.to_vec(),
    };
    let mut derived_specs: Vec<DerivedSpec> = Vec::new();
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
        if let Some((select, alias)) = as_derived(&j.this)? {
            let kind = consumer_join_kind(j)?;
            if kind == JoinKind::Cross {
                if j.on.is_some() {
                    return Err(cross_join_on());
                }
                if !j.using.is_empty() {
                    return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
                }
                if scope.bindings.is_empty() {
                    return Err(unsupported(
                        "Layer 2 CROSS JOIN of a derived table needs a model dataset in FROM",
                    ));
                }
                let taken = scope
                    .bindings
                    .iter()
                    .map(|b| &b.alias)
                    .chain(&scope.derived)
                    .any(|a| a.eq_ignore_ascii_case(&alias));
                if taken {
                    return Err(Error::Request(format!(
                        "Layer 2 derived table alias `{alias}` collides with another relation in FROM/JOIN"
                    )));
                }
                let attach_ds = scope.bindings[0].dataset;
                scope.derived.push(alias.clone());
                derived_specs.push(DerivedSpec {
                    alias,
                    select: select.clone(),
                    kind,
                    attach_ds,
                    attach_role: None,
                    from_fields: vec![],
                    cols: vec![],
                    null_safe: false,
                });
                continue;
            }
            let spec = parse_derived_join(graph, &scope, j, select, alias)?;
            scope.derived.push(spec.alias.clone());
            derived_specs.push(spec);
            continue;
        }
        let kind = consumer_join_kind(j)?;
        if !j.using.is_empty() {
            return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
        }
        let (right_ds, right_alias) = table_expr(graph, &j.this)?;
        let right_binding = TableBinding {
            dataset: right_ds,
            alias: right_alias.unwrap_or_else(|| graph.datasets[right_ds].name.clone()),
        };
        if kind == JoinKind::Cross {
            if j.on.is_some() {
                return Err(cross_join_on());
            }
            if scope.bindings.is_empty() {
                return Err(unsupported(
                    "Layer 2 CROSS JOIN needs a dataset already in FROM",
                ));
            }
            if scope.bindings.iter().any(|b| b.dataset == right_ds) {
                return Err(unsupported(format!(
                    "Layer 2 CROSS JOIN target `{}` is already in FROM",
                    graph.datasets[right_ds].name
                )));
            }
            let attach = &scope.bindings[0];
            consumer_joins.push(ConsumerJoin {
                rel: None,
                kind,
                reverse: false,
                attach_ds: attach.dataset,
                attach_alias: attach.alias.clone(),
                other_ds: right_ds,
                other_alias: right_binding.alias.clone(),
                attach_role: None,
            });
            scope.bindings.push(right_binding);
            continue;
        }
        let on = j.on.as_ref().ok_or_else(|| {
            unsupported("Layer 2 JOIN requires an ON condition matching a model relationship")
        })?;
        let pairs = on_equality_pairs(on)?;
        let join_scope = QueryScope {
            bindings: scope
                .bindings
                .iter()
                .chain(std::iter::once(&right_binding))
                .cloned()
                .collect(),
            derived: scope.derived.clone(),
            derived_outs: vec![],
            outer: scope.outer.clone(),
        };
        let rel = match_relationship(graph, &join_scope, &pairs)?;
        if consumer_joins
            .iter()
            .any(|c: &ConsumerJoin| c.rel == Some(rel))
        {
            return Err(unsupported(format!(
                "Layer 2 duplicate join on relationship `{}`",
                graph.rels[rel].name
            )));
        }
        let rel_info = &graph.rels[rel];
        let from_in_scope = scope.bindings.iter().any(|b| b.dataset == rel_info.from);
        let to_in_scope = scope.bindings.iter().any(|b| b.dataset == rel_info.to);
        let forward = from_in_scope && right_ds == rel_info.to;
        let reverse = to_in_scope && right_ds == rel_info.from;
        if forward == reverse {
            return Err(unsupported(format!(
                "Layer 2 JOIN on `{}` is ambiguous; one side must already be in FROM/JOIN",
                rel_info.name
            )));
        }
        let (attach_ds, other_ds) = if reverse {
            (rel_info.to, rel_info.from)
        } else {
            (rel_info.from, rel_info.to)
        };
        let attach_alias = scope
            .bindings
            .iter()
            .find(|b| b.dataset == attach_ds)
            .map(|b| b.alias.clone())
            .ok_or_else(|| {
                Error::Request(format!(
                    "Layer 2 JOIN on `{}`: attach dataset `{}` must already be in scope",
                    rel_info.name, graph.datasets[attach_ds].name
                ))
            })?;
        let attach_role = on_attach_role(graph, &join_scope, &pairs, attach_ds)?;
        consumer_joins.push(ConsumerJoin {
            rel: Some(rel),
            kind,
            reverse,
            attach_ds,
            attach_alias,
            other_ds,
            other_alias: right_binding.alias.clone(),
            attach_role,
        });
        scope.bindings.push(right_binding);
    }
    Ok(ParsedFrom {
        scope,
        consumer_joins,
        derived: derived_specs,
    })
}

/// `JOIN (SELECT ...) t ON dataset.col = t.col AND ...` in an `AGG` stage.
fn parse_derived_join(
    graph: &SemanticGraph,
    scope: &QueryScope,
    j: &Join,
    select: &Select,
    alias: String,
) -> Result<DerivedSpec> {
    let kind = consumer_join_kind(j)?;
    if !j.using.is_empty() {
        return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
    }
    let on = j.on.as_ref().ok_or_else(|| {
        unsupported("Layer 2 derived-table JOIN requires an ON of dataset-column = derived-column")
    })?;
    let taken = scope
        .bindings
        .iter()
        .map(|b| &b.alias)
        .chain(&scope.derived)
        .any(|a| a.eq_ignore_ascii_case(&alias));
    if taken {
        return Err(Error::Request(format!(
            "Layer 2 derived table alias `{alias}` collides with another relation in FROM/JOIN"
        )));
    }
    let on_err = || {
        unsupported(format!(
            "Layer 2 derived-table JOIN ON must be AND of `<dataset column> = {alias}.<column>` (or IS NOT DISTINCT FROM)"
        ))
    };
    let is_derived_col = |c: &Column| {
        c.table
            .as_ref()
            .is_some_and(|t| t.name.eq_ignore_ascii_case(&alias))
    };
    let mut attach_ds: Option<usize> = None;
    let mut attach_role: Option<Option<usize>> = None;
    let mut from_fields = vec![];
    let mut cols = vec![];
    let mut null_safe: Option<bool> = None;
    for (l, r, ns) in on_pairs_null_safe(on)? {
        let (dcol, col) = match (is_derived_col(&l), is_derived_col(&r)) {
            (true, false) => (l, r),
            (false, true) => (r, l),
            _ => return Err(on_err()),
        };
        let (ds, field) = resolve_field_ref(graph, scope, &col)?;
        if *attach_ds.get_or_insert(ds) != ds {
            return Err(unsupported(
                "Layer 2 derived-table JOIN ON must attach to one dataset",
            ));
        }
        let role = column_role(graph, &col, ds)?;
        match attach_role {
            None => attach_role = Some(role),
            Some(prev) if prev != role => {
                return Err(unsupported(format!(
                    "Layer 2 derived-table JOIN ON must attach to one role of `{}`",
                    graph.datasets[ds].name
                )));
            }
            _ => {}
        }
        if *null_safe.get_or_insert(ns) != ns {
            return Err(unsupported(
                "Layer 2 derived-table JOIN ON cannot mix = and IS NOT DISTINCT FROM",
            ));
        }
        let f = graph.datasets[ds]
            .fields
            .iter()
            .position(|f| f.name == field)
            .expect("resolved field exists");
        from_fields.push((ds, f));
        cols.push(dcol.name.name.clone());
    }
    let attach_ds = attach_ds.ok_or_else(on_err)?;
    Ok(DerivedSpec {
        alias,
        select: select.clone(),
        kind,
        attach_ds,
        from_fields,
        cols,
        null_safe: null_safe.unwrap_or(false),
        attach_role: attach_role.flatten(),
    })
}

/// ON equalities, each flagged null-safe (`IS NOT DISTINCT FROM`) or plain `=`.
fn on_pairs_null_safe(on: &Expression) -> Result<Vec<(Column, Column, bool)>> {
    let pair = |l: &Expression, r: &Expression, ns: bool| -> Result<Vec<(Column, Column, bool)>> {
        let l = as_column(l).ok_or_else(on_must_be_equalities)?;
        let r = as_column(r).ok_or_else(on_must_be_equalities)?;
        Ok(vec![(l.to_owned(), r.to_owned(), ns)])
    };
    match on {
        Expression::Eq(b) => pair(&b.left, &b.right, false),
        Expression::NullSafeEq(b) => pair(&b.left, &b.right, true),
        Expression::And(a) => {
            let mut out = on_pairs_null_safe(&a.left)?;
            out.extend(on_pairs_null_safe(&a.right)?);
            Ok(out)
        }
        _ => Err(on_must_be_equalities()),
    }
}

pub(super) fn cross_join_on() -> Error {
    unsupported(
        "Layer 2 CROSS JOIN does not take an ON condition; it is a Cartesian product (relational_semantics §7.6). Drop ON, or use INNER, LEFT, RIGHT, or FULL JOIN",
    )
}

fn consumer_join_kind(j: &Join) -> Result<JoinKind> {
    match j.kind {
        SqlJoinKind::Inner | SqlJoinKind::Implicit => Ok(JoinKind::Inner),
        SqlJoinKind::Left | SqlJoinKind::NaturalLeft => Ok(JoinKind::Left),
        SqlJoinKind::Right => Ok(JoinKind::Right),
        SqlJoinKind::Full => Ok(JoinKind::Full),
        SqlJoinKind::Cross => Ok(JoinKind::Cross),
        SqlJoinKind::AsOf
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
            "Layer 2 supports INNER, LEFT, RIGHT, FULL, and CROSS JOIN (relational_semantics §7.6); this join kind is not supported",
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
        if scope
            .derived
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&t.name))
        {
            return Err(unsupported(format!(
                "Layer 2 derived-table column `{}.{}` may only appear in that table's JOIN ... ON (not in SELECT, WHERE, GROUP BY or ORDER BY of an AGG stage)",
                t.name, col.name.name
            )));
        }
        if scope.outer.iter().any(|a| a.eq_ignore_ascii_case(&t.name)) {
            return Err(unsupported(format!(
                "Layer 2 correlated subquery: `{}.{}` refers to an enclosing query; derived tables must be self-contained",
                t.name, col.name.name
            )));
        }
        if let Some(role) = relationship_by_name(graph, &t.name, t.quoted)? {
            let rel = &graph.rels[role];
            if scope.bindings.iter().any(|b| b.dataset == rel.from) {
                return Ok(rel.to);
            }
            return Err(Error::Request(format!(
                "column `{}.{}` names relationship `{}`, but dataset `{}` is not in FROM/JOIN",
                t.name, col.name.name, rel.name, graph.datasets[rel.from].name
            )));
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
        Expression::Subquery(_) => Err(unsupported(
            "Layer 2 AGG stage must start FROM a model dataset; join derived tables (Appendix A.2) \
             with JOIN (SELECT ...) t ON <dataset column> = t.<column>. A query over derived \
             tables only (A.3) is compiled by compile_layer2, not layer2_to_request",
        )),
        _ => Err(unsupported(
            "Layer 2 FROM must be a model dataset (table or alias) or a derived table",
        )),
    }
}

fn resolve_table(graph: &SemanticGraph, t: &TableRef) -> Result<(usize, Option<String>)> {
    // A model's relations live in one schema named after the model (§11.2).
    let model = &graph.model().name;
    let in_model = t
        .schema
        .as_ref()
        .is_none_or(|s| s.name.eq_ignore_ascii_case(model));
    if t.catalog.is_some() || !in_model {
        return Err(unsupported(format!(
            "Layer 2 FROM should name a model dataset, as `dataset` or `{model}.dataset`, not a catalog.schema.table",
        )));
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

/// The argument of `AGG(...)`: a dataset measure, or a derived table's
/// unevaluated measure column `AGG(t.m)` (§7.7), which stands for the metric it
/// passes through, or for the leaves of its `TO MEASURE` formula.
fn resolve_agg(graph: &SemanticGraph, scope: &QueryScope, arg: &Expression) -> Result<L2Formula> {
    if let Some(col) = as_column(arg) {
        if let Some(t) = &col.table {
            if let Some((alias, outs)) = scope
                .derived_outs
                .iter()
                .find(|(a, _)| a.eq_ignore_ascii_case(&t.name))
            {
                let name = &col.name.name;
                let out = outs
                    .iter()
                    .find(|o| o.name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| {
                        Error::Request(format!(
                            "derived table `{alias}` has no column `{name}`; it has: {}",
                            outs.iter()
                                .map(|o| o.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;
                return match &out.kind {
                    OutKind::UnevaluatedMeasure { metric, formula } => {
                        Ok(formula.clone().unwrap_or_else(|| L2Formula::Agg(metric.clone())))
                    }
                    OutKind::EvaluatedMeasure => Err(unsupported(format!(
                        "Layer 2 AGG() applies to measures; `{alias}.{name}` of a derived table is already evaluated: use it as a plain column (outer AVG/SUM/...) or pass the measure through unevaluated"
                    ))),
                    OutKind::Field => Err(Error::Request(format!(
                        "AGG() argument `{alias}.{name}` is a dimension of the derived table, not a measure"
                    ))),
                };
            }
        }
    }
    Ok(L2Formula::Agg(resolve_measure(graph, scope, arg)?))
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
    let field_name = &graph.datasets[dataset].fields[f].name;
    if let Some(role) = column_role(graph, col, dataset)? {
        return Ok(format!("{}.{}", graph.rels[role].name, field_name));
    }
    Ok(format!("{ds_name}.{field_name}"))
}

/// The relationship a column's qualifier names, when that relationship's `to`
/// dataset is `dataset`. That is Layer 3's `relationship.field` role.
fn column_role(graph: &SemanticGraph, col: &Column, dataset: usize) -> Result<Option<usize>> {
    let Some(t) = &col.table else {
        return Ok(None);
    };
    let Some(role) = relationship_by_name(graph, &t.name, t.quoted)? else {
        return Ok(None);
    };
    if graph.rels[role].to == dataset {
        Ok(Some(role))
    } else {
        Ok(None)
    }
}

fn relationship_by_name(graph: &SemanticGraph, name: &str, quoted: bool) -> Result<Option<usize>> {
    crate::graph::lookup(graph.rels.iter().map(|r| r.name.as_str()), name, quoted)
        .map_err(Error::Request)
}

/// Role shared by every ON column of `attach_ds` (`None` when they name the dataset).
fn on_attach_role(
    graph: &SemanticGraph,
    scope: &QueryScope,
    pairs: &[(Column, Column)],
    attach_ds: usize,
) -> Result<Option<usize>> {
    let mut found: Option<Option<usize>> = None;
    for (l, r) in pairs {
        for col in [l, r] {
            let ds = resolve_column_dataset(graph, scope, col)?;
            if ds != attach_ds {
                continue;
            }
            let role = column_role(graph, col, ds)?;
            match found {
                None => found = Some(role),
                Some(prev) if prev != role => {
                    return Err(unsupported(format!(
                        "Layer 2 JOIN ON names `{}` both as itself and as a role; qualify every attach column the same way",
                        graph.datasets[attach_ds].name
                    )));
                }
                _ => {}
            }
        }
    }
    Ok(found.flatten())
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
        return match resolve_agg(ctx.graph, ctx.scope, arg)? {
            L2Formula::Agg(m) => Ok(m),
            _ => Err(unsupported(
                "Layer 2 ORDER BY AGG(...) of a TO MEASURE column: select it with an alias and ORDER BY the alias",
            )),
        };
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
