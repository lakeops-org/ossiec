//! Lowering: turn a [`LogicalPlan`] into one SQL statement (as a polyglot AST).
//!
//! Shape, with one *part* per evaluation strategy:
//!
//! ```text
//! one part:      SELECT <dims>, <metrics over base.tN> FROM (<part>) AS base [WHERE <metric filters>]
//! several parts: SELECT <dims>, <metrics over MAX(stacked.tN)>
//!                FROM (<part> UNION ALL <part> ...) AS stacked GROUP BY <dims> [HAVING ...]
//! ```
//!
//! Every part groups by the same dimensions; a part projects `NULL` for the
//! terms it doesn't compute, so `MAX` per group picks the one real value and
//! parts never join each other.

use std::collections::{BTreeMap, BTreeSet};

use polyglot_sql::expressions::Identifier;
use polyglot_sql::{get_columns, transform, DialectType, Expression};

use crate::graph::{Ctx, SemanticGraph, TERM};
use crate::plan::{
    DerivedJoin, Direction, JoinKind, JoinStep, Leg, LogicalPlan, Nulls, Strategy, OUT_DIM,
};
use crate::sql::{self, Tpl};
use crate::Result;

pub(crate) fn lower(g: &SemanticGraph, p: &LogicalPlan, ctx: &mut Ctx) -> Result<Expression> {
    let n_terms = p.terms.len();
    let inline: Vec<usize> = (0..n_terms)
        .filter(|&i| p.terms[i].strategy == Strategy::Inline)
        .collect();
    let mut parts = vec![];
    for leg in 0..p.legs.len() {
        // A leg's base part always runs when there are dimensions, even with no
        // inline terms: the groups come from every joined row, so a group keeps
        // existing when one measure has no rows for it (spec §8.4, closure).
        // Dimension-only requests are just this part. With several legs
        // (independent facts), the groups are the union across facts.
        let here: Vec<usize> = inline
            .iter()
            .copied()
            .filter(|&i| p.terms[i].leg == leg)
            .collect();
        let leg_terms = p.terms.iter().any(|t| t.leg == leg);
        if !here.is_empty() || n_terms == 0 || (!p.dims.is_empty() && leg_terms) {
            parts.push((here, Part::Base { leg }));
        }
    }
    for (i, t) in p.terms.iter().enumerate() {
        if let Strategy::PkDedup { grain } = t.strategy {
            parts.push((vec![i], Part::Dedup { grain }));
        }
    }
    let stacked = parts.len() > 1;
    let mut queries = vec![];
    for (computes, kind) in &parts {
        queries.push(match kind {
            Part::Base { leg } => base_part(g, p, &p.legs[*leg], computes, stacked, ctx)?,
            Part::Dedup { grain } => dedup_part(g, p, computes[0], *grain, stacked, ctx)?,
        });
    }

    let from_alias = if stacked { "stacked" } else { "base" };
    // A part that lacks a group projects NULL there; for COUNT-like terms the
    // answer over no rows is 0.
    let term_ref = |k: usize| match (stacked, p.terms[k].term.zero_if_empty) {
        (true, true) => format!("COALESCE(MAX(stacked.t{k}), 0)"),
        (true, false) => format!("MAX(stacked.t{k})"),
        (false, _) => format!("base.t{k}"),
    };
    let from = if stacked {
        sql::union_all(queries)?
    } else {
        queries.pop().expect("one part")
    };
    let over_terms = |e: &Expression| -> Expression {
        transform(e.clone(), &|e| {
            Ok(Some(match e {
                Expression::Column(c) if c.table.is_none() && c.name.name.starts_with(TERM) => {
                    let k: usize = c.name.name[TERM.len()..].parse().expect("term placeholder");
                    sql::col(&term_ref(k))
                }
                Expression::Column(c) if c.table.is_none() && c.name.name.starts_with(OUT_DIM) => {
                    let i: usize = c.name.name[OUT_DIM.len()..]
                        .parse()
                        .expect("dimension placeholder");
                    sql::col(&format!("{from_alias}.d{i}"))
                }
                other => other,
            }))
        })
        .expect("placeholder substitution cannot fail")
    };

    let mut t = Tpl::new();
    let mut select = vec![];
    for (i, (name, _)) in p.dims.iter().enumerate() {
        let a = t.alias(name);
        select.push(format!("{from_alias}.d{i} AS {a}"));
    }
    for (name, outer) in &p.outputs {
        let e = t.e(over_terms(outer));
        let a = t.alias(name);
        select.push(format!("{e} AS {a}"));
    }
    let src = t.e(sql::derived(from, Identifier::new(from_alias)));
    t.push(&format!("SELECT {} FROM {src}", select.join(", ")));
    if stacked && !p.dims.is_empty() {
        let keys: Vec<String> = (0..p.dims.len()).map(|i| format!("stacked.d{i}")).collect();
        t.push(&format!(" GROUP BY {}", keys.join(", ")));
    }
    // Windows are computed after WHERE/HAVING, so with windows present metric
    // filters move to a wrapper and see the finished rows (e.g. `rank <= 3`).
    let wrap = p.windowed && !p.metric_filters.is_empty();
    if !p.metric_filters.is_empty() && !wrap {
        let conds: Vec<String> = p
            .metric_filters
            .iter()
            .map(|(f, _)| t.e(over_terms(f)))
            .collect();
        let kw = if stacked { "HAVING" } else { "WHERE" };
        t.push(&format!(" {kw} {}", conds.join(" AND ")));
    }
    if wrap {
        let inner = t.build()?;
        t = Tpl::new();
        let src = t.e(sql::derived(inner, Identifier::new("windowed")));
        let conds: Vec<String> = p
            .metric_filters
            .iter()
            .map(|(_, f)| t.e(f.clone()))
            .collect();
        t.push(&format!(
            "SELECT * FROM {src} WHERE {}",
            conds.join(" AND ")
        ));
    }
    if !p.order.is_empty() {
        let keys: Vec<String> = p
            .order
            .iter()
            .map(|(name, dir, nulls)| {
                let a = t.alias(name);
                let dir = if *dir == Direction::Desc {
                    "DESC"
                } else {
                    "ASC"
                };
                let nulls = match nulls {
                    Some(Nulls::First) => " NULLS FIRST",
                    Some(Nulls::Last) => " NULLS LAST",
                    None => "",
                };
                format!("{a} {dir}{nulls}")
            })
            .collect();
        t.push(&format!(" ORDER BY {}", keys.join(", ")));
    }
    if let Some(n) = p.limit {
        t.push(&format!(" LIMIT {n}"));
    }
    t.build()
}

