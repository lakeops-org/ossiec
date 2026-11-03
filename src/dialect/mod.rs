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
        ),
    }
}

/// Hook 1: rewrite the dialect-neutral AST for `target`. Runs before rendering.
pub(crate) fn adapt(
    ast: Expression,
    target: DialectType,
    plan: &LogicalPlan,
) -> Result<Expression> {
    match target {
        DialectType::BigQuery => bigquery::adapt(ast, plan),
        DialectType::ClickHouse => clickhouse::adapt(ast, plan),
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
    match target {
        DialectType::ClickHouse => clickhouse::finish(sql, pretty),
        _ => Ok(sql),
    }
}

fn internal(e: polyglot_sql::Error) -> Error {
    Error::Internal(e.to_string())
}

/// Engines that treat `\` as a `LIKE` escape by default (Postgres, ClickHouse,
/// StarRocks, MySQL) would read `'C:\%'` as `C:%`. SQL declares escapes with
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
    polyglot_sql::transform(ast, &|e| {
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
