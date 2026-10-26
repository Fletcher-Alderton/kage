//! Renewable bearer credentials for providers whose token expires.
//!
//! A provider built with a fixed API key holds a `String`. A provider
//! backed by an OAuth login instead holds a [`BearerSource`]: it asks
//! the source for the token of the moment on every request, and when
//! the backend answers 401 it asks the source to
//! [`refresh`](BearerSource::refresh) once and retries before failing.
//! Implementations live with the host, so the renewed token reaches the
//! credential store without the provider knowing about storage.

use std::sync::Arc;

use crate::ProviderError;

/// A renewable bearer credential.
pub trait BearerSource: Send + Sync {
    /// The access token to send as `Authorization: Bearer` right now.
    fn token(&self) -> String;

    /// `rejected` came back refused (HTTP 401): return the replacement,
    /// renewing and persisting it unless another request already did.
    /// Implementations must serialize renewals and compare `rejected`
    /// with the stored token, since a refresh token may rotate only
    /// once. An `Err` fails the request; the user has to log in again.
    ///
    /// # Errors
    ///
    /// Whatever the renewal path reports, surfaced as the request's
    /// own error.
    fn refresh(&self, rejected: &str) -> Result<String, ProviderError>;
}

/// A [`BearerSource`] shared between the registry builder and the
/// providers it constructs.
pub type SharedBearer = Arc<dyn BearerSource>;
