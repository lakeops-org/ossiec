//! `GROUPING SETS`, `CUBE` and `ROLLUP` (§7.4).
//!
//! Grouping ranges over fields, and the grouping sets the level each measure is
//! reported at (G1). So a query with several grouping sets is that query once per
//! set, each at its own level, stacked: `ROLLUP(region, status)` is the query
//! grouped by `(region, status)`, by `(region)` and by nothing. Each set is
//! compiled as an ordinary stage, where a grouping column outside the set is left
//! out of the select list and reads as NULL in `HAVING` (outside an aggregate), as
//! SQL's subtotal rows do.
//! [`super::nested`] stacks the stages with `UNION ALL`.

use polyglot_sql::expressions::{Column, Expression, Literal, Null, Select};
use polyglot_sql::ExpressionWalk;

use super::unsupported;
use crate::sql::transform;
use crate::{Error, Result};

/// At most this many sets: `CUBE` over n columns is 2^n of them.
pub(super) const MAX_SETS: usize = 16;

/// The grouping sets of `sel`, each a list of columns, or `None` for an ordinary
/// `GROUP BY`. Plain columns beside `ROLLUP`/`CUBE`/`GROUPING SETS` are in every
/// set: `GROUP BY c, ROLLUP(a)` is `(c, a), (c)`. Duplicates are kept, as in SQL.
pub(super) fn grouping_sets(sel: &Select) -> Result<Option<Vec<Vec<Column>>>> {
    let Some(gb) = &sel.group_by else {
        return Ok(None);
    };
    let is_grouping = |e: &Expression| {
        matches!(e, Expression::Function(f)
            if matches!(kind(&f.name).as_str(), "ROLLUP" | "CUBE" | "GROUPINGSETS"))
    };
    if !gb.expressions.iter().any(is_grouping) {
        return Ok(None);
    }
    if gb.all.is_some() {
        return Err(unsupported(
            "Layer 2 GROUP BY ALL or DISTINCT can't be combined with ROLLUP, CUBE or GROUPING SETS",
        ));
    }
    let mut sets: Vec<Vec<Column>> = vec![vec![]];
    for item in &gb.expressions {
        let choices: Vec<Vec<Column>> = match item {
            Expression::Function(f) if is_grouping(item) => {
                match kind(&f.name).as_str() {
                    "ROLLUP" => {
                        let cols = columns(&f.args)?;
                        (0..=cols.len()).rev().map(|k| cols[..k].to_vec()).collect()
                    }
                    "CUBE" => {
                        let cols = columns(&f.args)?;
                        if cols.len() > 4 {
                            return Err(unsupported(format!(
                                "Layer 2 CUBE takes at most 4 columns ({} sets); it has {}",
                                MAX_SETS,
                                cols.len()
                            )));
                        }
                        // Largest first, each subset in its columns' order.
                        let mut subsets: Vec<Vec<Column>> = (0..1u32 << cols.len())
                            .rev()
                            .map(|mask| {
                                (0..cols.len())
                                    .filter(|i| mask & (1 << (cols.len() - 1 - i)) != 0)
                                    .map(|i| cols[i].clone())
                                    .collect()
                            })
                            .collect();
                        subsets.sort_by_key(|s| std::cmp::Reverse(s.len()));
                        subsets
                    }
                    _ => f
                        .args
                        .iter()
                        .map(|a| match a {
                            Expression::Tuple(t) => columns(&t.expressions),
                            other => columns(std::slice::from_ref(other)),
                        })
                        .collect::<Result<_>>()?,
                }
            }
            other => vec![columns(std::slice::from_ref(other))?],
        };
        sets = sets
            .iter()
            .flat_map(|s| {
                choices.iter().map(move |c| {
                    let mut s = s.clone();
                    s.extend(c.iter().cloned());
                    s
                })
            })
            .collect();
        if sets.len() > MAX_SETS {
            return Err(unsupported(format!(
                "Layer 2 grouping makes more than {MAX_SETS} grouping sets"
            )));
        }
    }
    Ok(Some(sets))
}

