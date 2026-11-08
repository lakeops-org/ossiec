//! `describe`: the model's metrics and dimensions, for catalogs and tools.

use serde::Serialize;

use super::{Ctx, Flavor, SemanticGraph, Variant};

pub(super) fn dialect_names(vs: &[Variant]) -> Vec<String> {
    vs.iter()
        .map(|v| match &v.flavor {
            Flavor::Portable => "OSSIE_SQL_2026".to_string(),
            Flavor::Ansi => "ANSI_SQL".to_string(),
            Flavor::Vendor(n, _) => n.clone(),
        })
        .collect()
}

/// What a model offers, as returned by [`SemanticGraph::describe`].
#[derive(Debug, Serialize)]
pub struct Catalog {
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub ai_context: Option<serde_json::Value>,
    pub metrics: Vec<MetricDoc>,
    pub dimensions: Vec<DimensionDoc>,
    pub relationships: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct MetricDoc {
    pub name: String,
    pub description: Option<String>,
    pub datatype: Option<String>,
    pub ai_context: Option<serde_json::Value>,
    pub queryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_queryable_because: Option<String>,
    /// Dimensions a window metric partitions or orders by; requests must include them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requires_dimensions: Vec<String>,
    /// The spec defers this metric: a request must set `deferredMetrics` to get it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub requires_deferred_metrics: bool,
    pub dialects: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DimensionDoc {
    pub name: String,
    pub label: Option<String>,
    pub description: Option<String>,
    pub datatype: Option<String>,
    pub is_time: bool,
    /// The field declares a `dimension` block. Any field can be grouped by.
    pub declared_dimension: bool,
    pub ai_context: Option<serde_json::Value>,
    pub dialects: Vec<String>,
}

impl SemanticGraph {
    /// A description of everything a caller may ask for, for agents and UIs.
    pub fn describe(&self) -> Catalog {
        let mut dimensions = vec![];
        for (d, ds) in self.model.datasets.iter().zip(&self.datasets) {
            for (f, fi) in d.fields.iter().zip(&ds.fields) {
                dimensions.push(DimensionDoc {
                    name: format!("{}.{}", d.name, f.name),
                    label: f.label.clone(),
                    description: f.description.clone(),
                    datatype: f.datatype.clone(),
                    is_time: fi.is_time,
                    declared_dimension: f.dimension.is_some(),
                    ai_context: f.ai_context.clone(),
                    dialects: dialect_names(&fi.variants),
                });
            }
        }
        let scoped = self.model.datasets.iter().flat_map(|d| d.metrics.iter());
        let metrics = scoped
            .chain(self.model.metrics.iter())
            .zip(&self.metrics)
            .enumerate()
            .map(|(i, (m, mi))| MetricDoc {
                name: mi.name.clone(),
                description: m.description.clone(),
                datatype: m.datatype.clone(),
                ai_context: m.ai_context.clone(),
                queryable: mi.unsupported.is_none(),
                not_queryable_because: mi.unsupported.clone(),
                requires_dimensions: self.required_dims(&mi.name),
                requires_deferred_metrics: self
                    .metric_rejections(i, &mut Ctx::default())
                    .is_ok_and(|r| r.iter().any(|r| r.deferred)),
                dialects: dialect_names(&mi.variants),
            })
            .collect();
        Catalog {
            name: self.model.name.clone(),
            version: self.model.version.clone(),
            description: self.model.description.clone(),
            ai_context: self.model.ai_context.clone(),
            metrics,
            dimensions,
            relationships: self
                .rels
                .iter()
                .map(|r| {
                    format!(
                        "{}: {} → {}",
                        r.name, self.datasets[r.from].name, self.datasets[r.to].name
                    )
                })
                .collect(),
        }
    }
}
