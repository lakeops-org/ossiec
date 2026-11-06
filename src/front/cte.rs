//! `WITH` (§7.7): a CTE is a named derived table, so measures propagate through it
//! the same way. Each reference becomes a copy of the CTE's body, before any other
//! stage looks at the query. A query's own CTEs shadow a dataset of that name, and
//! a CTE's body sees only the ones before it, so `WITH orders AS (... FROM orders)`
//! reads the dataset.

use polyglot_sql::expressions::{Select, TableRef};
use polyglot_sql::Expression;

use super::unsupported;
use crate::sql;
use crate::{Error, Result};

/// The CTEs in scope: name and body, innermost last.
type Scope = Vec<(String, Select)>;

/// Replace every `WITH` in `sel` and its derived tables with what it names.
pub(super) fn expand(sel: &mut Select) -> Result<()> {
    expand_in(sel, &vec![])
}

fn expand_in(sel: &mut Select, outer: &Scope) -> Result<()> {
    let mut scope = outer.clone();
    if let Some(with) = sel.with.take() {
        if with.recursive {
            return Err(unsupported("Layer 2 WITH RECURSIVE is not supported"));
        }
        let mut own: Vec<String> = vec![];
        for cte in with.ctes {
            let name = cte.alias.name.to_ascii_lowercase();
            if own.contains(&name) {
                return Err(Error::Request(format!(
                    "Layer 2 WITH names `{}` twice",
                    cte.alias.name
                )));
            }
            if !cte.columns.is_empty() {
                return Err(unsupported(format!(
                    "Layer 2 WITH `{}`: a column list is not supported; alias the columns in its SELECT",
                    cte.alias.name
                )));
            }
            let Expression::Select(mut body) = cte.this else {
                return Err(unsupported(format!(
                    "Layer 2 WITH `{}` must be a single SELECT",
                    cte.alias.name
                )));
            };
            expand_in(&mut body, &scope)?;
            own.push(name.clone());
            scope.push((name, *body));
        }
    }
    if let Some(from) = &mut sel.from {
        for e in &mut from.expressions {
            expand_source(e, &scope)?;
        }
    }
    for join in &mut sel.joins {
        expand_source(&mut join.this, &scope)?;
    }
    Ok(())
}

fn expand_source(e: &mut Expression, scope: &Scope) -> Result<()> {
    match e {
        Expression::Table(t) => {
            if let Some(body) = named(t, scope) {
                let alias = t.alias.clone().unwrap_or_else(|| t.name.clone());
                *e = sql::derived(Expression::Select(Box::new(body.clone())), alias);
            }
        }
        Expression::Subquery(s) => {
            if let Expression::Select(inner) = &mut s.this {
                expand_in(inner, scope)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// The body `t` names: an unqualified table with a CTE's name, nearest first.
fn named<'a>(t: &TableRef, scope: &'a Scope) -> Option<&'a Select> {
    if t.schema.is_some() || t.catalog.is_some() {
        return None;
    }
    let name = t.name.name.to_ascii_lowercase();
    scope
        .iter()
        .rev()
        .find(|(n, _)| *n == name)
        .map(|(_, body)| body)
}
