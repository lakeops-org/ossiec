//! Nested AGG stages: derived tables (Appendix A.2–A.3).
//!
//! **One recursive compile, no nested [`Request`] / [`LogicalPlan`].** Every
//! `AGG` stage is the flat pipeline (request → `plan` → `lower` → FILTER lowering →
//! per-engine `adapt`) run with *its own* plan, because `adapt` reads the plan
//! (Postgres string collation, MySQL string expressions). A derived table is not
//! spliced into the plan as SQL text:
//!
//! 1. The outer stage plans and lowers with a **placeholder relation**
//!    (`__ossiec_derivN`) joined through a [`plan::DerivedJoin`] on each leg that
//!    hosts the attach dataset, so the join exists in every part `lower` builds
//!    for those legs (base, stacked, `PkDedup`). A leg that does not host the
//!    dataset is left unrestricted.
//! 2. Each stage is adapted for the target with its own plan.
//! 3. [`resolve`] substitutes every placeholder, bottom-up, with the (already
//!    adapted) inner AST.
//! 4. The substituted whole is rendered once, and the engine's text-level
//!    `finish` (ClickHouse `SETTINGS`) runs once, on the outermost statement.
//!
//! A.3's outer `AVG`/`SUM`/… over derived columns is ordinary SQL over scalars,
//! assembled by [`super::shell`] outside `plan`/`lower`.
//!
//! **G2 for derived tables.** In a stage that evaluates `AGG()`, a derived table
//! joined in must be unique on its join columns, else the join would repeat the
//! measure's input rows. [`Node::unique_key`] tracks this; the outer shell
//! evaluates no `AGG()`, so it needs no such rule.

use polyglot_sql::expressions::Select;
use polyglot_sql::{generate, DialectType, Expression, ExpressionWalk};

use super::catalog::{self, Layer2Catalog};
use super::{
    apply_consumer_joins, apply_select_formulas, mark_narrowing_legs, parse_stage_from,
    parse_stage_prep, parse_top_select, planned_alias_on_leg, pragma_include_unmatched,
    protect_inflating_joins, rename_consumer_outputs, stage_aliases, unsupported, L2Formula,
    StageCfg,
};
use crate::graph::{Ctx, SemanticGraph};
use crate::plan::{self, DerivedJoin, Request};
use crate::{lower_stage, render_compiled, sql, Compiled, Error, Options, Result};

/// Nesting depth: the top query is 0, its derived tables 1, theirs 2, …
pub(super) const MAX_DEPTH: usize = 4;
/// Derived tables in one query, at any depth.
pub(super) const MAX_DERIVED: usize = 8;

/// What a column of a stage's result is.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum OutKind {
    /// A dimension / grouping column: a value of a field.
    Field,
    /// An `AGG(measure)` or SQL aggregate, already evaluated to a scalar.
    EvaluatedMeasure,
    /// A measure passed through a select list for a later `AGG()` (§7.7), or a
    /// query-local `TO MEASURE` item. Not a value: only an outer `AGG(t.col)`
    /// reads it, and it evaluates `metric` (or `formula`, for `TO MEASURE`) at
    /// the outer stage's grain.
    UnevaluatedMeasure {
        /// Canonical metric a pass-through names; the alias of a `TO MEASURE`.
        metric: String,
        /// `TO MEASURE`: the deferred composition of leaf metrics.
        formula: Option<L2Formula>,
    },
}

/// One consumer-visible output column of a [`Node`].
#[derive(Debug, Clone)]
pub(crate) struct OutCol {
    pub name: String,
    pub kind: OutKind,
}

/// The result of compiling any `SELECT` in the tree.
pub(super) struct Node {
    /// Adapted for the target; may contain placeholder relations.
    pub ast: Expression,
    /// Output columns in the order the statement emits them.
    pub outputs: Vec<OutCol>,
    /// Output indexes that are unique per row (`Some(vec![])`: at most one row).
    pub unique_key: Option<Vec<usize>>,
    /// Placeholder name → the inner node still to substitute.
    pub pending: Vec<(String, Node)>,
}

/// One `FROM` / `JOIN` item in a scope.
pub(super) enum Relation {
    Dataset {
        dataset: usize,
        alias: String,
    },
    Derived {
        alias: String,
        placeholder: String,
        outputs: Vec<OutCol>,
    },
}

impl Relation {
    pub(super) fn alias(&self) -> &str {
        match self {
            Relation::Dataset { alias, .. } | Relation::Derived { alias, .. } => alias,
        }
    }

