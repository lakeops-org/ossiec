//! Ossie Layer 2 frontend: consumer SQL with `AGG(measure)` → Layer 3 request.
//!
//! Spec: `relational_semantics.md` (draft). Appendix **A.1** (single `FROM` relation),
//! **A.4** (multi-fact measures sharing dimensions, plus `AGG` arithmetic in the
//! select list), and **A.5–A.6** (consumer joins along model relationships,
//! including unused many→one and reverse fan-out, with `INNER` vs `LEFT`) translate
//! to the existing [`Request`] planner. [`to_request`] / [`layer2_to_request`] expose
//! only **leaf** measures referenced inside `AGG(...)`, not composed output names,
//! and only for flat queries.
//!
//! **A.2–A.3** (derived tables) compile recursively in [`nested`]: every `AGG`
//! stage is an ordinary [`Request`] → plan → lower pass, derived tables join in
//! through the planner's `DerivedJoin`, and an outer SQL aggregate over derived
//! tables (A.3) is a thin shell outside plan/lower.
//!
//! **§7.7** (unevaluated measures): a derived table may pass a dataset-scoped
//! measure through its select list; an outer `AGG(t.m)` evaluates it. A query-local
//! `(AGG(a) / AGG(b)) TO MEASURE AS m` item (Appendix A.7 / §5.2 surface) defers a
//! composed measure the same way. Other join forms (`RIGHT`/`FULL`/`CROSS`/`ASOF`,
//! `CREATE VIEW`, …) stay [`Error::Unsupported`].
//!
//! Only **dataset-scoped** metrics are measures (spec §10.3). Model-scoped
//! metrics are refused inside `AGG()`.

use std::collections::HashMap;

use polyglot_sql::builder::{func, lit, Expr};
use polyglot_sql::expressions::{
    BinaryOp, Column, Join, JoinKind as SqlJoinKind, Literal, Select, TableRef,
};
use polyglot_sql::{parse_one, DialectType, Expression};

use crate::graph::{self, SemanticGraph};
use crate::plan::{
    self, Direction, Filter, JoinKind, JoinStep, LogicalPlan, Nulls, Op, OrderBy, Request, Strategy,
};
use crate::{Compiled, Error, Options, Result};

mod fold;
mod nested;
mod shell;
mod to_measure;

use nested::{OutCol, OutKind};

/// Compile a Layer 2 SQL query (`AGG(measure)` over model relations) to SQL.
/// Derived tables (Appendix A.2–A.3) compile recursively; see [`nested`].
pub fn compile_layer2(graph: &SemanticGraph, sql: &str, opts: &Options) -> Result<Compiled> {
    nested::compile(graph, sql, opts)
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
}

