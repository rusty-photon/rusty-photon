//! SDK-library steps (`sdk_library.feature`): a service started without the
//! QHYCCD SDK library, and the library installed while it serves.

use cucumber::{given, when};

use crate::world::CameraWorld;

#[given("the QHY SDK library is not installed")]
async fn sdk_library_not_installed(world: &mut CameraWorld) {
    // The simulated library loads once a file exists at this path (PF5);
    // nothing is written there yet.
    world.sdk_library = Some(world.scratch_dir().join("qhyccd-sdk-library"));
}

#[when("the QHY SDK library is installed")]
async fn sdk_library_installed(world: &mut CameraWorld) {
    let library = world
        .sdk_library
        .as_ref()
        .expect("the scenario started without the library");
    std::fs::write(library, b"").expect("install the simulated library");
}
