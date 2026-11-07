//! BigQuery rules.

use crate::sql::transform;
use polyglot_sql::expressions::Literal;
use polyglot_sql::Expression;

use super::internal;
use crate::plan::LogicalPlan;
use crate::Result;

/// BigQuery's `DATE_TRUNC(d, WEEK)` starts weeks on Sunday; Ossie's week starts
/// on Monday (`expression_language.md`, DATE_TRUNC), which is BigQuery's `ISOWEEK`.
/// Evidence: Ossie spec and BigQuery's documented `WEEK` = `WEEK(SUNDAY)`; BigQuery
/// isn't in the executed engine set, so this rule is checked by golden SQL only
/// (`shop_week_grain_bigquery`).
pub(super) fn adapt(ast: Expression, _plan: &LogicalPlan) -> Result<Expression> {
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Function(mut f) if f.name.eq_ignore_ascii_case("DATE_TRUNC") => {
                if let Some(Expression::Literal(l)) = f.args.first_mut() {
                    if let Literal::String(unit) = l.as_mut() {
                        if unit.eq_ignore_ascii_case("week") {
                            *unit = "isoweek".into();
                        }
                    }
                }
                Expression::Function(f)
            }
            other => other,
        }))
    })
    .map_err(internal)
}
