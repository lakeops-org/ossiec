//! MySQL rules.

use polyglot_sql::builder::{cast, Expr};
use polyglot_sql::{transform, DialectType, Expression};

use super::{internal, literal_backslashes_in_like};
use crate::plan::LogicalPlan;
use crate::sql;
use crate::Result;

/// - **Byte-exact strings.** MySQL's default collation (`utf8mb4_0900_ai_ci`)
///   compares case-insensitively, so `Ada` and `ada` became one group and
///   `name IN ('ada')` also matched `Ada`. String expressions used as
///   dimensions or filtered on get `COLLATE utf8mb4_bin`, as every other engine
///   compares by byte. Evidence: `string_ordering` (a group vanished) and
///   `case_sensitive_filter`.
/// - **Unrounded `AVG`.** `AVG` of integers is rounded to 4 decimal places
///   (`div_precision_increment`): 1566.6667 instead of 1566.666…. The argument
///   is cast to `DOUBLE`. Evidence: `averages_of_integers_and_one_side_values`.
/// - Backslashes in `LIKE` patterns (see `literal_backslashes_in_like`).
///
/// `NULLS FIRST/LAST` is left to polyglot (≥0.13.5), which rewrites unsupported
/// modifiers to a leading `CASE WHEN x IS NULL …` key ([polyglot#515](https://github.com/tobilg/polyglot/issues/515)).
/// The unrounded-`AVG` rule alone, for the Layer 2 outer shell (see [`adapt`]).
pub(super) fn adapt_avg(ast: Expression) -> Result<Expression> {
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Avg(mut f) => {
                f.this = cast(Expr(f.this), "DOUBLE").into_inner();
                Expression::Avg(f)
            }
            other => other,
        }))
    })
    .map_err(internal)
}

pub(super) fn adapt(ast: Expression, plan: &LogicalPlan) -> Result<Expression> {
    let ast = literal_backslashes_in_like(ast)?;
    let binary = match sql::parse("x COLLATE utf8mb4_bin", DialectType::Generic) {
        Ok(Expression::Collation(c)) => c,
        _ => return Err(crate::Error::Internal("collation template".into())),
    };
    transform(ast, &|e| {
        if plan.string_exprs.contains(&e) {
            let mut c = binary.clone();
            c.this = e;
            return Ok(Some(Expression::Collation(c)));
        }
        Ok(Some(match e {
            Expression::Avg(mut f) => {
                f.this = cast(Expr(f.this), "DOUBLE").into_inner();
                Expression::Avg(f)
            }
            other => other,
        }))
    })
    .map_err(internal)
}
