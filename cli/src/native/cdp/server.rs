use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::discovery::discover_cdp_url_with_timeout;

const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const SERVER_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(500);
const MAX_LOG_LINES: usize = 40;

pub struct CdpServerProcess {
    child: Child,
    pub ws_url: String,
    _log_drainers: Vec<std::thread::JoinHandle<()>>,
}

impl CdpServerProcess {
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
    }
}

impl Drop for CdpServerProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

#[derive(Clone, Default)]
struct LaunchLogBuffer {
    stdout: Arc<Mutex<VecDeque<String>>>,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

impl LaunchLogBuffer {
    fn push_stdout(&self, line: String) {
        push_bounded(&self.stdout, line);
    }

    fn push_stderr(&self, line: String) {
        push_bounded(&self.stderr, line);
    }

    fn snapshot_stdout(&self) -> Vec<String> {
        self.stdout
            .lock()
            .expect("stdout log buffer poisoned")
            .iter()
            .cloned()
            .collect()
    }

    fn snapshot_stderr(&self) -> Vec<String> {
        self.stderr
            .lock()
            .expect("stderr log buffer poisoned")
            .iter()
            .cloned()
            .collect()
    }
}

fn push_bounded(buffer: &Mutex<VecDeque<String>>, line: String) {
    let mut guard = buffer.lock().expect("log buffer poisoned");
    if guard.len() >= MAX_LOG_LINES {
        guard.pop_front();
    }
    guard.push_back(line);
}

pub fn find_binary(name: &str, fallback_paths: &[PathBuf]) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        if let Ok(output) = Command::new("which").arg(name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }

    #[cfg(windows)]
    {
        if let Ok(output) = Command::new("where").arg(name).output() {
            if output.status.success() {
                let path = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !path.is_empty() {
                    return Some(PathBuf::from(path));
                }
            }
        }
    }

    fallback_paths.iter().find(|path| path.exists()).cloned()
}

pub fn available_port(product: &str) -> Result<u16, String> {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| {
            format!(
                "Failed to find an available port for {}: {}",
                product, error
            )
        })
}

pub async fn launch_cdp_server(
    product: &str,
    binary_path: &Path,
    args: &[String],
    port: u16,
    startup_timeout: Duration,
) -> Result<CdpServerProcess, String> {
    let mut child = Command::new(binary_path)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "Failed to launch {} at {:?}: {}",
                product, binary_path, error
            )
        })?;

    let (log_buffer, log_drainers) = start_log_drainers(product, &mut child)?;

    let ws_url =
        match wait_for_cdp_server_ready(product, &mut child, port, &log_buffer, startup_timeout)
            .await
        {
            Ok(url) => url,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };

    Ok(CdpServerProcess {
        child,
        ws_url,
        _log_drainers: log_drainers,
    })
}

fn start_log_drainers(
    product: &str,
    child: &mut Child,
) -> Result<(LaunchLogBuffer, Vec<std::thread::JoinHandle<()>>), String> {
    let stdout = child.stdout.take().ok_or_else(|| {
        let _ = child.kill();
        format!("Failed to capture {} stdout", product)
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        let _ = child.kill();
        format!("Failed to capture {} stderr", product)
    })?;

    let logs = LaunchLogBuffer::default();
    let stdout_logs = logs.clone();
    let stderr_logs = logs.clone();

    let stdout_handle =
        std::thread::spawn(move || drain_reader(stdout, move |line| stdout_logs.push_stdout(line)));
    let stderr_handle =
        std::thread::spawn(move || drain_reader(stderr, move |line| stderr_logs.push_stderr(line)));

    Ok((logs, vec![stdout_handle, stderr_handle]))
}

fn drain_reader<R, F>(reader: R, mut push: F)
where
    R: std::io::Read,
    F: FnMut(String),
{
    for line in BufReader::new(reader).lines() {
        match line {
            Ok(line) => push(line),
            Err(_) => break,
        }
    }
}

