//! Per-engine rules: what ossiec emits differently for one engine.
//!
//! polyglot renders each dialect; these rules only exist where its rendering
//! runs but answers differently on a real engine. Every rule names the
//! evidence (an engine test case) that it is needed. Add rules only from a
//! failing engine test, never speculatively.
//!
//! One file per engine, each with the same two hooks:
//!
//! - `adapt(ast, plan)` rewrites the dialect-neutral AST before rendering.
//! - `finish(sql, pretty)` edits the rendered SQL, parsed in the engine's own
//!   dialect, for syntax only that dialect has (it can't pass through the
//!   neutral AST). Nothing is translated.
//!
//! Engines without a file need no rules. To add one: create `<engine>.rs`
//! with `adapt` (and `finish` if needed) and add its arms below.

mod filter;
pub(crate) use filter::lower_filters;
mod bigquery;
mod clickhouse;
mod mysql;
mod postgres;
mod starrocks;

use polyglot_sql::{DialectType, Expression};

use crate::plan::LogicalPlan;
use crate::{Error, Result};

/// Engines verified to run standard `agg(...) FILTER (WHERE …)`.
/// Everyone else (and `target: None`) gets the CASE rewrite: polyglot still
/// passes FILTER through to engines that lack it (StarRocks, MySQL, ClickHouse,
/// Redshift, BigQuery, Snowflake, …).
pub(crate) fn supports_aggregate_filter(target: Option<DialectType>) -> bool {
    matches!(
        target,
        Some(
            DialectType::DuckDB
                | DialectType::PostgreSQL
                | DialectType::Trino
                | DialectType::Athena
                | DialectType::Databricks
                | DialectType::Spark
        )
    )
}

/// Engines verified to run standard `a IS NOT DISTINCT FROM b`. Everyone else
/// gets `(a = b OR (a IS NULL AND b IS NULL))`. `None` (the statement's own
/// generic dialect, translated downstream) stays native. The list grows only
/// from engine-tier evidence, as above.
pub(crate) fn supports_null_safe_eq(target: Option<DialectType>) -> bool {
    match target {
        None => true,
        Some(t) => matches!(
            t,
            DialectType::DuckDB
                | DialectType::PostgreSQL
                | DialectType::Trino
                | DialectType::Athena
                | DialectType::Databricks
                | DialectType::Spark
        ),
    }
}

/// Engines that run a correlated subquery as the right side of `IN`. ClickHouse
/// runs correlated `EXISTS` but refuses one in `IN` ("not supported as IN function
/// arguments yet", 26.8), so it gets `EXISTS` forms with `IN`'s three-valued
/// answer. Evidence: `where_correlated_subqueries`.
pub(crate) fn supports_correlated_in(target: Option<DialectType>) -> bool {
    !matches!(target, Some(DialectType::ClickHouse))
}

/// Engines whose `x IN (subquery)` answers as SQL does when `x` or a row is NULL.
/// ClickHouse's doesn't (`transform_null_in = 0`, 26.8): `7 NOT IN (SELECT NULL)`
/// is 1, not NULL, and `NULL NOT IN (<no rows>)` is NULL, not TRUE. So every `IN`
/// subquery gets `EXISTS` forms there, uncorrelated ones too. Evidence:
/// `in_subquery_nulls`.
pub(crate) fn in_is_three_valued(target: Option<DialectType>) -> bool {
    !matches!(target, Some(DialectType::ClickHouse))
}

/// The engine whose SQL `target` runs. Athena's engine (v3) is Trino, and its
/// queries are Trino's: same functions, same type names (polyglot's Athena dialect
/// writes Hive's DDL types such as `STRING`, which an Athena query rejects). So an
/// Athena query is rendered, and its functions lowered, as Trino's. Evidence: the
/// engine tier runs every Athena case on Trino.
///
/// Databricks SQL is Spark SQL, and Apache Spark is what the engine tier runs, so a
/// Databricks query is rendered, and its functions lowered, as Spark's: what ships
/// for Databricks is what was tested.
pub(crate) fn runs_as(target: DialectType) -> DialectType {
    match target {
        DialectType::Athena => DialectType::Trino,
        DialectType::Databricks => DialectType::Spark,
        t => t,
    }
}

/// Hook 1: rewrite the dialect-neutral AST for `target`. Runs before rendering.
pub(crate) fn adapt(
    ast: Expression,
    target: DialectType,
    plan: &LogicalPlan,
) -> Result<Expression> {
    match runs_as(target) {
        DialectType::BigQuery => bigquery::adapt(ast, plan),
        DialectType::ClickHouse => clickhouse::adapt(ast, plan),
        // Spark SQL (and Databricks, which runs it): only the LIKE escape differs here.
        DialectType::Spark => literal_backslashes_in_like(ast),
        DialectType::MySQL => mysql::adapt(ast, plan),
        DialectType::PostgreSQL => postgres::adapt(ast, plan),
        DialectType::StarRocks => starrocks::adapt(ast, plan),
        _ => Ok(ast),
    }
}

/// Hook 1 for the Layer 2 outer SQL shell (A.3), which has no [`LogicalPlan`]:
/// only the rules that concern ordinary aggregates over derived columns.
pub(crate) fn adapt_shell(ast: Expression, target: DialectType) -> Result<Expression> {
    match target {
        DialectType::MySQL => mysql::adapt_avg(ast),
        _ => Ok(ast),
    }
}

