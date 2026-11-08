//! Keeping measures duplicate-free over joins that add rows (relational
//! semantics §7.6, G2). `RIGHT`/`FULL` add NULL-extended rows and `CROSS`
//! multiplies them, so an inline aggregate would see rows that are no
//! measure's source rows. Works on a planned query only.

use polyglot_sql::Expression;

use crate::graph::{self, SemanticGraph};
use crate::plan::{self, JoinKind, LogicalPlan, Strategy};
use crate::sql::Tpl;
use crate::{Error, Result};

fn unsupported(msg: impl Into<String>) -> Error {
    Error::Unsupported(msg.into())
}

/// RIGHT/FULL add null-extended rows and CROSS multiplies them. Inline
/// `COUNT(*)` / `SUM` would count those rows. De-duplicate the fact by its
/// primary key (the same path as fan-out) so each source row enters a group
/// once, while the base part still emits every group the join produced (§7.6).
pub(crate) fn protect_inflating_joins(
    graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    ctx: &mut graph::Ctx,
) -> Result<()> {
    for leg_i in 0..logical.legs.len() {
        let skipped = fact_row_present(graph, &logical.legs[leg_i], ctx)
            .is_some_and(|present| filter_inline_terms(logical, leg_i, &present));
        if skipped {
            continue;
        }
        let inflates = logical.legs[leg_i]
            .joins
            .iter()
            .any(|s| s.kind.inflates_inline())
            || logical.legs[leg_i]
                .derived
                .iter()
                .any(|d| d.kind.inflates_inline());
        if !inflates {
            continue;
        }
        let root = logical.legs[leg_i].root;
        if graph.datasets[root].primary_key.is_empty() {
            return Err(unsupported(format!(
                "Layer 2 RIGHT, FULL, or CROSS JOIN requires a primary_key on `{}` so measures stay duplicate-free (relational_semantics §7.6 G2)",
                graph.datasets[root].name
            )));
        }
        for term in &mut logical.terms {
            if term.leg != leg_i
                || term.strategy != Strategy::Inline
                || term.term.dup_insensitive
                || term.term.raw
            {
                continue;
            }
            ctx.warn(format!(
                "FAN_TRAP_RISK: `{}` is computed over `{}` rows de-duplicated by primary key; a `{}` row that belongs to several groups counts in each, so groups do not add up to a grand total",
                term.term.key,
                graph.datasets[root].name,
                graph.datasets[root].name
            ));
            term.strategy = Strategy::PkDedup { grain: root };
        }
    }
    Ok(())
}

/// The predicate that holds on exactly the leg's rows that carry a fact row, when
/// every join that adds rows is a `RIGHT JOIN` along a many-to-one relationship,
/// as `customers LEFT JOIN orders` is planned (`orders RIGHT JOIN customers`).
/// Such a join repeats no left-side row; it only adds a row for each right-side
/// row nothing matched, with everything to its left NULL. Those rows keep their
/// groups but derive from no fact row (§8.1). Every matched row has the join's
/// left-side key and every added row lacks it, at any depth of the chain.
///
/// `None` when a join could add rows some other way: a `FULL` join's unmatched
/// fact rows have a NULL key and do count, a `RIGHT` join to the many side fans
/// rows out, and `CROSS` multiplies them.
fn fact_row_present(
    graph: &SemanticGraph,
    leg: &plan::Leg,
    ctx: &mut graph::Ctx,
) -> Option<Expression> {
    if leg.derived.iter().any(|d| d.kind.inflates_inline()) {
        return None;
    }
    let mut keys = vec![];
    for s in leg.joins.iter().filter(|s| s.kind.inflates_inline()) {
        let r = s.rel.filter(|_| s.kind == JoinKind::Right && !s.reverse)?;
        let rel = &graph.rels[r];
        for &f in &rel.from_fields {
            keys.push(graph.field_sql_as(rel.from, f, &s.from_alias, ctx));
        }
    }
    if keys.is_empty() {
        return None;
    }
    let mut t = Tpl::new();
    let parts: Vec<String> = keys
        .into_iter()
        .map(|k| format!("{} IS NOT NULL", t.e(k)))
        .collect();
    t.push(&parts.join(" AND "));
    t.build().ok()
}

/// Add `present` to the `FILTER` of every inline term on the leg, so none needs
/// de-duplicating. All or nothing: `false`, and no term changed, when one of them
/// is not an aggregate that takes a filter.
fn filter_inline_terms(logical: &mut LogicalPlan, leg_i: usize, present: &Expression) -> bool {
    let mut filtered = vec![];
    for (i, term) in logical.terms.iter().enumerate() {
        if term.leg != leg_i || term.strategy != Strategy::Inline || term.term.raw {
            continue;
        }
        match crate::sql::with_aggregate_filter(term.term.ast.clone(), present.clone()) {
            Some(ast) => filtered.push((i, ast)),
            None => return false,
        }
    }
    for (i, ast) in filtered {
        logical.terms[i].term.ast = ast;
    }
    true
}
