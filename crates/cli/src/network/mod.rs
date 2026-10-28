//! Network settings: `fbk network` changes the network section of `.firebrick.yml` and applies it
//! to the sandbox of the working directory, so the file and the sandbox keep the same network
//! switch and rules. `fbk network log` shows the network activity fbkd recorded for the sandbox.

mod log;
pub mod presets;
mod update;

use tonic::transport::Channel;

use crate::api::sandbox_management_service_client::SandboxManagementServiceClient;

pub use log::log;
pub use update::{NetworkChange, update};

type Client = SandboxManagementServiceClient<Channel>;

#[cfg(test)]
mod tests {
    use crate::spec_file::SPEC_FILE_NAME;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Returns a working directory with a spec file that names the sandbox `dev`, and the path
    /// of that file.
    pub(super) fn workspace() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let spec_path = dir.path().join(SPEC_FILE_NAME);
        fs::write(&spec_path, "name: dev\n").unwrap();
        (dir, spec_path)
    }
}
