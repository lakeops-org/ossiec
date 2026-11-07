//! StarRocks rules.

use crate::sql::transform;
use polyglot_sql::expressions::Literal;
use polyglot_sql::Expression;

use super::{internal, literal_backslashes_in_like};
use crate::plan::LogicalPlan;
use crate::Result;

/// StarRocks reads `\` in string literals as an escape, and polyglot doesn't
/// double it for StarRocks (it does for MySQL), so `'C:\temp'` would match
/// `C:<tab>emp`. Evidence: `string_literals_with_quote_and_backslash`.
///
/// `LIKE` patterns are escaped first (see `literal_backslashes_in_like`), then
/// every string literal: a `LIKE` backslash needs both levels.
pub(super) fn adapt(ast: Expression, _plan: &LogicalPlan) -> Result<Expression> {
    let ast = literal_backslashes_in_like(ast)?;
    transform(ast, &|e| {
        Ok(Some(match e {
            Expression::Literal(mut l) => {
                if let Literal::String(s) = l.as_mut() {
                    *s = s.replace('\\', "\\\\");
                }
                Expression::Literal(l)
            }
            other => other,
        }))
    })
    .map_err(internal)
}