pub(super) struct ConsumerJoin {
    rel: usize,
    kind: JoinKind,
    reverse: bool,
    attach_ds: usize,
    attach_alias: String,
    other_ds: usize,
    other_alias: String,
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

struct QueryScope {
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
pub(super) fn parse_top_select(sql: &str) -> Result<(Select, Vec<Expression>)> {
    let (text, to_measures) = to_measure::extract(sql.trim())?;
    let ast = parse_one(&text, DialectType::Generic)
        .map_err(|e| Error::Request(format!("Layer 2 SQL: {e}")))?;
    match ast {
        Expression::Select(sel) => Ok((*sel, to_measures)),
        _ => Err(unsupported(
            "Layer 2 accepts a single SELECT (set operators are not supported)",
        )),
    }
}

fn parse_layer2(graph: &SemanticGraph, sql: &str) -> Result<Layer2Parsed> {
    let (sel, to_measures) = parse_top_select(sql)?;
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
    parse_stage_from(graph, &sel, &cfg, parsed_from, &[])
}

/// Clause checks and the `FROM` / `JOIN` list of one `AGG` stage.
fn parse_stage_prep(graph: &SemanticGraph, sel: &Select, cfg: &StageCfg<'_>) -> Result<ParsedFrom> {
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
fn parse_stage_from(
    graph: &SemanticGraph,
    sel: &Select,
    cfg: &StageCfg<'_>,
    mut parsed_from: ParsedFrom,
    derived_outs: &[(String, Vec<OutCol>)],
) -> Result<Layer2Parsed> {
    parsed_from.scope.derived_outs = derived_outs.to_vec();
    let scope = &parsed_from.scope;

    let mut metrics: Vec<String> = Vec::new();
    let mut dimensions = Vec::new();
    let mut measure_outputs = Vec::new();
    let mut dim_labels: Vec<(String, String)> = Vec::new();
    let mut unevaluated: Vec<UnevaluatedSpec> = Vec::new();
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
    })
}

/// `dataset.field` → `field`.
fn bare_field(dim: &str) -> &str {
    dim.split_once('.').map_or(dim, |(_, f)| f)
}

/// A bare column that names a dataset-scoped measure, not a field: the dataset
/// it is a measure of (the unevaluated-measure pass-through of spec §7.7).
fn bare_measure(graph: &SemanticGraph, scope: &QueryScope, col: &Column) -> Option<usize> {
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
        let mut patched = false;
        for leg in &mut logical.legs {
            for step in &mut leg.joins {
                if step.rel == cj.rel {
                    step.kind = cj.kind;
                    patched = true;
                }
            }
        }
        if patched {
            continue;
        }
        let leg_i = consumer_join_leg(graph, logical, cj)?;
        // ON must use the same aliases `tree_sql` uses for the leg (dataset
        // name for the root, `to_alias` for prior joins)—not the consumer SQL
        // alias (`o` vs `orders`).
        let from_alias = planned_alias_on_leg(graph, &logical.legs[leg_i], cj.attach_ds)
            .ok_or_else(|| {
                unsupported(format!(
                    "Layer 2 join on `{}`: attach dataset `{}` has no planned alias on its leg",
                    graph.rels[cj.rel].name, graph.datasets[cj.attach_ds].name
                ))
            })?;
        let leg = &mut logical.legs[leg_i];
        leg.joins.push(JoinStep {
            rel: cj.rel,
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
        if graph.datasets[root].primary_key.is_empty() {
            return Err(unsupported(format!(
                "Layer 2 fan-out join requires a primary_key on `{}` so measures stay duplicate-free (G2)",
                graph.datasets[root].name
            )));
        }
        for term in &mut logical.terms {
            if term.leg != leg_i {
                continue;
            }
            if term.strategy == Strategy::Inline && !term.term.dup_insensitive {
                ctx.warn(format!(
                    "FAN_TRAP_RISK: `{}` is computed over `{}` rows de-duplicated by primary key; a `{}` row that belongs to several groups counts in each, so groups do not add up to a grand total",
                    term.term.key,
                    graph.datasets[root].name,
                    graph.datasets[root].name
                ));
                term.strategy = Strategy::PkDedup { grain: root };
            }
        }
    }
    Ok(())
}

fn consumer_join_leg(
    graph: &SemanticGraph,
    logical: &LogicalPlan,
    cj: &ConsumerJoin,
) -> Result<usize> {
    for (i, leg) in logical.legs.iter().enumerate() {
        if leg_hosts_attach(graph, leg, cj.attach_ds, &cj.attach_alias) {
            return Ok(i);
        }
    }
    Err(unsupported(format!(
        "Layer 2 join on relationship `{}` cannot attach to the planned query; no leg includes `{}` as `{}`",
        graph.rels[cj.rel].name,
        graph.datasets[cj.attach_ds].name,
        cj.attach_alias
    )))
}

fn leg_hosts_attach(
    graph: &SemanticGraph,
    leg: &plan::Leg,
    attach_ds: usize,
    _attach_alias: &str,
) -> bool {
    planned_alias_on_leg(graph, leg, attach_ds).is_some()
}

/// Alias `lower` will use for `ds` on this leg (`tree_sql` / `leg_sql`).
fn planned_alias_on_leg(graph: &SemanticGraph, leg: &plan::Leg, ds: usize) -> Option<String> {
    if leg.root == ds {
        return Some(graph.datasets[ds].name.clone());
    }
    for s in &leg.joins {
        let joined = if s.reverse {
            graph.rels[s.rel].from
        } else {
            s.to
        };
        if joined == ds {
            return Some(s.to_alias.clone());
        }
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

struct ParsedFrom {
    scope: QueryScope,
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

fn parse_from_clause(
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
            let spec = parse_derived_join(graph, &scope, j, select, alias)?;
            scope.derived.push(spec.alias.clone());
            derived_specs.push(spec);
            continue;
        }
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
            derived: scope.derived.clone(),
            derived_outs: vec![],
            outer: scope.outer.clone(),
        };
        let rel = match_relationship(graph, &join_scope, &pairs)?;
        if consumer_joins.iter().any(|c: &ConsumerJoin| c.rel == rel) {
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
        consumer_joins.push(ConsumerJoin {
            rel,
            kind,
            reverse,
            attach_ds,
            attach_alias,
            other_ds,
            other_alias: right_binding.alias.clone(),
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