enum Part {
    Base { leg: usize },
    Dedup { grain: usize },
}

/// `FROM <start> <kind> JOIN ... ON ...` for `steps` then the leg's derived
/// tables, with every alias passed through `rename`. Returns the SQL text (with
/// placeholders) to append to `t`.
fn tree_sql(
    g: &SemanticGraph,
    start: (usize, &str),
    steps: &[&JoinStep],
    derived: &[DerivedJoin],
    rename: &dyn Fn(&str) -> String,
    t: &mut Tpl,
    ctx: &mut Ctx,
) -> String {
    let mut out = format!(" FROM {}", t.e(g.source_as(start.0, &rename(start.1))));
    for step in steps {
        if step.kind == JoinKind::Cross {
            let to = t.e(g.source_as(step.to, &rename(&step.to_alias)));
            out.push_str(&format!(" CROSS JOIN {to}"));
            continue;
        }
        let r = &g.rels[step.rel.expect("relationship join")];
        let joined_ds = if step.reverse { r.from } else { step.to };
        let to = t.e(g.source_as(joined_ds, &rename(&step.to_alias)));
        let conds: Vec<String> = if step.reverse {
            r.from_fields
                .iter()
                .zip(&r.to_fields)
                .map(|(&ff, &tf)| {
                    let a = g.field_sql_as(r.to, tf, &rename(&step.from_alias), ctx);
                    let b = g.field_sql_as(r.from, ff, &rename(&step.to_alias), ctx);
                    format!("{} = {}", t.e(a), t.e(b))
                })
                .collect()
        } else {
            r.from_fields
                .iter()
                .zip(&r.to_fields)
                .map(|(&ff, &tf)| {
                    let a = g.field_sql_as(r.from, ff, &rename(&step.from_alias), ctx);
                    let b = g.field_sql_as(step.to, tf, &rename(&step.to_alias), ctx);
                    format!("{} = {}", t.e(a), t.e(b))
                })
                .collect()
        };
        out.push_str(&format!(
            " {} {to} ON {}",
            step.kind.sql(),
            conds.join(" AND ")
        ));
    }
    // Derived tables (Layer 2 A.2) come last: their ON reads attach-side
    // fields, which relationship joins may have just introduced. Every part
    // (base, stacked, de-duplicated) builds its FROM here, so all get the
    // same restriction.
    for d in derived {
        let rel = t.e(sql::placeholder_relation(&d.rel_placeholder, &d.rel_alias));
        let conds: Vec<String> = d
            .from_fields
            .iter()
            .zip(&d.rel_cols)
            .map(|(&(ds, f), col)| {
                let a = t.e(g.field_sql_as(ds, f, &rename(&d.from_alias), ctx));
                let b = t.e(sql::qualified_quoted_col(&d.rel_alias, col));
                if d.null_safe {
                    // Written out (not `IS NOT DISTINCT FROM`): this stage is
                    // lowered before its target dialect is consulted.
                    format!("({a} = {b} OR ({a} IS NULL AND {b} IS NULL))")
                } else {
                    format!("{a} = {b}")
                }
            })
            .collect();
        if d.kind == JoinKind::Cross {
            out.push_str(&format!(" CROSS JOIN {rel}"));
            continue;
        }
        out.push_str(&format!(
            " {} {rel} ON {}",
            d.kind.sql(),
            conds.join(" AND ")
        ));
    }
    out
}

