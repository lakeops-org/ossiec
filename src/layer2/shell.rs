//! The A.3 outer SQL shell: ordinary SQL aggregates over derived tables.
//!
//! ```sql
//! SELECT d.status, AVG(m.n)
//! FROM (SELECT status, customer_id FROM orders GROUP BY status, customer_id) d
//! JOIN (SELECT customer_id, AGG(order_count) AS n FROM orders GROUP BY customer_id) m
//!   ON d.customer_id IS NOT DISTINCT FROM m.customer_id
//! GROUP BY d.status
//! ```
//!
//! Every `FROM` item is a derived table that already evaluated its `AGG()`s, so
//! the outer query is plain SQL over scalars: no `plan`/`lower`, no G2 rule, no
//! `AGG()` (a measure column of a derived table is already a value). Grammar:
//!
//! - select items: a derived column, or `AVG | SUM | COUNT | MIN | MAX` over one
//!   derived column, or `COUNT(*)`; each may carry an alias. Without one the
//!   output is named after the column, or `{func}_{column}` (`avg_n`,
//!   `count_star`).
//! - `FROM <derived> [INNER|LEFT|RIGHT|FULL] JOIN <derived> ON a = b [AND ...]`,
//!   or `CROSS JOIN` with no `ON`; `=` or `IS NOT DISTINCT FROM` per pair.
//! - `WHERE column <op> literal`, `HAVING aggregate <op> literal` joined by `AND`;
//!   `GROUP BY` the selected plain columns; `ORDER BY` an output name (or the
//!   item it names); `LIMIT n`.
//!
//! Engine rules that need a [`crate::plan::LogicalPlan`] (Postgres string
//! collation, MySQL string `COLLATE`) do not apply here; MySQL's unrounded `AVG`
//! does (see `dialect::adapt_shell`).

use std::collections::BTreeSet;

use polyglot_sql::expressions::{Column, Select};
use polyglot_sql::Expression;

use super::nested::{check_unique_names, Compiler, Node, OutCol, OutKind, Relation};
use super::{
    as_column, as_derived, consumer_join_kind, cross_join_on, limit_u64, table_expr, take_agg,
    unsupported,
};
use crate::dialect;
use crate::plan::JoinKind;
use crate::sql::{self, Tpl};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AggFn {
    Avg,
    Sum,
    Count,
    Min,
    Max,
}

impl AggFn {
    fn name(self) -> &'static str {
        match self {
            AggFn::Avg => "AVG",
            AggFn::Sum => "SUM",
            AggFn::Count => "COUNT",
            AggFn::Min => "MIN",
            AggFn::Max => "MAX",
        }
    }
}

/// One `FROM` / `JOIN` item: alias, the derived `SELECT`, and for a join its kind and `ON`.
/// `CROSS JOIN` carries `None` for the condition.
type ShellFrom<'a> = (
    String,
    &'a Select,
    Option<(JoinKind, Option<&'a Expression>)>,
);

/// `(relation, column)` indexes into the shell's relations.
type ColRef = (usize, usize);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Item {
    Bare(ColRef),
    /// `None`: `COUNT(*)`.
    Agg(AggFn, Option<ColRef>),
}

struct Selected {
    label: String,
    item: Item,
}

