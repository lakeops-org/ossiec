//! The semantic request (Layer 3's query): metrics, dimensions, filters, order and
//! limit, and how it reads and writes as JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::wide_table::ErrorCode;
use crate::{Error, Result};

/// A semantic query: which metrics, sliced by which dimensions.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    /// Ignored by the compiler; lets callers keep the target model in the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `metric` (model-scoped) or `dataset.metric` (dataset-scoped), or an object
    /// `{"metric": ..., "alias": ...}`. Empty to list the distinct values of
    /// `dimensions` instead.
    #[serde(
        default,
        deserialize_with = "metrics_from_json",
        serialize_with = "metrics_to_json"
    )]
    pub metrics: Vec<Selected>,
    /// `dataset.field`, or `relationship.field` when that dataset plays the role, or
    /// an object `{"field": ..., "alias": ...}`.
    #[serde(
        default,
        deserialize_with = "dimensions_from_json",
        serialize_with = "dimensions_to_json"
    )]
    pub dimensions: Vec<Selected>,
    /// One row predicate over one dataset's fields (wide table §3.4), applied in
    /// every branch: `customers.region <> 'WEST'`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// Structured predicates, all applied in every branch. Unlike `filter` they
    /// can read several datasets: each must be reachable from every metric.
    #[serde(default)]
    pub filters: Vec<Filter>,
    /// Conditions on requested metrics (SQL `HAVING`).
    #[serde(default)]
    pub metric_filters: Vec<MetricFilter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_grain: Option<TimeGrain>,
    /// Which requested dimension `timeGrain` applies to, when several are time dimensions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_dimension: Option<String>,
    #[serde(default)]
    pub order_by: Vec<OrderBy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
    /// Answer the model-scoped metrics the spec defers (wide table §5.2): windows,
    /// computed over the grouped result, and aggregates reading several datasets,
    /// computed from the one that reaches the others. Without this they raise
    /// `E_DEFERRED_WINDOWED_METRIC` and `E_AMBIGUOUS_MEASURE_GRAIN`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deferred_metrics: bool,
    /// Layer 2 only: also count one-side rows no fact row reaches (products never
    /// sold), as a group of their own where the fact's dimensions are NULL (relational
    /// semantics §10.2). [`crate::compile`] raises `E_INVALID_QUERY`: in Layer 3 a
    /// metric already reads all the rows of its home dataset.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub include_unmatched: bool,
    /// Datasets that must appear in the join tree even when no selected
    /// dimension, filter or metric names them. Used by the AGG SQL frontend so a
    /// derived-table JOIN can attach to `FROM customers …` while the measures
    /// live on another fact. Not part of the JSON request surface (`serde(skip)`).
    #[serde(skip)]
    #[doc(hidden)]
    pub ensure_datasets: Vec<usize>,
}

/// One entry of `dimensions` or `metrics`. In Layer 3 its output name is the alias
/// if given, or the final part of the reference (wide table §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    pub name: String,
    pub alias: Option<String>,
}

impl From<&str> for Selected {
    fn from(name: &str) -> Self {
        name.to_string().into()
    }
}

impl From<String> for Selected {
    fn from(name: String) -> Self {
        Selected { name, alias: None }
    }
}

impl PartialEq<&str> for Selected {
    fn eq(&self, other: &&str) -> bool {
        self.alias.is_none() && self.name == *other
    }
}

