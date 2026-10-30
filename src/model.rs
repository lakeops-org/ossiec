//! Serde types for the Apache Ossie semantic model (core spec `0.2.0.dev0`).
//!
//! These mirror `core-spec/ossie-schema.json` and nothing more: no validation,
//! no resolution. Unknown keys are ignored so additive spec changes keep loading.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    #[serde(default)]
    pub version: Option<String>,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub ai_context: Option<serde_json::Value>,
    pub datasets: Vec<Dataset>,
    #[serde(default)]
    pub relationships: Vec<Relationship>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    #[serde(default)]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dataset {
    pub name: String,
    /// A table reference (`catalog.schema.table`) or a query.
    pub source: String,
    #[serde(default)]
    pub primary_key: Vec<String>,
    #[serde(default)]
    pub unique_keys: Vec<Vec<String>>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub ai_context: Option<serde_json::Value>,
    #[serde(default)]
    pub fields: Vec<Field>,
    #[serde(default)]
    pub metrics: Vec<Metric>,
    #[serde(default)]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub expression: Expression,
    #[serde(default)]
    pub dimension: Option<Dimension>,
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub ai_context: Option<serde_json::Value>,
    #[serde(default)]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Dimension {
    #[serde(default)]
    pub is_time: Option<bool>,
}

/// `sql` is shorthand for an `OSSIE_SQL_2026` entry and wins over one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Expression {
    #[serde(default)]
    pub sql: Option<String>,
    #[serde(default)]
    pub dialects: Vec<DialectExpression>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DialectExpression {
    pub dialect: String,
    pub expression: String,
}

/// `from` is the many side, `to` the one side. Columns name fields, not source columns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relationship {
    pub name: String,
    pub from: String,
    pub to: String,
    pub from_columns: Vec<String>,
    pub to_columns: Vec<String>,
    #[serde(default)]
    pub ai_context: Option<serde_json::Value>,
    #[serde(default)]
    pub custom_extensions: Vec<CustomExtension>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub expression: Expression,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub datatype: Option<String>,
    #[serde(default)]
    pub ai_context: Option<serde_json::Value>,
    #[serde(default)]
    pub custom_extensions: Vec<CustomExtension>,
}

/// `data` is an opaque JSON string; Ossie does not interpret it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomExtension {
    pub vendor_name: String,
    pub data: String,
}