/// Hook 2: finish rendered SQL for `target`. Runs after rendering.
pub(crate) fn finish(sql: String, target: DialectType, pretty: bool) -> Result<String> {
    // MySQL's generator upper-cases function names, which it reads either way.
    let sql = sql
        .replace("__ossiec_exact_", "")
        .replace("__OSSIEC_EXACT_", "");
    match target {
        DialectType::ClickHouse => clickhouse::finish(sql, pretty),
        _ => Ok(sql),
    }
}

fn internal(e: polyglot_sql::Error) -> Error {
    Error::Internal(e.to_string())
}

/// Engines that treat `\` as a `LIKE` escape by default (Postgres, ClickHouse,
/// StarRocks, MySQL, Spark) would read `'C:\%'` as `C:%`. SQL declares escapes with
/// `ESCAPE`, so without one a backslash is an ordinary character (Trino, DuckDB).
/// Doubling it in the pattern keeps it literal. Evidence: `like_with_a_backslash`.
fn literal_backslashes_in_like(ast: Expression) -> Result<Expression> {
    use polyglot_sql::expressions::Literal;
    let double = |op: &mut polyglot_sql::expressions::LikeOp| {
        if op.escape.is_none() {
            if let Expression::Literal(l) = &mut op.right {
                if let Literal::String(s) = l.as_mut() {
                    *s = s.replace('\\', "\\\\");
                }
            }
        }
    };
    crate::sql::transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Like(mut op) => {
                double(&mut op);
                Expression::Like(op)
            }
            Expression::ILike(mut op) => {
                double(&mut op);
                Expression::ILike(op)
            }
            other => other,
        }))
    })
    .map_err(internal)
}

/// StarRocks refuses a subquery in `ORDER BY` ("ORDER BY clause cannot contain
/// subquery"), and ClickHouse 26.8 a correlated one ("Correlated subqueries in
/// ORDER BY are not supported"); both take one in the select list. Each key is
/// computed in a derived table instead, and the query around it orders by the
/// keys' names, then applies `LIMIT`/`OFFSET`. Evidence:
/// `having_and_order_by_subqueries`, `having_and_order_by_exists_and_in`.
pub(super) fn order_by_subqueries_by_name(ast: Expression) -> Result<Expression> {
    use polyglot_sql::expressions::{Alias, Identifier};
    use polyglot_sql::{is_query, is_subquery, ExpressionWalk};
    let Expression::Select(sel) = &ast else {
        return Ok(ast);
    };
    let Some(order) = &sel.order_by else {
        return Ok(ast);
    };
    let reads_query = order
        .expressions
        .iter()
        .any(|k| k.this.contains(|x| is_query(x) || is_subquery(x)));
    // Every output is named, so the query around can name it.
    let names: Option<Vec<String>> = sel
        .expressions
        .iter()
        .map(|e| match e {
            Expression::Alias(a) => Some(a.alias.name.clone()),
            Expression::Column(c) => Some(c.name.name.clone()),
            _ => None,
        })
        .collect();
    let (true, Some(names), false) = (reads_query, names, sel.distinct) else {
        return Ok(ast);
    };
    const OUTER: &str = "__ossiec_ordered";
    const KEY: &str = "__ossiec_ord";
    // Each key's name, unlike any output's (engines may fold case).
    let mut key_names: Vec<String> = vec![];
    for i in 0..order.expressions.len() {
        let mut name = format!("{KEY}{i}");
        while names
            .iter()
            .chain(&key_names)
            .any(|n| n.eq_ignore_ascii_case(&name))
        {
            name.push('_');
        }
        key_names.push(name);
    }
    let mut inner = (**sel).clone();
    inner.order_by = None;
    inner.limit = None;
    inner.offset = None;
    inner.fetch = None;
    for (k, name) in order.expressions.iter().zip(&key_names) {
        inner
            .expressions
            .push(Expression::Alias(Box::new(Alias::new(
                k.this.clone(),
                Identifier::new(name.clone()),
            ))));
    }
    let mut t = crate::sql::Tpl::new();
    let src = t.e(crate::sql::derived(
        Expression::Select(Box::new(inner)),
        Identifier::new(OUTER),
    ));
    // Output names go in as placeholders, never as text: a name like `__ossiec_a0`
    // would read as a template slot.
    let mut cols: Vec<String> = Vec::with_capacity(names.len());
    for n in &names {
        let mut column = crate::sql::col(&format!("{OUTER}.x"));
        if let Expression::Column(c) = &mut column {
            c.name = crate::sql::quoted(n);
        }
        cols.push(format!("{} AS {}", t.e(column), t.alias(n)));
    }
    t.push(&format!("SELECT {} FROM {src}", cols.join(", ")));
    let Expression::Select(mut outer) = t.build()? else {
        return Ok(ast);
    };
    let mut keys = order.clone();
    for (k, name) in keys.expressions.iter_mut().zip(&key_names) {
        k.this = crate::sql::col(&format!("{OUTER}.{name}"));
    }
    outer.order_by = Some(keys);
    outer.limit = sel.limit.clone();
    outer.offset = sel.offset.clone();
    outer.fetch = sel.fetch.clone();
    Ok(Expression::Select(outer))
}
