//! MySQL rules.

use crate::sql::transform;
use polyglot_sql::builder::{cast, Expr};
use polyglot_sql::expressions::CollationExpr;
use polyglot_sql::Expression;

use super::{internal, literal_backslashes_in_like};
use crate::plan::LogicalPlan;
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
    transform(ast, &|e| {
        if plan.string_exprs.contains(&e) {
            return Ok(Some(Expression::Collation(Box::new(CollationExpr {
                this: e,
                collation: "utf8mb4_bin".into(),
                quoted: false,
                double_quoted: false,
            }))));
        }
        Ok(Some(match e {
            Expression::Avg(mut f) => {
                f.this = cast(Expr(f.this), "DOUBLE").into_inner();
                Expression::Avg(f)
            }
            // LIKE is case-sensitive (expression language), and the default
            // collation isn't. Evidence: `s LIKE 'he%'` in `expression_language.rs`.
            Expression::Like(mut op) if !matches!(op.left, Expression::Collation(_)) => {
                op.left = Expression::Collation(Box::new(CollationExpr {
                    this: op.left,
                    collation: "utf8mb4_bin".into(),
                    quoted: false,
                    double_quoted: false,
                }));
                Expression::Like(op)
            }
            other => other,
        }))
    })
    .map_err(internal)
}
