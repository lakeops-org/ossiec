//! Session `CREATE VIEW` relations (Appendix A.7).
//!
//! A view is a named derived table: its select list's fields, passed-through
//! measures and `(...) TO MEASURE` items. [`define_layer2_view`] checks the body
//! and stores it on a [`Layer2Catalog`]. [`super::persist_layer2_view`] can later
//! write the view's `TO MEASURE` items into the model as permanent metrics.
//! [`super::compile_with`] then rewrites `FROM` / `JOIN` of that name into
//! the defining `SELECT`, so `AGG(view.measure)` is the same §7.7 path as a
//! derived table (sole `FROM` folds; a join attaches the view like Appendix A.2).

use polyglot_sql::expressions::{Identifier, Select, Subquery, TableRef};
use polyglot_sql::{parse_one, DialectType, Expression};

use super::nested::{self, OutCol, OutKind};
use super::to_measure::{self, marker_index, MARKER};
use super::unsupported;
use crate::{Compiled, Error, Options, Result, SemanticGraph};

/// What a column of a [`Layer2Catalog`] view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer2ColumnKind {
    /// A dimension / grouping column.
    Field,
    /// An `AGG(measure)` already evaluated to a scalar inside the view.
    EvaluatedMeasure,
    /// A §7.7 pass-through or `TO MEASURE` item. Only an outer `AGG(view.col)`
    /// reads it, at the outer query's grain.
    UnevaluatedMeasure,
}

/// One output column of a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layer2Column {
    pub name: String,
    pub kind: Layer2ColumnKind,
}

/// Named Layer 2 views for one session. Not part of the model, and not written
/// back to YAML.
#[derive(Debug, Clone, Default)]
pub struct Layer2Catalog {
    views: Vec<View>,
}

/// One view: the defining `SELECT` (with `TO MEASURE` markers), the expressions
/// those markers stand for, and the columns a consumer sees.
#[derive(Debug, Clone)]
pub(super) struct View {
    pub(super) name: String,
    pub(super) select: Select,
    pub(super) to_measures: Vec<Expression>,
    pub(super) outputs: Vec<OutCol>,
}

impl Layer2Catalog {
    pub fn new() -> Self {
        Self { views: Vec::new() }
    }

    /// Columns of `view`, in select-list order.
    pub fn columns(&self, view: &str) -> Option<Vec<Layer2Column>> {
        self.lookup(view).map(|v| {
            v.outputs
                .iter()
                .map(|o| Layer2Column {
                    name: o.name.clone(),
                    kind: match &o.kind {
                        OutKind::Field => Layer2ColumnKind::Field,
                        OutKind::EvaluatedMeasure => Layer2ColumnKind::EvaluatedMeasure,
                        OutKind::UnevaluatedMeasure { .. } => Layer2ColumnKind::UnevaluatedMeasure,
                    },
                })
                .collect()
        })
    }

    pub(super) fn lookup(&self, name: &str) -> Option<&View> {
        self.views
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(name))
    }
}