/// A leg's whole join tree, as `FROM ...`.
fn leg_sql(g: &SemanticGraph, leg: &Leg, t: &mut Tpl, ctx: &mut Ctx) -> String {
    let steps: Vec<&JoinStep> = leg.joins.iter().collect();
    tree_sql(
        g,
        (leg.root, &g.datasets[leg.root].name),
        &steps,
        &leg.derived,
        &|a| a.to_string(),
        t,
        ctx,
    )
}

/// `FROM <leg> WHERE <filters and extra>`, appended to `t`.
fn from_where(
    g: &SemanticGraph,
    p: &LogicalPlan,
    leg: &Leg,
    extra: Vec<Expression>,
    t: &mut Tpl,
    ctx: &mut Ctx,
) {
    let from = leg_sql(g, leg, t, ctx);
    t.push(&from);
    let preds: Vec<String> = p
        .filters
        .iter()
        .cloned()
        .chain(extra)
        .map(|f| t.e(f))
        .collect();
    if !preds.is_empty() {
        t.push(&format!(" WHERE {}", preds.join(" AND ")));
    }
}

fn group_by(p: &LogicalPlan, t: &mut Tpl) {
    if !p.dims.is_empty() {
        let keys: Vec<String> = p.dims.iter().map(|(_, e)| t.e(e.clone())).collect();
        t.push(&format!(" GROUP BY {}", keys.join(", ")));
    }
}

/// Inline terms over the joined rows, grouped by the dimensions.
fn base_part(
    g: &SemanticGraph,
    p: &LogicalPlan,
    leg: &Leg,
    computes: &[usize],
    stacked: bool,
    ctx: &mut Ctx,
) -> Result<Expression> {
    let mut t = Tpl::new();
    let mut cols = vec![];
    for (i, (_, e)) in p.dims.iter().enumerate() {
        cols.push(format!("{} AS d{i}", t.e(e.clone())));
    }
    for k in 0..p.terms.len() {
        if computes.contains(&k) {
            cols.push(format!("{} AS t{k}", t.e(p.terms[k].term.ast.clone())));
        } else if stacked {
            cols.push(format!("NULL AS t{k}"));
        }
    }
    t.push(&format!("SELECT {}", cols.join(", ")));
    from_where(g, p, leg, vec![], &mut t, ctx);
    group_by(p, &mut t);
    t.build()
}

