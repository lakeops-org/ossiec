//! Consumer joins applied to a planned query (relational_semantics §7.6).
//!
//! The planner builds each leg from the model's relationships, rooted at a
//! fact. These functions make the plan carry the joins the consumer wrote:
//! their kinds (mirrored when a step runs the other way), the `CROSS` and
//! fan-out joins the planner wouldn't add, and which legs narrow the others.

use super::{unsupported, ConsumerJoin};
use crate::graph::{self, SemanticGraph};
use crate::plan::{self, JoinKind, JoinStep, LogicalPlan, Strategy};
use crate::Result;

pub(super) fn apply_consumer_joins(
    graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    consumer: &[ConsumerJoin],
    ctx: &mut graph::Ctx,
) -> Result<()> {
    let mut added_reverse: Vec<usize> = vec![];
    for cj in consumer {
        if cj.kind == JoinKind::Cross {
            apply_cross_join(graph, logical, cj)?;
            continue;
        }
        let rel = cj.rel.expect("relationship join");
        let mut patched = false;
        for leg in &mut logical.legs {
            for step in &mut leg.joins {
                if step.rel == Some(rel) {
                    // The plan joins from its root, which can be the side the
                    // consumer wrote second: `customers LEFT JOIN orders` is planned
                    // as `orders -> customers`, and keeps every customer, not every
                    // order. Then LEFT and RIGHT trade places (§7.6, §8.1).
                    step.kind = if step.to == cj.other_ds {
                        cj.kind
                    } else {
                        cj.kind.mirrored()
                    };
                    patched = true;
                }
            }
        }
        if patched {
            continue;
        }
        let leg_i = consumer_join_leg(graph, logical, cj, rel)?;
        // ON must use the same aliases `tree_sql` uses for the leg (dataset
        // name for the root, `to_alias` for prior joins)—not the consumer SQL
        // alias (`o` vs `orders`).
        let from_alias =
            planned_alias_on_leg(graph, &logical.legs[leg_i], cj.attach_ds, cj.attach_role)
                .ok_or_else(|| {
                    unsupported(format!(
                        "Layer 2 join on `{}`: attach dataset `{}` has no planned alias on its leg",
                        graph.rels[rel].name, graph.datasets[cj.attach_ds].name
                    ))
                })?;
        let leg = &mut logical.legs[leg_i];
        leg.joins.push(JoinStep {
            rel: Some(rel),
            from_alias,
            to: cj.other_ds,
            to_alias: cj.other_alias.clone(),
            kind: cj.kind,
            reverse: cj.reverse,
        });
        if cj.reverse {
            added_reverse.push(leg_i);
        }
    }
    for leg_i in added_reverse {
        let root = logical.legs[leg_i].root;
        // G2 protects measures. A stage that only passes a measure through (or
        // lists dimensions) has nothing to duplicate; the join still filters.
        let dedup: Vec<usize> = logical
            .terms
            .iter()
            .enumerate()
            .filter(|(_, term)| {
                term.leg == leg_i && term.strategy == Strategy::Inline && !term.term.dup_insensitive
            })
            .map(|(i, _)| i)
            .collect();
        if dedup.is_empty() {
            continue;
        }
        if graph.datasets[root].primary_key.is_empty() {
            return Err(unsupported(format!(
                "Layer 2 fan-out join requires a primary_key on `{}` so measures stay duplicate-free (G2)",
                graph.datasets[root].name
            )));
        }
        for i in dedup {
            let term = &mut logical.terms[i];
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

/// `CROSS JOIN` has no relationship. One measure leg: either replace the planned
/// equijoin of that dataset with `CROSS JOIN`, or append one when the dataset
/// was not already required. Several legs have no single join tree to put a
/// Cartesian product on.
pub(super) fn apply_cross_join(
    graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    cj: &ConsumerJoin,
) -> Result<()> {
    if logical.legs.len() != 1 {
        return Err(unsupported(
            "Layer 2 CROSS JOIN cannot span independent facts (relational_semantics §7.6); a Cartesian product has no single measure leg to attach to",
        ));
    }
    let root = logical.legs[0].root;
    let on_leg = |ds: usize| planned_alias_on_leg(graph, &logical.legs[0], ds, None).is_some();
    if !on_leg(cj.attach_ds) && !on_leg(cj.other_ds) {
        return Err(unsupported(
            "Layer 2 CROSS JOIN cannot attach to the planned query; neither side is on the measure's join tree (relational_semantics §7.6)",
        ));
    }
    // The fact stays the root. Cross the other dataset onto it.
    let target = if cj.other_ds == root {
        cj.attach_ds
    } else {
        cj.other_ds
    };
    if target == root {
        return Err(unsupported(
            "Layer 2 CROSS JOIN needs two datasets; both sides resolved to the same fact",
        ));
    }
    let alias = if target == cj.other_ds {
        cj.other_alias.clone()
    } else {
        cj.attach_alias.clone()
    };
    let name = graph.datasets[target].name.clone();
    let hits: Vec<usize> = logical.legs[0]
        .joins
        .iter()
        .enumerate()
        .filter(|(_, s)| s.to == target)
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [i] => {
            let step = &mut logical.legs[0].joins[*i];
            if step.kind == JoinKind::Cross {
                return Err(unsupported(format!(
                    "Layer 2 duplicate CROSS JOIN of `{name}`"
                )));
            }
            step.kind = JoinKind::Cross;
        }
        [] => {
            logical.legs[0].joins.push(JoinStep {
                rel: None,
                from_alias: graph.datasets[root].name.clone(),
                to: target,
                to_alias: alias,
                kind: JoinKind::Cross,
                reverse: false,
            });
        }
        _ => {
            return Err(unsupported(format!(
                "Layer 2 CROSS JOIN of `{name}` is ambiguous; the planned query joins it more than once"
            )));
        }
    }
    Ok(())
}

/// Mark each leg whose own written joins can drop rows it shares with the other
/// legs: an `INNER` or `LEFT` relationship join the consumer wrote, or a derived
/// table joined other than `LEFT`/`FULL`, that the other legs don't carry too
/// (one they all carry restricts each of them already). Joins the planner added
/// to reach a dimension are not the consumer's and narrow nothing. `lower` then
/// keeps every other leg to the shared rows that survive this leg's tree,
/// matched on the datasets the consumer bound.
pub(super) fn mark_narrowing_legs(
    graph: &SemanticGraph,
    logical: &mut LogicalPlan,
    consumer: &[ConsumerJoin],
    include_unmatched: bool,
) -> Result<()> {
    if logical.legs.len() < 2 {
        return Ok(());
    }
    let written: Vec<usize> = consumer.iter().filter_map(|c| c.rel).collect();
    let mut marks = vec![];
    for (i, leg) in logical.legs.iter().enumerate() {
        let others = || {
            logical
                .legs
                .iter()
                .enumerate()
                .filter(move |(j, _)| *j != i)
        };
        let own_step = leg.joins.iter().any(|s| {
            s.rel.is_some_and(|r| written.contains(&r))
                && matches!(s.kind, JoinKind::Inner | JoinKind::Left)
                && !others().all(|(_, o)| {
                    o.joins
                        .iter()
                        .any(|x| x.rel == s.rel && x.to_alias == s.to_alias)
                })
        });
        let own_derived = leg.derived.iter().any(|d| {
            !matches!(d.kind, JoinKind::Left | JoinKind::Full)
                && !others().all(|(_, o)| o.derived.iter().any(|x| x.rel_alias == d.rel_alias))
        });
        if !(own_step || own_derived) {
            marks.push(None);
            continue;
        }
        let mut bound: Vec<String> = consumer
            .iter()
            .flat_map(|c| [(c.attach_ds, c.attach_role), (c.other_ds, None)])
            .filter_map(|(ds, role)| planned_alias_on_leg(graph, leg, ds, role))
            .collect();
        bound.sort();
        bound.dedup();
        marks.push(Some(bound));
    }
    for (leg, mark) in logical.legs.iter_mut().zip(marks) {
        leg.narrows_on = mark;
    }
    if include_unmatched && logical.legs.iter().any(|l| l.narrows_on.is_some()) {
        return Err(unsupported(
            "Layer 2 includeUnmatched with independent facts whose joins narrow each other is not implemented yet",
        ));
    }
    Ok(())
}

pub(super) fn consumer_join_leg(
    graph: &SemanticGraph,
    logical: &LogicalPlan,
    cj: &ConsumerJoin,
    rel: usize,
) -> Result<usize> {
    for (i, leg) in logical.legs.iter().enumerate() {
        if leg_hosts_attach(graph, leg, cj.attach_ds, cj.attach_role) {
            return Ok(i);
        }
    }
    Err(unsupported(format!(
        "Layer 2 join on relationship `{}` cannot attach to the planned query; no leg includes `{}` as `{}`",
        graph.rels[rel].name,
        graph.datasets[cj.attach_ds].name,
        cj.attach_alias
    )))
}

pub(super) fn leg_hosts_attach(
    graph: &SemanticGraph,
    leg: &plan::Leg,
    attach_ds: usize,
    attach_role: Option<usize>,
) -> bool {
    planned_alias_on_leg(graph, leg, attach_ds, attach_role).is_some()
}

/// Alias `lower` will use for `ds` on this leg (`tree_sql` / `leg_sql`).
///
/// `role` selects the join that plays that relationship (`dataset__relationship`,
/// or the consumer alias of a join that was added for it). Without a role, a
/// plain dataset alias wins; a single join to `ds` is used otherwise. Several
/// role aliases and no role is ambiguous — this returns `None` rather than the
/// first one.
pub(super) fn planned_alias_on_leg(
    graph: &SemanticGraph,
    leg: &plan::Leg,
    ds: usize,
    role: Option<usize>,
) -> Option<String> {
    if role.is_none() && leg.root == ds {
        return Some(graph.datasets[ds].name.clone());
    }
    let mut hits = Vec::new();
    for s in &leg.joins {
        let joined = if s.reverse {
            graph.rels[s.rel.expect("reverse relationship join")].from
        } else {
            s.to
        };
        if joined != ds {
            continue;
        }
        if let Some(r) = role {
            if s.rel == Some(r) {
                return Some(s.to_alias.clone());
            }
            continue;
        }
        hits.push(s.to_alias.clone());
    }
    if role.is_some() {
        return None;
    }
    let plain = graph.datasets[ds].name.as_str();
    if let Some(alias) = hits.iter().find(|a| a.as_str() == plain) {
        return Some(alias.clone());
    }
    if hits.len() == 1 {
        return Some(hits[0].clone());
    }
    None
}
