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

use polyglot_sql::expressions::{Alias, Column, Identifier, Null, Select};
use polyglot_sql::{generate, DialectType, Expression, ExpressionWalk};

use super::catalog::{self, Layer2Catalog};
use super::correlate;
use super::{
    apply_consumer_joins, apply_select_formulas, mark_narrowing_legs, parse_stage_from,
    parse_stage_prep, parse_top_select, planned_alias_on_leg, pragma_include_unmatched,
    protect_inflating_joins, rename_consumer_outputs, stage_aliases, unsupported, L2Formula,
    StageCfg,
};
use crate::graph::{Ctx, SemanticGraph};
use crate::plan::{self, DerivedJoin};
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
    /// Placeholders handed to grouping-set branches, numbered apart from derived tables.
    pub grouping_count: usize,
    /// Next `__l2_bmN` for a query-local base measure.
    pub(super) base_seq: usize,
    /// Outermost `AGG` stage also returns one-side rows no fact reaches.
    include_unmatched: bool,
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
        grouping_count: 0,
        base_seq: 0,
        include_unmatched: opts.include_unmatched || pragma_include_unmatched(sql_text),
        to_measures,
    };
    let node = c.select(&sel, 0, false, &[])?;
    let ast = resolve(node)?;
    // The identity is the normalized consumer SQL plus the TO MEASURE formulas:
    // two queries that differ only in a join kind compile to different SQL, so
    // they must not share a hash.
    let render = |e: Expression| {
        generate(&e, DialectType::Generic).map_err(|e| Error::Internal(e.to_string()))
    };
    let text = render(Expression::Select(Box::new(sel)))?;
    let mut identity = serde_json::json!({ "layer2_sql": text });
    if !c.to_measures.is_empty() {
        let tm = c
            .to_measures
            .iter()
            .cloned()
            .map(render)
            .collect::<Result<Vec<_>>>()?;
        identity["to_measures"] = serde_json::json!(tm);
    }
    if c.include_unmatched {
        identity["includeUnmatched"] = serde_json::json!(true);
    }
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
        grouping_count: 0,
        base_seq: 0,
        include_unmatched: false,
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
        if let Some(sets) = super::grouping::grouping_sets(sel)? {
            return self.grouping(sel, &sets, depth, derived_mode, outer);
        }
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
            compiled: &[],
        };
        let parsed_from = parse_stage_prep(&self.graph, sel, &cfg)?;

        // Nested queries see this stage's aliases as "outer" (correlation check).
        let stage = stage_aliases(&self.graph, &parsed_from);
        let mut inner_outer = outer.to_vec();
        inner_outer.extend(stage.iter().cloned());

        // Compile the derived tables first: the select list may read their
        // unevaluated measures (`AGG(t.m)`, §7.7).
        let mut compiled: Vec<(String, Node)> = vec![];
        let mut schemas: Vec<(String, Vec<OutCol>)> = vec![];
        for spec in &parsed_from.derived {
            let (placeholder, node) = self.derived(&spec.select, depth + 1, &inner_outer)?;
            schemas.push((spec.alias.clone(), node.outputs.clone()));
            compiled.push((placeholder, node));
        }
        // Subqueries in WHERE (§7.3): each a Layer 2 query of its own, read through
        // a placeholder until `resolve` puts it in place.
        let (sel, subqueries) = self.where_subqueries(sel, depth, &inner_outer, &stage)?;
        let sel = &sel;
        let where_placeholders: Vec<String> = subqueries.iter().map(|(p, _)| p.clone()).collect();
        let cfg = StageCfg {
            compiled: &where_placeholders,
            ..cfg
        };
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
        let graph = &self.graph;

        let mut pending = subqueries;
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

