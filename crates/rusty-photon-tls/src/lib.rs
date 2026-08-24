#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(
    test,
    allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::indexing_slicing
    )
)]
//! TLS serving utilities for Rusty Photon services.
//!
//! Provides TLS server helpers, client CA trust, and shared configuration
//! types for opt-in HTTPS across all services. Certificate *provisioning*
//! (self-signed issuance, ACME, DNS-01) lives in doctor — the one binary
//! that mints material — so services carry only what serving needs.

// Curated test-scope allow list — documented in the root Cargo.toml [workspace.lints] block.
#![cfg_attr(
    test,
    allow(
        clippy::needless_pass_by_ref_mut,
        clippy::needless_pass_by_value,
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        clippy::used_underscore_binding,
        clippy::significant_drop_tightening,
        clippy::significant_drop_in_scrutinee,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap,
        clippy::suboptimal_flops,
        clippy::too_many_lines,
        clippy::option_if_let_else,
        clippy::match_same_arms,
        clippy::float_cmp,
        clippy::similar_names,
        clippy::struct_excessive_bools,
    )
)]

pub mod client;
pub mod config;
pub mod error;
pub mod permissions;
pub mod resolver;
pub mod server;
pub mod test_cert;

/// Install `aws-lc-rs` as the process-wide default rustls `CryptoProvider`.
///
/// Required because both `aws-lc-rs` and `ring` end up feature-activated on
/// `rustls` via our transitive deps (reqwest 0.13 + reqwest 0.12 via cloudflare
/// rustls-tls), which defeats rustls's automatic provider selection.
///
/// The install is attempted exactly once per process via `Once`. If some other
/// code path installed a different provider first, the failure is logged at
/// `error!` level so the root cause is visible — downstream TLS operations
/// will then use that pre-existing provider rather than aws-lc-rs.
pub fn install_default_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Err(existing) = rustls::crypto::aws_lc_rs::default_provider().install_default() {
            tracing::error!(
                cipher_suites = existing.cipher_suites.len(),
                kx_groups = existing.kx_groups.len(),
                "rustls crypto provider was already installed before rusty-photon-tls could register aws-lc-rs; keeping existing provider"
            );
        }
    });
}