/// One term over rows de-duplicated on its dataset's primary key (spec §9 strategy):
///
/// ```text
/// SELECT dedup.d0, AGG(dedup.c0) AS tK FROM (
///   SELECT DISTINCT <dims> AS dN, <primary key> AS kN, <columns the term reads> AS cN
///   FROM <joins> WHERE <filters> AND <primary key> IS NOT NULL
/// ) AS dedup GROUP BY dedup.d0
/// ```
fn dedup_part(
    g: &SemanticGraph,
    p: &LogicalPlan,
    k: usize,
    grain: usize,
    stacked: bool,
    ctx: &mut Ctx,
) -> Result<Expression> {
    let term = &p.terms[k].term.ast;
    let leg = &p.legs[p.terms[k].leg];
    let mut inputs: BTreeMap<String, (usize, Expression)> = BTreeMap::new();
    for c in get_columns(term) {
        let key = polyglot_sql::generate(c, DialectType::Generic)
            .map_err(|e| crate::Error::Internal(e.to_string()))?;
        let n = inputs.len();
        inputs.entry(key).or_insert((n, c.clone()));
    }

    let mut inner = Tpl::new();
    let mut cols = vec![];
    for (i, (_, e)) in p.dims.iter().enumerate() {
        cols.push(format!("{} AS d{i}", inner.e(e.clone())));
    }
    let pk: Vec<Expression> = g.datasets[grain]
        .primary_key
        .iter()
        .map(|&f| g.field_sql(grain, f, ctx))
        .collect();
    for (i, e) in pk.iter().enumerate() {
        cols.push(format!("{} AS k{i}", inner.e(e.clone())));
    }
    let mut by_index: Vec<_> = inputs.values().collect();
    by_index.sort_by_key(|(i, _)| *i);
    for (i, e) in by_index {
        cols.push(format!("{} AS c{i}", inner.e(e.clone())));
    }
    // With unmatched rows the DISTINCT goes over the union instead (below).
    let distinct = if p.include_unmatched { "" } else { "DISTINCT " };
    inner.push(&format!("SELECT {distinct}{}", cols.join(", ")));
    let not_null = {
        let mut t = Tpl::new();
        let x = t.e(pk[0].clone());
        t.push(&format!("{x} IS NOT NULL"));
        t.build()?
    };
    from_where(g, p, leg, vec![not_null], &mut inner, ctx);
    let mut inner = inner.build()?;
    if p.include_unmatched {
        let mut ordered: Vec<_> = inputs.values().collect();
        ordered.sort_by_key(|(i, _)| *i);
        let columns: Vec<Expression> = ordered.into_iter().map(|(_, e)| e.clone()).collect();
        let unmatched = unmatched_rows(g, p, leg, grain, &pk, &columns, ctx)?;
        // One DISTINCT over both branches, not one per branch: ClickHouse's
        // analyzer prunes a branch's DISTINCT columns to the ones the outer
        // query reads, dropping the key, so equal prices collapse
        // (engine test: unmatched_items_included).
        let mut t = Tpl::new();
        let u = t.e(sql::derived(
            sql::union_all(vec![inner, unmatched])?,
            Identifier::new("rows"),
        ));
        t.push(&format!("SELECT DISTINCT * FROM {u}"));
        inner = t.build()?;
    }

    let rewritten = transform(term.clone(), &|e| {
        Ok(Some(match e {
            Expression::Column(c) => {
                let key =
                    polyglot_sql::generate(&Expression::Column(c.clone()), DialectType::Generic)?;
                match inputs.get(&key) {
                    Some((i, _)) => sql::col(&format!("dedup.c{i}")),
                    None => Expression::Column(c),
                }
            }
            other => other,
        }))
    })
    .map_err(|e| crate::Error::Internal(e.to_string()))?;

    let mut t = Tpl::new();
    let mut cols = vec![];
    for i in 0..p.dims.len() {
        cols.push(format!("dedup.d{i} AS d{i}"));
    }
    for j in 0..p.terms.len() {
        if j == k {
            cols.push(format!("{} AS t{j}", t.e(rewritten.clone())));
        } else if stacked {
            cols.push(format!("NULL AS t{j}"));
        }
    }
    let src = t.e(sql::derived(inner, Identifier::new("dedup")));
    t.push(&format!("SELECT {} FROM {src}", cols.join(", ")));
    if !p.dims.is_empty() {
        let keys: Vec<String> = (0..p.dims.len()).map(|i| format!("dedup.d{i}")).collect();
        t.push(&format!(" GROUP BY {}", keys.join(", ")));
    }
    t.build()
}

