//! `FROM` and `JOIN`: tables, derived tables, and matching each `ON` to a model
//! relationship (§7.6).

use polyglot_sql::expressions::{Column, Join, JoinKind as SqlJoinKind, Select, TableRef};
use polyglot_sql::Expression;

use crate::graph::{self, SemanticGraph};
use crate::plan::JoinKind;
use crate::{Error, Result};

use super::{
    as_column, column_role, resolve_column_dataset, resolve_field_ref, unsupported, ConsumerJoin,
    DerivedSpec, FieldRef, QueryScope, TableBinding,
};

pub(super) struct ParsedFrom {
    pub(super) scope: QueryScope,
    pub(super) consumer_joins: Vec<ConsumerJoin>,
    pub(super) derived: Vec<DerivedSpec>,
}

/// `(SELECT ...) alias` as a `FROM` / `JOIN` item: the inner `SELECT` and its alias.
pub(super) fn as_derived(e: &Expression) -> Result<Option<(&Select, String)>> {
    let (sub, outer_alias) = match e {
        Expression::Subquery(s) => (s.as_ref(), None),
        Expression::Alias(a) => match &a.this {
            Expression::Subquery(s) => (s.as_ref(), Some(a.alias.name.clone())),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let alias = sub
        .alias
        .as_ref()
        .map(|a| a.name.clone())
        .or(outer_alias)
        .ok_or_else(|| Error::Request("Layer 2 derived table needs an alias".into()))?;
    if sub.lateral {
        return Err(unsupported(
            "Layer 2 correlated (LATERAL) derived tables are not supported",
        ));
    }
    if !sub.column_aliases.is_empty()
        || sub.order_by.is_some()
        || sub.limit.is_some()
        || sub.offset.is_some()
    {
        return Err(unsupported(
            "Layer 2 derived table: name output columns with AS in its select list, and put ORDER BY / LIMIT inside the subquery's SELECT",
        ));
    }
    match &sub.this {
        Expression::Select(s) => Ok(Some((s.as_ref(), alias))),
        _ => Err(unsupported(
            "Layer 2 derived table must be a plain SELECT (set operators and nested parentheses are not supported)",
        )),
    }
}

pub(super) fn parse_from_clause(
    graph: &SemanticGraph,
    from: Option<&polyglot_sql::expressions::From>,
    joins: &[Join],
    outer: &[String],
) -> Result<ParsedFrom> {
    let from = from.ok_or_else(|| Error::Request("Layer 2 query needs a FROM clause".into()))?;
    if from.expressions.is_empty() {
        return Err(Error::Request("Layer 2 query needs a FROM clause".into()));
    }
    let mut scope = QueryScope {
        bindings: vec![],
        derived: vec![],
        derived_outs: vec![],
        outer: outer.to_vec(),
    };
    let mut derived_specs: Vec<DerivedSpec> = Vec::new();
    let joins = comma_items_as_cross_joins(&from.expressions[1..], joins)?;
    let joins = joins.as_slice();
    for e in &from.expressions[..1] {
        let (dataset, alias) = table_expr(graph, e)?;
        let alias = alias.unwrap_or_else(|| graph.datasets[dataset].name.clone());
        if scope
            .bindings
            .iter()
            .any(|b| b.alias.eq_ignore_ascii_case(&alias))
        {
            return Err(Error::Request(format!(
                "Layer 2 duplicate relation alias `{alias}` in FROM"
            )));
        }
        scope.bindings.push(TableBinding { dataset, alias });
    }
    let mut consumer_joins = Vec::new();
    for j in joins {
        if let Some((select, alias)) = as_derived(&j.this)? {
            let kind = consumer_join_kind(j)?;
            if kind == JoinKind::Cross {
                if j.on.is_some() {
                    return Err(cross_join_on());
                }
                if !j.using.is_empty() {
                    return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
                }
                if scope.bindings.is_empty() {
                    return Err(unsupported(
                        "Layer 2 CROSS JOIN of a derived table needs a model dataset in FROM",
                    ));
                }
                let taken = scope
                    .bindings
                    .iter()
                    .map(|b| &b.alias)
                    .chain(&scope.derived)
                    .any(|a| a.eq_ignore_ascii_case(&alias));
                if taken {
                    return Err(Error::Request(format!(
                        "Layer 2 derived table alias `{alias}` collides with another relation in FROM/JOIN"
                    )));
                }
                let attach_ds = scope.bindings[0].dataset;
                scope.derived.push(alias.clone());
                derived_specs.push(DerivedSpec {
                    alias,
                    select: select.clone(),
                    kind,
                    attach_ds,
                    attach_role: None,
                    from_fields: vec![],
                    cols: vec![],
                    null_safe: false,
                });
                continue;
            }
            let spec = parse_derived_join(graph, &scope, j, select, alias)?;
            scope.derived.push(spec.alias.clone());
            derived_specs.push(spec);
            continue;
        }
        let kind = consumer_join_kind(j)?;
        if !j.using.is_empty() {
            return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
        }
        let (right_ds, right_alias) = table_expr(graph, &j.this)?;
        let right_binding = TableBinding {
            dataset: right_ds,
            alias: right_alias.unwrap_or_else(|| graph.datasets[right_ds].name.clone()),
        };
        if kind == JoinKind::Cross {
            if j.on.is_some() {
                return Err(cross_join_on());
            }
            if scope.bindings.is_empty() {
                return Err(unsupported(
                    "Layer 2 CROSS JOIN needs a dataset already in FROM",
                ));
            }
            if scope.bindings.iter().any(|b| b.dataset == right_ds) {
                return Err(unsupported(format!(
                    "Layer 2 CROSS JOIN target `{}` is already in FROM",
                    graph.datasets[right_ds].name
                )));
            }
            let attach = &scope.bindings[0];
            consumer_joins.push(ConsumerJoin {
                rel: None,
                kind,
                reverse: false,
                attach_ds: attach.dataset,
                attach_alias: attach.alias.clone(),
                other_ds: right_ds,
                other_alias: right_binding.alias.clone(),
                attach_role: None,
            });
            scope.bindings.push(right_binding);
            continue;
        }
        let on = j.on.as_ref().ok_or_else(|| {
            unsupported("Layer 2 JOIN requires an ON condition matching a model relationship")
        })?;
        let pairs = on_equality_pairs(on)?;
        let join_scope = QueryScope {
            bindings: scope
                .bindings
                .iter()
                .chain(std::iter::once(&right_binding))
                .cloned()
                .collect(),
            derived: scope.derived.clone(),
            derived_outs: vec![],
            outer: scope.outer.clone(),
        };
        let rel = match_relationship(graph, &join_scope, &pairs)?;
        if consumer_joins
            .iter()
            .any(|c: &ConsumerJoin| c.rel == Some(rel))
        {
            return Err(unsupported(format!(
                "Layer 2 duplicate join on relationship `{}`",
                graph.rels[rel].name
            )));
        }
        let rel_info = &graph.rels[rel];
        let from_in_scope = scope.bindings.iter().any(|b| b.dataset == rel_info.from);
        let to_in_scope = scope.bindings.iter().any(|b| b.dataset == rel_info.to);
        let forward = from_in_scope && right_ds == rel_info.to;
        let reverse = to_in_scope && right_ds == rel_info.from;
        if forward == reverse {
            return Err(unsupported(format!(
                "Layer 2 JOIN on `{}` is ambiguous; one side must already be in FROM/JOIN",
                rel_info.name
            )));
        }
        let (attach_ds, other_ds) = if reverse {
            (rel_info.to, rel_info.from)
        } else {
            (rel_info.from, rel_info.to)
        };
        let attach_alias = scope
            .bindings
            .iter()
            .find(|b| b.dataset == attach_ds)
            .map(|b| b.alias.clone())
            .ok_or_else(|| {
                Error::Request(format!(
                    "Layer 2 JOIN on `{}`: attach dataset `{}` must already be in scope",
                    rel_info.name, graph.datasets[attach_ds].name
                ))
            })?;
        let attach_role = on_attach_role(graph, &join_scope, &pairs, attach_ds)?;
        consumer_joins.push(ConsumerJoin {
            rel: Some(rel),
            kind,
            reverse,
            attach_ds,
            attach_alias,
            other_ds,
            other_alias: right_binding.alias.clone(),
            attach_role,
        });
        scope.bindings.push(right_binding);
    }
    Ok(ParsedFrom {
        scope,
        consumer_joins,
        derived: derived_specs,
    })
}

/// `JOIN (SELECT ...) t ON dataset.col = t.col AND ...` in an `AGG` stage.
pub(super) fn parse_derived_join(
    graph: &SemanticGraph,
    scope: &QueryScope,
    j: &Join,
    select: &Select,
    alias: String,
) -> Result<DerivedSpec> {
    let kind = consumer_join_kind(j)?;
    if !j.using.is_empty() {
        return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
    }
    let on = j.on.as_ref().ok_or_else(|| {
        unsupported("Layer 2 derived-table JOIN requires an ON of dataset-column = derived-column")
    })?;
    let taken = scope
        .bindings
        .iter()
        .map(|b| &b.alias)
        .chain(&scope.derived)
        .any(|a| a.eq_ignore_ascii_case(&alias));
    if taken {
        return Err(Error::Request(format!(
            "Layer 2 derived table alias `{alias}` collides with another relation in FROM/JOIN"
        )));
    }
    let on_err = || {
        unsupported(format!(
            "Layer 2 derived-table JOIN ON must be AND of `<dataset column> = {alias}.<column>` (or IS NOT DISTINCT FROM)"
        ))
    };
    let is_derived_col = |c: &Column| {
        c.table
            .as_ref()
            .is_some_and(|t| t.name.eq_ignore_ascii_case(&alias))
    };
    let mut attach_ds: Option<usize> = None;
    let mut attach_role: Option<Option<usize>> = None;
    let mut from_fields = vec![];
    let mut cols = vec![];
    let mut null_safe: Option<bool> = None;
    for (l, r, ns) in on_pairs_null_safe(on)? {
        let (dcol, col) = match (is_derived_col(&l), is_derived_col(&r)) {
            (true, false) => (l, r),
            (false, true) => (r, l),
            _ => return Err(on_err()),
        };
        let (ds, field) = resolve_field_ref(graph, scope, &col)?;
        if *attach_ds.get_or_insert(ds) != ds {
            return Err(unsupported(
                "Layer 2 derived-table JOIN ON must attach to one dataset",
            ));
        }
        let role = column_role(graph, &col, ds)?;
        match attach_role {
            None => attach_role = Some(role),
            Some(prev) if prev != role => {
                return Err(unsupported(format!(
                    "Layer 2 derived-table JOIN ON must attach to one role of `{}`",
                    graph.datasets[ds].name
                )));
            }
            _ => {}
        }
        if *null_safe.get_or_insert(ns) != ns {
            return Err(unsupported(
                "Layer 2 derived-table JOIN ON cannot mix = and IS NOT DISTINCT FROM",
            ));
        }
        let f = graph.datasets[ds]
            .fields
            .iter()
            .position(|f| f.name == field)
            .expect("resolved field exists");
        from_fields.push((ds, f));
        cols.push(dcol.name.name.clone());
    }
    let attach_ds = attach_ds.ok_or_else(on_err)?;
    Ok(DerivedSpec {
        alias,
        select: select.clone(),
        kind,
        attach_ds,
        from_fields,
        cols,
        null_safe: null_safe.unwrap_or(false),
        attach_role: attach_role.flatten(),
    })
}

/// ON equalities, each flagged null-safe (`IS NOT DISTINCT FROM`) or plain `=`.
pub(super) fn on_pairs_null_safe(on: &Expression) -> Result<Vec<(Column, Column, bool)>> {
    let pair = |l: &Expression, r: &Expression, ns: bool| -> Result<Vec<(Column, Column, bool)>> {
        let l = as_column(l).ok_or_else(on_must_be_equalities)?;
        let r = as_column(r).ok_or_else(on_must_be_equalities)?;
        Ok(vec![(l.to_owned(), r.to_owned(), ns)])
    };
    match on {
        Expression::Eq(b) => pair(&b.left, &b.right, false),
        Expression::NullSafeEq(b) => pair(&b.left, &b.right, true),
        Expression::And(a) => {
            let mut out = on_pairs_null_safe(&a.left)?;
            out.extend(on_pairs_null_safe(&a.right)?);
            Ok(out)
        }
        _ => Err(on_must_be_equalities()),
    }
}

/// SQL reads `FROM a, b` as `a CROSS JOIN b` (§7.6, §10.2), and so does ossiec:
/// each item after the first becomes a `CROSS JOIN`, ahead of the written joins.
/// A comma binds looser than `JOIN`, so `FROM a, b JOIN c` is `a × (b JOIN c)`.
/// Flattening keeps that meaning for inner, left and cross joins, but not for a
/// `RIGHT` or `FULL` join, whose padded rows would pair with `a` differently;
/// those are refused.
fn comma_items_as_cross_joins(items: &[Expression], joins: &[Join]) -> Result<Vec<Join>> {
    if items.is_empty() {
        return Ok(joins.to_vec());
    }
    if joins
        .iter()
        .any(|j| matches!(j.kind, SqlJoinKind::Right | SqlJoinKind::Full))
    {
        return Err(unsupported(
            "Layer 2 comma-separated FROM with a RIGHT or FULL JOIN is not supported; write the comma as CROSS JOIN, with parentheses where needed",
        ));
    }
    let cross = items.iter().map(|e| Join {
        this: e.clone(),
        on: None,
        using: vec![],
        kind: SqlJoinKind::Cross,
        use_inner_keyword: false,
        use_outer_keyword: false,
        deferred_condition: false,
        join_hint: None,
        match_condition: None,
        pivots: vec![],
        comments: vec![],
        nesting_group: 0,
        directed: false,
    });
    Ok(cross.chain(joins.iter().cloned()).collect())
}

pub(super) fn cross_join_on() -> Error {
    unsupported(
        "Layer 2 CROSS JOIN does not take an ON condition; it is a Cartesian product (relational_semantics §7.6). Drop ON, or use INNER, LEFT, RIGHT, or FULL JOIN",
    )
}

pub(super) fn consumer_join_kind(j: &Join) -> Result<JoinKind> {
    match j.kind {
        SqlJoinKind::Inner | SqlJoinKind::Implicit => Ok(JoinKind::Inner),
        SqlJoinKind::Left | SqlJoinKind::NaturalLeft => Ok(JoinKind::Left),
        SqlJoinKind::Right => Ok(JoinKind::Right),
        SqlJoinKind::Full => Ok(JoinKind::Full),
        SqlJoinKind::Cross => Ok(JoinKind::Cross),
        SqlJoinKind::AsOf
        | SqlJoinKind::AsOfLeft
        | SqlJoinKind::AsOfRight
        | SqlJoinKind::Outer
        | SqlJoinKind::Natural
        | SqlJoinKind::NaturalRight
        | SqlJoinKind::NaturalFull
        | SqlJoinKind::Semi
        | SqlJoinKind::Anti
        | SqlJoinKind::LeftSemi
        | SqlJoinKind::LeftAnti
        | SqlJoinKind::RightSemi
        | SqlJoinKind::RightAnti
        | SqlJoinKind::CrossApply
        | SqlJoinKind::OuterApply
        | SqlJoinKind::Lateral
        | SqlJoinKind::LeftLateral
        | SqlJoinKind::Straight
        | SqlJoinKind::Array
        | SqlJoinKind::LeftArray
        | SqlJoinKind::Paste
        | SqlJoinKind::Positional => Err(unsupported(
            "Layer 2 supports INNER, LEFT, RIGHT, FULL, and CROSS JOIN (relational_semantics §7.6); this join kind is not supported",
        )),
    }
}

pub(super) fn on_equality_pairs(on: &Expression) -> Result<Vec<(Column, Column)>> {
    match on {
        Expression::Eq(b) => {
            let l = as_column(&b.left).ok_or_else(on_must_be_equalities)?;
            let r = as_column(&b.right).ok_or_else(on_must_be_equalities)?;
            Ok(vec![(l.to_owned(), r.to_owned())])
        }
        Expression::And(a) => {
            let mut out = on_equality_pairs(&a.left)?;
            out.extend(on_equality_pairs(&a.right)?);
            Ok(out)
        }
        _ => Err(on_must_be_equalities()),
    }
}

pub(super) fn on_must_be_equalities() -> Error {
    unsupported("Layer 2 JOIN ON must be column equalities (and AND of equalities)")
}

pub(super) fn match_relationship(
    graph: &SemanticGraph,
    scope: &QueryScope,
    pairs: &[(Column, Column)],
) -> Result<usize> {
    if pairs.is_empty() {
        return Err(on_must_be_equalities());
    }
    let resolved: Result<Vec<(FieldRef, FieldRef)>> = pairs
        .iter()
        .map(|(l, r)| {
            Ok((
                resolve_field_ref(graph, scope, l)?,
                resolve_field_ref(graph, scope, r)?,
            ))
        })
        .collect();
    let resolved = resolved?;
    for (ri, rel) in graph.rels.iter().enumerate() {
        if rel_matches_pairs(graph, rel, &resolved) {
            return Ok(ri);
        }
    }
    Err(unsupported(
        "Layer 2 JOIN ON must match a model relationship (equality on its from_columns and to_columns, in either direction)",
    ))
}

pub(super) fn rel_matches_pairs(
    graph: &SemanticGraph,
    rel: &crate::graph::RelInfo,
    pairs: &[(FieldRef, FieldRef)],
) -> bool {
    if pairs.len() != rel.from_fields.len() {
        return false;
    }
    let forward: Vec<(FieldRef, FieldRef)> = rel
        .from_fields
        .iter()
        .zip(rel.to_fields.iter())
        .map(|(&ff, &tf)| {
            (
                (rel.from, graph.datasets[rel.from].fields[ff].name.clone()),
                (rel.to, graph.datasets[rel.to].fields[tf].name.clone()),
            )
        })
        .collect();
    pairs_match_set(pairs, &forward) || pairs_match_set(pairs, &reverse_pairs(&forward))
}

pub(super) fn reverse_pairs(pairs: &[(FieldRef, FieldRef)]) -> Vec<(FieldRef, FieldRef)> {
    pairs.iter().map(|(a, b)| (b.clone(), a.clone())).collect()
}

pub(super) fn pairs_match_set(got: &[(FieldRef, FieldRef)], want: &[(FieldRef, FieldRef)]) -> bool {
    want.iter().all(|w| {
        got.iter()
            .any(|g| field_pair_eq(g, w) || field_pair_eq(g, &(w.1.clone(), w.0.clone())))
    }) && got.iter().all(|g| {
        want.iter()
            .any(|w| field_pair_eq(g, w) || field_pair_eq(g, &(w.1.clone(), w.0.clone())))
    })
}

pub(super) fn field_pair_eq(a: &(FieldRef, FieldRef), b: &(FieldRef, FieldRef)) -> bool {
    a.0 .0 == b.0 .0
        && a.1 .0 == b.1 .0
        && a.0 .1.eq_ignore_ascii_case(&b.0 .1)
        && a.1 .1.eq_ignore_ascii_case(&b.1 .1)
}

pub(super) fn table_expr(graph: &SemanticGraph, e: &Expression) -> Result<(usize, Option<String>)> {
    match e {
        Expression::Table(t) => resolve_table(graph, t),
        Expression::Alias(a) => {
            let (ds, _) = table_expr(graph, &a.this)?;
            Ok((ds, Some(a.alias.name.clone())))
        }
        Expression::Subquery(_) => Err(unsupported(
            "Layer 2 AGG stage must start FROM a model dataset; join derived tables (Appendix A.2) \
             with JOIN (SELECT ...) t ON <dataset column> = t.<column>. A query over derived \
             tables only (A.3) is compiled by compile_layer2, not layer2_to_request",
        )),
        _ => Err(unsupported(
            "Layer 2 FROM must be a model dataset (table or alias) or a derived table",
        )),
    }
}

pub(super) fn resolve_table(
    graph: &SemanticGraph,
    t: &TableRef,
) -> Result<(usize, Option<String>)> {
    // A model's relations live in one schema named after the model (§11.2).
    let model = &graph.model().name;
    let in_model = t
        .schema
        .as_ref()
        .is_none_or(|s| s.name.eq_ignore_ascii_case(model));
    if t.catalog.is_some() || !in_model {
        return Err(unsupported(format!(
            "Layer 2 FROM should name a model dataset, as `dataset` or `{model}.dataset`, not a catalog.schema.table",
        )));
    }
    let name = &t.name.name;
    let ds = graph::lookup(
        graph.datasets.iter().map(|d| d.name.as_str()),
        name,
        t.name.quoted,
    )
    .map_err(Error::Request)?
    .ok_or_else(|| {
        Error::Request(format!(
            "unknown Layer 2 relation `{name}`; datasets: {}",
            graph
                .datasets
                .iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })?;
    Ok((ds, t.alias.as_ref().map(|a| a.name.clone())))
}

/// Role shared by every ON column of `attach_ds` (`None` when they name the dataset).
pub(super) fn on_attach_role(
    graph: &SemanticGraph,
    scope: &QueryScope,
    pairs: &[(Column, Column)],
    attach_ds: usize,
) -> Result<Option<usize>> {
    let mut found: Option<Option<usize>> = None;
    for (l, r) in pairs {
        for col in [l, r] {
            let ds = resolve_column_dataset(graph, scope, col)?;
            if ds != attach_ds {
                continue;
            }
            let role = column_role(graph, col, ds)?;
            match found {
                None => found = Some(role),
                Some(prev) if prev != role => {
                    return Err(unsupported(format!(
                        "Layer 2 JOIN ON names `{}` both as itself and as a role; qualify every attach column the same way",
                        graph.datasets[attach_ds].name
                    )));
                }
                _ => {}
            }
        }
    }
    Ok(found.flatten())
}
