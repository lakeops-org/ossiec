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
    let ast = order_by_subqueries_by_name(ast)?;
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

/// StarRocks refuses a subquery in `ORDER BY` ("ORDER BY clause cannot contain
/// subquery"). Each key is computed in a derived table instead, and the query
/// around it orders by the keys' names, then applies `LIMIT`/`OFFSET`. Evidence:
/// `having_and_order_by_subqueries`.
fn order_by_subqueries_by_name(ast: Expression) -> Result<Expression> {
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
    let quote = |n: &str| format!("\"{}\"", n.replace('"', "\"\""));
    let mut t = crate::sql::Tpl::new();
    let src = t.e(crate::sql::derived(
        Expression::Select(Box::new(inner)),
        Identifier::new(OUTER),
    ));
    let cols: Vec<String> = names
        .iter()
        .map(|n| format!("{OUTER}.{} AS {}", quote(n), quote(n)))
        .collect();
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
