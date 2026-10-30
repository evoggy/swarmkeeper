//! File dialogs. Here they return paths on disk. A browser build copies
//! the picked file into [`crate::host::fs`] under its name and returns that
//! path, and a saved file is downloaded.

use std::path::{Path, PathBuf};

/// A file the user picked.
pub struct Picked(PathBuf);

impl Picked {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// A file dialog, built like `rfd::AsyncFileDialog`. It starts in the data
/// folder unless told otherwise.
pub struct FileDialog(rfd::AsyncFileDialog);

impl FileDialog {
    pub fn new() -> Self {
        FileDialog(rfd::AsyncFileDialog::new().set_directory(crate::host::env::data_dir()))
    }

    pub fn set_title(self, title: impl Into<String>) -> Self {
        FileDialog(self.0.set_title(title))
    }

    pub fn add_filter(self, name: impl Into<String>, extensions: &[impl ToString]) -> Self {
        FileDialog(self.0.add_filter(name, extensions))
    }

    pub fn set_file_name(self, name: impl Into<String>) -> Self {
        FileDialog(self.0.set_file_name(name))
    }

    pub async fn pick_file(self) -> Option<Picked> {
        self.0.pick_file().await.map(|file| Picked(file.path().to_path_buf()))
    }

    pub async fn save_file(self) -> Option<Picked> {
        self.0.save_file().await.map(|file| Picked(file.path().to_path_buf()))
    }
}
