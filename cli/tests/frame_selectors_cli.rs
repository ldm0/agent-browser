use serde_json::{json, Value};
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const BIN: &str = env!("CARGO_BIN_EXE_agent-browser");

struct Session(TempDir);

impl Session {
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("config.json"), "{}").unwrap();
        Self(dir)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if (name.starts_with("AGENT_BROWSER_") && name != "AGENT_BROWSER_EXECUTABLE_PATH")
                || matches!(
                    name.to_ascii_lowercase().as_str(),
                    "http_proxy" | "https_proxy" | "all_proxy" | "no_proxy"
                )
            {
                command.env_remove(key);
            }
        }
        command
            .current_dir(self.0.path())
            .env("AGENT_BROWSER_SESSION", "frame-selectors")
            .env("AGENT_BROWSER_SOCKET_DIR", self.0.path())
            .env("AGENT_BROWSER_CONFIG", self.0.path().join("config.json"))
            .env("NO_COLOR", "1");
        command
    }

    async fn cli(&self, args: &[&str]) -> Value {
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::from(self.command())
                .args(args)
                .arg("--json")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("CLI timed out")
        .expect("failed to invoke CLI");
        assert!(
            output.status.success(),
            "CLI {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["success"], true, "{response}");
        response["data"].clone()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.command().arg("close").output();
    }
}

async fn serve(listener: tokio::net::TcpListener) {
    while let Ok((mut stream, _)) = listener.accept().await {
        tokio::spawn(async move {
            let mut request = [0; 4096];
            let n = stream.read(&mut request).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..n]);
            let Some(path) = request.split_whitespace().nth(1) else {
                return;
            };
            let url = url::Url::parse(&format!("http://fixture{path}")).unwrap();
            let body = match url.path() {
                "/parent" => {
                    let child = url.query_pairs().find(|(key, _)| key == "child").unwrap().1;
                    format!(r#"<!doctype html><input id="value" value="parent"><div role="none">Parent</div>
<script>addEventListener('message',e=>window.childReady=e.data==='ready')</script>
<iframe id="child" name="child" title="Child frame" src="{child}" style="display:block;margin:60px;border:10px solid;width:600px;height:250px"></iframe>"#)
                }
                "/next" => "<!doctype html><input id=after-navigation value=new-document>".into(),
                _ => r#"<!doctype html><input id="value" value="initial" data-probe="dom">
<button id="act" onclick="document.querySelector('#value').value+='-clicked';document.querySelector('#ready').hidden=false">Act</button>
<button id="navigate" onclick="setTimeout(()=>location.href='/next',150)">Navigate</button>
<div id="ready" hidden>Ready</div><div role="none">Child</div>
<script>const el=document.querySelector('#value');const get=el.getAttribute;el.getAttribute=function(name){return name==='data-probe'?'page':get.call(this,name)};parent.postMessage('ready','*')</script>"#.into(),
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

async fn rpc(
    stdin: &mut tokio::process::ChildStdin,
    stdout: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    method: &str,
    params: Value,
) -> Value {
    let request = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params});
    stdin
        .write_all(format!("{request}\n").as_bytes())
        .await
        .unwrap();
    let response: Value = serde_json::from_str(
        &tokio::time::timeout(Duration::from_secs(20), stdout.next_line())
            .await
            .expect("MCP timed out")
            .unwrap()
            .expect("MCP closed stdout"),
    )
    .unwrap();
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

#[tokio::test]
#[ignore = "requires Chrome"]
async fn e2e_frame_selectors_cli_and_mcp_share_context() {
    let child = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let parent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let child_port = child.local_addr().unwrap().port();
    let parent_port = parent.local_addr().unwrap().port();
    let child_server = tokio::spawn(serve(child));
    let parent_server = tokio::spawn(serve(parent));
    let session = Session::new();
    let mut mcp = tokio::process::Command::from(session.command())
        .args(["mcp", "--tools", "all"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = mcp.stdin.take().unwrap();
    let mut stdout = BufReader::new(mcp.stdout.take().unwrap()).lines();
    rpc(
        &mut stdin,
        &mut stdout,
        "initialize",
        json!({"protocolVersion":"2024-11-05", "capabilities":{}, "clientInfo":{"name":"frame-test", "version":"1"}}),
    ).await;
    stdin
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    for (child_url, cross_origin, attribute) in [
        (
            format!("http://127.0.0.1:{parent_port}/child"),
            false,
            "page",
        ),
        (format!("http://127.0.0.1:{child_port}/child"), true, "dom"),
        (format!("http://localhost:{child_port}/child"), true, "page"),
    ] {
        let url = format!(
            "http://127.0.0.1:{parent_port}/parent?child={}",
            urlencoding::encode(&child_url)
        );
        session.cli(&["open", &url]).await;
        session
            .cli(&["wait", "--fn", "window.childReady === true"])
            .await;
        assert_eq!(
            session
                .cli(&[
                    "eval",
                    "document.querySelector('#child').contentDocument === null"
                ])
                .await["result"],
            cross_origin
        );
        let snapshot = session.cli(&["snapshot"]).await;
        let frame_ref = snapshot["refs"]
            .as_object()
            .unwrap()
            .iter()
            .find(|(_, entry)| entry["name"] == "Child frame")
            .unwrap()
            .0;
        let frame = format!("@{frame_ref}");
        session.cli(&["frame", &frame]).await;
        session.cli(&["wait", "#value"]).await;
        assert_eq!(
            session.cli(&["get", "attr", "#value", "data-probe"]).await["value"],
            attribute
        );
        session.cli(&["fill", "#value", "cli-value"]).await;
        assert_eq!(
            session
                .cli(&["get", "value", "xpath=//input[@id='value']"])
                .await["value"],
            "cli-value"
        );
        assert_eq!(
            session.cli(&["find", "role", "none", "text"]).await["text"],
            "Child"
        );
        for (tool, arguments, expected_value) in [
            ("get_value", json!({"selector":"#value"}), Some("cli-value")),
            (
                "fill",
                json!({"selector":"#value", "text":"mcp-value"}),
                None,
            ),
            ("click", json!({"selector":"#act"}), None),
            ("wait_for_selector", json!({"selector":"#ready"}), None),
            (
                "get_value",
                json!({"selector":"#value"}),
                Some("mcp-value-clicked"),
            ),
            ("frame_main", json!({}), None),
            ("get_value", json!({"selector":"#value"}), Some("parent")),
            ("frame_switch", json!({"frame":frame}), None),
        ] {
            let result = rpc(
                &mut stdin,
                &mut stdout,
                "tools/call",
                json!({"name":format!("agent_browser_{tool}"), "arguments":arguments}),
            )
            .await;
            assert_eq!(result["isError"], false, "{result}");
            let response = &result["structuredContent"]["response"];
            assert_eq!(response["success"], true, "{response}");
            if let Some(value) = expected_value {
                assert_eq!(response["data"]["value"], value);
            }
        }
        assert_eq!(
            session.cli(&["get", "value", "#value"]).await["value"],
            "mcp-value-clicked"
        );
        session.cli(&["click", "#navigate"]).await;
        session.cli(&["wait", "#after-navigation"]).await;
        assert_eq!(
            session.cli(&["get", "value", "#after-navigation"]).await["value"],
            "new-document"
        );
        session.cli(&["frame", "main"]).await;
        assert_eq!(
            session.cli(&["get", "value", "#value"]).await["value"],
            "parent"
        );
    }
    session.cli(&["close"]).await;
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(5), mcp.wait())
        .await
        .unwrap()
        .unwrap();
    child_server.abort();
    parent_server.abort();
}
