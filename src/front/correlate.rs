//! Correlated subqueries in `WHERE` (§7.3): a subquery that reads the row of the
//! query around it.
//!
//! ```sql
//! SELECT status, AGG(order_count) FROM orders o
//! WHERE EXISTS (SELECT c.id FROM customers c WHERE c.id = o.customer_id AND c.region = 'EU')
//! GROUP BY status
//! ```
//!
//! The subquery is still compiled as a Layer 2 query of its own, so it can't read
//! the outer row. Each correlation (`c.id = o.customer_id`, an `=` between a
//! column of the subquery and one of the enclosing query, `AND`-ed into its
//! `WHERE`) is taken out, and its inner side becomes an output of the subquery,
//! grouped by: one result per value of the key. The stand-in then keeps the rows
//! for the outer row's value, which is the correlated subquery:
//!
//! ```sql
//! EXISTS (SELECT sq."id" FROM (<subquery by c.id>) AS sq WHERE sq."__ossiec_k0" = o.customer_id)
//! ```
//!
//! The only correlated part left is that `WHERE`, over a relation that reads
//! nothing outside it: the shape every engine with correlated subqueries runs.
//! Grouping by the key keeps each value's rows apart, so an aggregate or `HAVING`
//! in the subquery reads only the rows a correlated subquery would.
//!
//! One case differs. A subquery that aggregates without `GROUP BY` returns one
//! row even when no row matches (`COUNT(*)` is 0), but grouped by the key it has
//! no row for that value. As a scalar it is therefore
//! `CASE WHEN <a row for the value> THEN <its value> ELSE <the subquery over no rows> END`.
//! Its `HAVING` and its use in `EXISTS` or `IN` are refused.

use polyglot_sql::expressions::{Alias, Column, GroupBy, Identifier, Select};
use polyglot_sql::{generate, DialectType, Expression, ExpressionWalk};

use super::{strip_alias, unsupported, unwrap_paren};
use crate::{sql, Error, Result};

/// Output columns the subquery gains for its correlation keys.
pub(super) const KEY: &str = "__ossiec_k";

/// What a correlated subquery compares with the enclosing row.
pub(super) struct Correlation {
    /// The enclosing query's side of each correlation, as written.
    pub outer: Vec<Expression>,
    /// The subquery's output that each correlation compares it with.
    pub keys: Vec<String>,
    /// For a subquery that aggregates without `GROUP BY`: the same query over
    /// no rows, whose single row is its answer for a value nothing matches.
    pub empty: Option<Select>,
}

/// Names one query level defines: a table's alias (or its name, without one), a
/// derived table's alias, a CTE's name. A qualifier that is one of them reads
/// that level, even when the enclosing query uses the same one. Queries nested
/// in it define their own, which don't reach it.
fn level_names(sel: &Select) -> Vec<String> {
    let mut out = vec![];
    let mut add = |e: &Expression| match e {
        Expression::Table(t) => out.push(t.alias.as_ref().unwrap_or(&t.name).name.clone()),
        Expression::Subquery(s) => out.extend(s.alias.as_ref().map(|a| a.name.clone())),
        Expression::Alias(a) => out.push(a.alias.name.clone()),
        _ => {}
    };
    for e in sel.from.iter().flat_map(|f| &f.expressions) {
        add(e);
    }
    for j in &sel.joins {
        add(&j.this);
    }
    if let Some(w) = &sel.with {
        out.extend(w.ctes.iter().map(|c| c.alias.name.clone()));
    }
    out
}

/// The queries nested in `e`, not `e` itself, outermost only.
fn nested_selects(e: &Expression) -> Vec<&Expression> {
    let mut out: Vec<&Expression> = vec![];
    for x in e.dfs() {
        if matches!(x, Expression::Select(_))
            && !std::ptr::eq(x, e)
            && !out.iter().any(|n| n.contains(|y| std::ptr::eq(y, x)))
        {
            out.push(x);
        }
    }
    out
}

/// The columns of `e` outside the queries nested in it: those its own level reads.
fn own_columns(e: &Expression) -> Vec<&Column> {
    let nested = nested_selects(e);
    e.dfs()
        .filter_map(|x| match x {
            Expression::Column(c) => Some((x, c.as_ref())),
            _ => None,
        })
        .filter(|(x, _)| !nested.iter().any(|n| n.contains(|y| std::ptr::eq(y, *x))))
        .map(|(_, c)| c)
        .collect()
}

