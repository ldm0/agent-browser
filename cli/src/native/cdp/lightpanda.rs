use std::path::PathBuf;
use std::time::Duration;

use super::server::{available_port, find_binary, launch_cdp_server, CdpServerProcess};

const LIGHTPANDA_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const LIGHTPANDA_SESSION_TIMEOUT_SECS: u64 = 604800; // One week, the documented maximum.

pub type LightpandaProcess = CdpServerProcess;

#[derive(Default)]
pub struct LightpandaLaunchOptions {
    pub executable_path: Option<String>,
    pub proxy: Option<String>,
    pub port: Option<u16>,
}

fn build_lightpanda_serve_args(port: u16, proxy: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "serve".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--timeout".to_string(),
        LIGHTPANDA_SESSION_TIMEOUT_SECS.to_string(),
    ];

    if let Some(proxy) = proxy {
        args.push("--http_proxy".to_string());
        args.push(proxy.to_string());
    }

    args
}

pub fn find_lightpanda() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".lightpanda/lightpanda"));
        candidates.push(home.join(".local/bin/lightpanda"));
    }
    find_binary("lightpanda", &candidates)
}

pub async fn launch_lightpanda(
    options: &LightpandaLaunchOptions,
) -> Result<LightpandaProcess, String> {
    let binary_path = match &options.executable_path {
        Some(path) => PathBuf::from(path),
        None => find_lightpanda().ok_or(
            "Lightpanda not found. Install it from https://lightpanda.io/docs/open-source/installation or use --executable-path.",
        )?,
    };
    let port = match options.port {
        Some(port) => port,
        None => available_port("Lightpanda")?,
    };
    let args = build_lightpanda_serve_args(port, options.proxy.as_deref());

    launch_cdp_server(
        "Lightpanda",
        &binary_path,
        &args,
        port,
        LIGHTPANDA_STARTUP_TIMEOUT,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options() {
        let options = LightpandaLaunchOptions::default();
        assert!(options.executable_path.is_none());
        assert!(options.proxy.is_none());
        assert!(options.port.is_none());
    }

    #[test]
    fn serve_args_set_explicit_session_timeout() {
        let args = build_lightpanda_serve_args(9222, None);

        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "9222".to_string(),
                "--timeout".to_string(),
                "604800".to_string(),
            ]
        );
    }

    #[test]
    fn serve_args_include_proxy() {
        let args = build_lightpanda_serve_args(9333, Some("http://127.0.0.1:8080"));

        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "9333".to_string(),
                "--timeout".to_string(),
                "604800".to_string(),
                "--http_proxy".to_string(),
                "http://127.0.0.1:8080".to_string(),
            ]
        );
    }
}
