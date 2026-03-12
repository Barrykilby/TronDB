use async_trait::async_trait;
use trondb_core::types::VectorData;
use trondb_core::vectoriser::{FieldSet, VectorKind, Vectoriser, VectoriserError};

pub struct NetworkVectoriser {
    id: String,
    model_id: String,
    endpoint: String,
    dimensions: usize,
    client: reqwest::Client,
}

impl NetworkVectoriser {
    pub fn new(model_id: &str, endpoint: &str, dimensions: usize) -> Self {
        Self {
            id: format!("network:{model_id}"),
            model_id: model_id.to_string(),
            endpoint: endpoint.to_string(),
            dimensions,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Vectoriser for NetworkVectoriser {
    fn id(&self) -> &str { &self.id }
    fn model_id(&self) -> &str { &self.model_id }
    fn output_size(&self) -> usize { self.dimensions }
    fn output_kind(&self) -> VectorKind { VectorKind::Dense }

    async fn encode(&self, fields: &FieldSet) -> Result<VectorData, VectoriserError> {
        let mut pairs: Vec<_> = fields.iter().collect();
        pairs.sort_by_key(|(k, _)| (*k).clone());
        let text: String = pairs.iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join(". ");
        self.encode_query(&text).await
    }

    async fn encode_query(&self, query: &str) -> Result<VectorData, VectoriserError> {
        let body = serde_json::json!({
            "model": self.model_id,
            "input": query,
        });
        let resp = self.client.post(&self.endpoint)
            .json(&body)
            .send().await
            .map_err(|e| VectoriserError::Network(e.to_string()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(VectoriserError::Network(format!("{status}: {body}")));
        }
        let json: serde_json::Value = resp.json().await
            .map_err(|e| VectoriserError::Network(e.to_string()))?;
        let embedding = json.get("embedding")
            .or_else(|| json.get("data").and_then(|d: &serde_json::Value| d.get(0)).and_then(|d: &serde_json::Value| d.get("embedding")))
            .ok_or_else(|| VectoriserError::Network("no embedding in response".into()))?;
        let vector: Vec<f32> = embedding.as_array()
            .ok_or_else(|| VectoriserError::Network("embedding is not an array".into()))?
            .iter()
            .map(|v: &serde_json::Value| v.as_f64().unwrap_or(0.0) as f32)
            .collect();
        Ok(VectorData::Dense(vector))
    }
}