/// A column of a query nested in the subquery (`e`, inside `names` already) that
/// reads the enclosing query: no level from the subquery down to it defines
/// its qualifier.
fn nested_outer<'a>(e: &'a Expression, names: &[String], stage: &[String]) -> Option<&'a Column> {
    nested_selects(e).into_iter().find_map(|n| {
        let Expression::Select(sel) = n else {
            return None;
        };
        let mut scope = names.to_vec();
        scope.extend(level_names(sel));
        own_columns(n)
            .into_iter()
            .find(|c| reads(c, stage, &scope))
            .or_else(|| nested_outer(n, &scope, stage))
    })
}

/// `c` names a relation of the enclosing stage that `scope` doesn't redefine.
fn reads(c: &Column, stage: &[String], scope: &[String]) -> bool {
    c.table.as_ref().is_some_and(|q| {
        stage.iter().any(|s| s.eq_ignore_ascii_case(&q.name))
            && !scope.iter().any(|s| s.eq_ignore_ascii_case(&q.name))
    })
}

fn conjuncts(e: &Expression, out: &mut Vec<Expression>) {
    match e {
        Expression::And(a) => {
            conjuncts(&a.left, out);
            conjuncts(&a.right, out);
        }
        Expression::Paren(p) if matches!(p.this, Expression::And(_)) => conjuncts(&p.this, out),
        other => out.push(other.clone()),
    }
}

fn and_all(parts: Vec<Expression>) -> Option<Expression> {
    parts
        .into_iter()
        .reduce(|a, b| Expression::And(Box::new(polyglot_sql::expressions::BinaryOp::new(a, b))))
}

fn columns(e: &Expression) -> impl Iterator<Item = &Column> {
    e.dfs().filter_map(|x| match x {
        Expression::Column(c) => Some(c.as_ref()),
        _ => None,
    })
}

fn text(e: &Expression) -> String {
    generate(e, DialectType::Generic).unwrap_or_else(|_| "…".into())
}

