//! Step definitions for `restart.feature`: the shared steps from
//! `bdd-infra`.

use crate::world::FilemonitorWorld;

bdd_infra::service_restart_steps!(FilemonitorWorld);
