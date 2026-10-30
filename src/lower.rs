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

use std::collections::BTreeMap;

use polyglot_sql::expressions::Identifier;
use polyglot_sql::{get_columns, transform, DialectType, Expression};

use crate::graph::{Ctx, SemanticGraph, TERM};
use crate::plan::{Direction, LogicalPlan, Strategy, OUT_DIM};
use crate::sql::{self, Tpl};
use crate::Result;

pub(crate) fn lower(g: &SemanticGraph, p: &LogicalPlan, ctx: &mut Ctx) -> Result<Expression> {
    let n_terms = p.terms.len();
    let inline: Vec<usize> = (0..n_terms)
        .filter(|&i| p.terms[i].strategy == Strategy::Inline)
        .collect();
    let mut parts = vec![];
    // Dimension-only requests have no terms: one base part that just groups.
    if !inline.is_empty() || n_terms == 0 {
        parts.push((inline.clone(), Part::Base));
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
            Part::Base => base_part(g, p, computes, stacked, ctx)?,
            Part::Dedup { grain } => dedup_part(g, p, computes[0], *grain, stacked, ctx)?,
        });
    }

    let (from_alias, term_ref): (&str, fn(usize) -> String) = if stacked {
        ("stacked", |k| format!("MAX(stacked.t{k})"))
    } else {
        ("base", |k| format!("base.t{k}"))
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
            .map(|(name, dir)| {
                let a = t.alias(name);
                format!(
                    "{a} {}",
                    if *dir == Direction::Desc {
                        "DESC"
                    } else {
                        "ASC"
                    }
                )
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
    Base,
    Dedup { grain: usize },
}

/// `FROM root LEFT JOIN ... ON ...` plus `WHERE`, appended to `t`.
fn from_where(
    g: &SemanticGraph,
    p: &LogicalPlan,
    extra: Vec<Expression>,
    t: &mut Tpl,
    ctx: &mut Ctx,
) {
    let root = t.e(g.datasets[p.root].source.clone());
    t.push(&format!(" FROM {root}"));
    for &ri in &p.joins {
        let r = &g.rels[ri];
        let to = t.e(g.datasets[r.to].source.clone());
        let conds: Vec<String> = r
            .from_fields
            .iter()
            .zip(&r.to_fields)
            .map(|(&ff, &tf)| {
                let (a, b) = (g.field_sql(r.from, ff, ctx), g.field_sql(r.to, tf, ctx));
                format!("{} = {}", t.e(a), t.e(b))
            })
            .collect();
        t.push(&format!(" LEFT JOIN {to} ON {}", conds.join(" AND ")));
    }
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
    from_where(g, p, vec![], &mut t, ctx);
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
    inner.push(&format!("SELECT DISTINCT {}", cols.join(", ")));
    let not_null = {
        let mut t = Tpl::new();
        let x = t.e(pk[0].clone());
        t.push(&format!("{x} IS NOT NULL"));
        t.build()?
    };
    from_where(g, p, vec![not_null], &mut inner, ctx);
    let inner = inner.build()?;

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