/// Take the correlations out of `sel`, a subquery of a stage whose `FROM` names
/// `stage`, and make each one's inner side an output. `None` when `sel` reads
/// nothing of the enclosing query. `same` tells whether two of the subquery's
/// expressions read the same field (`o2.customer_id` and `customer_id`).
pub(super) fn correlate(
    sel: &mut Select,
    stage: &[String],
    same: &dyn Fn(&Expression, &Expression) -> bool,
) -> Result<Option<Correlation>> {
    let inner = level_names(sel);
    let is_outer = |c: &Column| reads(c, stage, &inner);
    let whole = Expression::Select(Box::new(sel.clone()));
    let deep = nested_outer(&whole, &inner, stage).cloned();
    if deep.is_none() && !own_columns(&whole).into_iter().any(is_outer) {
        return Ok(None);
    }
    let name = |c: &Column| {
        format!(
            "{}.{}",
            c.table.as_ref().map_or("", |t| t.name.as_str()),
            c.name.name
        )
    };
    let refuse = |c: &Column, why: &str| {
        unsupported(format!(
            "Layer 2 correlated subquery reads `{}` of the enclosing query {why}; a correlation compares a column of the enclosing query with `=` to one of the subquery, AND-ed into the subquery's own WHERE (`WHERE c.id = o.customer_id AND …`)",
            name(c)
        ))
    };

    if let Some(c) = &deep {
        return Err(refuse(c, "from a query nested in the subquery"));
    }
    if super::grouping::grouping_sets(sel)?.is_some() {
        // A subtotal row's key is NULL, so keeping the enclosing row's value
        // would drop it.
        return Err(unsupported(
            "Layer 2 correlated subquery with ROLLUP, CUBE or GROUPING SETS is not supported: a subtotal row has no value of the correlated column to match",
        ));
    }

    let mut kept = vec![];
    let mut pairs: Vec<(Expression, Expression)> = vec![];
    let mut parts = vec![];
    if let Some(w) = &sel.where_clause {
        conjuncts(&w.this, &mut parts);
    }
    for part in parts {
        let Some(c) = own_columns(&part)
            .into_iter()
            .find(|c| is_outer(c))
            .cloned()
        else {
            kept.push(part);
            continue;
        };
        let Expression::Eq(eq) = unwrap_paren(&part) else {
            return Err(refuse(&c, &format!("in `{}`", text(&part))));
        };
        let all_outer = |e: &Expression| {
            let cols = own_columns(e);
            !cols.is_empty() && cols.into_iter().all(is_outer)
        };
        let no_outer = |e: &Expression| !own_columns(e).into_iter().any(is_outer);
        let (inside, outside) = if all_outer(&eq.right) && no_outer(&eq.left) {
            (&eq.left, &eq.right)
        } else if all_outer(&eq.left) && no_outer(&eq.right) {
            (&eq.right, &eq.left)
        } else {
            return Err(refuse(&c, &format!("in `{}`", text(&part))));
        };
        if !columns(inside).any(|_| true) {
            return Err(refuse(&c, "against a constant"));
        }
        pairs.push((inside.clone(), outside.clone()));
    }
    let mut rest = sel.clone();
    rest.where_clause = and_all(kept.clone()).map(|this| polyglot_sql::expressions::Where { this });
    let rest_whole = Expression::Select(Box::new(rest.clone()));
    if let Some(c) = own_columns(&rest_whole).into_iter().find(|c| is_outer(c)) {
        return Err(refuse(c, "outside its WHERE"));
    }
    if sel.limit.is_some() || sel.offset.is_some() || sel.fetch.is_some() {
        return Err(unsupported(
            "Layer 2 correlated subquery with LIMIT, OFFSET or FETCH is not supported: it would keep rows across every value of the enclosing row, not per value",
        ));
    }

    let grand_total = super::is_aggregated(sel) && sel.group_by.is_none();
    if grand_total && sel.having.is_some() {
        return Err(unsupported(
            "Layer 2 correlated subquery that aggregates without GROUP BY cannot have HAVING yet",
        ));
    }
    let empty = grand_total.then(|| {
        let mut empty = rest.clone();
        let mut parts = kept.clone();
        parts.push(Expression::Boolean(
            polyglot_sql::expressions::BooleanLiteral { value: false },
        ));
        empty.where_clause = and_all(parts).map(|this| polyglot_sql::expressions::Where { this });
        empty
    });

    let mut keys = vec![];
    for (i, (inside, _)) in pairs.iter().enumerate() {
        // An item the subquery already selects is its own key: Layer 2 selects a
        // field once.
        let selected = rest.expressions.iter().find_map(|item| {
            let (e, alias) = strip_alias(item);
            same(e, inside).then(|| {
                alias.or_else(|| match e {
                    Expression::Column(c) => Some(c.name.name.clone()),
                    _ => None,
                })
            })?
        });
        let key = match selected {
            Some(key) => key,
            None => {
                let key = format!("{KEY}{i}");
                rest.expressions.push(Expression::Alias(Box::new(Alias::new(
                    inside.clone(),
                    Identifier::new(key.clone()),
                ))));
                match &mut rest.group_by {
                    // Already grouped by it: the key adds nothing to the grain.
                    Some(g) if g.all != Some(true) => {
                        if !g.expressions.iter().any(|x| same(x, inside)) {
                            g.expressions.push(inside.clone());
                        }
                    }
                    Some(_) => {}
                    None if grand_total => {
                        rest.group_by = Some(GroupBy {
                            expressions: vec![inside.clone()],
                            all: None,
                            totals: false,
                            comments: vec![],
                        })
                    }
                    None => {}
                }
                if grand_total {
                    // Later keys join the GROUP BY made for the first.
                    if let Some(g) = &mut rest.group_by {
                        if !g.expressions.iter().any(|x| same(x, inside)) {
                            g.expressions.push(inside.clone());
                        }
                    }
                }
                key
            }
        };
        keys.push(key);
    }
    *sel = rest;
    Ok(Some(Correlation {
        outer: pairs.into_iter().map(|(_, o)| o).collect(),
        keys,
        empty,
    }))
}

