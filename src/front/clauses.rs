//! `WHERE`, `HAVING` and `ORDER BY` (§7.3, §7.5): row predicates to request
//! filters, group predicates and sort keys over measure placeholders.

use std::cell::RefCell;
use std::collections::HashMap;

use polyglot_sql::expressions::{BinaryOp, Column, Literal};
use polyglot_sql::{
    contains_aggregate, contains_window_function, get_columns, is_query, is_subquery, DialectType,
    Expression, ExpressionWalk,
};

use crate::graph::{self, SemanticGraph};
use crate::plan::{self, Filter, LogicalPlan, Op};
use crate::sql::{self, transform};
use crate::{Error, Result};

use super::{
    as_column, formula_to_expr, is_base_agg_expr, is_measure_agg_expr, resolve_agg, resolve_dim,
    take_agg, unsupported, GroupExpr, L2Formula, OrderExpr, QueryScope, RawAggs, HAVING_MEASURE,
};

/// A conjunct of `WHERE`: a structured filter where it fits, so a literal is typed
/// by its field, or else a row predicate over the query's columns (§7.3).
pub(super) enum Conjunct {
    Filter(Filter),
    Predicate(String),
}

pub(super) fn where_to_filters(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
) -> Result<Vec<Conjunct>> {
    if let Expression::And(a) = e {
        let mut out = where_to_filters(graph, scope, &a.left)?;
        out.extend(where_to_filters(graph, scope, &a.right)?);
        return Ok(out);
    }
    let structured = match e {
        Expression::Eq(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Eq)?,
        Expression::Neq(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Ne)?,
        Expression::Lt(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Lt)?,
        Expression::Lte(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Le)?,
        Expression::Gt(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Gt)?,
        Expression::Gte(b) => cmp_filter(graph, scope, &b.left, &b.right, Op::Ge)?,
        Expression::IsNull(i) => match as_column(&i.this) {
            Some(col) => Some(Filter {
                field: resolve_dim(graph, scope, col)?,
                op: if i.not { Op::IsNotNull } else { Op::IsNull },
                value: None,
                values: vec![],
            }),
            None => None,
        },
        // `col LIKE 'pattern'`; with ESCAPE, or `NOT LIKE`, it stays a predicate.
        Expression::Like(l) if l.escape.is_none() && l.quantifier.is_none() => {
            match (as_column(&l.left), literal_json(&l.right)) {
                (Some(col), Some(v @ serde_json::Value::String(_))) => Some(Filter {
                    field: resolve_dim(graph, scope, col)?,
                    op: Op::Like,
                    value: Some(v),
                    values: vec![],
                }),
                _ => None,
            }
        }
        // `col [NOT] IN (literal, ...)`. A NULL in the list stays in the predicate,
        // where it keeps SQL's meaning (`NOT IN (1, NULL)` keeps no row).
        Expression::In(i) if i.query.is_none() && i.unnest.is_none() && !i.global => {
            let values: Option<Vec<serde_json::Value>> = i
                .expressions
                .iter()
                .map(|x| literal_json(x).filter(|v| !v.is_null()))
                .collect();
            match (as_column(&i.this), values) {
                (Some(col), Some(values)) if !values.is_empty() => Some(Filter {
                    field: resolve_dim(graph, scope, col)?,
                    op: if i.not { Op::NotIn } else { Op::In },
                    value: None,
                    values,
                }),
                _ => None,
            }
        }
        // `col BETWEEN lo AND hi`; `NOT BETWEEN` and SYMMETRIC stay predicates.
        Expression::Between(b) if !b.not && b.symmetric.is_none() => {
            let bound = |x: &Expression| literal_json(x).filter(|v| !v.is_null());
            match (as_column(&b.this), bound(&b.low), bound(&b.high)) {
                (Some(col), Some(lo), Some(hi)) => Some(Filter {
                    field: resolve_dim(graph, scope, col)?,
                    op: Op::Between,
                    value: None,
                    values: vec![lo, hi],
                }),
                _ => None,
            }
        }
        _ => None,
    };
    Ok(vec![match structured {
        Some(f) => Conjunct::Filter(f),
        None => Conjunct::Predicate(where_predicate(graph, scope, e)?),
    }])
}