/// polyglot names `GROUPING SETS` with or without its space, by how it was read.
fn kind(name: &str) -> String {
    name.split_whitespace()
        .collect::<String>()
        .to_ascii_uppercase()
}

/// Grouping elements must be columns: grouping ranges over fields (§7.4).
fn columns(items: &[Expression]) -> Result<Vec<Column>> {
    items
        .iter()
        .map(|e| match e {
            Expression::Column(c) => Ok((**c).clone()),
            Expression::Paren(p) => match &p.this {
                Expression::Column(c) => Ok((**c).clone()),
                _ => Err(not_a_column()),
            },
            _ => Err(not_a_column()),
        })
        .collect()
}

fn not_a_column() -> Error {
    unsupported(
        "Layer 2 ROLLUP, CUBE and GROUPING SETS take columns, or lists of columns, and not nested groupings or expressions",
    )
}

/// The same column, read loosely: a qualifier on one side only still matches.
pub(super) fn same(a: &Column, b: &Column) -> bool {
    a.name.name.eq_ignore_ascii_case(&b.name.name)
        && match (&a.table, &b.table) {
            (Some(x), Some(y)) => x.name.eq_ignore_ascii_case(&y.name),
            _ => true,
        }
}

/// Every column any set groups by, in order of first appearance.
pub(super) fn all_columns(sets: &[Vec<Column>]) -> Vec<Column> {
    let mut all: Vec<Column> = vec![];
    for c in sets.iter().flatten() {
        if !all.iter().any(|x| same(x, c)) {
            all.push(c.clone());
        }
    }
    all
}

/// `sel` at the level of `set`: grouped by it, without the grouping columns
/// outside it in its select list, with those reading as NULL in `HAVING`, and
/// without `ORDER BY`/`LIMIT`, which apply to the stacked whole. `empty` adds
/// `WHERE 1 = 0`, for a branch that only types the columns.
pub(super) fn branch(sel: &Select, all: &[Column], set: &[Column], empty: bool) -> Result<Select> {
    let missing: Vec<&Column> = all
        .iter()
        .filter(|c| !set.iter().any(|s| same(s, c)))
        .collect();
    let is_missing = |e: &Expression| match e {
        Expression::Column(c) => missing.iter().any(|m| same(m, c)),
        _ => false,
    };
    let mut b = sel.clone();
    b.expressions.retain(|item| {
        let (inner, _) = super::strip_alias(item);
        !is_missing(inner)
    });
    // A missing column anywhere else in the select list has no value at this level.
    if b.expressions
        .iter()
        .any(|item| item.contains(|x| is_missing(x)) && !item.contains(super::is_measure_agg_expr))
    {
        return Err(unsupported(
            "Layer 2 with ROLLUP, CUBE or GROUPING SETS: a grouping column is selected as itself, not inside an expression",
        ));
    }
    match &mut b.group_by {
        Some(gb) if !set.is_empty() => {
            gb.expressions = set
                .iter()
                .map(|c| Expression::Column(Box::new(c.clone())))
                .collect();
        }
        _ => b.group_by = None,
    }
    if let Some(h) = b.having.as_mut() {
        h.this = null_outside_aggregates(&h.this, &is_missing)?;
    }
    b.order_by = None;
    b.limit = None;
    b.offset = None;
    if empty {
        let never = crate::sql::parse("1 = 0", polyglot_sql::DialectType::Generic)
            .map_err(Error::Internal)?;
        match b.where_clause.as_mut() {
            Some(w) => {
                w.this = Expression::And(Box::new(polyglot_sql::expressions::BinaryOp::new(
                    w.this.clone(),
                    never,
                )))
            }
            None => {
                b.where_clause = Some(polyglot_sql::expressions::Where { this: never });
            }
        }
    }
    Ok(b)
}