async fn wait_for_cdp_server_ready(
    product: &str,
    child: &mut Child,
    port: u16,
    logs: &LaunchLogBuffer,
    startup_timeout: Duration,
) -> Result<String, String> {
    let deadline = std::time::Instant::now() + startup_timeout;
    let mut last_probe_error = None;

    loop {
        if let Ok(Some(status)) = child.try_wait() {
            // Give the drainer threads a brief chance to capture the final
            // process output before constructing the startup error.
            tokio::time::sleep(Duration::from_millis(25)).await;
            return Err(cdp_server_launch_error(
                product,
                &format!(
                    "{} exited before CDP became ready (status: {})",
                    product, status
                ),
                logs,
                last_probe_error.as_deref(),
            ));
        }

        match discover_cdp_url_with_timeout("127.0.0.1", port, None, SERVER_DISCOVERY_TIMEOUT).await
        {
            Ok(ws_url) => return Ok(ws_url),
            Err(error) => last_probe_error = Some(error),
        }

        if std::time::Instant::now() >= deadline {
            return Err(cdp_server_launch_error(
                product,
                &format!(
                    "Timed out after {}ms waiting for {} CDP endpoint on port {}",
                    startup_timeout.as_millis(),
                    product,
                    port
                ),
                logs,
                last_probe_error.as_deref(),
            ));
        }

        tokio::time::sleep(SERVER_POLL_INTERVAL).await;
    }
}

fn cdp_server_launch_error(
    product: &str,
    message: &str,
    logs: &LaunchLogBuffer,
    last_probe_error: Option<&str>,
) -> String {
    let stdout_lines = logs.snapshot_stdout();
    let stderr_lines = logs.snapshot_stderr();
    let mut details = Vec::new();

    if let Some(error) = last_probe_error {
        details.push(format!("Last probe error: {}", error));
    }

    if !stderr_lines.is_empty() {
        details.push(format!(
            "{} stderr (last {} lines):\n  {}",
            product,
            stderr_lines.len(),
            stderr_lines.join("\n  ")
        ));
    }

    if !stdout_lines.is_empty() {
        details.push(format!(
            "{} stdout (last {} lines):\n  {}",
            product,
            stdout_lines.len(),
            stdout_lines.join("\n  ")
        ));
    }

    if details.is_empty() {
        format!("{} (no stdout/stderr output from {})", message, product)
    } else {
        format!("{}\n{}", message, details.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener as TokioTcpListener;

    fn unused_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn serve_json_version_once_after_delay(port: u16, delay_ms: u64, body: &'static str) {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        let listener = TokioTcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        let _ = socket.read(&mut buf).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn waits_for_ready_without_logs() {
        let port = unused_port();
        tokio::spawn(serve_json_version_once_after_delay(
            port,
            150,
            r#"{"webSocketDebuggerUrl":"ws://127.0.0.1:9222/"}"#,
        ));

        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 5"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let (logs, _drainers) = start_log_drainers("Test browser", &mut child).unwrap();
        let ws_url = wait_for_cdp_server_ready(
            "Test browser",
            &mut child,
            port,
            &logs,
            Duration::from_secs(10),
        )
        .await
        .unwrap();

        assert_eq!(ws_url, format!("ws://127.0.0.1:{}/", port));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_exit_surfaces_logs() {
        let port = unused_port();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "echo boom >&2; sleep 0.1; exit 23"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let (logs, _drainers) = start_log_drainers("Test browser", &mut child).unwrap();
        let error = wait_for_cdp_server_ready(
            "Test browser",
            &mut child,
            port,
            &logs,
            Duration::from_secs(10),
        )
        .await
        .unwrap_err();

        assert!(error.contains("Test browser exited before CDP became ready"));
        assert!(error.contains("boom"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_reports_last_probe_error() {
        let port = unused_port();
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        let timeout = Duration::from_millis(300);
        let (logs, _drainers) = start_log_drainers("Test browser", &mut child).unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_cdp_server_ready("Test browser", &mut child, port, &logs, timeout),
        )
        .await
        .expect("ready wait should return before outer timeout")
        .unwrap_err();

        assert!(error.contains("Timed out after 300ms waiting for Test browser CDP endpoint"));
        assert!(
            error.contains("Failed to connect to CDP")
                || error.contains("Timeout connecting to CDP")
        );

        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn launch_error_includes_logs() {
        let logs = LaunchLogBuffer::default();
        logs.push_stdout("stdout line".to_string());
        logs.push_stderr("stderr line".to_string());
        let message = cdp_server_launch_error(
            "Test browser",
            "Test browser exited",
            &logs,
            Some("connect failed"),
        );
        assert!(message.contains("stdout line"));
        assert!(message.contains("stderr line"));
        assert!(message.contains("Last probe error: connect failed"));
    }

    #[test]
    fn launch_error_without_logs_names_product() {
        let logs = LaunchLogBuffer::default();
        let message = cdp_server_launch_error("Test browser", "startup failed", &logs, None);
        assert_eq!(
            message,
            "startup failed (no stdout/stderr output from Test browser)"
        );
    }
}
