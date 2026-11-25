//! Bearer-token checks shared by REST and gRPC.

// Tonic fixes the interceptor signature, so its large error type is unavoidable.
#![allow(clippy::result_large_err)]

#[cfg(feature = "grpc")]
use std::sync::Arc;

#[cfg(feature = "rest")]
use axum::{
    extract::{Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
#[cfg(feature = "grpc")]
use tokio::sync::RwLock;

#[cfg(feature = "rest")]
use crate::api::rest::{AppState, ErrorResponse};
use crate::config::AuthMode;
#[cfg(feature = "grpc")]
use crate::config::{ApiConfig, Config};

#[derive(Debug, PartialEq, Eq)]
enum Denied {
    NoTokenConfigured,
    BadToken,
}

fn check(token_hash: Option<&str>, header: Option<&str>) -> Result<(), Denied> {
    let expected = token_hash
        .and_then(|h| blake3::Hash::from_hex(h).ok())
        .ok_or(Denied::NoTokenConfigured)?;
    let presented = header.and_then(|v| v.strip_prefix("Bearer "));
    // blake3::Hash compares in constant time.
    if presented.is_some_and(|t| blake3::hash(t.as_bytes()) == expected) {
        Ok(())
    } else {
        Err(Denied::BadToken)
    }
}

#[cfg(feature = "rest")]
async fn authorize_rest(state: &AppState, request: Request, next: Next) -> Response {
    let token_hash = state.config.read().await.api.auth_token_hash.clone();
    let header = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let (status, error) = match check(token_hash.as_deref(), header) {
        Ok(()) => return next.run(request).await,
        Err(Denied::NoTokenConfigured) => (
            StatusCode::FORBIDDEN,
            "this route is disabled: set api.auth_token_hash",
        ),
        Err(Denied::BadToken) => (StatusCode::UNAUTHORIZED, "missing or invalid bearer token"),
    };
    let body = ErrorResponse {
        error: error.into(),
    };
    (status, Json(body)).into_response()
}

/// Guards routes that change files, whatever `api.auth_mode` is.
#[cfg(feature = "rest")]
pub async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    authorize_rest(&state, request, next).await
}

/// Guards every other route, but only under `auth_mode = "all"`.
#[cfg(feature = "rest")]
pub async fn require_token_in_all_mode(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if state.config.read().await.api.auth_mode == AuthMode::All {
        authorize_rest(&state, request, next).await
    } else {
        next.run(request).await
    }
}

#[cfg(feature = "grpc")]
fn check_grpc(api: &ApiConfig, header: Option<&str>) -> Result<(), tonic::Status> {
    if api.auth_mode != AuthMode::All {
        return Ok(());
    }
    check(api.auth_token_hash.as_deref(), header)
        .map_err(|_| tonic::Status::unauthenticated("missing or invalid bearer token"))
}

/// For gRPC calls that need the token in every `api.auth_mode`, as REST's
/// routes that change things do.
#[cfg(feature = "grpc")]
pub fn require_grpc_token(api: &ApiConfig, header: Option<&str>) -> Result<(), tonic::Status> {
    check(api.auth_token_hash.as_deref(), header).map_err(|denied| match denied {
        Denied::NoTokenConfigured => {
            tonic::Status::permission_denied("this call is disabled: set api.auth_token_hash")
        }
        Denied::BadToken => tonic::Status::unauthenticated("missing or invalid bearer token"),
    })
}

/// Builds the interceptor for gRPC services.
///
/// Interceptors are synchronous, so the config lock can only be tried. It is
/// held just for the swap during a config change, and clients retry `UNAVAILABLE`.
#[cfg(feature = "grpc")]
pub fn grpc_interceptor(
    config: Arc<RwLock<Config>>,
) -> impl Fn(tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> + Clone {
    move |request| {
        let config = config
            .try_read()
            .map_err(|_| tonic::Status::unavailable("config is reloading"))?;
        let header = request
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok());
        check_grpc(&config.api, header)?;
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "grpc")]
    fn api(mode: AuthMode) -> ApiConfig {
        ApiConfig {
            auth_mode: mode,
            auth_token_hash: Some(blake3::hash(b"secret").to_hex().to_string()),
            ..ApiConfig::default()
        }
    }

    #[test]
    fn check_tells_a_bad_token_from_a_missing_configuration() {
        let hash = blake3::hash(b"secret").to_hex().to_string();
        assert_eq!(check(Some(&hash), Some("Bearer secret")), Ok(()));
        assert_eq!(
            check(Some(&hash), Some("Bearer nope")),
            Err(Denied::BadToken)
        );
        assert_eq!(check(Some(&hash), Some("secret")), Err(Denied::BadToken));
        assert_eq!(check(Some(&hash), None), Err(Denied::BadToken));
        assert_eq!(
            check(None, Some("Bearer secret")),
            Err(Denied::NoTokenConfigured)
        );
        assert_eq!(
            check(Some("not-hex"), Some("Bearer secret")),
            Err(Denied::NoTokenConfigured)
        );
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn grpc_needs_a_token_only_in_all_mode() {
        assert!(check_grpc(&api(AuthMode::Writes), None).is_ok());
        assert_eq!(
            check_grpc(&api(AuthMode::All), None).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        assert!(check_grpc(&api(AuthMode::All), Some("Bearer secret")).is_ok());
    }

    #[cfg(feature = "grpc")]
    #[test]
    fn grpc_writes_need_a_token_in_every_mode() {
        let writes = api(AuthMode::Writes);
        assert_eq!(
            require_grpc_token(&writes, None).unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        assert!(require_grpc_token(&writes, Some("Bearer secret")).is_ok());
        assert_eq!(
            require_grpc_token(&ApiConfig::default(), Some("Bearer secret"))
                .unwrap_err()
                .code(),
            tonic::Code::PermissionDenied
        );
    }
}
