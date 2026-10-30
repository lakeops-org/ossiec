//! Per-engine rules: what ossiec emits differently for one engine.
//!
//! polyglot renders each dialect; this module only exists for cases where its
//! rendering runs but answers differently on a real engine. Every rule names
//! the evidence (a `tests/engines.rs` case) that it is needed. Add rules only
//! from a failing engine test, never speculatively.

use polyglot_sql::{transform, DialectType, Expression};

use crate::sql::{self, Tpl};
use crate::{Error, Result};

/// Finish rendered SQL for `target`: syntax that only exists in the target
/// dialect, so it can't ride through the dialect-neutral AST. The SQL is parsed
/// and regenerated in the target dialect itself; nothing is translated.
pub(crate) fn finish(sql: String, target: DialectType, pretty: bool) -> Result<String> {
    match target {
        DialectType::ClickHouse => clickhouse_settings(&sql, pretty),
        _ => Ok(sql),
    }
}

/// ClickHouse fills unmatched LEFT JOIN columns with the type's default (0, '')
/// instead of NULL unless `join_use_nulls = 1`, which would merge orphan rows
/// into real groups. The setting goes on the query itself.
///
/// Evidence: `orphans_on_a_non_nullable_column` (without it, the guest checkout
/// lands under segment '' instead of NULL).
fn clickhouse_settings(sql: &str, pretty: bool) -> Result<String> {
    let ch = DialectType::ClickHouse;
    let settings = match sql::parse("SELECT 1 SETTINGS join_use_nulls = 1", ch) {
        Ok(Expression::Select(s)) => s.settings,
        _ => return Err(Error::Internal("settings template".into())),
    };
    let mut ast = sql::parse(sql, ch)
        .map_err(|e| Error::Internal(format!("re-reading ClickHouse SQL: {e}")))?;
    if let Expression::Select(s) = &mut ast {
        s.settings = settings;
    }
    let d = polyglot_sql::dialects::Dialect::get(ch);
    let out = if pretty {
        d.generate_pretty(&ast)
    } else {
        d.generate(&ast)
    };
    out.map_err(|e| Error::Internal(e.to_string()))
}

/// Rewrite the dialect-neutral AST for `target`. Runs before rendering.
pub(crate) fn adapt(ast: Expression, target: DialectType) -> Result<Expression> {
    match target {
        DialectType::ClickHouse => clickhouse(ast),
        _ => Ok(ast),
    }
}

/// ClickHouse has no standard `LAG`/`LEAD`. Its `lag`/`lagInFrame` return the
/// type's default (0) instead of NULL when there is no previous row, and
/// `lagInFrame` only sees rows inside the window frame. The standard result
/// needs `lagInFrame(toNullable(x), n) OVER (... ROWS BETWEEN UNBOUNDED
/// PRECEDING AND UNBOUNDED FOLLOWING)`.
///
/// Evidence: `running_total_and_change` on ClickHouse 25.8 (`LAG` fails to
/// resolve; ClickHouse's own `lag` returns 0 for the first period).
fn clickhouse(ast: Expression) -> Result<Expression> {
    let full_frame = match sql::parse(
        "SUM(1) OVER (ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING)",
        DialectType::Generic,
    ) {
        Ok(Expression::WindowFunction(w)) => w.over.frame,
        _ => return Err(Error::Internal("frame template".into())),
    };
    transform(ast, &|e| {
        let Expression::WindowFunction(mut w) = e else {
            return Ok(Some(e));
        };
        let (name, f) = match w.this {
            Expression::Lag(f) => ("lagInFrame", f),
            Expression::Lead(f) => ("leadInFrame", f),
            other => {
                w.this = other;
                return Ok(Some(Expression::WindowFunction(w)));
            }
        };
        let f = *f;
        let mut t = Tpl::new();
        let mut args = vec![format!("toNullable({})", t.e(f.this))];
        args.push(match f.offset {
            Some(o) => t.e(o),
            None => "1".into(),
        });
        if let Some(d) = f.default {
            args.push(t.e(d));
        }
        t.push(&format!("{name}({})", args.join(", ")));
        w.this = t
            .build()
            .map_err(|e| polyglot_sql::Error::internal(e.to_string()))?;
        w.over.frame = full_frame.clone();
        Ok(Some(Expression::WindowFunction(w)))
    })
    .map_err(|e| Error::Internal(e.to_string()))
}
