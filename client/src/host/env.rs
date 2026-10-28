//! The environment Swarmkeeper runs in: the data folder, environment
//! variables, the computer's name and the web browser.

use std::path::PathBuf;

/// Swarms, scenes, plans, log configs, journals and recordings are all kept
/// relative to the working directory. Run from a source checkout (`cargo run`
/// in client/) that is the checkout itself; started any other way (installed,
/// from a desktop menu) they go in ~/Documents/Swarmkeeper instead.
pub fn enter_data_dir() {
    if std::path::Path::new("swarms").is_dir() {
        return;
    }
    let Some(base) = dirs_next::document_dir().or_else(dirs_next::home_dir) else {
        return;
    };
    let dir = base.join("Swarmkeeper");
    if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::env::set_current_dir(&dir)) {
        eprintln!("Could not use {} as data directory: {e}", dir.display());
    }
}

/// The data folder, where file dialogs start.
pub fn data_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_default()
}

/// An environment variable, when set and valid Unicode.
pub fn var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// This computer's name, as other programs on a connection server see it.
pub fn hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// Open a web page in the user's browser, without waiting for it.
pub fn open_url(url: &str) -> std::io::Result<()> {
    open::that_detached(url)
}