/// Rows of `grain` (with what they join to) that no row of the leg reaches:
/// products never sold, customers who never ordered. The leg's other columns
/// are NULL there, so they form their own group; filters on those columns
/// exclude them, as SQL NULL semantics say. Same columns as the matched rows:
///
/// ```text
/// SELECT <dims, fact columns NULL>, <primary key>, <inputs>
/// FROM <grain> LEFT JOIN <what it joins to>          -- aliases `…__unmatched`
/// WHERE <filters> AND NOT EXISTS (SELECT 1 FROM <leg> WHERE <same primary key>)
/// ```
fn unmatched_rows(
    g: &SemanticGraph,
    p: &LogicalPlan,
    leg: &Leg,
    grain: usize,
    pk: &[Expression],
    inputs: &[Expression],
    ctx: &mut Ctx,
) -> Result<Expression> {
    // The grain and everything the leg reaches through it.
    let mut subtree: BTreeSet<String> = BTreeSet::from([g.datasets[grain].name.clone()]);
    let mut steps = vec![];
    for s in &leg.joins {
        if subtree.contains(&s.from_alias) {
            subtree.insert(s.to_alias.clone());
            steps.push(s);
        }
    }
    let rename = |a: &str| format!("{a}__unmatched");
    // Re-point columns at the unmatched aliases; columns of the fact side are NULL.
    let retarget = |e: &Expression| -> Expression {
        transform(e.clone(), &|e| {
            Ok(Some(match e {
                Expression::Column(mut c) => match c.table.as_ref().map(|t| t.name.clone()) {
                    Some(t) if subtree.contains(&t) => {
                        c.table = Some(Identifier::quoted(rename(&t)));
                        Expression::Column(c)
                    }
                    Some(_) => Expression::Null(polyglot_sql::expressions::Null),
                    None => Expression::Column(c),
                },
                other => other,
            }))
        })
        .expect("retargeting cannot fail")
    };

    let mut t = Tpl::new();
    let mut cols = vec![];
    for (i, (_, e)) in p.dims.iter().enumerate() {
        // A dimension wholly on the fact side is NULL here. Give the NULL its
        // type: Postgres can type a bare NULL as text, which then
        // won't combine with the matched rows' column (engine test:
        // unmatched_items_included, "UNION types bigint and text").
        let fact_side = get_columns(e).iter().all(|c| {
            !matches!(c, Expression::Column(c) if c.table.as_ref().is_some_and(|t| subtree.contains(&t.name)))
        });
        let value = match (fact_side, p.dim_types[i]) {
            (true, Some(ty)) => {
                polyglot_sql::builder::cast(polyglot_sql::builder::null(), ty).into_inner()
            }
            _ => retarget(e),
        };
        cols.push(format!("{} AS d{i}", t.e(value)));
    }
    for (i, e) in pk.iter().enumerate() {
        cols.push(format!("{} AS k{i}", t.e(retarget(e))));
    }
    for (i, e) in inputs.iter().enumerate() {
        cols.push(format!("{} AS c{i}", t.e(retarget(e))));
    }
    let from = tree_sql(
        g,
        (grain, &g.datasets[grain].name),
        &steps,
        &[],
        &rename,
        &mut t,
        ctx,
    );
    t.push(&format!("SELECT {}{from}", cols.join(", ")));
    let mut preds: Vec<String> = p.filters.iter().map(|f| t.e(retarget(f))).collect();
    let reached = leg_sql(g, leg, &mut t, ctx);
    let same_key: Vec<String> = pk
        .iter()
        .map(|e| format!("{} = {}", t.e(e.clone()), t.e(retarget(e))))
        .collect();
    preds.push(format!(
        "NOT EXISTS (SELECT 1{reached} WHERE {})",
        same_key.join(" AND ")
    ));
    t.push(&format!(" WHERE {}", preds.join(" AND ")));
    t.build()
}