    /// `alias (dataset orders)` / `alias (derived: a, b)`, for messages.
    pub(super) fn describe(&self, graph: &SemanticGraph) -> String {
        match self {
            Relation::Dataset { dataset, alias } => {
                format!("{alias} (dataset {})", graph.datasets[*dataset].name)
            }
            Relation::Derived { alias, outputs, .. } => format!(
                "{alias} (derived: {})",
                outputs
                    .iter()
                    .map(|o| match o.kind {
                        OutKind::Field => o.name.clone(),
                        OutKind::EvaluatedMeasure => format!("{} [evaluated]", o.name),
                        OutKind::UnevaluatedMeasure { .. } => {
                            format!("{} [unevaluated measure]", o.name)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// State shared by every stage of one top-level compile: one `Ctx` (a single
/// source-dialect decision, merged warnings), one placeholder counter.
pub(super) struct Compiler<'a> {
    /// Clone of the caller's graph. Base `TO MEASURE` installs ephemeral
    /// metrics here; the bound model is not mutated.
    pub graph: SemanticGraph,
    pub opts: &'a Options,
    pub ctx: Ctx,
    pub derived_count: usize,
    /// Next `__l2_bmN` for a query-local base measure.
    pub(super) base_seq: usize,
    /// Outermost `AGG` stage also returns one-side rows no fact reaches.
    include_unmatched: bool,
    /// The flat top-level request, when the query has no derived tables.
    flat_request: Option<Request>,
    /// Expressions of the query's `TO MEASURE` items, by marker index.
    pub to_measures: Vec<Expression>,
}

pub(super) fn compile(
    graph: &SemanticGraph,
    catalog: &Layer2Catalog,
    sql_text: &str,
    opts: &Options,
) -> Result<Compiled> {
    let (mut sel, mut to_measures) = parse_top_select(graph, sql_text)?;
    catalog::expand_query(catalog, &mut sel, &mut to_measures, None)?;
    let mut c = Compiler {
        graph: graph.clone(),
        opts,
        ctx: Ctx {
            prefer: opts.prefer_dialect.clone(),
            ..Default::default()
        },
        derived_count: 0,
        base_seq: 0,
        include_unmatched: opts.include_unmatched || pragma_include_unmatched(sql_text),
        flat_request: None,
        to_measures,
    };
    let node = c.select(&sel, 0, false, &[])?;
    let ast = resolve(node)?;
    // Flat queries keep their Request as identity. Nested SQL, and any query with
    // TO MEASURE (markers only appear in the select list; fold can leave
    // derived_count at 0), hashes the normalized consumer SQL plus formulas.
    let identity = match c.flat_request.take() {
        Some(req) if c.derived_count == 0 && c.to_measures.is_empty() => serde_json::json!(req),
        _ => {
            let render = |e: Expression| {
                generate(&e, DialectType::Generic).map_err(|e| Error::Internal(e.to_string()))
            };
            let text = render(Expression::Select(Box::new(sel)))?;
            let mut id = serde_json::json!({ "layer2_sql": text });
            if !c.to_measures.is_empty() {
                let tm = c
                    .to_measures
                    .iter()
                    .cloned()
                    .map(render)
                    .collect::<Result<Vec<_>>>()?;
                id["to_measures"] = serde_json::json!(tm);
            }
            if c.include_unmatched {
                id["includeUnmatched"] = serde_json::json!(true);
            }
            id
        }
    };
    render_compiled(graph, identity, ast, c.ctx, opts)
}

/// Compile a view body as a derived table and return the columns it exposes.
pub(super) fn view_outputs(
    graph: &SemanticGraph,
    sel: &Select,
    to_measures: &[Expression],
    opts: &Options,
) -> Result<Vec<OutCol>> {
    let mut c = Compiler {
        graph: graph.clone(),
        opts,
        ctx: Ctx {
            prefer: opts.prefer_dialect.clone(),
            ..Default::default()
        },
        derived_count: 0,
        base_seq: 0,
        include_unmatched: false,
        flat_request: None,
        to_measures: to_measures.to_vec(),
    };
    Ok(c.select(sel, 0, true, &[])?.outputs)
}

/// Substitute placeholders, innermost first, so each inner statement is
/// already whole when it replaces its relation.
pub(super) fn resolve(node: Node) -> Result<Expression> {
    let Node {
        mut ast, pending, ..
    } = node;
    for (placeholder, inner) in pending {
        let inner = resolve(inner)?;
        ast = sql::substitute_relation(ast, &placeholder, &inner)?;
    }
    Ok(ast)
}

enum Kind {
    /// `FROM <dataset> [JOIN ...]`: the semantic pipeline.
    Agg,
    /// `AGG()` over a sole derived table (§7.7): the derived table is folded into
    /// the stage, see [`super::fold`].
    Fold,
    /// `FROM <derived> [JOIN <derived> ...]`: ordinary SQL over derived tables.
    Shell,
}

/// Select items no stage reads yet, refused by name before a stage would read
/// them as something else (`*` as multiplication, a subquery's `MAX` as a plain
/// aggregate) and refuse them with a misleading reason.
fn refuse_unsupported_items(sel: &Select) -> Result<()> {
    for item in &sel.expressions {
        let (inner, _) = super::strip_alias(item);
        if matches!(inner, Expression::Star(_)) {
            return Err(unsupported(
                "Layer 2 `SELECT *` is not supported yet: name the columns (§7.1)",
            ));
        }
        if inner.contains(|e| matches!(e, Expression::Subquery(_) | Expression::Select(_))) {
            return Err(unsupported(
                "Layer 2 subqueries in the select list are not supported yet (§7.7)",
            ));
        }
    }
    Ok(())
}

fn classify(sel: &Select) -> Result<Kind> {
    let Some(from) = &sel.from else {
        return Ok(Kind::Agg);
    };
    let derived: Vec<bool> = from
        .expressions
        .iter()
        .map(|e| super::as_derived(e).map(|d| d.is_some()))
        .collect::<Result<_>>()?;
    match derived.as_slice() {
        [true]
            if sel.joins.is_empty()
                && (sel.expressions.iter().any(super::fold::has_agg)
                    || sel
                        .having
                        .as_ref()
                        .is_some_and(|h| h.this.contains(super::is_measure_agg_expr))
                    || sel.order_by.iter().any(|o| {
                        o.expressions
                            .iter()
                            .any(|k| k.this.contains(super::is_measure_agg_expr))
                    })) =>
        {
            Ok(Kind::Fold)
        }
        [true] => Ok(Kind::Shell),
        d if d.iter().any(|x| *x) => Err(unsupported(
            "Layer 2 comma-separated FROM cannot include derived tables; use JOIN (SELECT ...) t ON ...",
        )),
        _ => Ok(Kind::Agg),
    }
}

impl Compiler<'_> {
    /// Compile one `SELECT` (the top query or a derived table).
    pub(super) fn select(
        &mut self,
        sel: &Select,
        depth: usize,
        derived_mode: bool,
        outer: &[String],
    ) -> Result<Node> {
        if depth > MAX_DEPTH {
            return Err(unsupported(format!(
                "Layer 2 derived tables nest at most {MAX_DEPTH} levels deep"
            )));
        }
        refuse_unsupported_items(sel)?;
        match classify(sel)? {
            Kind::Agg => self.agg_stage(sel, depth, derived_mode, outer),
            Kind::Fold => self.fold(sel, depth, derived_mode, outer),
            Kind::Shell => self.shell(sel, depth, outer),
        }
    }

    /// A derived table: the next placeholder and the compiled inner query.
    pub(super) fn derived(
        &mut self,
        select: &Select,
        depth: usize,
        outer: &[String],
    ) -> Result<(String, Node)> {
        if self.derived_count >= MAX_DERIVED {
            return Err(unsupported(format!(
                "Layer 2 allows at most {MAX_DERIVED} derived tables in one query"
            )));
        }
        let placeholder = format!("__ossiec_deriv{}", self.derived_count);
        self.derived_count += 1;
        let node = self.select(select, depth, true, outer)?;
        Ok((placeholder, node))
    }

    /// An `AGG` stage: the flat request → plan → lower pipeline, plus the
    /// derived tables the consumer joined in.
    pub(super) fn agg_stage(
        &mut self,
        sel: &Select,
        depth: usize,
        derived_mode: bool,
        outer: &[String],
    ) -> Result<Node> {
        let to_measures = self.to_measures.clone();
        let cfg = StageCfg {
            derived: derived_mode,
            outer,
            to_measures: &to_measures,
        };
        let parsed_from = parse_stage_prep(&self.graph, sel, &cfg)?;

        // Nested queries see this stage's aliases as "outer" (correlation check).
        let mut inner_outer = outer.to_vec();
        inner_outer.extend(stage_aliases(&self.graph, &parsed_from));

        // Compile the derived tables first: the select list may read their
        // unevaluated measures (`AGG(t.m)`, §7.7).
        let mut compiled: Vec<(String, Node)> = vec![];
        let mut schemas: Vec<(String, Vec<OutCol>)> = vec![];
        for spec in &parsed_from.derived {
            let (placeholder, node) = self.derived(&spec.select, depth + 1, &inner_outer)?;
            schemas.push((spec.alias.clone(), node.outputs.clone()));
            compiled.push((placeholder, node));
        }
        let mut base_seq = self.base_seq;
        let mut parsed =
            parse_stage_from(&self.graph, sel, &cfg, parsed_from, &schemas, &mut base_seq)?;
        self.base_seq = base_seq;
        for ep in std::mem::take(&mut parsed.ephemeral) {
            self.graph
                .install_ephemeral_metric(&ep.name, ep.owner, ep.body, ep.raw)?;
        }
        if depth == 0 && self.include_unmatched {
            // The outermost AGG stage (a flat query, or the outer stage of a
            // derived join). Inner stages keep the default: the flag is about
            // the query the caller wrote, not every nested aggregate.
            parsed.request.include_unmatched = true;
        }
        // A derived JOIN attaches to a dataset that may not be named by the
        // selected measures (grand-total AGG over a pass-through with
        // `FROM customers JOIN (…orders…) m`). Pull those datasets into the
        // join tree so `planned_alias_on_leg` can resolve them — only when the
        // attach dataset is not already required by a measure or dimension,
        // so A.4 independent-fact stacking is not collapsed into one tree.
        let already: std::collections::BTreeSet<usize> = parsed
            .request
            .metrics
            .iter()
            .filter_map(|m| {
                self.graph
                    .metric_by_name(m)
                    .ok()
                    .and_then(|i| self.graph.metrics[i].owner)
            })
            .chain(
                parsed
                    .request
                    .dimensions
                    .iter()
                    .filter_map(|d| self.graph.field_by_name(d).ok().map(|(ds, _, _)| ds)),
            )
            .collect();
        for spec in &parsed.derived {
            if already.contains(&spec.attach_ds) {
                continue;
            }
            if !parsed.request.ensure_datasets.contains(&spec.attach_ds) {
                parsed.request.ensure_datasets.push(spec.attach_ds);
            }
        }
        if depth == 0
            && parsed.derived.is_empty()
            && parsed.having.is_none()
            && parsed.order_exprs.is_empty()
        {
            self.flat_request = Some(parsed.request.clone());
        }
        let graph = &self.graph;

        let mut pending = vec![];
        // (placeholder, derived columns as emitted) per derived table.
        let mut prepared: Vec<(String, Vec<String>)> = vec![];
        let aggregates = !parsed.request.metrics.is_empty();
        for (spec, (placeholder, node)) in parsed.derived.iter().zip(compiled) {
            let mut rel_cols = vec![];
            let mut key_idx = vec![];
            for c in &spec.cols {
                let i = node
                    .outputs
                    .iter()
                    .position(|o| o.name.eq_ignore_ascii_case(c))
                    .ok_or_else(|| {
                        Error::Request(format!(
                            "Layer 2 derived table `{}` has no column `{c}`; it has: {}",
                            spec.alias,
                            node.outputs
                                .iter()
                                .map(|o| o.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;
                if matches!(node.outputs[i].kind, OutKind::UnevaluatedMeasure { .. }) {
                    return Err(unsupported(format!(
                        "Layer 2 derived-table JOIN ON cannot use `{}.{c}`: it is an unevaluated measure, not a value",
                        spec.alias
                    )));
                }
                rel_cols.push(node.outputs[i].name.clone());
                key_idx.push(i);
            }
            // CROSS has no join columns; primary-key de-duplication (below) keeps
            // G2. An equijoin must still be unique on its keys.
            if aggregates && spec.kind != plan::JoinKind::Cross {
                let unique = node
                    .unique_key
                    .as_ref()
                    .is_some_and(|k| k.iter().all(|i| key_idx.contains(i)));
                if !unique {
                    return Err(unsupported(format!(
                        "Layer 2 derived table `{}` is joined on ({}), which is not unique in it: joining would repeat the input rows of AGG() (relational_semantics G2). Group the derived table by exactly its join columns (or a subset of them)",
                        spec.alias,
                        spec.cols.join(", ")
                    )));
                }
            }
            prepared.push((placeholder.clone(), rel_cols));
            pending.push((placeholder, node));
        }

        let mut logical = plan::plan(
            graph,
            &parsed.request,
            plan::Interface::Relational,
            &mut self.ctx,
        )?;
        apply_consumer_joins(graph, &mut logical, &parsed.consumer_joins, &mut self.ctx)?;
        if !parsed.derived.is_empty() {
            // Attach only to legs that host the dataset. A leg that does not
            // read it stays unrestricted, so one independent fact can be filtered
            // without filtering the others. Refuse only when no leg hosts it.
            // `includeUnmatched` is allowed: `unmatched_rows` renames subtree
            // aliases to `…__unmatched` and repeats derived joins that attach there.
            for (spec, (placeholder, rel_cols)) in parsed.derived.iter().zip(prepared) {
                let mut attached = false;
                for leg in &mut logical.legs {
                    let Some(from_alias) =
                        planned_alias_on_leg(graph, leg, spec.attach_ds, spec.attach_role)
                    else {
                        continue;
                    };
                    attached = true;
                    leg.derived.push(DerivedJoin {
                        from_alias,
                        from_fields: spec.from_fields.clone(),
                        rel_placeholder: placeholder.clone(),
                        rel_alias: spec.alias.clone(),
                        rel_cols: rel_cols.clone(),
                        null_safe: spec.null_safe,
                        kind: spec.kind,
                    });
                }
                if !attached {
                    let attach = match spec.attach_role {
                        Some(r) => format!(
                            "{} via `{}`",
                            graph.datasets[spec.attach_ds].name, graph.rels[r].name
                        ),
                        None => graph.datasets[spec.attach_ds].name.clone(),
                    };
                    return Err(if logical.legs.len() > 1 {
                        unsupported(format!(
                            "Layer 2 derived-table JOIN `{}` with measures from independent facts (A.4): `{attach}` is not part of any planned leg",
                            spec.alias
                        ))
                    } else {
                        unsupported(format!(
                            "Layer 2 derived-table JOIN `{}`: dataset `{attach}` is not part of the planned query",
                            spec.alias
                        ))
                    });
                }
            }
        }
        mark_narrowing_legs(
            graph,
            &mut logical,
            &parsed.consumer_joins,
            self.include_unmatched,
        )?;
        if logical.terms.iter().any(|t| t.term.raw) {
            // A plain aggregate reads the query's joined rows. Independent facts
            // are planned as separate trees, none of which is those rows.
            if logical.legs.len() > 1 {
                return Err(unsupported(
                    "Layer 2 plain aggregates (SUM, COUNT, ...) beside measures from independent facts are not implemented yet",
                ));
            }
            if logical.include_unmatched {
                return Err(unsupported(
                    "Layer 2 plain aggregates (SUM, COUNT, ...) with includeUnmatched are not implemented yet",
                ));
            }
        }
        protect_inflating_joins(graph, &mut logical, &mut self.ctx)?;
        super::apply_group_exprs(&mut logical, parsed.having.as_ref(), &parsed.order_exprs)?;
        apply_select_formulas(
            graph,
            &mut logical,
            &parsed.measure_outputs,
            parsed.having.is_some() || !parsed.order_exprs.is_empty(),
        )?;
        rename_consumer_outputs(&mut logical, &parsed.measure_outputs, &parsed.dim_labels);

        let mut outputs: Vec<OutCol> = logical
            .dims
            .iter()
            .map(|(n, _)| OutCol {
                name: n.clone(),
                kind: OutKind::Field,
            })
            .collect();
        outputs.extend(logical.outputs.iter().map(|(n, _)| OutCol {
            name: n.clone(),
            kind: OutKind::EvaluatedMeasure,
        }));
        // Unevaluated measures: a `TO MEASURE` item is a column of the statement
        // (evaluated at this grain) that the outer query reads as a measure; a
        // pass-through has no column at all, only a name.
        for u in &parsed.unevaluated {
            let kind = OutKind::UnevaluatedMeasure {
                metric: u.metric.clone(),
                formula: u.formula.clone(),
            };
            match outputs.iter_mut().find(|o| o.name == u.name) {
                Some(o) if u.formula.is_some() => o.kind = kind,
                _ => outputs.push(OutCol {
                    name: u.name.clone(),
                    kind,
                }),
            }
        }
        if derived_mode {
            check_unique_names(&outputs)?;
        }
        // Grouped by its dimensions (or one row without any): the dimensions
        // identify the row, whatever ORDER BY / LIMIT / joins did.
        let unique_key = Some((0..logical.dims.len()).collect());
        let ast = lower_stage(graph, &logical, &mut self.ctx, self.opts)?;
        Ok(Node {
            ast,
            outputs,
            unique_key,
            pending,
        })
    }
}

/// A derived table's columns are addressed by name: duplicates are ambiguous.
pub(super) fn check_unique_names(outputs: &[OutCol]) -> Result<()> {
    for (i, o) in outputs.iter().enumerate() {
        if outputs[..i]
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&o.name))
        {
            return Err(Error::Request(format!(
                "Layer 2 derived table has two output columns named `{}`; give one an alias",
                o.name
            )));
        }
    }
    Ok(())
}
