//! Query-local `TO MEASURE` (Appendix A.7 / §5.2 surface).
//!
//! ```sql
//! SELECT c.region,
//!        (AGG(o.order_amt) / AGG(t.ticket_count)) TO MEASURE AS sales_per_ticket
//! FROM customers c ...
//! GROUP BY c.region
//! ```
//!
//! The SQL parser does not know `TO MEASURE`, so [`extract`] cuts every
//! `TO MEASURE AS <name>` item out of the text before parsing. In its place the
//! select list gets a marker column, `__l2_tmN AS <name>`, and the expression is
//! parsed on its own and kept at index `N`. The stage compiler recognises the
//! marker ([`marker_index`]) and registers the item as a deferred (unevaluated)
//! measure: an outer `AGG(derived.<name>)` evaluates it.
//!
//! Two shapes are accepted, both at the start of a select item:
//!
//! * a parenthesized expression, `(AGG(a) / AGG(b)) TO MEASURE AS name`
//! * one base aggregate, `SUM|COUNT|AVG|MIN|MAX(...) TO MEASURE AS name`
//!   (also `(SUM(x)) TO MEASURE`)
//!
//! A bare column (`x TO MEASURE`) is refused.

use polyglot_sql::expressions::Column;
use polyglot_sql::{parse_one, DialectType, Expression};

use super::unsupported;
use crate::{Error, Result};

const MARKER: &str = "__l2_tm";

/// `SUM` / `COUNT` / `AVG` / `MIN` / `MAX`: a base aggregate in front of `TO MEASURE`.
pub(super) fn is_base_agg_word(name: &str) -> bool {
    matches!(
        name.to_ascii_uppercase().as_str(),
        "SUM" | "COUNT" | "AVG" | "MIN" | "MAX"
    )
}

/// The `TO MEASURE` index a marker column stands for.
pub(super) fn marker_index(col: &Column) -> Option<usize> {
    if col.table.is_some() {
        return None;
    }
    col.name.name.strip_prefix(MARKER)?.parse().ok()
}

/// Replace every `TO MEASURE AS <name>` item by `__l2_tmN AS <name>`.
/// Returns the rewritten SQL and the parsed expressions, indexed by `N`.
pub(super) fn extract(sql: &str) -> Result<(String, Vec<Expression>)> {
    let code = code_mask(sql);
    let words = words(sql, &code);
    let bytes = sql.as_bytes();

    let mut out = String::new();
    let mut exprs: Vec<Expression> = Vec::new();
    let mut copied = 0usize;

    let mut i = 0;
    while i + 1 < words.len() {
        let (to_s, to_e) = words[i];
        let (m_s, m_e) = words[i + 1];
        let is_pair = sql[to_s..to_e].eq_ignore_ascii_case("TO")
            && sql[m_s..m_e].eq_ignore_ascii_case("MEASURE")
            && sql[to_e..m_s].chars().all(char::is_whitespace);
        if !is_pair {
            i += 1;
            continue;
        }
        let shape = || {
            unsupported(
                "Layer 2 TO MEASURE must follow SUM|COUNT|AVG|MIN|MAX(...) or a parenthesized expression at the start of a select item: `SUM(amount) TO MEASURE AS name` or `(AGG(a) / AGG(b)) TO MEASURE AS name`",
            )
        };

        // The `)` that closes the expression (or the base aggregate's argument list).
        let close = prev_code(bytes, &code, to_s).ok_or_else(shape)?;
        if bytes[close] != b')' {
            return Err(shape());
        }
        let open = matching_open(bytes, &code, close).ok_or_else(shape)?;
        // Parenthesized form: the text inside the outer parens. Base aggregate:
        // `SUM(...)` including the function name.
        let mut expr_lo = open;
        let mut text_lo = open + 1;
        let mut text_hi = close;
        // What precedes `(`: the select list start (`SELECT`), a `,`, or a base
        // aggregate name. Any other word is not a measure.
        match prev_code(bytes, &code, open) {
            Some(p) if bytes[p] == b',' => {}
            Some(p) if bytes[p].is_ascii_alphanumeric() || bytes[p] == b'_' => {
                let mut s = p;
                while s > 0 && (bytes[s - 1].is_ascii_alphanumeric() || bytes[s - 1] == b'_') {
                    s -= 1;
                }
                let word = &sql[s..=p];
                if word.eq_ignore_ascii_case("SELECT") {
                    // `(...)` is the whole select item.
                } else if is_base_agg_word(word) {
                    expr_lo = s;
                    text_lo = s;
                    text_hi = close + 1;
                } else if word.eq_ignore_ascii_case("AGG") || word.eq_ignore_ascii_case("MEASURE") {
                    return Err(unsupported(format!(
                        "Layer 2 `{word}(...) TO MEASURE` must be parenthesized: `({word}(...) ...) TO MEASURE AS name`"
                    )));
                } else {
                    return Err(unsupported(format!(
                        "Layer 2 TO MEASURE accepts SUM|COUNT|AVG|MIN|MAX(...) or a parenthesized AGG() expression, not `{word}(...)`"
                    )));
                }
            }
            _ => return Err(shape()),
        }
        if expr_lo < copied {
            return Err(unsupported("Layer 2 TO MEASURE items cannot be nested"));
        }

        // `AS <name>` after MEASURE.
        let after = i + 2;
        let as_kw = words.get(after).copied().filter(|&(s, e)| {
            sql[s..e].eq_ignore_ascii_case("AS") && sql[m_e..s].chars().all(char::is_whitespace)
        });
        let Some((_, as_e)) = as_kw else {
            return Err(Error::Request(
                "Layer 2 TO MEASURE needs `AS <name>`".into(),
            ));
        };
        let (name_s, name_e) = identifier_after(sql, &code, as_e)
            .ok_or_else(|| Error::Request("Layer 2 TO MEASURE needs `AS <name>`".into()))?;

        let text = &sql[text_lo..text_hi];
        let expr = parse_item(text)?;
        let n = exprs.len();
        exprs.push(expr);

        out.push_str(&sql[copied..expr_lo]);
        out.push_str(&format!("{MARKER}{n} AS {}", &sql[name_s..name_e]));
        copied = name_e;
        // Skip the words the alias consumed.
        i = after + 1;
        while i < words.len() && words[i].0 < name_e {
            i += 1;
        }
    }
    out.push_str(&sql[copied..]);
    Ok((out, exprs))
}

