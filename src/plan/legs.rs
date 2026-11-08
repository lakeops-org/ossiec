//! Join trees: the root a request is computed from, the many-to-one joins from
//! it to every dataset it reads, and one tree per independent fact.

use std::collections::BTreeSet;

use crate::graph::{SemanticGraph, Term};

use crate::{Error, Result};

use super::{alias, req_err, JoinKind, JoinStep, Leg, Node};

/// Wide table §5.4: one leg per home dataset, joined to every dimension and
/// filter. [`wide_table::check`] has refused a request where a home has no single
/// path to something it needs.
pub(super) fn branches(
    g: &SemanticGraph,
    required: &BTreeSet<Node>,
    terms: &[Term],
) -> Result<(Vec<Leg>, Vec<usize>)> {
    let mut homes: Vec<(usize, BTreeSet<Node>)> = vec![];
    let mut term_legs = vec![];
    for t in terms {
        let home = grain_of(g, &t.datasets)?;
        let leg = homes
            .iter()
            .position(|(h, _)| *h == home)
            .unwrap_or_else(|| {
                homes.push((home, required.clone()));
                homes.len() - 1
            });
        // A deferred aggregate also reads the datasets its home joins to.
        homes[leg].1.extend(t.datasets.iter().map(|&d| (d, None)));
        term_legs.push(leg);
    }
    let legs = homes
        .iter()
        .map(|(home, nodes)| tree_from(g, *home, nodes))
        .collect::<Result<_>>()?;
    Ok((legs, term_legs))
}

/// One join tree for everything, or, when no dataset reaches all of `required`
/// (independent facts sharing dimensions), one per root: each term gets the tree
/// for its own datasets plus every dimension and filter. Returns the legs and each
/// term's leg.
pub(super) fn plan_legs(
    g: &SemanticGraph,
    required: &BTreeSet<Node>,
    terms: &[Term],
) -> Result<(Vec<Leg>, Vec<usize>)> {
    let mut all = required.clone();
    for t in terms {
        all.extend(t.datasets.iter().map(|&d| (d, None)));
    }
    let single = join_tree(g, &all);
    let independent =
        matches!(&single, Err(Error::Unsupported(m)) if m.contains("independent facts"));
    if !independent || terms.is_empty() {
        return Ok((vec![single?], vec![0; terms.len()]));
    }
    // One leg per root: terms whose own tree roots at the same fact share it.
    let mut by_root: Vec<(usize, BTreeSet<Node>)> = vec![];
    let mut term_roots = vec![];
    for t in terms {
        let mut req = required.clone();
        req.extend(t.datasets.iter().map(|&d| (d, None)));
        let root = join_tree(g, &req)?.root;
        match by_root.iter_mut().find(|(r, _)| *r == root) {
            Some((_, nodes)) => nodes.extend(req),
            None => by_root.push((root, req)),
        }
        term_roots.push(root);
    }
    let legs = by_root
        .iter()
        .map(|(_, nodes)| join_tree(g, nodes))
        .collect::<Result<Vec<_>>>()?;
    let term_legs = term_roots
        .iter()
        .map(|r| by_root.iter().position(|(x, _)| x == r).expect("leg"))
        .collect();
    Ok((legs, term_legs))
}

