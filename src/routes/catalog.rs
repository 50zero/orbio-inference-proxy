//! `GET /v1/model/list` in the NEAR public cloud-api shape.
//!
//! One model: this process's `MODEL_NAME`. Prices are
//! [`DecimalPrice`](https://docs.near.ai/api-reference/models/list-models-with-pricing):
//! `{amount, scale, currency}` with scale 9 (nano-dollars) and currency `USD`
//! unless overridden. No authentication.

use axum::extract::{Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct ModelListQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct DecimalPrice {
    pub amount: i64,
    pub scale: i64,
    pub currency: String,
}

impl DecimalPrice {
    fn to_json(&self) -> Value {
        json!({
            "amount": self.amount,
            "scale": self.scale,
            "currency": self.currency,
        })
    }
}

#[derive(Debug, Clone)]
pub struct CatalogSettings {
    pub context_length: i64,
    pub max_output_length: i64,
    pub input: DecimalPrice,
    pub output: DecimalPrice,
    pub per_image: DecimalPrice,
    pub display_name: String,
    pub description: String,
    pub owned_by: String,
}

fn env_i64(name: &str, default: i64) -> Result<i64, AppError> {
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(value) if value.trim().is_empty() => Ok(default),
        Ok(value) => value.trim().parse::<i64>().map_err(|_| {
            tracing::error!(env_var = name, "catalog env var is not an integer");
            AppError::Internal(anyhow::anyhow!("{name} is not an integer"))
        }),
    }
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= 128)
        .unwrap_or_else(|| default.to_string())
}

impl CatalogSettings {
    pub fn from_env(model_name: &str) -> Result<Self, AppError> {
        let scale = env_i64("COST_SCALE", 9)?;
        let currency = env_string("COST_CURRENCY", "USD");
        let price = |name: &str| {
            Ok::<_, AppError>(DecimalPrice {
                amount: env_i64(name, 0)?,
                scale,
                currency: currency.clone(),
            })
        };
        Ok(Self {
            context_length: env_i64("CONTEXT_LENGTH", 262_144)?,
            max_output_length: env_i64("MAX_OUTPUT_LENGTH", 8192)?,
            input: price("INPUT_COST_PER_TOKEN_AMOUNT")?,
            output: price("OUTPUT_COST_PER_TOKEN_AMOUNT")?,
            per_image: price("COST_PER_IMAGE_AMOUNT")?,
            display_name: env_string("MODEL_DISPLAY_NAME", model_name),
            description: env_string("MODEL_DESCRIPTION", "Orbio private inference"),
            owned_by: env_string("MODEL_OWNED_BY", "orbio"),
        })
    }
}

pub fn catalog_body(
    model_name: &str,
    ready: bool,
    settings: &CatalogSettings,
    limit: i64,
    offset: i64,
) -> Value {
    let entry = json!({
        "modelId": model_name,
        "inputCostPerToken": settings.input.to_json(),
        "outputCostPerToken": settings.output.to_json(),
        "costPerImage": settings.per_image.to_json(),
        "metadata": {
            "providerType": "vllm",
            "verifiable": true,
            "attestationSupported": true,
            "isReady": ready,
            "architecture": {
                "inputModalities": ["text"],
                "outputModalities": ["text"],
            },
            "contextLength": settings.context_length,
            "maxOutputLength": settings.max_output_length,
            "modelDisplayName": settings.display_name,
            "modelDescription": settings.description,
            "ownedBy": settings.owned_by,
        }
    });
    let include = offset == 0 && limit > 0;
    json!({
        "models": if include { vec![entry] } else { Vec::<Value>::new() },
        "limit": limit,
        "offset": offset,
        "total": 1,
    })
}

fn deployment_ready(state: &AppState) -> bool {
    if let Some(list) = &state.models {
        list.iter()
            .any(|model| model.backend_pool.healthy_count() > 0)
    } else {
        state.backend_pool.healthy_count() > 0
    }
}

/// GET /v1/model/list
pub async fn model_list(
    State(state): State<AppState>,
    Query(query): Query<ModelListQuery>,
) -> Result<Json<Value>, AppError> {
    let limit = query.limit.unwrap_or(100);
    let offset = query.offset.unwrap_or(0);
    if limit < 0 || offset < 0 {
        return Err(AppError::BadRequest(
            "limit and offset must be non-negative".to_string(),
        ));
    }
    let settings = CatalogSettings::from_env(&state.config.model_name)?;
    Ok(Json(catalog_body(
        &state.config.model_name,
        deployment_ready(&state),
        &settings,
        limit,
        offset,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> CatalogSettings {
        CatalogSettings {
            context_length: 262_144,
            max_output_length: 8192,
            input: DecimalPrice {
                amount: 150,
                scale: 9,
                currency: "USD".into(),
            },
            output: DecimalPrice {
                amount: 750,
                scale: 9,
                currency: "USD".into(),
            },
            per_image: DecimalPrice {
                amount: 0,
                scale: 9,
                currency: "USD".into(),
            },
            display_name: "Orbio".into(),
            description: "private".into(),
            owned_by: "orbio".into(),
        }
    }

    #[test]
    fn catalog_matches_near_decimal_price_shape() {
        let body = catalog_body(
            "orbio/deepseek-v4.1-flash-uncensored",
            true,
            &settings(),
            100,
            0,
        );
        assert_eq!(body["total"], 1);
        assert_eq!(body["limit"], 100);
        assert_eq!(body["offset"], 0);
        let model = &body["models"][0];
        assert_eq!(model["modelId"], "orbio/deepseek-v4.1-flash-uncensored");
        assert_eq!(model["metadata"]["providerType"], "vllm");
        assert_eq!(model["metadata"]["verifiable"], true);
        assert_eq!(model["metadata"]["attestationSupported"], true);
        assert_eq!(model["metadata"]["isReady"], true);
        assert_eq!(model["metadata"]["contextLength"], 262_144);
        assert_eq!(model["metadata"]["maxOutputLength"], 8192);
        assert_eq!(
            model["metadata"]["architecture"]["inputModalities"][0],
            "text"
        );
        assert_eq!(
            model["metadata"]["architecture"]["outputModalities"][0],
            "text"
        );
        assert_eq!(model["inputCostPerToken"]["amount"], 150);
        assert_eq!(model["inputCostPerToken"]["scale"], 9);
        assert_eq!(model["inputCostPerToken"]["currency"], "USD");
        assert_eq!(model["outputCostPerToken"]["amount"], 750);
        assert_eq!(model["costPerImage"]["amount"], 0);
    }

    #[test]
    fn catalog_pagination_can_return_an_empty_page() {
        let body = catalog_body("m", false, &settings(), 100, 1);
        assert_eq!(body["models"].as_array().unwrap().len(), 0);
        assert_eq!(body["total"], 1);
        assert_eq!(body["models"].as_array().unwrap().len(), 0);
        let none = catalog_body("m", true, &settings(), 0, 0);
        assert!(none["models"].as_array().unwrap().is_empty());
    }
}