/// `sel` with each unnamed expression named as the whole query names it, by its
/// position (`__l2_N`, [`super::formula::default_measure_name`]). A column, a
/// single `AGG(m)` and a plain aggregate are named by what they read, so they
/// keep their names in every branch already.
pub(super) fn stable_names(sel: &Select) -> Select {
    let mut out = sel.clone();
    for (i, item) in out.expressions.iter_mut().enumerate() {
        let inner = super::unwrap_paren(item);
        let named_by_itself = matches!(item, Expression::Alias(_) | Expression::Column(_))
            || super::is_base_agg_expr(inner)
            || super::take_agg(inner)
                .ok()
                .flatten()
                .is_some_and(|arg| super::as_column(arg).is_some());
        if !named_by_itself {
            let this = std::mem::replace(item, Expression::Null(Null));
            *item = Expression::Alias(Box::new(polyglot_sql::expressions::Alias::new(
                this,
                polyglot_sql::expressions::Identifier::new(format!("__l2_{i}")),
            )));
        }
    }
    out
}

/// `HAVING` at a subtotal: a grouping column the set leaves out reads as NULL,
/// as SQL's subtotal row does, but inside an aggregate (`COUNT(status)`,
/// `AGG(m)`) it is the rows' own values. The walk is bottom-up, so aggregates
/// are set aside behind stand-ins first and put back after.
fn null_outside_aggregates(
    e: &Expression,
    is_missing: &dyn Fn(&Expression) -> bool,
) -> Result<Expression> {
    const STAND_IN: &str = "__ossiec_having_agg";
    let is_agg = |x: &Expression| super::is_base_agg_expr(x) || super::is_measure_agg_expr(x);
    let mut aggs: Vec<&Expression> = vec![];
    for x in e.dfs() {
        if is_agg(x) && !aggs.iter().any(|a| a.contains(|y| std::ptr::eq(y, x))) {
            aggs.push(x);
        }
    }
    let internal = |e: polyglot_sql::Error| Error::Internal(e.to_string());
    let hidden = transform(e.clone(), &|x| {
        Ok(Some(match aggs.iter().position(|a| **a == x) {
            Some(i) => Expression::column(format!("{STAND_IN}{i}")),
            None => x,
        }))
    })
    .map_err(internal)?;
    transform(hidden, &|x| {
        Ok(Some(match &x {
            Expression::Column(c) if c.table.is_none() && c.name.name.starts_with(STAND_IN) => {
                let i: usize = c.name.name[STAND_IN.len()..].parse().unwrap_or(usize::MAX);
                aggs.get(i).map_or(x, |a| (*a).clone())
            }
            _ if is_missing(&x) => Expression::Null(Null),
            _ => x,
        }))
    })
    .map_err(internal)
}

/// A key of the stacked whole's `ORDER BY`: an output column, by name or position.
pub(super) fn order_key(e: &Expression, names: &[String], sel: &Select) -> Result<usize> {
    let by_name = |n: &str| names.iter().position(|x| x.eq_ignore_ascii_case(n));
    let found = match e {
        Expression::Literal(l) => match l.as_ref() {
            Literal::Number(n) => n
                .parse::<usize>()
                .ok()
                .filter(|k| (1..=names.len()).contains(k))
                .map(|k| k - 1),
            _ => None,
        },
        Expression::Column(c) => by_name(&c.name.name).or_else(|| {
            // A column the select list names under an alias.
            sel.expressions.iter().find_map(|item| match item {
                Expression::Alias(a) => match &a.this {
                    Expression::Column(x) if same(x, c) => by_name(&a.alias.name),
                    _ => None,
                },
                _ => None,
            })
        }),
        _ => None,
    };
    found.ok_or_else(|| {
        unsupported(
            "Layer 2 with ROLLUP, CUBE or GROUPING SETS orders by an output column, by name or position",
        )
    })
}