/// Register one `CREATE VIEW name AS SELECT ...` on `catalog`.
///
/// The body is checked as a derived table (fields, passed-through dataset
/// measures, query-local `TO MEASURE`). Returns the view name. A second view
/// with the same name, `CREATE OR REPLACE`, `DROP`, and a name that matches a
/// model dataset are refused.
pub fn define_layer2_view(
    graph: &SemanticGraph,
    catalog: &mut Layer2Catalog,
    sql: &str,
    opts: &Options,
) -> Result<String> {
    let stmt = one_statement(sql)?;
    let (text, to_measures) = to_measure::extract(&stmt)?;
    let ast = parse_one(&text, DialectType::Generic)
        .map_err(|e| Error::Request(format!("Layer 2 SQL: {e}")))?;
    let (ast, to_measures) = super::strip_model_from_columns(graph, ast, to_measures)?;
    let cv = match ast {
        Expression::CreateView(cv) => cv,
        Expression::DropView(_) => {
            return Err(unsupported(
                "Layer 2 DROP VIEW is not supported; views live in a session Layer2Catalog",
            ))
        }
        _ => {
            return Err(unsupported(
                "define_layer2_view expects one `CREATE VIEW name AS SELECT ...`",
            ))
        }
    };
    refuse_view_options(&cv)?;
    let name = cv.name.name.name.clone();
    if name.is_empty() {
        return Err(Error::Request("Layer 2 CREATE VIEW needs a name".into()));
    }
    if let Some(ds) = graph
        .datasets
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(&name))
    {
        return Err(Error::Request(format!(
            "Layer 2 view `{name}` collides with model dataset `{}`; views are a session catalog and cannot replace a dataset",
            ds.name
        )));
    }
    if catalog.lookup(&name).is_some() {
        return Err(Error::Request(format!(
            "Layer 2 view `{name}` is already defined; CREATE OR REPLACE is not supported"
        )));
    }
    let Expression::Select(sel) = cv.query else {
        return Err(unsupported(
            "Layer 2 CREATE VIEW body must be a single SELECT",
        ));
    };
    let select = *sel;
    let mut expanded = select.clone();
    let mut expanded_measures = to_measures.clone();
    expand_query(catalog, &mut expanded, &mut expanded_measures, Some(&name))?;
    let outputs = nested::view_outputs(graph, &expanded, &expanded_measures, opts)?;
    catalog.views.push(View {
        name: name.clone(),
        select,
        to_measures,
        outputs,
    });
    Ok(name)
}

/// Compile a script: zero or more `CREATE VIEW` statements, then one `SELECT`.
///
/// `compile-l2` uses this, so a file can define measure relations and query
/// them. [`super::compile`] does not: it has an empty catalog.
pub fn compile_script(graph: &SemanticGraph, sql: &str, opts: &Options) -> Result<Compiled> {
    let parts = split_statements(sql);
    if parts.is_empty() {
        return Err(Error::Request("Layer 2 SQL is empty".into()));
    }
    let mut catalog = Layer2Catalog::new();
    let mut query_at: Option<usize> = None;
    for (i, part) in parts.iter().enumerate() {
        match first_keyword(part).as_deref() {
            Some("CREATE" | "DROP") => {
                if query_at.is_some() {
                    return Err(Error::Request(
                        "Layer 2 script: CREATE VIEW statements come before the SELECT".into(),
                    ));
                }
                define_layer2_view(graph, &mut catalog, part, opts)?;
            }
            _ => {
                if query_at.is_some() {
                    return Err(Error::Request(
                        "Layer 2 script has more than one SELECT".into(),
                    ));
                }
                query_at = Some(i);
            }
        }
    }
    let Some(query_at) = query_at else {
        return Err(Error::Request(
            "Layer 2 script defines views but has no SELECT to compile".into(),
        ));
    };
    super::compile_with(graph, &catalog, &parts[query_at], opts)
}

/// Rewrite view names in `sel` into their defining selects. `defining` is the
/// view currently being registered, so a body that reads itself is refused
/// before it is stored.
pub(super) fn expand_query(
    catalog: &Layer2Catalog,
    sel: &mut Select,
    to_measures: &mut Vec<Expression>,
    defining: Option<&str>,
) -> Result<()> {
    if catalog.views.is_empty() && defining.is_none() {
        return Ok(());
    }
    let mut exp = Expand {
        catalog,
        to_measures,
        stack: Vec::new(),
        defining,
    };
    exp.select(sel)
}

struct Expand<'a> {
    catalog: &'a Layer2Catalog,
    to_measures: &'a mut Vec<Expression>,
    stack: Vec<String>,
    defining: Option<&'a str>,
}

