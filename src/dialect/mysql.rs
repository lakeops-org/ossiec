//! MySQL rules.

use polyglot_sql::builder::{cast, Expr};
use polyglot_sql::{transform, DialectType, Expression};

use super::{internal, literal_backslashes_in_like};
use crate::plan::LogicalPlan;
use crate::sql::{self, Tpl};
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
/// - **`NULLS FIRST/LAST`.** MySQL has no such syntax, and polyglot drops it,
///   so `orderBy.nulls` became MySQL's default (NULLs first on ASC). A leading
///   `CASE WHEN x IS NULL THEN 1 ELSE 0 END` key places them. Evidence: TPC-DS
///   `q12` (`nulls: last`).
/// - Backslashes in `LIKE` patterns (see `literal_backslashes_in_like`).
pub(super) fn adapt(ast: Expression, plan: &LogicalPlan) -> Result<Expression> {
    let ast = literal_backslashes_in_like(ast)?;
    let ast = nulls_order(ast)?;
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

fn nulls_order(ast: Expression) -> Result<Expression> {
    transform(ast, &|e| {
        let Expression::Select(mut s) = e else {
            return Ok(Some(e));
        };
        if let Some(order) = s.order_by.as_mut() {
            let mut keys = Vec::with_capacity(order.expressions.len());
            for mut o in std::mem::take(&mut order.expressions) {
                if let Some(first) = o.nulls_first.take() {
                    let mut t = Tpl::new();
                    let x = t.e(o.this.clone());
                    t.push(&format!("CASE WHEN {x} IS NULL THEN 1 ELSE 0 END"));
                    let mut is_null = o.clone();
                    is_null.this = t
                        .build()
                        .map_err(|e| polyglot_sql::Error::internal(e.to_string()))?;
                    is_null.desc = first;
                    keys.push(is_null);
                }
                keys.push(o);
            }
            order.expressions = keys;
        }
        Ok(Some(Expression::Select(s)))
    })
    .map_err(internal)
}
