use std::path::PathBuf;
use std::time::Duration;

use super::server::{available_port, find_binary, launch_cdp_server, CdpServerProcess};

const MOLI_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub type MoliProcess = CdpServerProcess;

#[derive(Default)]
pub struct MoliLaunchOptions {
    pub executable_path: Option<String>,
    /// Proxy URL forwarded to `moli serve --http-proxy`.
    pub proxy: Option<String>,
    /// Proxy bypass list forwarded to `moli serve --http-no-proxy`.
    pub proxy_bypass: Option<String>,
    pub user_agent: Option<String>,
    pub ignore_https_errors: bool,
    pub port: Option<u16>,
}

fn build_moli_serve_args(options: &MoliLaunchOptions, port: u16) -> Vec<String> {
    let mut args = vec![
        "serve".to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--layout".to_string(),
    ];

    if options.ignore_https_errors {
        args.push("--insecure-disable-tls-host-verification".to_string());
    }
    if let Some(proxy) = &options.proxy {
        args.push("--http-proxy".to_string());
        args.push(proxy.clone());
    }
    if let Some(proxy_bypass) = &options.proxy_bypass {
        args.push("--http-no-proxy".to_string());
        args.push(proxy_bypass.clone());
    }
    if let Some(user_agent) = &options.user_agent {
        args.push("--user-agent".to_string());
        args.push(user_agent.clone());
    }

    args
}

pub fn find_moli() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = dirs::home_dir() {
        #[cfg(windows)]
        candidates.push(home.join(".local/bin/moli.exe"));
        #[cfg(not(windows))]
        {
            candidates.push(home.join(".local/bin/moli"));
            candidates.push(home.join(".cargo/bin/moli"));
        }
    }
    #[cfg(windows)]
    if let Some(local_data) = dirs::data_local_dir() {
        candidates.push(local_data.join("Moli/bin/moli.exe"));
    }

    find_binary("moli", &candidates)
}

pub async fn launch_moli(options: &MoliLaunchOptions) -> Result<MoliProcess, String> {
    let binary_path = match &options.executable_path {
        Some(path) => PathBuf::from(path),
        None => find_moli().ok_or(
            "Moli not found. Download a prebuilt archive from https://github.com/lexmount/moli/releases/latest, put the Moli binary on PATH, or use --executable-path.",
        )?,
    };
    let port = match options.port {
        Some(port) => port,
        None => available_port("Moli")?,
    };
    let args = build_moli_serve_args(options, port);

    launch_cdp_server("Moli", &binary_path, &args, port, MOLI_STARTUP_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_options() {
        let options = MoliLaunchOptions::default();
        assert!(options.executable_path.is_none());
        assert!(options.proxy.is_none());
        assert!(options.proxy_bypass.is_none());
        assert!(options.user_agent.is_none());
        assert!(!options.ignore_https_errors);
        assert!(options.port.is_none());
    }

    #[test]
    fn serve_args_enable_layout() {
        let args = build_moli_serve_args(&MoliLaunchOptions::default(), 9222);
        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "9222".to_string(),
                "--layout".to_string(),
            ]
        );
    }

    #[test]
    fn serve_args_forward_supported_options() {
        let options = MoliLaunchOptions {
            proxy: Some("http://127.0.0.1:8080".to_string()),
            proxy_bypass: Some("localhost,*.internal.example".to_string()),
            user_agent: Some("agent-browser-test".to_string()),
            ignore_https_errors: true,
            ..Default::default()
        };
        let args = build_moli_serve_args(&options, 9333);

        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--host".to_string(),
                "127.0.0.1".to_string(),
                "--port".to_string(),
                "9333".to_string(),
                "--layout".to_string(),
                "--insecure-disable-tls-host-verification".to_string(),
                "--http-proxy".to_string(),
                "http://127.0.0.1:8080".to_string(),
                "--http-no-proxy".to_string(),
                "localhost,*.internal.example".to_string(),
                "--user-agent".to_string(),
                "agent-browser-test".to_string(),
            ]
        );
    }
}
