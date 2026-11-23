//! Identity of the vcer fork, kept in one module so upstream merges stay small.

use std::path::{Path, PathBuf};

pub(crate) const PRODUCT_NAME: &str = "vcer";

/// vcer has no release channel yet, and upstream channels would replace it
/// with stock herdr.
pub(crate) const UPSTREAM_DISTRIBUTION_ENABLED: bool = false;

pub(crate) const UPDATES_DISABLED_MESSAGE: &str =
    "vcer does not self-update yet: pull the fork and rebuild it from source";

pub(crate) const REMOTE_INSTALL_DISABLED_MESSAGE: &str =
    "vcer cannot install itself on remote hosts yet";

const STOCK_APP_DIRS: &[&str] = &["herdr", "herdr-dev"];

const INHERITED_RUNTIME_ENV_VARS: &[&str] = &[
    "HERDR_SOCKET_PATH",
    "HERDR_CLIENT_SOCKET_PATH",
    "HERDR_ENV",
    "HERDR_PANE_ID",
    "HERDR_TAB_ID",
    "HERDR_WORKSPACE_ID",
    "HERDR_BIN_PATH",
    "HERDR_SESSION",
];

/// Stops a vcer started inside a stock herdr pane from attaching to that
/// herdr server. Must run at the start of `main`, before any thread spawns.
pub(crate) fn detach_from_stock_herdr_env() {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg_config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let inherited_from_stock = ["HERDR_SOCKET_PATH", "HERDR_CLIENT_SOCKET_PATH"]
        .iter()
        .filter_map(std::env::var_os)
        .any(|socket| {
            is_stock_runtime_path(Path::new(&socket), home.as_deref(), xdg_config.as_deref())
        });
    if inherited_from_stock {
        for name in INHERITED_RUNTIME_ENV_VARS {
            std::env::remove_var(name);
        }
    }
}

fn is_stock_runtime_path(path: &Path, home: Option<&Path>, xdg_config: Option<&Path>) -> bool {
    let default_config = home.map(|home| home.join(".config"));
    let config_roots = [default_config.as_deref(), xdg_config];
    let is_stock = config_roots.into_iter().flatten().any(|config_root| {
        STOCK_APP_DIRS
            .iter()
            .any(|app_dir| path.starts_with(config_root.join(app_dir)))
    });
    is_stock
}

#[cfg(test)]
mod tests {
    use super::is_stock_runtime_path;
    use std::path::Path;

    #[test]
    fn stock_default_and_named_session_sockets_are_detected() {
        let home = Some(Path::new("/home/me"));
        for socket in [
            "/home/me/.config/herdr/herdr.sock",
            "/home/me/.config/herdr/sessions/work/herdr.sock",
            "/home/me/.config/herdr-dev/herdr.sock",
        ] {
            assert!(is_stock_runtime_path(Path::new(socket), home, None));
        }
    }

    #[test]
    fn stock_socket_under_xdg_config_is_detected() {
        assert!(is_stock_runtime_path(
            Path::new("/xdg/herdr/herdr.sock"),
            Some(Path::new("/home/me")),
            Some(Path::new("/xdg")),
        ));
    }

    #[test]
    fn vcer_and_custom_sockets_are_kept() {
        let home = Some(Path::new("/home/me"));
        for socket in [
            "/home/me/.config/vcer/herdr.sock",
            "/home/me/.config/vcer-dev/herdr.sock",
            "/tmp/custom.sock",
            "relative.sock",
        ] {
            assert!(!is_stock_runtime_path(Path::new(socket), home, None));
        }
    }
}
