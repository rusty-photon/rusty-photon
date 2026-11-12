//! TLS + HTTP Basic Auth smoke steps, expanded from the shared macro. The
//! service-specific parts (config template, the mock-feature binary's
//! launch) live in the `TlsAuthSmokeWorld` impl in `world.rs`.

use crate::world::FalconRotatorWorld;

bdd_infra::tls_auth_smoke_steps!(FalconRotatorWorld);
