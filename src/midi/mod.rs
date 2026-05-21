pub mod backend;

#[cfg(target_os = "macos")]
pub mod coremidi_backend;

#[cfg(not(target_os = "macos"))]
pub mod midir_backend;
