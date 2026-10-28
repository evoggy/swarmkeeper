//! What Swarmkeeper needs from the computer it runs on.
//!
//! Swarmkeeper should also run in a browser (swarmkeeper.bitcraze.io),
//! reaching the Crazyflies through a connection server. Everything a browser
//! does differently, or can't do at all, is in this module:
//!
//! - [`task`]: spawning, sleeping, timeouts, threads and the clock
//! - [`fs`], [`dialog`], [`store`], [`env`]: files, file dialogs, stored
//!   settings, environment variables and opening web pages
//! - [`gamepad`], [`sniffer`] (serial port): devices
//! - [`renderer`], [`rendering`]: OpenGL for the 3D views, and
//!   [`screenshot`] for pictures of the UI without a display
//! - [`server`], [`radios`], [`swarm_connect`], [`bootloader_flash`],
//!   [`toc_cache`], [`radio_test`]: the connection server in this process,
//!   driving this computer's Crazyradios in `shared` and `direct` mode
//! - [`account`]: signing in to arc.bitcraze.io through a browser on this
//!   computer, and the credentials kept on disk
//!
//! The rest of the app uses the Crazyflies only through the connection
//! server's client API (`crazyflie_connection_server::{Client, Crazyflie}`),
//! which works the same against a remote server. A browser build replaces
//! the modules here, not the code that uses them. `main.rs` is the native
//! entry point and may use them directly too.
//!
//! `portable_code_stays_portable` below fails when code outside this module
//! names one of these APIs.

pub mod account;
pub mod bootloader_flash;
pub mod dialog;
pub mod env;
pub mod fs;
pub mod gamepad;
pub mod radio_test;
pub mod radios;
pub mod server;
pub mod renderer;
pub mod rendering;
pub mod screenshot;
pub mod sniffer;
pub mod store;
pub mod swarm_connect;
pub mod task;
pub mod toc_cache;

pub use task::{interval, sleep, spawn, spawn_thread, timeout, Instant, JoinHandle, JoinSet, ThreadHandle};

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    /// What portable code may not name, and what to use instead.
    const FORBIDDEN: &[(&str, &str)] = &[
        ("tokio::spawn", "host::spawn"),
        ("tokio::time", "host::sleep, host::timeout, host::Instant"),
        ("tokio::task", "host::JoinSet, host::JoinHandle"),
        ("tokio::fs", "host::fs"),
        ("tokio::net", "the connection server"),
        ("tokio::process", "nothing: a browser can't start programs"),
        ("tokio::runtime", "host::spawn"),
        ("std::fs", "host::fs"),
        ("std::thread", "host::spawn_thread"),
        ("std::env", "host::env"),
        ("std::process", "nothing: a browser can't start programs"),
        ("std::net", "the connection server"),
        ("std::time::Instant", "host::Instant"),
        ("std::time::SystemTime", "chrono"),
        ("crazyflie_link::", "the connection server client API, or host::radios"),
        ("crazyflie_lib::", "the connection server client API, or host::radios"),
        ("crazyradio::", "host::radios"),
        ("rusb::", "host::radios"),
        ("serialport::", "host::sniffer"),
        ("gilrs::", "host::gamepad"),
        ("rfd::", "host::dialog"),
        ("confy::", "host::store"),
        ("dirs_next::", "host::env"),
        ("gethostname::", "host::env::hostname"),
        ("open::that", "host::env::open_url"),
        ("glow::", "host::renderer"),
        ("BorrowedOpenGLTexture", "host::renderer"),
        ("set_rendering_notifier", "host::rendering"),
        (".lib()", "the client API; the crazyflie-lib connection only exists in shared and direct mode"),
        ("open_with(", "connection::Connection::open"),
        ("InProcess", "connection::Connection::open"),
        (".adopt(", "host::swarm_connect"),
    ];

    /// Modules named inside a grouped `use std::{...}` that are not portable.
    const FORBIDDEN_IN_GROUPS: &[&str] = &["fs", "thread", "env", "process", "net", "Instant", "SystemTime"];

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn portable_code_stays_portable() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&src, &mut files);
        files.sort();

        let mut problems = Vec::new();
        for file in files {
            let relative = file.strip_prefix(&src).unwrap();
            if relative.starts_with("host") || relative == Path::new("main.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            for (number, line) in text.lines().enumerate() {
                let code = line.trim();
                // Tests only run natively.
                if code == "#[cfg(test)]" {
                    break;
                }
                if code.starts_with("//") {
                    continue;
                }
                for (pattern, instead) in FORBIDDEN {
                    if code.contains(pattern) {
                        problems.push(format!("{}:{}: {} (use {})", relative.display(), number + 1, pattern, instead));
                    }
                }
                if let Some(group) = code.strip_prefix("use std::").and_then(|rest| rest.split_once('{')) {
                    for name in group.1.split(|c: char| !c.is_alphanumeric() && c != '_') {
                        if FORBIDDEN_IN_GROUPS.contains(&name) {
                            problems.push(format!("{}:{}: std {} (see host)", relative.display(), number + 1, name));
                        }
                    }
                }
            }
        }
        assert!(
            problems.is_empty(),
            "Code outside src/host names what a browser build can't do:\n{}",
            problems.join("\n")
        );
    }
}
