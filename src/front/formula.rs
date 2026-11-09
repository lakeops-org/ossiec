//! `AGG()` arithmetic in the select list (§7.2): parsing it, its leaf measures,
//! and applying it to the planned outputs.

use std::collections::HashMap;

use polyglot_sql::builder::{func, lit, Expr};
use polyglot_sql::expressions::{BinaryOp, Literal};
use polyglot_sql::Expression;

use crate::graph::{self, SemanticGraph};
use crate::plan::LogicalPlan;
use crate::{Error, Result};

use super::{
    as_column, bare_field, is_base_agg_expr, literal_json, resolve_measure, take_agg, unsupported,
    L2Formula, OutKind, QueryScope, RawAggs,
};

pub(super) fn apply_select_formulas(
    _graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    measure_outputs: &[(String, L2Formula)],
    has_hidden_measures: bool,
) -> Result<()> {
    if measure_outputs.is_empty() {
        // Measures only `HAVING` or `ORDER BY` read aren't columns of the result.
        if has_hidden_measures {
            logical.outputs.clear();
        }
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

pub(super) fn formula_to_expr(
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

pub(super) fn literal_expr(v: &serde_json::Value) -> Result<Expression> {
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

pub(super) fn parse_measure_formula(
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

pub(super) fn parse_formula(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
) -> Result<L2Formula> {
    formula_in(graph, scope, e, None)
}

/// A select item, `HAVING` or `ORDER BY` expression that may also read plain
/// aggregates (§7.2): each becomes a raw leaf beside the `AGG()` ones.
pub(super) fn parse_formula_with_plain(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
    raw: &mut RawAggs,
) -> Result<L2Formula> {
    formula_or_agg_in(graph, scope, e, Some(raw))
}

fn formula_or_agg_in(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
    raw: Option<&mut RawAggs>,
) -> Result<L2Formula> {
    let e = unwrap_paren(e);
    if let Some(measure) = take_agg(e)? {
        return resolve_agg(graph, scope, measure);
    }
    formula_in(graph, scope, e, raw)
}

fn formula_in(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
    mut raw: Option<&mut RawAggs>,
) -> Result<L2Formula> {
    let e = unwrap_paren(e);
    let side = |x: &Expression, raw: &mut Option<&mut RawAggs>| -> Result<Box<L2Formula>> {
        Ok(Box::new(formula_or_agg_in(
            graph,
            scope,
            x,
            raw.as_deref_mut(),
        )?))
    };
    match e {
        e if is_base_agg_expr(e) && raw.is_some() => Ok(L2Formula::Agg(
            raw.as_deref_mut().unwrap().metric(graph, scope, e)?,
        )),
        Expression::Add(b) => Ok(L2Formula::Add(
            side(&b.left, &mut raw)?,
            side(&b.right, &mut raw)?,
        )),
        Expression::Sub(b) => Ok(L2Formula::Sub(
            side(&b.left, &mut raw)?,
            side(&b.right, &mut raw)?,
        )),
        Expression::Mul(b) => Ok(L2Formula::Mul(
            side(&b.left, &mut raw)?,
            side(&b.right, &mut raw)?,
        )),
        Expression::Div(b) => Ok(L2Formula::Div(
            side(&b.left, &mut raw)?,
            side(&b.right, &mut raw)?,
        )),
        Expression::Function(f)
            if f.name.eq_ignore_ascii_case("NULLIF") && f.args.len() == 2 =>
        {
            Ok(L2Formula::NullIf(
                side(&f.args[0], &mut raw)?,
                side(&f.args[1], &mut raw)?,
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

pub(super) fn parse_formula_or_agg(
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

pub(super) fn unwrap_paren(e: &Expression) -> &Expression {
    match e {
        Expression::Paren(p) => unwrap_paren(&p.this),
        other => other,
    }
}

pub(super) fn formula_leaves(f: &L2Formula) -> Vec<String> {
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

pub(super) fn default_measure_name(f: &L2Formula, idx: usize) -> String {
    match f {
        L2Formula::Agg(m) => bare_field(m).to_string(),
        _ => format!("__l2_{idx}"),
    }
}

/// The argument of `AGG(...)`: a dataset measure, or a derived table's
/// unevaluated measure column `AGG(t.m)` (§7.7), which stands for the metric it
/// passes through, or for the leaves of its `TO MEASURE` formula.
pub(super) fn resolve_agg(
    graph: &SemanticGraph,
    scope: &QueryScope,
    arg: &Expression,
) -> Result<L2Formula> {
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
