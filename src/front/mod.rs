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

use polyglot_sql::expressions::{DotAccess, Select};
use polyglot_sql::{parse_one, transform_all, DialectType, Expression};

use crate::graph::SemanticGraph;
use crate::plan::{self, Direction, JoinKind, LogicalPlan, Nulls, Request};
use crate::{Compiled, Error, Options, Result};

use crate::protect::protect_inflating_joins;
use joins::{apply_consumer_joins, mark_narrowing_legs, planned_alias_on_leg};

mod catalog;
mod clauses;
mod cte;
mod fold;
mod formula;
mod from;
mod joins;
mod measures;
mod nested;
mod persist;
mod resolve;
mod shell;
mod stage;
mod to_measure;

pub use catalog::{
    compile_script, define_layer2_view, Layer2Catalog, Layer2Column, Layer2ColumnKind,
};
pub use persist::persist_layer2_view;

use clauses::{
    apply_group_exprs, limit_u64, literal_json, order_field, order_key, parse_having,
    where_to_filters, Conjunct, OrderCtx,
};
use formula::{
    apply_select_formulas, default_measure_name, formula_leaves, formula_to_expr,
    parse_formula_or_agg, parse_formula_with_plain, parse_measure_formula, resolve_agg,
    unwrap_paren,
};
use from::{
    as_derived, consumer_join_kind, cross_join_on, parse_from_clause, table_expr, ParsedFrom,
};
use measures::{
    aggregated_measure, bare_field, bare_measure, classify_to_measure, is_agg_name, is_aggregated,
    is_base_agg_expr, is_measure_agg_expr, is_model_metric, model_scoped, take_agg, RawAggs,
};
use nested::{OutCol, OutKind};
use resolve::{
    as_column, column_role, resolve_column_dataset, resolve_dim, resolve_field_ref,
    resolve_measure, strip_alias,
};
use stage::{expand_group_by_all, parse_stage_from, parse_stage_prep, stage_aliases};

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
    for (key, _, _) in &mut logical.order {
        if let plan::OrderKey::Output(name) = key {
            if let Some(alias) = rename.get(name).or_else(|| dim_rename.get(name)) {
                *name = alias.clone();
            }
        }
    }
}

/// Translate Layer 2 SQL into a Layer 3 [`Request`] (join kinds are not
/// represented). Flat queries only: derived tables (A.2–A.3) are not one request.
pub fn to_request(graph: &SemanticGraph, sql: &str) -> Result<Request> {
    let parsed = parse_layer2(graph, sql)?;
    if parsed.having.is_some() || !parsed.order_exprs.is_empty() {
        return Err(unsupported(
            "a request can't carry this HAVING or ORDER BY expression; compile the SQL with compile_sql",
        ));
    }
    Ok(parsed.request)
}

pub(super) struct Layer2Parsed {
    pub request: Request,
    /// Join kinds from the consumer `FROM` / `JOIN` list (Appendix A.5–A.6).
    pub consumer_joins: Vec<ConsumerJoin>,
    /// Composed select-list measures: (output column name, formula tree).
    pub measure_outputs: Vec<(String, L2Formula)>,
    /// Selected dimension (`dataset.field`) and the label the consumer sees it under.
    pub dim_labels: Vec<(String, String)>,
    /// `HAVING`, once its `AGG()` leaves and columns are placeholders.
    pub having: Option<GroupExpr>,
    /// `ORDER BY` keys that aren't output columns, by their place among the keys.
    pub order_exprs: Vec<OrderExpr>,
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
    /// A plain SQL aggregate in the select list (§7.2), not a `TO MEASURE`.
    pub raw: bool,
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

/// An expression over a finished group, in placeholders: `__ossiec_h{i}` for
/// `measures[i]`, and [`plan::OUT_DIM`]`{i}` for selected dimension `i`.
pub(super) struct GroupExpr {
    pub expr: Expression,
    pub measures: Vec<L2Formula>,
}

pub(super) struct OrderExpr {
    pub position: usize,
    pub key: GroupExpr,
    pub direction: Direction,
    pub nulls: Option<Nulls>,
}

const HAVING_MEASURE: &str = "__ossiec_h";

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
        Expression::Select(mut sel) => {
            cte::expand(&mut sel)?;
            Ok((*sel, to_measures))
        }
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
        return Err(unsupported(
            "a plain SQL aggregate (SUM, COUNT, ...) beside AGG() is not a Layer 3 request; use compile_sql",
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

fn unsupported(msg: impl Into<String>) -> Error {
    Error::Unsupported(msg.into())
}

/// Resolved endpoints of one ON equality: `(dataset, field_name)`.
type FieldRef = (usize, String);
