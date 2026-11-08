//! Naming: which dataset, field, dimension or measure a column refers to, by
//! alias, dataset name or relationship role.

use polyglot_sql::expressions::Column;
use polyglot_sql::Expression;

use crate::graph::{self, SemanticGraph};
use crate::plan::{self};
use crate::{Error, Result};

use super::{unsupported, FieldRef, QueryScope};

pub(super) fn resolve_field_ref(
    graph: &SemanticGraph,
    scope: &QueryScope,
    col: &Column,
) -> Result<FieldRef> {
    let ds = resolve_column_dataset(graph, scope, col)?;
    let field = &col.name.name;
    let fields = &graph.datasets[ds].fields;
    let f = graph::lookup(
        fields.iter().map(|f| f.name.as_str()),
        field,
        col.name.quoted,
    )
    .map_err(Error::Request)?
    .ok_or_else(|| {
        Error::Request(format!(
            "unknown field `{field}` on dataset `{}`",
            graph.datasets[ds].name
        ))
    })?;
    Ok((ds, fields[f].name.clone()))
}

pub(super) fn resolve_column_dataset(
    graph: &SemanticGraph,
    scope: &QueryScope,
    col: &Column,
) -> Result<usize> {
    if let Some(t) = &col.table {
        for b in &scope.bindings {
            let ds_name = plan::unquoted(&graph.datasets[b.dataset].name);
            if t.name.eq_ignore_ascii_case(&b.alias) || t.name.eq_ignore_ascii_case(ds_name) {
                return Ok(b.dataset);
            }
        }
        if scope
            .derived
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&t.name))
        {
            return Err(unsupported(format!(
                "Layer 2 derived-table column `{}.{}` may only appear in that table's JOIN ... ON (not in SELECT, WHERE, GROUP BY or ORDER BY of an AGG stage)",
                t.name, col.name.name
            )));
        }
        if scope.outer.iter().any(|a| a.eq_ignore_ascii_case(&t.name)) {
            return Err(unsupported(format!(
                "Layer 2 correlated subquery: `{}.{}` refers to an enclosing query; derived tables must be self-contained",
                t.name, col.name.name
            )));
        }
        if let Some(role) = relationship_by_name(graph, &t.name, t.quoted)? {
            let rel = &graph.rels[role];
            if scope.bindings.iter().any(|b| b.dataset == rel.from) {
                return Ok(rel.to);
            }
            return Err(Error::Request(format!(
                "column `{}.{}` names relationship `{}`, but dataset `{}` is not in FROM/JOIN",
                t.name, col.name.name, rel.name, graph.datasets[rel.from].name
            )));
        }
        return Err(Error::Request(format!(
            "column `{}.{}` does not refer to a relation in FROM/JOIN",
            t.name, col.name.name
        )));
    }
    if scope.bindings.len() == 1 {
        return Ok(scope.bindings[0].dataset);
    }
    let field = &col.name.name;
    let hits: Vec<usize> = scope
        .bindings
        .iter()
        .filter(|b| {
            let fields = &graph.datasets[b.dataset].fields;
            graph::lookup(
                fields.iter().map(|f| f.name.as_str()),
                field,
                col.name.quoted,
            )
            .is_ok_and(|hit| hit.is_some())
        })
        .map(|b| b.dataset)
        .collect();
    match hits.as_slice() {
        [one] => Ok(*one),
        _ => Err(Error::Request(format!(
            "column `{field}` is ambiguous; qualify it with a FROM/JOIN alias"
        ))),
    }
}

pub(super) fn resolve_measure(
    graph: &SemanticGraph,
    scope: &QueryScope,
    arg: &Expression,
) -> Result<String> {
    let col = as_column(arg).ok_or_else(|| {
        Error::Request(
            "AGG() argument must be a measure column reference (AGG(m) or AGG(alias.m)); \
             write AGG(a) + AGG(b), not AGG(a + b)"
                .into(),
        )
    })?;
    let dataset = resolve_column_dataset(graph, scope, col)?;
    let ds_name = &graph.datasets[dataset].name;
    let measure = &col.name.name;

    let scoped = format!("{ds_name}.{measure}");
    if let Ok(i) = graph.metric_by_name(&scoped) {
        if graph.metrics[i].owner == Some(dataset) {
            return Ok(graph.metrics[i].name.clone());
        }
    }
    if let Ok(i) = graph.metric_by_name(measure) {
        if graph.metrics[i].owner == Some(dataset) {
            return Ok(graph.metrics[i].name.clone());
        }
        if graph.metrics[i].owner.is_none() {
            return Err(Error::Request(format!(
                "`{measure}` is a model-scoped metric; Layer 2 only exposes dataset-scoped metrics as measures (relational_semantics §10.3)"
            )));
        }
        return Err(Error::Request(format!(
            "`{measure}` is not a measure on dataset `{ds_name}`"
        )));
    }
    if graph
        .metrics
        .iter()
        .any(|m| m.name.eq_ignore_ascii_case(measure) && m.owner.is_none())
    {
        return Err(Error::Request(format!(
            "`{measure}` is a model-scoped metric; Layer 2 only exposes dataset-scoped metrics as measures (relational_semantics §10.3)"
        )));
    }
    Err(Error::Request(format!(
        "unknown measure `{measure}` on dataset `{ds_name}`; dataset metrics: {}",
        graph
            .metrics
            .iter()
            .filter(|m| m.owner == Some(dataset))
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

pub(super) fn resolve_dim(
    graph: &SemanticGraph,
    scope: &QueryScope,
    col: &Column,
) -> Result<String> {
    let (dataset, field_name) = resolve_field_ref(graph, scope, col)?;
    let ds_name = &graph.datasets[dataset].name;
    if let Some(role) = column_role(graph, col, dataset)? {
        return Ok(format!("{}.{}", graph.rels[role].name, field_name));
    }
    Ok(format!("{ds_name}.{field_name}"))
}

/// The relationship a column's qualifier names, when that relationship's `to`
/// dataset is `dataset`. That is Layer 3's `relationship.field` role.
pub(super) fn column_role(
    graph: &SemanticGraph,
    col: &Column,
    dataset: usize,
) -> Result<Option<usize>> {
    let Some(t) = &col.table else {
        return Ok(None);
    };
    let Some(role) = relationship_by_name(graph, &t.name, t.quoted)? else {
        return Ok(None);
    };
    if graph.rels[role].to == dataset {
        Ok(Some(role))
    } else {
        Ok(None)
    }
}

pub(super) fn relationship_by_name(
    graph: &SemanticGraph,
    name: &str,
    quoted: bool,
) -> Result<Option<usize>> {
    crate::graph::lookup(graph.rels.iter().map(|r| r.name.as_str()), name, quoted)
        .map_err(Error::Request)
}

pub(super) fn strip_alias(e: &Expression) -> (&Expression, Option<String>) {
    match e {
        Expression::Alias(a) => (&a.this, Some(a.alias.name.clone())),
        other => (other, None),
    }
}

pub(super) fn as_column(e: &Expression) -> Option<&Column> {
    match e {
        Expression::Column(c) => Some(c),
        _ => None,
    }
}