impl Compiler<'_> {
    /// An outer query over derived tables only (Appendix A.3).
    pub(super) fn shell(&mut self, sel: &Select, depth: usize, outer: &[String]) -> Result<Node> {
        if sel.with.is_some() || sel.distinct || sel.qualify.is_some() {
            return Err(unsupported(
                "Layer 2 WITH / DISTINCT / QUALIFY are not implemented yet",
            ));
        }
        let graph = self.graph;
        let from = sel.from.as_ref().expect("classified as a shell");

        // Relations: every item must be a derived table.
        let mut items: Vec<ShellFrom> = vec![];
        let (first, alias) = as_derived(&from.expressions[0])?.expect("classified as a shell");
        items.push((alias, first, None));
        for j in &sel.joins {
            let Some((select, alias)) = as_derived(&j.this)? else {
                return Err(self.not_derived(&j.this));
            };
            if !j.using.is_empty() {
                return Err(unsupported("Layer 2 JOIN USING is not supported; use ON"));
            }
            let kind = consumer_join_kind(j)?;
            if kind == JoinKind::Cross {
                if j.on.is_some() {
                    return Err(cross_join_on());
                }
                items.push((alias, select, Some((kind, None))));
                continue;
            }
            let on = j.on.as_ref().ok_or_else(|| {
                unsupported("Layer 2 JOIN between derived tables requires an ON condition")
            })?;
            items.push((alias, select, Some((kind, Some(on)))));
        }
        for (i, (a, ..)) in items.iter().enumerate() {
            if items[..i].iter().any(|(b, ..)| b.eq_ignore_ascii_case(a)) {
                return Err(Error::Request(format!(
                    "Layer 2 duplicate relation alias `{a}` in FROM"
                )));
            }
        }

        let mut inner_outer = outer.to_vec();
        inner_outer.extend(items.iter().map(|(a, ..)| a.clone()));
        let mut rels: Vec<Relation> = vec![];
        let mut pending = vec![];
        for (alias, select, _) in &items {
            let (placeholder, node) = self.derived(select, depth + 1, &inner_outer)?;
            rels.push(Relation::Derived {
                alias: alias.clone(),
                placeholder: placeholder.clone(),
                outputs: node.outputs.clone(),
            });
            pending.push((placeholder, node));
        }
        let scope = Scope { rels: &rels, outer };

        // Select list.
        let mut selected: Vec<Selected> = vec![];
        for e in &sel.expressions {
            let (inner, alias) = match e {
                Expression::Alias(a) => (&a.this, Some(a.alias.name.clone())),
                other => (other, None),
            };
            let item = scope.item(inner, graph)?;
            let label = alias.unwrap_or_else(|| scope.default_label(&item));
            selected.push(Selected { label, item });
        }
        if selected.is_empty() {
            return Err(Error::Request(
                "Layer 2 query must select at least one column".into(),
            ));
        }
        let labels: Vec<OutCol> = selected
            .iter()
            .map(|s| OutCol {
                name: s.label.clone(),
                kind: OutKind::Field,
            })
            .collect();
        check_unique_names(&labels).map_err(|_| {
            Error::Request("Layer 2 query selects two columns with the same name; alias one".into())
        })?;

        // GROUP BY must be exactly the selected plain columns.
        let bare: BTreeSet<ColRef> = selected
            .iter()
            .filter_map(|s| match s.item {
                Item::Bare(c) => Some(c),
                Item::Agg(..) => None,
            })
            .collect();
        let has_agg = selected.iter().any(|s| matches!(s.item, Item::Agg(..)));
        let group: Option<BTreeSet<ColRef>> = match &sel.group_by {
            None => None,
            Some(gb) => {
                if gb.all.is_some() {
                    return Err(unsupported("GROUP BY ALL is not supported in Layer 2"));
                }
                let mut set = BTreeSet::new();
                for e in &gb.expressions {
                    let col = as_column(e).ok_or_else(|| {
                        unsupported("Layer 2 GROUP BY must name columns of the derived tables")
                    })?;
                    set.insert(scope.resolve(col, graph)?);
                }
                Some(set)
            }
        };
        match &group {
            None if has_agg && !bare.is_empty() => {
                return Err(Error::Request(
                    "Layer 2 query mixing plain columns and aggregates needs GROUP BY".into(),
                ));
            }
            Some(g) if *g != bare => {
                return Err(Error::Request(format!(
                    "Layer 2 GROUP BY must match the selected plain columns (got [{}], selected [{}])",
                    scope.names(g),
                    scope.names(&bare)
                )));
            }
            _ => {}
        }

        // Assemble: SELECT ... FROM <ph> [JOIN <ph> ON ...] [WHERE] [GROUP BY] [HAVING] [ORDER BY] [LIMIT].
        let native_null_safe = dialect::supports_null_safe_eq(self.opts.target);
        let mut t = Tpl::new();
        let cols: Vec<String> = selected
            .iter()
            .map(|s| {
                let e = scope.expr(&s.item, &mut t)?;
                let a = t.alias(&s.label);
                Ok(format!("{e} AS {a}"))
            })
            .collect::<Result<_>>()?;
        let mut text = format!("SELECT {}", cols.join(", "));
        for (k, rel) in rels.iter().enumerate() {
            let Relation::Derived {
                alias, placeholder, ..
            } = rel
            else {
                unreachable!("a shell holds derived tables only")
            };
            let r = t.e(sql::placeholder_relation(placeholder, alias));
            match &items[k].2 {
                None => text.push_str(&format!(" FROM {r}")),
                Some((JoinKind::Cross, None)) => text.push_str(&format!(" CROSS JOIN {r}")),
                Some((JoinKind::Cross, Some(_))) => return Err(cross_join_on()),
                Some((kind, Some(on))) => {
                    let cond = scope.join_condition(on, k, graph, &mut t, native_null_safe)?;
                    text.push_str(&format!(" {} {r} ON {cond}", kind.sql()));
                }
                Some((_, None)) => {
                    return Err(unsupported(
                        "Layer 2 JOIN between derived tables requires an ON condition",
                    ));
                }
            }
        }
        if let Some(w) = &sel.where_clause {
            text.push_str(&format!(
                " WHERE {}",
                scope.predicate(&w.this, false, graph, &mut t)?
            ));
        }
        if let Some(g) = &group {
            if !g.is_empty() {
                let keys: Vec<String> = g.iter().map(|&c| t.e(scope.column(c))).collect();
                text.push_str(&format!(" GROUP BY {}", keys.join(", ")));
            }
        }
        if let Some(h) = &sel.having {
            text.push_str(&format!(
                " HAVING {}",
                scope.predicate(&h.this, true, graph, &mut t)?
            ));
        }
        if let Some(ob) = &sel.order_by {
            let keys: Vec<String> = ob
                .expressions
                .iter()
                .map(|o| {
                    let label = scope.order_label(&o.this, &selected, graph)?;
                    let a = t.alias(label);
                    let dir = if o.desc { "DESC" } else { "ASC" };
                    let nulls = match o.nulls_first {
                        Some(true) => " NULLS FIRST",
                        Some(false) => " NULLS LAST",
                        None => "",
                    };
                    Ok(format!("{a} {dir}{nulls}"))
                })
                .collect::<Result<_>>()?;
            text.push_str(&format!(" ORDER BY {}", keys.join(", ")));
        }
        if let Some(l) = &sel.limit {
            if l.percent {
                return Err(unsupported("Layer 2 LIMIT PERCENT is not supported"));
            }
            text.push_str(&format!(" LIMIT {}", limit_u64(&l.this)?));
        }
        t.push(&text);
        let mut ast = t.build()?;
        if let Some(target) = self.opts.target {
            ast = dialect::adapt_shell(ast, target)?;
        }

        let outputs: Vec<OutCol> = selected
            .iter()
            .map(|s| OutCol {
                name: s.label.clone(),
                kind: match s.item {
                    Item::Bare((r, c)) => scope.out(r, c).kind.clone(),
                    Item::Agg(..) => OutKind::EvaluatedMeasure,
                },
            })
            .collect();
        let bare_idx: Vec<usize> = selected
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s.item, Item::Bare(_)))
            .map(|(i, _)| i)
            .collect();
        let unique_key = if group.is_some() || (has_agg && bare_idx.is_empty()) {
            Some(bare_idx)
        } else {
            None
        };
        Ok(Node {
            ast,
            outputs,
            unique_key,
            pending,
        })
    }

    /// A shell `FROM` item that is not a derived table.
    fn not_derived(&self, e: &Expression) -> Error {
        match table_expr(self.graph, e) {
            Ok((dataset, alias)) => {
                let alias = alias.unwrap_or_else(|| self.graph.datasets[dataset].name.clone());
                unsupported(format!(
                    "Layer 2 outer query over derived tables cannot also read the model dataset {}: ordinary SQL does not run over raw model tables; use a derived table (SELECT ... FROM {} ...)",
                    Relation::Dataset { dataset, alias }.describe(self.graph),
                    self.graph.datasets[dataset].name
                ))
            }
            Err(e) => e,
        }
    }
}