/// §7.5: `HAVING` keeps or drops whole groups. A conjunct that reads only grouping
/// columns says the same as a `WHERE` on them, so it goes there, where strings
/// compare by each engine's collation rule and fewer rows are read. The rest runs
/// over the finished groups.
pub(super) fn parse_having(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
    dimensions: &[String],
    raw: &mut RawAggs,
) -> Result<(Vec<Conjunct>, Option<GroupExpr>)> {
    fn split<'a>(e: &'a Expression, out: &mut Vec<&'a Expression>) {
        match e {
            Expression::And(a) => {
                split(&a.left, out);
                split(&a.right, out);
            }
            Expression::Paren(p) => split(&p.this, out),
            other => out.push(other),
        }
    }
    let mut conjuncts = vec![];
    split(e, &mut conjuncts);
    let mut pushed = vec![];
    let mut kept: Vec<Expression> = vec![];
    for c in conjuncts {
        if contains_window_function(c) {
            return Err(unsupported(
                "Layer 2 HAVING takes aggregates, AGG(measure) and selected columns; a window is not supported",
            ));
        }
        // A plain aggregate (§7.2) reads the group, as AGG() does: never pushed to WHERE.
        let has_agg = c.contains(is_measure_agg_expr) || c.contains(is_base_agg_expr);
        for column in get_columns(c) {
            // With an AGG(), its argument is a column too: the second pass below
            // checks those that aren't one.
            if !has_agg {
                if let Expression::Column(col) = column {
                    selected_dimension(graph, scope, col, dimensions)?;
                }
            }
        }
        if !has_agg && !get_columns(c).is_empty() {
            pushed.extend(where_to_filters(graph, scope, c)?);
        } else {
            kept.push(c.clone());
        }
    }
    let Some(first) = kept.pop() else {
        return Ok((pushed, None));
    };
    let joined = kept.into_iter().rev().fold(first, |acc, c| {
        Expression::And(Box::new(BinaryOp::new(c, acc)))
    });
    if joined.contains(|x| is_query(x) || is_subquery(x)) {
        return Err(unsupported(
            "Layer 2 HAVING does not support subqueries yet",
        ));
    }
    Ok((
        pushed,
        Some(group_expr(graph, scope, joined, dimensions, raw)?),
    ))
}

/// An `ORDER BY` key that isn't an output column (§7.5). An expression has to read
/// something: `ORDER BY 1` means a position in SQL, not a constant.
pub(super) fn order_key(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
    dimensions: &[String],
    raw: &mut RawAggs,
) -> Result<GroupExpr> {
    if contains_window_function(e) {
        return Err(unsupported(
            "Layer 2 ORDER BY takes aggregates, AGG(measure) and selected columns; a window is not supported",
        ));
    }
    if e.contains(|x| is_query(x) || is_subquery(x)) {
        return Err(unsupported(
            "Layer 2 ORDER BY does not support subqueries yet",
        ));
    }
    let key = group_expr(graph, scope, e.clone(), dimensions, raw)?;
    if key.measures.is_empty() && get_columns(&key.expr).is_empty() {
        return Err(unsupported(
            "Layer 2 ORDER BY must name a selected dimension, measure, or AGG(...); a position like `ORDER BY 1` is not supported",
        ));
    }
    Ok(key)
}

/// `e` over placeholders: each `AGG()` a measure, each plain aggregate a raw
/// one (§7.2), each column a selected dimension.
pub(super) fn group_expr(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: Expression,
    dimensions: &[String],
    raw: &mut RawAggs,
) -> Result<GroupExpr> {
    let failure = RefCell::new(None);
    let measures = RefCell::new(Vec::<L2Formula>::new());
    let raw = RefCell::new(raw);
    // Two passes: an aggregate's argument is not a column of the group, whichever
    // order the walk visits a node and its children in.
    let without_aggs = transform(e, &|x| {
        let found = if is_base_agg_expr(&x) {
            raw.borrow_mut()
                .metric(graph, scope, &x)
                .map(|m| Some(L2Formula::Agg(m)))
        } else {
            match take_agg(&x) {
                Ok(Some(arg)) => resolve_agg(graph, scope, arg).map(Some),
                Ok(None) => Ok(None),
                Err(err) => Err(err),
            }
        };
        Ok(Some(match found {
            Ok(Some(formula)) => {
                let mut m = measures.borrow_mut();
                m.push(formula);
                sql::col(&format!("{HAVING_MEASURE}{}", m.len() - 1))
            }
            Ok(None) => x,
            Err(err) => {
                failure.borrow_mut().get_or_insert(err);
                x
            }
        }))
    })
    .map_err(|e| Error::Internal(e.to_string()))?;
    let expr = transform(without_aggs, &|x| {
        Ok(Some(match x {
            Expression::Column(c) if !c.name.name.starts_with(HAVING_MEASURE) => {
                match selected_dimension(graph, scope, &c, dimensions) {
                    Ok(i) => sql::col(&format!("{}{i}", plan::OUT_DIM)),
                    Err(err) => {
                        failure.borrow_mut().get_or_insert(err);
                        Expression::Column(c)
                    }
                }
            }
            other => other,
        }))
    })
    .map_err(|e| Error::Internal(e.to_string()))?;
    if let Some(err) = failure.into_inner() {
        return Err(err);
    }
    Ok(GroupExpr {
        expr: graph::true_division(expr),
        measures: measures.into_inner(),
    })
}

