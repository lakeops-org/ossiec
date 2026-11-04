//! Postgres rules.

use polyglot_sql::builder::{cast, Expr};
use polyglot_sql::expressions::CollationExpr;
use polyglot_sql::{transform, Expression};

use super::{internal, literal_backslashes_in_like};
use crate::plan::LogicalPlan;
use crate::Result;

/// - **Typed NULLs in stacked parts.** Postgres types `UNION` branches pairwise,
///   left to right, so two bare `NULL`s become `text` and then clash with a
///   number in the next branch. Each `NULL AS tN` gets its term's type family.
///   Evidence: `three_parts_with_a_one_side_filter` ("UNION types text and
///   bigint cannot be matched").
/// - **Byte order for strings.** A locale collation (en_US) sorts 'ada' before
///   'Bob'; every other engine sorts by byte. String dimensions are output with
///   `COLLATE "C"`, so `ORDER BY` agrees across engines. Evidence: `string_ordering`.
///
/// Plus backslashes in `LIKE` patterns (see `literal_backslashes_in_like`).
pub(super) fn adapt(ast: Expression, plan: &LogicalPlan) -> Result<Expression> {
    let ast = literal_backslashes_in_like(ast)?;
    transform(ast, &|e| {
        let Expression::Alias(mut a) = e else {
            return Ok(Some(e));
        };
        let name = a.alias.name.as_str();
        if matches!(a.this, Expression::Null(_)) {
            let term = name.strip_prefix('t').and_then(|k| k.parse::<usize>().ok());
            if let Some(ty) = term
                .and_then(|k| plan.terms.get(k))
                .and_then(|t| t.term.null_type)
            {
                a.this = cast(Expr(a.this), ty).into_inner();
            }
        } else if a.alias.quoted && plan.string_dims.iter().any(|d| d == name) {
            a.this = Expression::Collation(Box::new(CollationExpr {
                this: a.this,
                collation: "C".into(),
                quoted: false,
                double_quoted: true,
            }));
        }
        Ok(Some(Expression::Alias(a)))
    })
    .map_err(internal)
}