/// Name resolution over the shell's derived tables.
struct Scope<'a> {
    rels: &'a [Relation],
    outer: &'a [String],
}

impl Scope<'_> {
    fn outputs(&self, r: usize) -> &[OutCol] {
        match &self.rels[r] {
            Relation::Derived { outputs, .. } => outputs,
            Relation::Dataset { .. } => &[],
        }
    }

    fn out(&self, r: usize, c: usize) -> &OutCol {
        &self.outputs(r)[c]
    }

    fn names(&self, cols: &BTreeSet<ColRef>) -> String {
        cols.iter()
            .map(|&(r, c)| format!("{}.{}", self.rels[r].alias(), self.out(r, c).name))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// `alias.column` / `column` → indexes, for plain SQL: an unevaluated measure
    /// (§7.7) is not a value, so only `AGG()` could read it.
    fn resolve(&self, col: &Column, graph: &crate::SemanticGraph) -> Result<ColRef> {
        let (r, c) = self.resolve_raw(col, graph)?;
        if matches!(self.out(r, c).kind, OutKind::UnevaluatedMeasure { .. }) {
            return Err(unsupported(format!(
                "Layer 2 `{}.{}` is an unevaluated measure (relational_semantics §7.7): plain SQL cannot read it, only an outer AGG() evaluates it",
                self.rels[r].alias(),
                self.out(r, c).name
            )));
        }
        Ok((r, c))
    }

    /// `alias.column` / `column` → indexes, whatever the column is.
    fn resolve_raw(&self, col: &Column, graph: &crate::SemanticGraph) -> Result<ColRef> {
        let name = &col.name.name;
        let find = |r: usize| {
            self.outputs(r)
                .iter()
                .position(|o| o.name.eq_ignore_ascii_case(name))
        };
        if let Some(t) = &col.table {
            let Some(r) = self
                .rels
                .iter()
                .position(|x| x.alias().eq_ignore_ascii_case(&t.name))
            else {
                if self.outer.iter().any(|a| a.eq_ignore_ascii_case(&t.name)) {
                    return Err(unsupported(format!(
                        "Layer 2 correlated subquery: `{}.{name}` refers to an enclosing query",
                        t.name
                    )));
                }
                return Err(Error::Request(format!(
                    "column `{}.{name}` does not refer to a relation in FROM/JOIN ({})",
                    t.name,
                    self.describe(graph)
                )));
            };
            return find(r).map(|c| (r, c)).ok_or_else(|| {
                Error::Request(format!(
                    "derived table `{}` has no column `{name}`: {}",
                    t.name,
                    self.rels[r].describe(graph)
                ))
            });
        }
        let hits: Vec<ColRef> = (0..self.rels.len())
            .filter_map(|r| find(r).map(|c| (r, c)))
            .collect();
        match hits.as_slice() {
            [one] => Ok(*one),
            [] => Err(Error::Request(format!(
                "unknown column `{name}`; {}",
                self.describe(graph)
            ))),
            _ => Err(Error::Request(format!(
                "column `{name}` is ambiguous; qualify it with a derived-table alias"
            ))),
        }
    }

    fn describe(&self, graph: &crate::SemanticGraph) -> String {
        self.rels
            .iter()
            .map(|r| r.describe(graph))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn column(&self, (r, c): ColRef) -> Expression {
        sql::qualified_quoted_col(self.rels[r].alias(), &self.out(r, c).name)
    }

    /// A select / HAVING / ORDER BY item: a derived column or an aggregate of one.
    fn item(&self, e: &Expression, graph: &crate::SemanticGraph) -> Result<Item> {
        if let Some(arg) = take_agg(e)? {
            let col = as_column(arg);
            let shown = col.map_or("the column".to_string(), |c| format!("`{}`", c.name.name));
            if let Some(Ok((r, c))) = col.map(|c| self.resolve_raw(c, graph)) {
                if matches!(self.out(r, c).kind, OutKind::UnevaluatedMeasure { .. }) {
                    return Err(unsupported(format!(
                        "Layer 2 AGG() over unevaluated measure {shown} of a derived table (relational_semantics §7.7) needs a sole derived FROM (folded) or `FROM <dataset> JOIN (...) t ON ...`; joining several derived tables only reads evaluated columns"
                    )));
                }
            }
            return Err(unsupported(format!(
                "Layer 2 AGG() applies to measures of a model dataset; {shown} of a derived table is already evaluated: use AVG/SUM/COUNT/MIN/MAX over it"
            )));
        }
        if let Some(col) = as_column(e) {
            return Ok(Item::Bare(self.resolve(col, graph)?));
        }
        let simple = |this: &Expression, distinct: bool, filter: bool, f: AggFn| -> Result<Item> {
            if distinct || filter {
                return Err(unsupported(format!(
                    "Layer 2 outer {}() does not take DISTINCT or FILTER",
                    f.name()
                )));
            }
            let col = as_column(this).ok_or_else(|| {
                unsupported(format!(
                    "Layer 2 outer {}() takes one derived column",
                    f.name()
                ))
            })?;
            Ok(Item::Agg(f, Some(self.resolve(col, graph)?)))
        };
        match e {
            Expression::Avg(f) => simple(&f.this, f.distinct, f.filter.is_some(), AggFn::Avg),
            Expression::Sum(f) => simple(&f.this, f.distinct, f.filter.is_some(), AggFn::Sum),
            Expression::Min(f) => simple(&f.this, f.distinct, f.filter.is_some(), AggFn::Min),
            Expression::Max(f) => simple(&f.this, f.distinct, f.filter.is_some(), AggFn::Max),
            Expression::Count(c) if c.star => {
                if c.filter.is_some() || c.distinct {
                    return Err(unsupported("Layer 2 outer COUNT(*) does not take FILTER"));
                }
                Ok(Item::Agg(AggFn::Count, None))
            }
            Expression::Count(c) => match &c.this {
                Some(this) => simple(this, c.distinct, c.filter.is_some(), AggFn::Count),
                None => Ok(Item::Agg(AggFn::Count, None)),
            },
            _ => Err(unsupported(
                "Layer 2 outer select list may contain derived columns and AVG/SUM/COUNT/MIN/MAX over one derived column (or COUNT(*))",
            )),
        }
    }

    fn default_label(&self, item: &Item) -> String {
        match item {
            Item::Bare((r, c)) => self.out(*r, *c).name.clone(),
            Item::Agg(f, Some((r, c))) => {
                format!(
                    "{}_{}",
                    f.name().to_ascii_lowercase(),
                    self.out(*r, *c).name
                )
            }
            Item::Agg(f, None) => format!("{}_star", f.name().to_ascii_lowercase()),
        }
    }

    /// SQL for an item, its column(s) as placeholders of `t`.
    fn expr(&self, item: &Item, t: &mut Tpl) -> Result<String> {
        Ok(match item {
            Item::Bare(c) => t.e(self.column(*c)),
            Item::Agg(f, Some(c)) => format!("{}({})", f.name(), t.e(self.column(*c))),
            Item::Agg(f, None) => format!("{}(*)", f.name()),
        })
    }

    /// `a = b` / `a IS NOT DISTINCT FROM b` pairs for relation `k` joining earlier ones.
    fn join_condition(
        &self,
        on: &Expression,
        k: usize,
        graph: &crate::SemanticGraph,
        t: &mut Tpl,
        native: bool,
    ) -> Result<String> {
        let mut pairs = vec![];
        collect_pairs(on, &mut pairs)?;
        let mut conds = vec![];
        for (l, r, null_safe) in pairs {
            let (a, b) = (self.resolve(&l, graph)?, self.resolve(&r, graph)?);
            let ok = (a.0 == k && b.0 < k) || (b.0 == k && a.0 < k);
            if !ok {
                return Err(unsupported(format!(
                    "Layer 2 JOIN ON must compare a column of `{}` with a column of a derived table before it",
                    self.rels[k].alias()
                )));
            }
            let (x, y) = (self.column(a), self.column(b));
            conds.push(if null_safe {
                t.e(sql::null_safe_eq(x, y, native)?)
            } else {
                format!("{} = {}", t.e(x), t.e(y))
            });
        }
        Ok(conds.join(" AND "))
    }

    /// `WHERE` (columns) or `HAVING` (aggregates) as AND of `lhs <op> literal`.
    fn predicate(
        &self,
        e: &Expression,
        having: bool,
        graph: &crate::SemanticGraph,
        t: &mut Tpl,
    ) -> Result<String> {
        let bad = || {
            unsupported(if having {
                "Layer 2 outer HAVING accepts `AVG/SUM/COUNT/MIN/MAX(column) <op> literal` joined by AND"
            } else {
                "Layer 2 outer WHERE accepts `column <op> literal` and IS [NOT] NULL joined by AND"
            })
        };
        let lhs = |e: &Expression, t: &mut Tpl| -> Result<String> {
            let item = self.item(e, graph)?;
            match (&item, having) {
                (Item::Bare(_), false) | (Item::Agg(..), true) => self.expr(&item, t),
                _ => Err(bad()),
            }
        };
        let cmp = |l: &Expression, r: &Expression, op: &str, t: &mut Tpl| -> Result<String> {
            let value = literal(r).ok_or_else(bad)?;
            let l = lhs(l, t)?;
            Ok(format!("{l} {op} {}", t.e(value)))
        };
        Ok(match e {
            Expression::And(a) => format!(
                "{} AND {}",
                self.predicate(&a.left, having, graph, t)?,
                self.predicate(&a.right, having, graph, t)?
            ),
            Expression::Eq(b) => cmp(&b.left, &b.right, "=", t)?,
            Expression::Neq(b) => cmp(&b.left, &b.right, "<>", t)?,
            Expression::Lt(b) => cmp(&b.left, &b.right, "<", t)?,
            Expression::Lte(b) => cmp(&b.left, &b.right, "<=", t)?,
            Expression::Gt(b) => cmp(&b.left, &b.right, ">", t)?,
            Expression::Gte(b) => cmp(&b.left, &b.right, ">=", t)?,
            Expression::IsNull(i) => {
                let l = lhs(&i.this, t)?;
                format!("{l} IS {}NULL", if i.not { "NOT " } else { "" })
            }
            _ => return Err(bad()),
        })
    }

    /// The output label an `ORDER BY` key refers to.
    fn order_label<'s>(
        &self,
        e: &Expression,
        selected: &'s [Selected],
        graph: &crate::SemanticGraph,
    ) -> Result<&'s str> {
        if let Some(col) = as_column(e) {
            if col.table.is_none() {
                if let Some(s) = selected.iter().find(|s| s.label == col.name.name) {
                    return Ok(&s.label);
                }
            }
        }
        let item = self.item(e, graph)?;
        selected
            .iter()
            .find(|s| s.item == item)
            .map(|s| s.label.as_str())
            .ok_or_else(|| {
                unsupported(
                    "Layer 2 outer ORDER BY must name a selected output (or the column/aggregate it selects)",
                )
            })
    }
}

fn collect_pairs(on: &Expression, out: &mut Vec<(Column, Column, bool)>) -> Result<()> {
    let bad = || {
        unsupported("Layer 2 JOIN ON must be AND of column equalities (= or IS NOT DISTINCT FROM)")
    };
    let mut pair = |l: &Expression, r: &Expression, ns: bool| -> Result<()> {
        let l = as_column(l).ok_or_else(bad)?;
        let r = as_column(r).ok_or_else(bad)?;
        out.push((l.to_owned(), r.to_owned(), ns));
        Ok(())
    };
    match on {
        Expression::Eq(b) => pair(&b.left, &b.right, false),
        Expression::NullSafeEq(b) => pair(&b.left, &b.right, true),
        Expression::And(a) => {
            collect_pairs(&a.left, out)?;
            collect_pairs(&a.right, out)
        }
        _ => Err(bad()),
    }
}

/// A numeric / string / boolean literal (optionally negative), as written.
fn literal(e: &Expression) -> Option<Expression> {
    match e {
        Expression::Literal(_) | Expression::Boolean(_) => Some(e.clone()),
        Expression::Neg(u) if matches!(u.this, Expression::Literal(_)) => Some(e.clone()),
        _ => None,
    }
}
