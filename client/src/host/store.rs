//! Settings kept between runs: a TOML file in the user's configuration
//! folder here, the browser's local storage in a browser build.

use serde::de::DeserializeOwned;
use serde::Serialize;

const APP: &str = "swarmkeeper";

/// The stored settings, or the defaults when there are none yet.
pub fn load<T: Serialize + DeserializeOwned + Default>() -> T {
    confy::load(APP, None).unwrap_or_default()
}

/// Store the settings.
pub fn save<T: Serialize>(settings: &T) -> Result<(), confy::ConfyError> {
    confy::store(APP, None, settings)
}

/// A file next to the stored settings, for data that isn't a setting
/// (the account's credentials).
pub fn file_beside(name: &str) -> Option<std::path::PathBuf> {
    confy::get_configuration_file_path(APP, None).ok().map(|path| path.with_file_name(name))
}
