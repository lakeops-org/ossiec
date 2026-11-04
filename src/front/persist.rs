//! Persist Appendix A.7 `CREATE VIEW` measures into the Ossie model.
//!
//! A session [`super::Layer2Catalog`] view can define unevaluated measures
//! (`TO MEASURE`). [`persist_layer2_view`] copies those into the model as
//! permanent metrics and re-binds:
//!
//! * `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE AS name` → a **dataset-scoped**
//!   metric on the aggregate's owner dataset
//! * `(AGG(a) / AGG(b)) TO MEASURE AS name` → a **model-scoped** metric whose
//!   SQL inlines the leaf measures' expressions
//!
//! Pass-through columns (bare measures already in the model) and evaluated
//! scalars are skipped. The session view is unchanged; callers that want YAML
//! use [`crate::to_yaml`].

use polyglot_sql::expressions::{Identifier, Select};
use polyglot_sql::{generate, parse_one, transform_all, DialectType, Expression};

use super::catalog::Layer2Catalog;
use super::nested::{self, OutCol};
use super::{parse_stage_from, parse_stage_prep, L2Formula, StageCfg};
use crate::graph::{bind, SemanticGraph};
use crate::model::{Expression as ModelExpr, Metric, Model};
use crate::{sql, Error, Options, Result};

/// One measure taken from a view that can become a model metric.
#[derive(Debug, Clone)]
struct Promotable {
    /// Consumer alias (`AS name`).
    name: String,
    kind: Kind,
}

#[derive(Debug, Clone)]
enum Kind {
    /// Dataset-scoped metric SQL (unqualified field references).
    Dataset { dataset: usize, sql: String },
    /// Model-scoped metric SQL (qualified field references).
    Model { sql: String },
}

/// Write the unevaluated `TO MEASURE` columns of `view` into the model.
///
/// Returns a freshly bound graph and the metric names added (`dataset.name` for
/// dataset-scoped, bare `name` for model-scoped). Existing metrics of the same
/// name are refused. Pass-through measures are skipped.
pub fn persist_layer2_view(
    graph: &SemanticGraph,
    catalog: &Layer2Catalog,
    view: &str,
    opts: &Options,
) -> Result<(SemanticGraph, Vec<String>)> {
    let view = catalog.lookup(view).ok_or_else(|| {
        Error::Request(format!(
            "Layer 2 view `{view}` is not defined; register it with define_layer2_view first"
        ))
    })?;
    let view_name = view.name.clone();
    let mut select = view.select.clone();
    let mut to_measures = view.to_measures.clone();
    super::catalog::expand_query(catalog, &mut select, &mut to_measures, Some(&view_name))?;
    // Confirm the body still type-checks as a view (same path as define).
    let _ = nested::view_outputs(graph, &select, &to_measures, opts)?;
    let promotable = collect_promotable(graph, &select, &to_measures)?;
    if promotable.is_empty() {
        return Err(Error::Request(format!(
            "Layer 2 view `{view_name}` has no TO MEASURE items to persist (pass-through measures are already in the model)"
        )));
    }

    let mut model = graph.model().clone();
    let mut added = Vec::new();
    for p in &promotable {
        let full = match &p.kind {
            Kind::Dataset { dataset, .. } => {
                format!("{}.{}", model.datasets[*dataset].name, p.name)
            }
            Kind::Model { .. } => p.name.clone(),
        };
        ensure_absent(&model, &full, &p.kind)?;
        let metric = |sql: &str| Metric {
            name: p.name.clone(),
            expression: ModelExpr {
                sql: Some(sql.to_string()),
                dialects: vec![],
            },
            description: Some(format!("Persisted from Layer 2 CREATE VIEW `{view_name}`")),
            datatype: None,
            ai_context: None,
            custom_extensions: vec![],
        };
        match &p.kind {
            Kind::Dataset { dataset, sql } => {
                model.datasets[*dataset].metrics.push(metric(sql));
            }
            Kind::Model { sql } => {
                model.metrics.push(metric(sql));
            }
        }
        added.push(full);
    }
    let g = bind(model)?;
    Ok((g, added))
}

fn ensure_absent(model: &Model, full: &str, kind: &Kind) -> Result<()> {
    let collision = match kind {
        Kind::Dataset { dataset, .. } => {
            let bare = full.rsplit_once('.').map(|(_, b)| b).unwrap_or(full);
            model.datasets[*dataset]
                .metrics
                .iter()
                .any(|m| m.name.eq_ignore_ascii_case(bare))
                || model.datasets[*dataset]
                    .fields
                    .iter()
                    .any(|f| f.name.eq_ignore_ascii_case(bare))
        }
        Kind::Model { .. } => model
            .metrics
            .iter()
            .any(|m| m.name.eq_ignore_ascii_case(full)),
    };
    if collision {
        return Err(Error::Request(format!(
            "cannot persist `{full}`: a metric or field with that name already exists in the model"
        )));
    }
    Ok(())
}

fn collect_promotable(
    graph: &SemanticGraph,
    sel: &Select,
    to_measures: &[Expression],
) -> Result<Vec<Promotable>> {
    let cfg = StageCfg {
        derived: true,
        outer: &[],
        to_measures,
    };
    let parsed_from = parse_stage_prep(graph, sel, &cfg)?;
    let schemas: Vec<(String, Vec<OutCol>)> = parsed_from
        .derived
        .iter()
        .map(|d| {
            Ok((
                d.alias.clone(),
                nested::view_outputs(graph, &d.select, to_measures, &Options::default())?,
            ))
        })
        .collect::<Result<_>>()?;
    let mut base_seq = 0usize;
    let parsed = parse_stage_from(graph, sel, &cfg, parsed_from, &schemas, &mut base_seq)?;

    let mut out = Vec::new();
    for u in &parsed.unevaluated {
        if let Some(formula) = &u.formula {
            let sql = formula_to_metric_sql(graph, formula)?;
            out.push(Promotable {
                name: u.name.clone(),
                kind: Kind::Model { sql },
            });
            continue;
        }
        if let Some(ep) = parsed.ephemeral.iter().find(|e| e.name == u.metric) {
            out.push(Promotable {
                name: u.name.clone(),
                kind: Kind::Dataset {
                    dataset: ep.owner,
                    sql: expr_sql(&ep.body)?,
                },
            });
        }
        // Else: bare pass-through of an existing measure — already in the model.
    }
    Ok(out)
}