impl Expand<'_> {
    fn names_view(&self, name: &str) -> bool {
        self.catalog.lookup(name).is_some()
            || self.defining.is_some_and(|d| d.eq_ignore_ascii_case(name))
    }

    fn select(&mut self, sel: &mut Select) -> Result<()> {
        if let Some(from) = &mut sel.from {
            for e in &mut from.expressions {
                self.relation(e)?;
            }
        }
        for j in &mut sel.joins {
            self.relation(&mut j.this)?;
        }
        Ok(())
    }

    fn relation(&mut self, e: &mut Expression) -> Result<()> {
        if let Some((name, alias)) = self.view_target(e)? {
            *e = self.splice(&name, alias)?;
            return Ok(());
        }
        match e {
            Expression::Alias(a) => self.relation(&mut a.this),
            Expression::Subquery(s) => match &mut s.this {
                Expression::Select(sel) => self.select(sel),
                _ => Err(unsupported(
                    "Layer 2 derived table must be a plain SELECT (set operators are not supported)",
                )),
            },
            _ => Ok(()),
        }
    }

    fn view_target(&self, e: &Expression) -> Result<Option<(String, Identifier)>> {
        match e {
            Expression::Table(t) if self.names_view(&t.name.name) => {
                plain_view_ref(t)?;
                Ok(Some((
                    t.name.name.clone(),
                    t.alias
                        .clone()
                        .unwrap_or_else(|| Identifier::new(t.name.name.clone())),
                )))
            }
            Expression::Alias(a) => match &a.this {
                Expression::Table(t) if self.names_view(&t.name.name) => {
                    plain_view_ref(t)?;
                    if t.alias.is_some() || !a.column_aliases.is_empty() {
                        return Err(unsupported(format!(
                            "Layer 2 view `{}` is referenced by name only (no schema, hints, or column aliases)",
                            t.name.name
                        )));
                    }
                    Ok(Some((t.name.name.clone(), a.alias.clone())))
                }
                _ => Ok(None),
            },
            _ => Ok(None),
        }
    }

    fn splice(&mut self, name: &str, alias: Identifier) -> Result<Expression> {
        if self.stack.iter().any(|n| n.eq_ignore_ascii_case(name))
            || self.defining.is_some_and(|d| d.eq_ignore_ascii_case(name))
        {
            return Err(Error::Request(format!(
                "Layer 2 view `{name}` refers to itself"
            )));
        }
        let (mut select, extra, canonical) = {
            let view = self
                .catalog
                .lookup(name)
                .ok_or_else(|| Error::Request(format!("Layer 2 view `{name}` refers to itself")))?;
            (
                view.select.clone(),
                view.to_measures.clone(),
                view.name.clone(),
            )
        };
        self.stack.push(canonical);
        let offset = self.to_measures.len();
        renumber_select(&mut select, offset);
        self.to_measures.extend(extra);
        self.select(&mut select)?;
        self.stack.pop();
        Ok(Expression::Subquery(Box::new(subquery(select, alias))))
    }
}

fn plain_view_ref(t: &TableRef) -> Result<()> {
    if t.schema.is_some()
        || t.catalog.is_some()
        || !t.column_aliases.is_empty()
        || t.only
        || t.final_
        || t.when.is_some()
        || t.table_sample.is_some()
        || !t.hints.is_empty()
        || t.system_time.is_some()
        || !t.partitions.is_empty()
        || t.identifier_func.is_some()
        || t.changes.is_some()
        || t.version.is_some()
    {
        return Err(unsupported(format!(
            "Layer 2 view `{}` is referenced by name only (no schema, hints, or column aliases)",
            t.name.name
        )));
    }
    Ok(())
}

fn subquery(select: Select, alias: Identifier) -> Subquery {
    Subquery {
        this: Expression::Select(Box::new(select)),
        alias: Some(alias),
        column_aliases: Vec::new(),
        alias_explicit_as: true,
        alias_keyword: None,
        order_by: None,
        limit: None,
        offset: None,
        distribute_by: None,
        sort_by: None,
        cluster_by: None,
        lateral: false,
        modifiers_inside: false,
        trailing_comments: Vec::new(),
        inferred_type: None,
    }
}

fn renumber_select(sel: &mut Select, offset: usize) {
    if offset == 0 {
        return;
    }
    for e in &mut sel.expressions {
        renumber_expr(e, offset);
    }
    if let Some(from) = &mut sel.from {
        for e in &mut from.expressions {
            renumber_relation(e, offset);
        }
    }
    for j in &mut sel.joins {
        renumber_relation(&mut j.this, offset);
    }
}

