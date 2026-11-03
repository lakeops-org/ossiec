//! Nested Layer 2: derived tables (Appendix A.2–A.3).
//!
//! **One recursive compile, no nested [`Request`] / [`LogicalPlan`].** Every
//! `AGG` stage is the flat pipeline (request → `plan` → `lower` → FILTER lowering →
//! per-engine `adapt`) run with *its own* plan, because `adapt` reads the plan
//! (Postgres string collation, MySQL string expressions). A derived table is not
//! spliced into the plan as SQL text:
//!
//! 1. The outer stage plans and lowers with a **placeholder relation**
//!    (`__ossiec_derivN`) joined through a [`plan::DerivedJoin`] on its leg, so the
//!    join exists in every part `lower` builds (base, stacked, `PkDedup`).
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
use polyglot_sql::{generate, DialectType, Expression};

use super::{
    apply_consumer_joins, apply_select_formulas, parse_stage, parse_top_select,
    planned_alias_on_leg, rename_consumer_outputs, unsupported, StageCfg,
};
use crate::graph::{Ctx, SemanticGraph};
use crate::plan::{self, DerivedJoin, Request};
use crate::{lower_stage, render_compiled, sql, Compiled, Error, Options, Result};

/// Nesting depth: the top query is 0, its derived tables 1, theirs 2, …
pub(super) const MAX_DEPTH: usize = 4;
/// Derived tables in one query, at any depth.
pub(super) const MAX_DERIVED: usize = 8;

/// What a column of a stage's result is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OutKind {
    /// A dimension / grouping column: a value of a field.
    Field,
    /// An `AGG(measure)` or SQL aggregate, already evaluated to a scalar.
    EvaluatedMeasure,
}

/// One consumer-visible output column of a [`Node`].
#[derive(Debug, Clone)]
pub(super) struct OutCol {
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
    pub graph: &'a SemanticGraph,
    pub opts: &'a Options,
    pub ctx: Ctx,
    pub derived_count: usize,
    /// The flat top-level request, when the query has no derived tables.
    flat_request: Option<Request>,
}

pub(super) fn compile(graph: &SemanticGraph, sql_text: &str, opts: &Options) -> Result<Compiled> {
    let sel = parse_top_select(sql_text)?;
    let mut c = Compiler {
        graph,
        opts,
        ctx: Ctx {
            prefer: opts.prefer_dialect.clone(),
            ..Default::default()
        },
        derived_count: 0,
        flat_request: None,
    };
    let node = c.select(&sel, 0, false, &[])?;
    let ast = resolve(node)?;
    // Flat queries keep their Layer 3 request as identity; nested ones hash the
    // normalized consumer SQL.
    let identity = match c.flat_request.take() {
        Some(req) if c.derived_count == 0 => serde_json::json!(req),
        _ => {
            let text = generate(&Expression::Select(Box::new(sel)), DialectType::Generic)
                .map_err(|e| Error::Internal(e.to_string()))?;
            serde_json::json!({ "layer2_sql": text })
        }
    };
    render_compiled(graph, identity, ast, c.ctx, opts)
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
    /// `FROM <derived> [JOIN <derived> ...]`: ordinary SQL over derived tables.
    Shell,
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
        match classify(sel)? {
            Kind::Agg => self.agg_stage(sel, depth, derived_mode, outer),
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
    fn agg_stage(
        &mut self,
        sel: &Select,
        depth: usize,
        derived_mode: bool,
        outer: &[String],
    ) -> Result<Node> {
        let graph = self.graph;
        let parsed = parse_stage(
            graph,
            sel,
            &StageCfg {
                derived: derived_mode,
                outer,
            },
        )?;
        if depth == 0 && parsed.derived.is_empty() {
            self.flat_request = Some(parsed.request.clone());
        }

        // Nested queries see this stage's aliases as "outer" (correlation check).
        let mut inner_outer = outer.to_vec();
        inner_outer.extend(parsed.scope_aliases.iter().cloned());

        let mut pending = vec![];
        // (placeholder, derived columns as emitted) per derived table.
        let mut prepared: Vec<(String, Vec<String>)> = vec![];
        let aggregates = !parsed.request.metrics.is_empty();
        for spec in &parsed.derived {
            let (placeholder, node) = self.derived(&spec.select, depth + 1, &inner_outer)?;
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
                rel_cols.push(node.outputs[i].name.clone());
                key_idx.push(i);
            }
            if aggregates {
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

        let mut logical = plan::plan(graph, &parsed.request, &mut self.ctx)?;
        apply_consumer_joins(graph, &mut logical, &parsed.consumer_joins, &mut self.ctx)?;
        if !parsed.derived.is_empty() {
            if logical.legs.len() != 1 {
                return Err(unsupported(
                    "Layer 2 derived-table JOIN with measures from independent facts (A.4) is not supported; a derived table restricts a single join tree",
                ));
            }
            if logical.include_unmatched {
                return Err(unsupported(
                    "Layer 2 derived-table JOIN cannot be combined with includeUnmatched",
                ));
            }
            for (spec, (placeholder, rel_cols)) in parsed.derived.iter().zip(prepared) {
                let from_alias = planned_alias_on_leg(graph, &logical.legs[0], spec.attach_ds)
                    .ok_or_else(|| {
                        unsupported(format!(
                            "Layer 2 derived-table JOIN `{}`: dataset `{}` is not part of the planned query",
                            spec.alias, graph.datasets[spec.attach_ds].name
                        ))
                    })?;
                logical.legs[0].derived.push(DerivedJoin {
                    from_alias,
                    from_fields: spec.from_fields.clone(),
                    rel_placeholder: placeholder,
                    rel_alias: spec.alias.clone(),
                    rel_cols,
                    null_safe: spec.null_safe,
                    kind: spec.kind,
                });
            }
        }
        apply_select_formulas(graph, &mut logical, &parsed.measure_outputs)?;
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