fn formula_to_metric_sql(graph: &SemanticGraph, f: &L2Formula) -> Result<String> {
    Ok(match f {
        L2Formula::Agg(m) => leaf_metric_sql(graph, m)?,
        L2Formula::Add(l, r) => format!(
            "({}) + ({})",
            formula_to_metric_sql(graph, l)?,
            formula_to_metric_sql(graph, r)?
        ),
        L2Formula::Sub(l, r) => format!(
            "({}) - ({})",
            formula_to_metric_sql(graph, l)?,
            formula_to_metric_sql(graph, r)?
        ),
        L2Formula::Mul(l, r) => format!(
            "({}) * ({})",
            formula_to_metric_sql(graph, l)?,
            formula_to_metric_sql(graph, r)?
        ),
        L2Formula::Div(l, r) => format!(
            "({}) / ({})",
            formula_to_metric_sql(graph, l)?,
            formula_to_metric_sql(graph, r)?
        ),
        L2Formula::NullIf(l, r) => format!(
            "NULLIF(({}), ({}))",
            formula_to_metric_sql(graph, l)?,
            formula_to_metric_sql(graph, r)?
        ),
        L2Formula::Lit(v) => match v {
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Bool(b) => {
                if *b {
                    "TRUE".into()
                } else {
                    "FALSE".into()
                }
            }
            serde_json::Value::Null => "NULL".into(),
            other => {
                return Err(Error::Unsupported(format!(
                    "Layer 2 persist: unsupported literal in TO MEASURE ({other})"
                )))
            }
        },
    })
}

/// Inline a leaf measure as model-scoped SQL (qualified fields).
fn leaf_metric_sql(graph: &SemanticGraph, name: &str) -> Result<String> {
    let i = graph.metric_by_name(name)?;
    let m = &graph.metrics[i];
    let (sql, owner) = match m.owner {
        Some(ds) => {
            let bare = name
                .rsplit_once('.')
                .map(|(_, b)| b)
                .ok_or_else(|| Error::Internal(format!("dataset metric `{name}` has no `.`")))?;
            let expr = graph.model.datasets[ds]
                .metrics
                .iter()
                .find(|x| x.name.eq_ignore_ascii_case(bare))
                .and_then(|x| x.expression.sql.as_ref())
                .ok_or_else(|| {
                    Error::Internal(format!("metric `{name}` missing from bound model"))
                })?;
            (expr.clone(), Some(ds))
        }
        None => {
            let expr = graph
                .model
                .metrics
                .iter()
                .find(|x| x.name.eq_ignore_ascii_case(name))
                .and_then(|x| x.expression.sql.as_ref())
                .ok_or_else(|| {
                    Error::Internal(format!("metric `{name}` missing from bound model"))
                })?;
            (expr.clone(), None)
        }
    };
    match owner {
        Some(ds) => qualify_dataset_sql(graph, ds, &sql),
        None => Ok(sql),
    }
}

/// Turn a dataset-scoped metric body into model-scoped SQL: qualify columns and
/// rewrite `COUNT(*)` to `COUNT(dataset.key)` so the expression is valid at
/// model scope.
fn qualify_dataset_sql(graph: &SemanticGraph, ds: usize, sql: &str) -> Result<String> {
    let ast = parse_one(sql, DialectType::Generic)
        .map_err(|e| Error::Internal(format!("persist: cannot parse metric SQL `{sql}`: {e}")))?;
    let ds_name = graph.datasets[ds].name.clone();
    let key = graph.datasets[ds]
        .primary_key
        .first()
        .copied()
        .or_else(|| {
            graph.datasets[ds]
                .fields
                .iter()
                .position(|f| f.name.eq_ignore_ascii_case("id"))
        })
        .or_else(|| (0..graph.datasets[ds].fields.len()).next())
        .map(|i| graph.datasets[ds].fields[i].name.clone());
    let ast = transform_all(ast, &|node| {
        Ok(match node {
            Expression::Count(mut c) if c.star || c.this.is_none() => {
                let col = key.as_ref().ok_or_else(|| {
                    polyglot_sql::Error::InvalidInput(format!(
                        "dataset `{ds_name}` has no field to rewrite COUNT(*) for model-scoped persist"
                    ))
                })?;
                let mut counted = sql::col(col);
                if let Expression::Column(ref mut column) = counted {
                    column.table = Some(Identifier::new(ds_name.as_str()));
                }
                c.star = false;
                c.this = Some(counted);
                Expression::Count(c)
            }
            Expression::Column(mut c) if c.table.is_none() => {
                c.table = Some(Identifier::new(ds_name.as_str()));
                Expression::Column(c)
            }
            other => other,
        })
    })
    .map_err(|e| Error::Internal(format!("persist: qualify `{sql}`: {e}")))?;
    expr_sql(&ast)
}

fn expr_sql(e: &Expression) -> Result<String> {
    generate(e, DialectType::Generic).map_err(|e| Error::Internal(format!("persist: render: {e}")))
}