fn renumber_relation(e: &mut Expression, offset: usize) {
    match e {
        Expression::Subquery(s) => {
            if let Expression::Select(sel) = &mut s.this {
                renumber_select(sel, offset);
            }
        }
        Expression::Alias(a) => renumber_relation(&mut a.this, offset),
        _ => {}
    }
}

fn renumber_expr(e: &mut Expression, offset: usize) {
    match e {
        Expression::Column(c) if c.table.is_none() => {
            if let Some(n) = marker_index(c) {
                c.name = Identifier::new(format!("{MARKER}{}", n + offset));
            }
        }
        Expression::Alias(a) => renumber_expr(&mut a.this, offset),
        Expression::Paren(p) => renumber_expr(&mut p.this, offset),
        _ => {}
    }
}

fn refuse_view_options(cv: &polyglot_sql::expressions::CreateView) -> Result<()> {
    if cv.or_replace || cv.or_alter {
        return Err(unsupported(
            "Layer 2 CREATE OR REPLACE / OR ALTER VIEW is not supported; define a new view name on the session catalog",
        ));
    }
    let extra = cv.if_not_exists
        || cv.materialized
        || cv.temporary
        || cv.secure
        || cv.copy_grants
        || cv.no_schema_binding
        || cv.algorithm.is_some()
        || cv.definer.is_some()
        || cv.security.is_some()
        || cv.comment.is_some()
        || cv.row_access_policy.is_some()
        || !cv.tags.is_empty()
        || !cv.options.is_empty()
        || cv.build.is_some()
        || cv.refresh.is_some()
        || cv.schema.is_some()
        || cv.unique_key.is_some()
        || cv.auto_refresh.is_some()
        || cv.clickhouse_population.is_some()
        || cv.on_cluster.is_some()
        || cv.to_table.is_some()
        || !cv.table_properties.is_empty()
        || cv.locking_mode.is_some()
        || cv.locking_access.is_some()
        || !cv.columns.is_empty();
    if extra {
        return Err(unsupported(
            "Layer 2 CREATE VIEW accepts only `CREATE VIEW name AS SELECT ...` (no column list, materialized, or temporary)",
        ));
    }
    if cv.name.schema.is_some() || cv.name.catalog.is_some() {
        return Err(unsupported(
            "Layer 2 CREATE VIEW name is a single identifier (no schema or catalog)",
        ));
    }
    Ok(())
}

fn one_statement(sql: &str) -> Result<String> {
    let mut parts = split_statements(sql);
    match parts.len() {
        1 => Ok(parts.pop().expect("len checked")),
        0 => Err(Error::Request("Layer 2 SQL is empty".into())),
        _ => Err(Error::Request(
            "define_layer2_view takes one CREATE VIEW statement".into(),
        )),
    }
}

fn split_statements(sql: &str) -> Vec<String> {
    let code = to_measure::code_mask(sql);
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    for (i, byte) in b.iter().enumerate() {
        if code[i] && *byte == b';' {
            push_stmt(&mut out, &sql[start..i]);
            start = i + 1;
        }
    }
    push_stmt(&mut out, &sql[start..]);
    out
}

fn push_stmt(out: &mut Vec<String>, raw: &str) {
    let s = raw.trim();
    if s.is_empty() || !has_code(s) {
        return;
    }
    out.push(s.to_string());
}

fn has_code(sql: &str) -> bool {
    let code = to_measure::code_mask(sql);
    let b = sql.as_bytes();
    code.iter()
        .enumerate()
        .any(|(i, is_code)| *is_code && !b[i].is_ascii_whitespace())
}

fn first_keyword(sql: &str) -> Option<String> {
    let code = to_measure::code_mask(sql);
    let b = sql.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if code[i] && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
            let s = i;
            while i < b.len() && code[i] && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            return Some(sql[s..i].to_ascii_uppercase());
        }
        i += 1;
    }
    None
}
