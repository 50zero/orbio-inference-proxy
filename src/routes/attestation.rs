use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::attestation::AttestationResult;
use crate::error::AppError;
use crate::types::AttestationResponse;
use crate::AppState;

#[derive(Deserialize)]
pub struct AttestationQuery {
    pub signing_algo: Option<String>,
    pub nonce: Option<String>,
    pub signing_address: Option<String>,
    /// Include TLS certificate fingerprint in the report data.
    /// Defaults to false; when true, report_data[..32] = SHA256(signing_address || cert_fingerprint).
    /// `false` (the Orbio gateway) binds report_data as pubkey || nonce.
    pub include_tls_fingerprint: Option<bool>,
    /// Canonical model id. When present it must equal this instance's
    /// `MODEL_NAME` (no aliasing). Omitted: this instance's model.
    pub model: Option<String>,
    /// Accepted so NEAR-shaped verifiers can send `provider`. Not used
    /// to select a backend; this process serves one model.
    pub provider: Option<String>,
}

fn normalize_hex_address(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X")
        .to_ascii_lowercase()
}

/// GET /v1/attestation/report
pub async fn attestation_report(
    State(state): State<AppState>,
    Query(query): Query<AttestationQuery>,
) -> Result<impl IntoResponse, AppError> {
    if state.config.non_tee_deployment {
        return Err(AppError::NotFound(
            "Attestation is not available on this endpoint: it does not run inside a TEE."
                .to_string(),
        ));
    }
    // `x-no-aliasing` and `Authorization` are accepted and not required.
    // A model query that is not this instance is rejected rather than rewritten.
    let _provider = query.provider.as_deref();
    if let Some(model) = query.model.as_deref() {
        if model.is_empty() {
            return Err(AppError::BadRequest(
                "model must be this instance's model name".to_string(),
            ));
        }
        if model != state.config.model_name {
            return Err(AppError::NotFound(
                "The requested model is not served by this instance".to_string(),
            ));
        }
    }

    let signing_algo = query.signing_algo.as_deref().unwrap_or("ecdsa");

    if signing_algo != "ecdsa" && signing_algo != "ed25519" {
        return Err(AppError::BadRequest(
            "Invalid signing algorithm. Must be 'ed25519' or 'ecdsa'".to_string(),
        ));
    }

    let (signing_address, signing_address_bytes, signing_public_key) = match signing_algo {
        "ecdsa" => (
            state.signing.ecdsa.signing_address.clone(),
            state.signing.ecdsa.signing_address_bytes.clone(),
            state.signing.ecdsa.signing_public_key.clone(),
        ),
        "ed25519" => (
            state.signing.ed25519.signing_address.clone(),
            state.signing.ed25519.signing_address_bytes.clone(),
            state.signing.ed25519.signing_public_key.clone(),
        ),
        _ => unreachable!(),
    };

    // A signing address for a different key is 503 with the exact
    // NEAR text `Signing address not found` (checked before any quote).
    if let Some(requested_addr) = &query.signing_address {
        if normalize_hex_address(&signing_address) != normalize_hex_address(requested_addr) {
            return Err(AppError::SigningAddressNotFound);
        }
    }

    let include_tls = query.include_tls_fingerprint.unwrap_or(false);
    let delegate_ctx = state.config.gpu_evidence_delegate_url.as_ref().map(|_| {
        crate::attestation::DelegateContext {
            config: &state.config,
            http_client: &state.http_client,
        }
    });
    // Snapshot the current TLS fingerprint into a local so its `&str` is
    // alive for the `AttestationParams` borrow below. The tracker's value
    // may have just been refreshed by the background cache task.
    let tls_fp_snapshot = if include_tls {
        state.tls_cert_fingerprint.current()
    } else {
        None
    };
    let compose = state
        .config
        .compose_manager_url
        .as_deref()
        .map(|url| (&state.http_client, url));
    let (result, cm_attestation) = crate::attestation::generate_attestation_with_compose(
        crate::attestation::AttestationParams {
            model_name: &state.config.model_name,
            signing_address: &signing_address,
            signing_algo,
            signing_public_key: &signing_public_key,
            signing_address_bytes: &signing_address_bytes,
            nonce: query.nonce.as_deref(),
            gpu_no_hw_mode: state.config.gpu_no_hw_mode,
            tls_cert_fingerprint: tls_fp_snapshot.as_deref(),
        },
        Some(&state.attestation_cache),
        delegate_ctx.as_ref(),
        compose,
    )
    .await
    .map_err(|e| match e {
        crate::attestation::AttestationError::AtCapacity => AppError::Overloaded {
            reason: "attestation",
            retry_after_secs: 2,
        },
        crate::attestation::AttestationError::InvalidNonce(msg) => AppError::BadRequest(msg),
        crate::attestation::AttestationError::Internal(e) => AppError::Internal(e),
    })?;

    match result {
        // Cache hit: return pre-serialized bytes directly (no clone, no serialization).
        // Compose-manager attestation is included in cached bytes by the background refresh task.
        AttestationResult::CachedBytes(bytes) => Ok((
            StatusCode::OK,
            [("content-type", "application/json")],
            bytes,
        )
            .into_response()),
        AttestationResult::Fresh(report) => {
            let ohttp_attestation = state.ohttp_attestation_ed25519.clone();

            // Cache nonce-less reports so subsequent requests get the full response
            // including compose-manager attestation.
            if query.nonce.is_none() {
                state
                    .attestation_cache
                    .set(
                        signing_algo,
                        include_tls,
                        report.as_ref().clone(),
                        cm_attestation.clone(),
                        ohttp_attestation.clone(),
                    )
                    .await;
            }

            let response = AttestationResponse::new(
                report.as_ref().clone(),
                vec![*report],
                cm_attestation,
                ohttp_attestation,
            );
            Ok(Json(response).into_response())
        }
    }
}
