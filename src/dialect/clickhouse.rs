//! ClickHouse rules.

use crate::sql::transform;
use polyglot_sql::expressions::{WindowFrame, WindowFrameBound, WindowFrameKind};
use polyglot_sql::{DialectType, Expression};

use super::{internal, literal_backslashes_in_like};
use crate::plan::LogicalPlan;
use crate::sql::{self, Tpl};
use crate::{Error, Result};

/// ClickHouse has no standard `LAG`/`LEAD`. Its `lag`/`lagInFrame` return the
/// type's default (0) instead of NULL when there is no previous row, and
/// `lagInFrame` only sees rows inside the window frame. The standard result
/// needs `lagInFrame(toNullable(x), n) OVER (... ROWS BETWEEN UNBOUNDED
/// PRECEDING AND UNBOUNDED FOLLOWING)`.
///
/// Evidence: `running_total_and_change` on ClickHouse 25.8 and 26.8 (`LAG` fails to
/// resolve; ClickHouse's own `lag` returns 0 for the first period).
///
/// Plus backslashes in `LIKE` patterns (see `literal_backslashes_in_like`).
pub(super) fn adapt(ast: Expression, _plan: &LogicalPlan) -> Result<Expression> {
    let ast = literal_backslashes_in_like(ast)?;
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
        w.over.frame = Some(WindowFrame {
            kind: WindowFrameKind::Rows,
            start: WindowFrameBound::UnboundedPreceding,
            end: Some(WindowFrameBound::UnboundedFollowing),
            exclude: None,
            kind_text: None,
            start_side_text: None,
            end_side_text: None,
        });
        Ok(Some(Expression::WindowFunction(w)))
    })
    .map_err(internal)
}

/// ClickHouse fills unmatched LEFT JOIN columns with the type's default (0, '')
/// instead of NULL unless `join_use_nulls = 1`, which would merge orphan rows
/// into real groups. The setting goes on the query itself, as a `SETTINGS`
/// clause, which only ClickHouse's dialect can carry.
///
/// Evidence: `orphans_on_a_non_nullable_column` (without it, the guest checkout
/// lands under segment '' instead of NULL) and `integer_ratio` (the default id
/// `0` is counted as a customer).
///
/// Its `sum`, `max` and `avg` over no rows are likewise 0 and `nan`, not the NULL
/// the spec requires (wide table §5.2), unless `aggregate_functions_null_for_empty
/// = 1`. `count` stays 0. Evidence: `filter_without_a_dataset`.
///
/// Typed `CAST(NULL AS T)` → `Nullable(T)` is left to polyglot (≥0.13.5)
/// ([polyglot#514](https://github.com/tobilg/polyglot/issues/514)).
pub(super) fn finish(sql: String, pretty: bool) -> Result<String> {
    let ch = DialectType::ClickHouse;
    let settings = match sql::parse(
        "SELECT 1 SETTINGS join_use_nulls = 1, aggregate_functions_null_for_empty = 1",
        ch,
    ) {
        Ok(Expression::Select(s)) => s.settings,
        _ => return Err(Error::Internal("settings template".into())),
    };
    let mut ast = sql::parse(&sql, ch)
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
    out.map_err(internal)
}