/// Parse `text` as one select-list expression.
fn parse_item(text: &str) -> Result<Expression> {
    let bad = |e: String| Error::Request(format!("Layer 2 TO MEASURE expression: {e}"));
    let ast = parse_one(&format!("SELECT {text}"), DialectType::Generic)
        .map_err(|e| bad(e.to_string()))?;
    match ast {
        Expression::Select(s) if s.expressions.len() == 1 && s.from.is_none() => {
            let e = s.expressions[0].clone();
            if matches!(e, Expression::Alias(_)) {
                return Err(bad(
                    "do not alias the expression; name it after TO MEASURE".into()
                ));
            }
            Ok(e)
        }
        _ => Err(bad("expected a single expression".into())),
    }
}

/// `true` where the byte is SQL code (not in a string, quoted identifier or comment).
fn code_mask(sql: &str) -> Vec<bool> {
    let b = sql.as_bytes();
    let mut mask = vec![true; b.len()];
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'"' | b'`') => {
                let start = i;
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if i + 1 < b.len() && b[i + 1] == q {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                let end = (i + 1).min(b.len());
                mask[start..end].fill(false);
                i = end;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                let start = i;
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                mask[start..i].fill(false);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let start = i;
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                let end = (i + 2).min(b.len());
                mask[start..end].fill(false);
                i = end;
            }
            _ => i += 1,
        }
    }
    mask
}

/// Byte spans of the identifier-like words in code.
fn words(sql: &str, code: &[bool]) -> Vec<(usize, usize)> {
    let b = sql.as_bytes();
    let is_word = |i: usize| code[i] && (b[i].is_ascii_alphanumeric() || b[i] == b'_');
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        if is_word(i) {
            let s = i;
            while i < b.len() && is_word(i) {
                i += 1;
            }
            out.push((s, i));
        } else {
            i += 1;
        }
    }
    out
}

/// Index of the last code, non-space byte before `pos`.
fn prev_code(b: &[u8], code: &[bool], pos: usize) -> Option<usize> {
    (0..pos)
        .rev()
        .find(|&i| code[i] && !b[i].is_ascii_whitespace())
}

/// The `(` that matches the `)` at `close`.
fn matching_open(b: &[u8], code: &[bool], close: usize) -> Option<usize> {
    let mut depth = 0usize;
    for i in (0..=close).rev() {
        if !code[i] {
            continue;
        }
        match b[i] {
            b')' => depth += 1,
            b'(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// The identifier (bare or quoted) that starts after `pos`, as a byte span.
fn identifier_after(sql: &str, code: &[bool], pos: usize) -> Option<(usize, usize)> {
    let b = sql.as_bytes();
    let mut i = pos;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    match b.get(i)? {
        q @ (b'"' | b'`') => {
            i += 1;
            while i < b.len() && b[i] != *q {
                i += 1;
            }
            (i < b.len()).then_some((start, i + 1))
        }
        c if c.is_ascii_alphabetic() || *c == b'_' => {
            while i < b.len() && code[i] && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            Some((start, i))
        }
        _ => None,
    }
}