impl Compiler<'_> {
    /// §7.4: one stage per grouping set ([`super::grouping`]), stacked with
    /// `UNION ALL` in the select list's column order, NULL where a set leaves a
    /// grouping column out. The set with every grouping column goes first, so
    /// each column has its type before a branch's NULL meets it (Postgres types
    /// `UNION` pairwise, and two bare NULLs would become text). When no set has
    /// them all, a branch that returns no rows types them. A stage emits
    /// dimensions before measures; the stack projects them back into the select
    /// list's order, which is what a positional `ORDER BY` indexes. `ORDER BY`
    /// and `LIMIT` apply to the stacked whole.
    fn grouping(
        &mut self,
        sel: &Select,
        sets: &[Vec<Column>],
        depth: usize,
        derived_mode: bool,
        outer: &[String],
    ) -> Result<Node> {
        use super::grouping::{all_columns, branch, order_key, same, stable_names};
        // A branch drops the grouping columns its set leaves out, which would
        // renumber the unnamed expressions after them (`__l2_N`).
        let sel = &stable_names(sel);
        if !matches!(classify(sel)?, Kind::Agg) {
            return Err(unsupported(
                "Layer 2 ROLLUP, CUBE and GROUPING SETS over derived tables are not supported yet",
            ));
        }
        if sel.distinct || sel.offset.is_some() {
            return Err(unsupported(
                "Layer 2 ROLLUP, CUBE and GROUPING SETS with DISTINCT or OFFSET are not supported yet",
            ));
        }
        let grouping_fn = |e: &Expression| {
            matches!(e, Expression::Function(f) if f.name.eq_ignore_ascii_case("GROUPING"))
                || matches!(e, Expression::Grouping(_))
        };
        if sel.expressions.iter().any(|e| e.contains(grouping_fn))
            || sel
                .having
                .as_ref()
                .is_some_and(|h| h.this.contains(grouping_fn))
        {
            return Err(unsupported(
                "Layer 2 GROUPING() is not supported yet: a subtotal row's grouping columns are NULL",
            ));
        }
        let all = all_columns(sets);
        let covers = |s: &[Column]| all.iter().all(|c| s.iter().any(|x| same(x, c)));
        let mut plan: Vec<(&[Column], bool)> = vec![];
        match sets.iter().position(|s| covers(s)) {
            Some(i) => {
                plan.push((&sets[i], false));
                plan.extend(
                    sets.iter()
                        .enumerate()
                        .filter(|(j, _)| *j != i)
                        .map(|(_, s)| (s.as_slice(), false)),
                );
            }
            None => {
                plan.push((&all, true));
                plan.extend(sets.iter().map(|s| (s.as_slice(), false)));
            }
        }
        let mut pending = vec![];
        // Each branch: the placeholder it reads (none for a constant row) and
        // the columns it has.
        let mut branches: Vec<(Option<String>, Vec<String>)> = vec![];
        let mut first_outputs: Vec<OutCol> = vec![];
        for (set, empty) in plan {
            let b = branch(sel, &all, set, empty)?;
            if b.expressions.is_empty() && set.is_empty() && b.having.is_none() {
                // `SELECT status … GROUP BY ROLLUP(status)`: the grand total
                // selects nothing. It is one row of NULLs, as SQL's empty
                // grouping set is even over no rows, so no stage reads anything.
                branches.push((None, vec![]));
                continue;
            }
            let mut b = b;
            // Every grouping column is selected (GROUP BY matches the selected
            // dimensions), so only the grand total can select nothing. With a
            // HAVING to apply, it gets one hidden aggregate (a plain one, so
            // not with `includeUnmatched`), which the outer list never reads.
            let hidden = b.expressions.is_empty();
            if hidden {
                b.expressions = vec![hidden_count()?];
            }
            // Grouping sets have their own bound (`MAX_SETS`, checked when they're
            // expanded), not the derived tables'. The name only has to be unique.
            let placeholder = format!("{}g{}", sql::PLACEHOLDER_PREFIX, self.grouping_count);
            self.grouping_count += 1;
            let node = self.agg_stage(&b, depth, derived_mode, outer)?;
            let mut cols = output_names(&node.ast)?;
            if hidden {
                cols.retain(|n| !n.starts_with(HIDDEN_GROUPING_COLUMN));
            }
            if first_outputs.is_empty() {
                first_outputs = node.outputs.clone();
            }
            branches.push((Some(placeholder.clone()), cols));
            pending.push((placeholder, node));
        }
        // The first branch holds every selected column (it is the full set, or
        // the branch that types them). Its stage lists dimensions, then measures.
        let names = names_in_select_order(sel, &first_outputs)?;
        for (i, n) in names.iter().enumerate() {
            if names[..i].iter().any(|p| p.eq_ignore_ascii_case(n)) {
                return Err(Error::Request(format!(
                    "Layer 2 with ROLLUP, CUBE or GROUPING SETS: two output columns are named `{n}`; give one an alias"
                )));
            }
        }
        let q = |n: &str| format!("\"{}\"", n.replace('"', "\"\""));
        let parts: Vec<String> = branches
            .iter()
            .enumerate()
            .map(|(i, (placeholder, have))| {
                let cols: Vec<String> = names
                    .iter()
                    .map(|n| {
                        if have.iter().any(|h| h.eq_ignore_ascii_case(n)) {
                            format!("__ossiec_g{i}.{} AS {}", q(n), q(n))
                        } else {
                            format!("NULL AS {}", q(n))
                        }
                    })
                    .collect();
                match placeholder {
                    Some(p) => format!("SELECT {} FROM {p} AS __ossiec_g{i}", cols.join(", ")),
                    None => format!("SELECT {}", cols.join(", ")),
                }
            })
            .collect();
        let mut text = format!(
            "SELECT * FROM ({}) AS __ossiec_grouping",
            parts.join(" UNION ALL ")
        );
        if let Some(ob) = &sel.order_by {
            let keys: Vec<String> = ob
                .expressions
                .iter()
                .map(|o| {
                    let k = order_key(&o.this, &names, sel)?;
                    let dir = if o.desc { "DESC" } else { "ASC" };
                    // Written out, as in `lower`: NULL is the smallest value
                    // whatever the source dialect, so subtotal rows sort first.
                    let nulls = match (o.nulls_first, o.desc) {
                        (Some(true), _) | (None, false) => " NULLS FIRST",
                        (Some(false), _) | (None, true) => " NULLS LAST",
                    };
                    Ok(format!("__ossiec_grouping.{} {dir}{nulls}", q(&names[k])))
                })
                .collect::<Result<_>>()?;
            text.push_str(&format!(" ORDER BY {}", keys.join(", ")));
        }
        if let Some(l) = &sel.limit {
            if l.percent {
                return Err(unsupported("Layer 2 LIMIT PERCENT is not supported"));
            }
            text.push_str(&format!(" LIMIT {}", super::clauses::limit_u64(&l.this)?));
        }
        let ast = sql::parse(&text, DialectType::Generic).map_err(Error::Internal)?;
        let outputs = names
            .iter()
            .map(|n| OutCol {
                name: n.clone(),
                kind: first_outputs
                    .iter()
                    .find(|o| o.name.eq_ignore_ascii_case(n))
                    .map_or(OutKind::EvaluatedMeasure, |o| o.kind.clone()),
            })
            .collect();
        Ok(Node {
            ast,
            outputs,
            // Subtotal rows repeat the dimensions' NULLs.
            unique_key: None,
            pending,
        })
    }

    /// §7.3: each subquery in `sel`'s `WHERE`, outermost first, compiled as a Layer 2
    /// query of its own (any nested in it are compiled with it), and replaced by
    /// `(SELECT * FROM <placeholder> AS …)`. A subquery that reads this stage's
    /// columns (`stage` names its relations) is correlated: see
    /// [`super::correlate`]. One that reads a query further out is refused by name.
    fn where_subqueries(
        &mut self,
        sel: &Select,
        depth: usize,
        outer: &[String],
        stage: &[String],
    ) -> Result<(Select, Vec<(String, Node)>)> {
        let Some(w) = &sel.where_clause else {
            return Ok((sel.clone(), vec![]));
        };
        // Outermost queries only: a pre-order walk meets each before those inside it.
        let mut tops: Vec<&Expression> = vec![];
        for x in w.this.dfs() {
            if matches!(x, Expression::Select(_))
                && !tops.iter().any(|t| t.contains(|y| std::ptr::eq(y, x)))
            {
                tops.push(x);
            }
        }
        if tops.is_empty() {
            return Ok((sel.clone(), vec![]));
        }
        // Bodies of EXISTS, which may return any columns; elsewhere, one value.
        let exists_bodies: Vec<&Expression> = w
            .this
            .dfs()
            .filter_map(|x| match x {
                Expression::Exists(ex) => Some(match &ex.this {
                    Expression::Subquery(s) => &s.this,
                    other => other,
                }),
                _ => None,
            })
            .collect();
        // Queries that are the right side of `IN`.
        let in_bodies: Vec<&Expression> = w
            .this
            .dfs()
            .filter_map(|x| match x {
                Expression::In(i) => i.query.as_ref().map(|q| match q {
                    Expression::Subquery(s) => &s.this,
                    other => other,
                }),
                _ => None,
            })
            .collect();
        let mut compiled: Vec<(Expression, Expression)> = vec![];
        let mut pending = vec![];
        for top in tops {
            let Expression::Select(select) = top else {
                unreachable!("tops are selects")
            };
            // Its columns are read by position, never by name, so an unnamed
            // aggregate needs none: give it one, as a derived table requires.
            let mut select = (**select).clone();
            for (i, item) in select.expressions.iter_mut().enumerate() {
                if !matches!(item, Expression::Alias(_) | Expression::Column(_)) {
                    let this = std::mem::replace(item, Expression::Null(Null));
                    *item = Expression::Alias(Box::new(Alias::new(
                        this,
                        Identifier::new(format!("__ossiec_v{i}")),
                    )));
                }
            }
            let correlation = correlate::correlate(&mut select, stage)?;
            let (placeholder, node) = self.derived(&select, depth + 1, outer)?;
            if let Some(m) = node
                .outputs
                .iter()
                .find(|o| matches!(o.kind, OutKind::UnevaluatedMeasure { .. }))
            {
                return Err(unsupported(format!(
                    "Layer 2 subquery in WHERE returns `{}`, an unevaluated measure: evaluate it with AGG()",
                    m.name
                )));
            }
            // The columns the consumer wrote, without the keys a correlation added.
            let outputs: Vec<String> = node
                .outputs
                .iter()
                .map(|o| o.name.clone())
                .filter(|n| correlation.is_none() || !n.starts_with(correlate::KEY))
                .collect();
            let exists = exists_bodies.iter().any(|b| std::ptr::eq(*b, top));
            if !exists && outputs.len() != 1 {
                return Err(Error::Request(format!(
                    "Layer 2 subquery in WHERE must return one column, as SQL requires outside EXISTS; it returns {}",
                    outputs.len()
                )));
            }
            let sq = format!("{}{}", sql::STAND_IN_PREFIX, pending.len());
            match correlation {
                None => {
                    let stand_in = sql::parse(
                        &format!("SELECT * FROM {placeholder} AS {sq}"),
                        DialectType::Generic,
                    )
                    .map_err(Error::Internal)?;
                    compiled.push((top.clone(), stand_in));
                    pending.push((placeholder, node));
                }
                Some(corr) => match &corr.empty {
                    // `x [NOT] IN (<correlated>)` where the target takes only
                    // correlated EXISTS: the whole IN becomes EXISTS forms.
                    None if !crate::dialect::supports_correlated_in(self.opts.target) => {
                        let in_node = w.this.dfs().find(|x| match x {
                            Expression::In(i) => i.query.as_ref().is_some_and(|q| match q {
                                Expression::Subquery(s) => std::ptr::eq(&s.this, top),
                                other => std::ptr::eq(other, top),
                            }),
                            _ => false,
                        });
                        let stand_in = match in_node {
                            Some(Expression::In(i)) => {
                                let replacement = correlate::in_as_exists(
                                    &corr,
                                    &placeholder,
                                    &sq,
                                    &outputs[0],
                                    &i.this,
                                    i.not,
                                )?;
                                compiled.push((in_node.cloned().expect("found"), replacement));
                                None
                            }
                            _ => Some(correlate::stand_in(&corr, &placeholder, &sq, &outputs)?),
                        };
                        if let Some(stand_in) = stand_in {
                            compiled.push((top.clone(), stand_in));
                        }
                        pending.push((placeholder, node));
                    }
                    None => {
                        let stand_in = correlate::stand_in(&corr, &placeholder, &sq, &outputs)?;
                        compiled.push((top.clone(), stand_in));
                        pending.push((placeholder, node));
                    }
                    Some(empty) => {
                        let scalar = !exists && !in_bodies.iter().any(|b| std::ptr::eq(*b, top));
                        // The `( … )` around the subquery, which the scalar replaces.
                        let around = w.this.dfs().find(
                            |x| matches!(x, Expression::Subquery(s) if std::ptr::eq(&s.this, top)),
                        );
                        let (true, Some(around)) = (scalar, around) else {
                            return Err(unsupported(
                                "Layer 2 correlated subquery that aggregates without GROUP BY is supported as a scalar value (`x > (SELECT AGG(m) …)`); in EXISTS or IN, group it by the correlated column instead",
                            ));
                        };
                        let (none, none_node) = self.derived(empty, depth + 1, outer)?;
                        let stand_in = correlate::scalar_with_default(
                            &corr,
                            &placeholder,
                            &sq,
                            &none,
                            &outputs[0],
                        )?;
                        compiled.push((around.clone(), stand_in));
                        pending.push((placeholder, node));
                        pending.push((none, none_node));
                    }
                },
            }
        }
        let this = sql::transform(w.this.clone(), &|x| {
            Ok(Some(
                compiled
                    .iter()
                    .find(|(original, _)| *original == x)
                    .map_or(x, |(_, stand_in)| stand_in.clone()),
            ))
        })
        .map_err(|e| Error::Internal(e.to_string()))?;
        // A subquery identical to one nested in another would have changed its
        // parent before the parent was matched.
        let left = this.contains(|x| {
            matches!(x, Expression::Select(_))
                && !sql::is_subquery_placeholder(x)
                && !this.contains(|p| {
                    sql::is_subquery_placeholder(p) && p.contains(|y| std::ptr::eq(y, x))
                })
        });
        if left {
            return Err(unsupported(
                "Layer 2 WHERE repeats a subquery inside another one; write it once",
            ));
        }
        let mut out = sel.clone();
        if let Some(w) = out.where_clause.as_mut() {
            w.this = this;
        }
        Ok((out, pending))
    }
}