impl std::ops::Deref for Selected {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

fn selected<'de, D: serde::Deserializer<'de>>(
    d: D,
    key: &str,
) -> std::result::Result<Vec<Selected>, D::Error> {
    use serde::de::Error as _;
    let entries = Vec::<Value>::deserialize(d)?;
    entries
        .into_iter()
        .map(|entry| match entry {
            Value::String(name) => Ok(name.into()),
            Value::Object(o) => {
                if let Some(k) = o.keys().find(|k| *k != key && *k != "alias") {
                    return Err(D::Error::custom(format!("unknown property `{k}`")));
                }
                let text = |k: &str| match o.get(k) {
                    None => Ok(None),
                    Some(Value::String(s)) if !s.is_empty() => Ok(Some(s.clone())),
                    Some(_) => Err(D::Error::custom(format!(
                        "`{k}` must be a non-empty string"
                    ))),
                };
                Ok(Selected {
                    name: text(key)?.ok_or_else(|| D::Error::custom(format!("missing `{key}`")))?,
                    alias: text("alias")?,
                })
            }
            other => Err(D::Error::custom(format!(
                "expected a reference or an object with `{key}`, got {other}"
            ))),
        })
        .collect()
}

fn dimensions_from_json<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Selected>, D::Error> {
    selected(d, "field")
}

fn metrics_from_json<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Vec<Selected>, D::Error> {
    selected(d, "metric")
}

/// A plain string without an alias, so a request's JSON and hash don't change.
fn selected_to_json<S: serde::Serializer>(
    entries: &[Selected],
    key: &str,
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    let json: Vec<Value> = entries
        .iter()
        .map(|e| match &e.alias {
            None => Value::String(e.name.clone()),
            Some(alias) => serde_json::json!({ key: e.name, "alias": alias }),
        })
        .collect();
    json.serialize(s)
}

fn dimensions_to_json<S: serde::Serializer>(
    entries: &[Selected],
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    selected_to_json(entries, "field", s)
}

fn metrics_to_json<S: serde::Serializer>(
    entries: &[Selected],
    s: S,
) -> std::result::Result<S::Ok, S::Error> {
    selected_to_json(entries, "metric", s)
}

/// The final part of a reference.
pub(crate) fn final_part(name: &str) -> &str {
    unquoted(name.rsplit('.').next().unwrap_or(name))
}

/// A name without the quotes the model or a request wrote it in.
pub(crate) fn unquoted(name: &str) -> &str {
    name.strip_prefix('"')
        .and_then(|n| n.strip_suffix('"'))
        .filter(|n| !n.is_empty())
        .unwrap_or(name)
}

impl Request {
    /// Read a request from JSON. A malformed one, or one with an unknown or `null`
    /// property, raises `E_INVALID_QUERY` (wide table §3.7).
    pub fn from_json(text: &str) -> Result<Request> {
        let invalid = |message: String| Error::Query {
            code: ErrorCode::InvalidQuery,
            message,
        };
        let value: Value = serde_json::from_str(text).map_err(|e| invalid(e.to_string()))?;
        if let Some((key, _)) = value
            .as_object()
            .and_then(|o| o.iter().find(|(_, v)| v.is_null()))
        {
            return Err(invalid(format!("`{key}` is null")));
        }
        serde_json::from_value(value).map_err(|e| invalid(e.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    pub field: String,
    pub op: Op,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricFilter {
    pub metric: String,
    pub op: Op,
    pub value: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderBy {
    pub field: String,
    #[serde(default)]
    pub direction: Direction,
    /// Where NULLs go. Default: first when ascending, last when descending,
    /// on every engine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nulls: Option<Nulls>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Nulls {
    First,
    Last,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    #[serde(alias = "ASC")]
    Asc,
    #[serde(alias = "DESC")]
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    #[serde(rename = "=")]
    Eq,
    #[serde(rename = "!=", alias = "<>")]
    Ne,
    #[serde(rename = "<")]
    Lt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = "IN", alias = "in")]
    In,
    #[serde(rename = "NOT IN", alias = "not in")]
    NotIn,
    #[serde(rename = "LIKE", alias = "like")]
    Like,
    #[serde(rename = "BETWEEN", alias = "between")]
    Between,
    #[serde(rename = "IS NULL", alias = "is null")]
    IsNull,
    #[serde(rename = "IS NOT NULL", alias = "is not null")]
    IsNotNull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeGrain {
    Day,
    Week,
    Month,
    Quarter,
    Year,
}

impl TimeGrain {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            TimeGrain::Day => "day",
            TimeGrain::Week => "week",
            TimeGrain::Month => "month",
            TimeGrain::Quarter => "quarter",
            TimeGrain::Year => "year",
        }
    }
}
