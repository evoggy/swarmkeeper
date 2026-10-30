//! Files: swarms, plans, scenes, journals, consoles and caches, kept in the
//! data folder (see [`crate::host::env::enter_data_dir`]). A browser build
//! keeps them in the browser's storage, with picked files under their
//! names (see [`crate::host::dialog`]).

pub use std::fs::{create_dir_all, read, read_dir, read_to_string, remove_dir, remove_file, rename, write};

/// Write a file only this user can read: the credentials act as the user.
#[cfg(unix)]
pub fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    // The mode only applies to a new file.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(contents)
}

#[cfg(not(unix))]
pub fn write_private(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, contents)
}