/// A `HAVING` column: which selected dimension it is.
pub(super) fn selected_dimension(
    graph: &SemanticGraph,
    scope: &QueryScope,
    col: &Column,
    dimensions: &[String],
) -> Result<usize> {
    let dim = resolve_dim(graph, scope, col)?;
    dimensions.iter().position(|d| *d == dim).ok_or_else(|| {
        Error::Request(format!(
            "Layer 2 HAVING or ORDER BY reads `{dim}`, which is not in GROUP BY; group by it"
        ))
    })
}

/// Add `HAVING` and `ORDER BY` expressions to the plan. Runs before the select list
/// replaces the plan's outputs, while each measure's expression can still be read.
pub(super) fn apply_group_exprs(
    logical: &mut LogicalPlan,
    having: Option<&GroupExpr>,
    order: &[OrderExpr],
) -> Result<()> {
    if having.is_none() && order.is_empty() {
        return Ok(());
    }
    if logical.windowed {
        return Err(unsupported(
            "Layer 2 HAVING or ORDER BY expression over a window metric is not supported",
        ));
    }
    let metrics: HashMap<String, Expression> = logical.outputs.iter().cloned().collect();
    if let Some(having) = having {
        let predicate = over_measures(having, &metrics)?;
        logical.metric_filters.push((predicate.clone(), predicate));
    }
    for o in order {
        logical.order.insert(
            o.position,
            (
                plan::OrderKey::Expr(Box::new(over_measures(&o.key, &metrics)?)),
                o.direction,
                o.nulls,
            ),
        );
    }
    Ok(())
}

pub(super) fn over_measures(
    e: &GroupExpr,
    metrics: &HashMap<String, Expression>,
) -> Result<Expression> {
    let failure = RefCell::new(None);
    let out = transform(e.expr.clone(), &|x| {
        Ok(Some(match x {
            Expression::Column(c) if c.name.name.starts_with(HAVING_MEASURE) => {
                let i: usize = c.name.name[HAVING_MEASURE.len()..]
                    .parse()
                    .expect("measure placeholder");
                match formula_to_expr(&e.measures[i], metrics) {
                    Ok(f) => f,
                    Err(err) => {
                        failure.borrow_mut().get_or_insert(err);
                        Expression::Column(c)
                    }
                }
            }
            other => other,
        }))
    })
    .map_err(|e| Error::Internal(e.to_string()))?;
    failure.into_inner().map_or(Ok(out), Err)
}

/// A predicate as `filter` text, each column as its `dataset.field`.
pub(super) fn where_predicate(
    graph: &SemanticGraph,
    scope: &QueryScope,
    e: &Expression,
) -> Result<String> {
    if contains_aggregate(e) || contains_window_function(e) || e.contains(is_measure_agg_expr) {
        return Err(Error::Request(
            "Layer 2 WHERE cannot contain an aggregate or a window function; use HAVING, or a measure's FILTER".into(),
        ));
    }
    if e.contains(|x| is_query(x) || is_subquery(x)) {
        return Err(unsupported("Layer 2 WHERE does not support subqueries yet"));
    }
    let failure = RefCell::new(None);
    let resolved = transform(e.clone(), &|x| {
        let Expression::Column(c) = x else {
            return Ok(Some(x));
        };
        let field = resolve_dim(graph, scope, &c)
            .and_then(|field| sql::parse(&field, DialectType::Generic).map_err(Error::Internal));
        Ok(Some(match field {
            Ok(column) => column,
            Err(err) => {
                failure.borrow_mut().get_or_insert(err);
                Expression::Column(c)
            }
        }))
    })
    .map_err(|e| Error::Internal(e.to_string()))?;
    if let Some(err) = failure.into_inner() {
        return Err(err);
    }
    polyglot_sql::generate(&resolved, DialectType::Generic)
        .map_err(|e| Error::Internal(e.to_string()))
}

/// `column <op> literal` as a filter, or `None` if the comparison is something else.
pub(super) fn cmp_filter(
    graph: &SemanticGraph,
    scope: &QueryScope,
    left: &Expression,
    right: &Expression,
    op: Op,
) -> Result<Option<Filter>> {
    let (col, lit) = match (as_column(left), as_column(right)) {
        (Some(col), None) => (col, right),
        (None, Some(col)) if matches!(op, Op::Eq | Op::Ne) => (col, left),
        _ => return Ok(None),
    };
    let Some(value) = literal_json(lit) else {
        return Ok(None);
    };
    Ok(Some(Filter {
        field: resolve_dim(graph, scope, col)?,
        op,
        value: Some(value),
        values: vec![],
    }))
}

pub(super) fn literal_json(e: &Expression) -> Option<serde_json::Value> {
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

pub(super) fn limit_u64(e: &Expression) -> Result<u64> {
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

pub(super) struct OrderCtx<'a> {
    pub(super) graph: &'a SemanticGraph,
    pub(super) scope: &'a QueryScope,
    pub(super) outputs: &'a [(String, String)],
    pub(super) metrics: &'a [String],
    pub(super) dimensions: &'a [String],
}

pub(super) fn order_field(ctx: &OrderCtx<'_>, e: &Expression) -> Result<String> {
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