/// The dataset every required node hangs off, and the joins to reach them.
pub(super) fn join_tree(g: &SemanticGraph, required: &BTreeSet<Node>) -> Result<Leg> {
    let plain: BTreeSet<usize> = required
        .iter()
        .map(|&(ds, role)| match role {
            None => ds,
            Some(r) => g.rels[r].from, // a role needs its relationship's source
        })
        .collect();
    let names = |s: &BTreeSet<usize>| {
        s.iter()
            .map(|&d| format!("`{}`", g.datasets[d].name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let all_paths: Vec<_> = (0..g.datasets.len()).map(|r| g.paths_from(r)).collect();
    let reaches = |r: usize| {
        plain
            .iter()
            .all(|t| *t == r || all_paths[r].contains_key(t))
    };
    let candidates: BTreeSet<usize> = (0..g.datasets.len()).filter(|&r| reaches(r)).collect();
    if candidates.is_empty() {
        return Err(Error::Unsupported(format!(
            "no dataset joins to all of {} along many-to-one relationships, and they don't split into independent facts sharing the requested dimensions",
            names(&plain)
        )));
    }
    let inside: Vec<usize> = candidates.intersection(&plain).copied().collect();
    let root = match inside.as_slice() {
        [one] => *one,
        [] => {
            // A common root outside the request, e.g. a fact joining two requested dimensions.
            let deepest: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&c| {
                    candidates
                        .iter()
                        .all(|&o| o == c || all_paths[o].contains_key(&c))
                })
                .collect();
            match deepest.as_slice() {
                [one] => *one,
                _ => {
                    return Err(Error::Unsupported(format!(
                        "{} are joined by several facts ({}); with no measure to say which, the request is ambiguous (independent facts need a measure from each)",
                        names(&plain),
                        names(&candidates)
                    )))
                }
            }
        }
        _ => {
            return Err(Error::Unsupported(format!(
                "relationships between {} form a cycle",
                names(&plain)
            )))
        }
    };
    tree_from(g, root, required)
}

/// The joins from `root` to every required node.
///
/// A plain dataset reachable more than one way takes the unique shortest path
/// (Layer 2: `date_dim` from `store_sales` is the sale date, not the customer's
/// first-sale date two hops away). A tie is an error that suggests naming the role.
/// Layer 3 has already refused both. A role node `(dataset, rel)` is reached through
/// `rel`, under its own alias.
pub(super) fn tree_from(g: &SemanticGraph, root: usize, required: &BTreeSet<Node>) -> Result<Leg> {
    let root_paths = g.paths_from(root);
    let show = |p: &Vec<usize>| {
        p.iter()
            .map(|&r| g.rels[r].name.as_str())
            .collect::<Vec<_>>()
            .join(" → ")
    };
    // The default path to `t`: the only one, or the unique shortest.
    let path_to = |t: usize| -> Result<Vec<usize>> {
        let paths = &root_paths[&t];
        let shortest = paths.iter().map(Vec::len).min().unwrap_or(0);
        let best: Vec<&Vec<usize>> = paths.iter().filter(|p| p.len() == shortest).collect();
        match best.as_slice() {
            [one] => Ok((*one).clone()),
            _ => Err(req_err(format!(
                "`{}` is reachable from `{}` in more than one way ({}); name the one you mean as `<relationship>.<field>`, e.g. `{}.<field>`",
                g.datasets[t].name,
                g.datasets[root].name,
                best.iter().map(|p| show(p)).collect::<Vec<_>>().join(" | "),
                g.rels[*best[0].last().expect("non-empty path")].name
            ))),
        }
    };

    // (depth, step)
    let mut steps: Vec<(usize, JoinStep)> = vec![];
    let add_path = |path: &[usize], steps: &mut Vec<(usize, JoinStep)>| -> Result<()> {
        for (depth, &r) in path.iter().enumerate() {
            let rel = &g.rels[r];
            match steps.iter().find(|(_, s)| s.to_alias == g.datasets[rel.to].name) {
                Some((_, s)) if s.rel == Some(r) => {}
                Some((_, s)) => {
                    return Err(req_err(format!(
                        "`{}` is reached both through `{}` and `{}`; name the role you mean as `<relationship>.<field>`",
                        g.datasets[rel.to].name,
                        g.rels[s.rel.expect("planned relationship join")].name,
                        rel.name
                    )))
                }
                None => steps.push((
                    depth,
                    JoinStep {
                        rel: Some(r),
                        from_alias: g.datasets[rel.from].name.clone(),
                        to: rel.to,
                        to_alias: g.datasets[rel.to].name.clone(),
                        kind: JoinKind::Left,
                        reverse: false,
                    },
                )),
            }
        }
        Ok(())
    };
    for &(ds, role) in required {
        let target = match role {
            None => ds,
            Some(r) => g.rels[r].from,
        };
        let path = if target == root {
            vec![]
        } else {
            path_to(target)?
        };
        add_path(&path, &mut steps)?;
        if let Some(r) = role {
            let to_alias = alias(g, (ds, role));
            if !steps.iter().any(|(_, s)| s.to_alias == to_alias) {
                let from_alias = g.datasets[g.rels[r].from].name.clone();
                steps.push((
                    path.len(),
                    JoinStep {
                        rel: Some(r),
                        from_alias,
                        to: ds,
                        to_alias,
                        kind: JoinKind::Left,
                        reverse: false,
                    },
                ));
            }
        }
    }
    steps.sort_by(|(da, a), (db, b)| (da, a.rel, &a.to_alias).cmp(&(db, b.rel, &b.to_alias)));
    Ok(Leg {
        root,
        joins: steps.into_iter().map(|(_, s)| s).collect(),
        derived: vec![],
        narrows_on: None,
    })
}

/// The dataset among `datasets` that reaches all the others: the term's row grain.
pub(crate) fn grain_of(g: &SemanticGraph, datasets: &BTreeSet<usize>) -> Result<usize> {
    datasets
        .iter()
        .copied()
        .find(|&d| {
            let p = g.paths_from(d);
            datasets.iter().all(|o| *o == d || p.contains_key(o))
        })
        .ok_or_else(|| {
            Error::Unsupported(format!(
                "an aggregate reads {} but none of them joins to the others",
                datasets
                    .iter()
                    .map(|&d| format!("`{}`", g.datasets[d].name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })
}