/// The hidden item of a grand total that selects nothing; the outer list never
/// names it.
const HIDDEN_GROUPING_COLUMN: &str = "__ossiec_grouping_row";

/// `COUNT(*) AS __ossiec_grouping_row0`, the one item of a grand total that
/// selects nothing but has a `HAVING` to apply.
fn hidden_count() -> Result<Expression> {
    let ast = sql::parse(
        &format!("SELECT COUNT(*) AS {HIDDEN_GROUPING_COLUMN}0"),
        DialectType::Generic,
    )
    .map_err(Error::Internal)?;
    match ast {
        Expression::Select(s) => s
            .expressions
            .into_iter()
            .next()
            .ok_or_else(|| Error::Internal("hidden count has no select item".into())),
        _ => Err(Error::Internal(
            "hidden count did not parse as a select".into(),
        )),
    }
}

/// `outputs` is a stage's columns, dimensions then measures. Walk the select
/// list and take the next dimension or measure, so the stack's column order
/// is the consumer's.
fn names_in_select_order(sel: &Select, outputs: &[OutCol]) -> Result<Vec<String>> {
    use std::collections::VecDeque;
    let mut fields: VecDeque<&str> = outputs
        .iter()
        .filter(|o| o.kind == OutKind::Field)
        .map(|o| o.name.as_str())
        .collect();
    let mut measures: VecDeque<&str> = outputs
        .iter()
        .filter(|o| o.kind != OutKind::Field)
        .map(|o| o.name.as_str())
        .collect();
    let mut names = Vec::with_capacity(sel.expressions.len());
    for expr in &sel.expressions {
        let next = if is_dimension_item(expr) {
            fields.pop_front()
        } else {
            measures.pop_front()
        };
        let Some(name) = next else {
            return Err(Error::Internal(
                "grouping output columns do not match the select list".into(),
            ));
        };
        names.push(name.to_string());
    }
    if !fields.is_empty() || !measures.is_empty() || names.len() != outputs.len() {
        return Err(Error::Internal(
            "grouping output columns do not match the select list".into(),
        ));
    }
    Ok(names)
}

/// A selected dimension: a column, not an aggregate. The stage rejects anything
/// else that isn't a measure, so the remainder of the select list is measures.
fn is_dimension_item(expr: &Expression) -> bool {
    let (inner, _) = super::strip_alias(expr);
    let inner = super::unwrap_paren(inner);
    super::as_column(inner).is_some()
        && !inner.contains(super::is_base_agg_expr)
        && !inner.contains(super::is_measure_agg_expr)
}

/// The column names a compiled stage's statement returns, in order.
fn output_names(ast: &Expression) -> Result<Vec<String>> {
    let shape = || {
        unsupported(
            "Layer 2 ROLLUP, CUBE and GROUPING SETS with this query shape are not supported yet",
        )
    };
    let Expression::Select(s) = ast else {
        return Err(shape());
    };
    s.expressions
        .iter()
        .map(|e| match e {
            Expression::Alias(a) => Ok(a.alias.name.clone()),
            Expression::Column(c) => Ok(c.name.name.clone()),
            _ => Err(shape()),
        })
        .collect()
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