/// `"name"`, quoted for the generic dialect.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `SELECT cols FROM placeholder AS sq WHERE <keys = the enclosing row's values>`,
/// with `extra` AND-ed on. Without a correlation, only `extra` filters.
fn keyed(
    t: &mut sql::Tpl,
    corr: Option<&Correlation>,
    placeholder: &str,
    sq: &str,
    cols: &str,
    extra: Option<&str>,
) -> String {
    let mut conds: Vec<String> = corr
        .map(|corr| {
            corr.keys
                .iter()
                .zip(&corr.outer)
                .map(|(k, o)| format!("{sq}.{} = {}", quoted(k), t.e(o.clone())))
                .collect()
        })
        .unwrap_or_default();
    conds.extend(extra.map(str::to_string));
    let query = format!("SELECT {cols} FROM {placeholder} AS {sq}");
    if conds.is_empty() {
        query
    } else {
        format!("{query} WHERE {}", conds.join(" AND "))
    }
}

/// The stand-in for a correlated subquery: its `outputs`, kept to the rows whose
/// keys equal the enclosing row's values.
pub(super) fn stand_in(
    corr: &Correlation,
    placeholder: &str,
    sq: &str,
    outputs: &[String],
) -> Result<Expression> {
    let mut t = sql::Tpl::new();
    let cols = outputs
        .iter()
        .map(|o| format!("{sq}.{}", quoted(o)))
        .collect::<Vec<_>>()
        .join(", ");
    let body = keyed(&mut t, Some(corr), placeholder, sq, &cols, None);
    t.push(&format!("({body})"));
    subquery_body(t.build()?)
}

/// The scalar stand-in for a subquery that aggregates without `GROUP BY`: its
/// value for the enclosing row's keys, or, with no row for them, its value over
/// no rows (`empty`).
pub(super) fn scalar_with_default(
    corr: &Correlation,
    placeholder: &str,
    sq: &str,
    empty: &str,
    output: &str,
) -> Result<Expression> {
    let mut t = sql::Tpl::new();
    let exists = keyed(&mut t, Some(corr), placeholder, sq, "1", None);
    // One row per key, so MAX only reads it: some engines take a correlated
    // scalar subquery only when it aggregates.
    let max = format!("MAX({sq}.{})", quoted(output));
    let value = keyed(&mut t, Some(corr), placeholder, sq, &max, None);
    let sqe = format!("{sq}_none");
    t.push(&format!(
        "(CASE WHEN EXISTS ({exists}) THEN ({value}) ELSE (SELECT {sqe}.{} FROM {empty} AS {sqe}) END)",
        quoted(output)
    ));
    t.build()
}

/// `x [NOT] IN (<subquery>)` as `EXISTS` forms, for an engine that takes no
/// correlated subquery in `IN`, or whose `IN` doesn't answer NULL as SQL does
/// (`corr` is `None` for an uncorrelated one). `IN`'s three values: TRUE when a
/// row equals `x`; else NULL when there are rows and `x` or one of them is NULL;
/// else FALSE. `NOT IN` is its negation, so a NULL keeps no row either way.
pub(super) fn in_as_exists(
    corr: Option<&Correlation>,
    placeholder: &str,
    sq: &str,
    output: &str,
    x: &Expression,
    not: bool,
) -> Result<Expression> {
    let mut t = sql::Tpl::new();
    let col = format!("{sq}.{}", quoted(output));
    let equal = format!("{col} = {}", t.e(x.clone()));
    let matched = keyed(&mut t, corr, placeholder, sq, "1", Some(&equal));
    let has_null = keyed(
        &mut t,
        corr,
        placeholder,
        sq,
        "1",
        Some(&format!("{col} IS NULL")),
    );
    let any = keyed(&mut t, corr, placeholder, sq, "1", None);
    // `x IS NULL` stays outside the subquery: ClickHouse 26.8 drops a correlated
    // EXISTS condition that reads only the enclosing row, and finds no row.
    let test = format!(
        "(CASE WHEN EXISTS ({matched}) THEN TRUE \
         WHEN EXISTS ({has_null}) OR ({} IS NULL AND EXISTS ({any})) THEN NULL \
         ELSE FALSE END)",
        t.e(x.clone())
    );
    t.push(&if not { format!("(NOT {test})") } else { test });
    t.build()
}

/// `( SELECT … )` parsed as an expression is a subquery; the stand-in replaces
/// the `SELECT` inside one, so return that `SELECT`.
fn subquery_body(e: Expression) -> Result<Expression> {
    match e {
        Expression::Subquery(s) => Ok(s.this),
        Expression::Paren(p) => subquery_body(p.this),
        other @ Expression::Select(_) => Ok(other),
        other => Err(Error::Internal(format!(
            "correlated stand-in is not a query: {}",
            text(&other)
        ))),
    }
}
