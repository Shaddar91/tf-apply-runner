//tf-apply-mcp — the per-agent MCP stdio door for the tf-apply runner. Exposes exactly ONE tool,
//tf_apply{dir,apply}: holds NO credential in a tool arg, runs NO terraform in-process, forwards over
//loopback with the runner token from its OWN env, and returns ONLY {"run_id"} — never logs or creds.

use reqwest::Client;
use rmcp::{
    handler::server::{tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    schemars, tool, tool_handler, tool_router, transport::stdio, ErrorData, ServerHandler,
    ServiceExt,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

//The runner returns a bare run id almost immediately (it detaches the run), so a short ceiling is safe
//and keeps a trigger from ever hanging the agent's tool call.
const RUNNER_TIMEOUT: Duration = Duration::from_secs(30);

//No token field by design — the X-Runner-Token is injected into the door's environment, never taken from
//a tool argument, so a prompt-injected agent cannot present or exfiltrate a credential here.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct TfApplyArgs {
    #[schemars(description = "Absolute path to the Terraform directory to run against")]
    dir: String,
    #[schemars(
        description = "false (default) = terraform plan only; true = apply. Omit for a plan-only run."
    )]
    #[serde(default)]
    apply: bool,
}

#[derive(Clone)]
pub struct TfApplyMcp {
    http: Client,
    base_url: Arc<String>,
    //Read from the door's OWN env (never a tool arg). Never logged.
    token: Arc<String>,
    tool_router: ToolRouter<TfApplyMcp>,
}

impl TfApplyMcp {
    pub fn new(base_url: String, token: String) -> Self {
        let http = Client::builder()
            .timeout(RUNNER_TIMEOUT)
            .build()
            .expect("reqwest client");
        Self {
            http,
            base_url: Arc::new(base_url),
            token: Arc::new(token),
            tool_router: Self::tf_apply_tools_router(),
        }
    }
}

#[tool_router(router = tf_apply_tools_router, vis = "pub")]
impl TfApplyMcp {
    #[tool(
        description = "Triggers a Terraform run against an absolute directory via the tf-apply runner \
                       service. Terraform plan/apply output is never returned; this tool returns only a \
                       run identifier. Pass `dir` (an absolute path to the Terraform directory) and \
                       `apply` (false = plan-only, the safe default; true = apply). The run streams to \
                       disk server-side and any HCL failure is routed to a fresh terraform-expert task — \
                       you get back nothing but {\"run_id\":\"...\"}, never logs, never credentials."
    )]
    async fn tf_apply(
        &self,
        Parameters(a): Parameters<TfApplyArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        //The token rides the X-Runner-Token header from the door's OWN env — never echoed, never from `a`.
        let resp = self
            .http
            .post(format!("{}/apply", self.base_url))
            .header("X-Runner-Token", self.token.as_str())
            .json(&json!({ "dir": a.dir, "apply": a.apply }))
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                //Lift ONLY the run id out — every other field the runner returns is dropped on the floor.
                match r.json::<serde_json::Value>().await {
                    Ok(v) => match v.get("run_id").and_then(|x| x.as_str()) {
                        Some(run_id) => Ok(CallToolResult::success(vec![Content::text(
                            json!({ "run_id": run_id }).to_string(),
                        )])),
                        None => Ok(CallToolResult::error(vec![Content::text(
                            "runner accepted the trigger but returned no run_id".to_string(),
                        )])),
                    },
                    Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "could not parse runner response: {e}"
                    ))])),
                }
            }
            //Rejection: surface ONLY the status code, never the body (no output/token leak).
            Ok(r) => Ok(CallToolResult::error(vec![Content::text(format!(
                "runner rejected the request: HTTP {}",
                r.status()
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "could not reach the tf-apply runner: {e}"
            ))])),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TfApplyMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tf-apply", env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2025_03_26)
            .with_instructions(
                "Per-agent door to the tf-apply Terraform runner. One tool, tf_apply, triggers a run \
                 against an absolute directory and returns ONLY a run id — Terraform plan/apply output \
                 is never returned. The runner token is injected into this door's environment, never \
                 passed as a tool argument.",
            )
    }
}

//A bare-invocation stdio server: the ONLY accepted args are --help/--version, anything else is rejected
//fail-loud. Returns true if the caller should exit (help/version handled); false => start the server.
fn handle_cli_args() -> bool {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(first) = args.first() else {
        return false;
    };
    match first.as_str() {
        "--help" | "-h" => {
            println!(
                "tf-apply-mcp {} — per-agent MCP stdio door for the tf-apply Terraform runner.\n\
                 \n\
                 USAGE:\n    \
                     tf-apply-mcp            Start the MCP stdio JSON-RPC server (the default).\n\
                 \n\
                 OPTIONS:\n    \
                     -h, --help       Print this help and exit.\n    \
                     -V, --version    Print the version and exit.\n\
                 \n\
                 ENVIRONMENT:\n    \
                     TF_RUNNER_URL    Base URL of the tf-apply runner (default http://127.0.0.1:1937).\n    \
                     X_RUNNER_TOKEN   Bearer presented as X-Runner-Token; injected by the per-agent MCP config.\n\
                 \n\
                 The server exposes exactly one tool, tf_apply{{dir,apply}}, which forwards the call over\n\
                 loopback and returns ONLY a run id — Terraform output is never returned.",
                env!("CARGO_PKG_VERSION")
            );
            true
        }
        "--version" | "-V" => {
            println!("tf-apply-mcp {}", env!("CARGO_PKG_VERSION"));
            true
        }
        other => {
            eprintln!(
                "tf-apply-mcp: unknown argument '{other}'. Only --help/--version are accepted; run with \
                 no arguments to start the stdio server."
            );
            std::process::exit(2);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if handle_cli_args() {
        return Ok(());
    }

    //Logs MUST go to stderr — stdout carries the JSON-RPC stream and any stray line corrupts it.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();

    let base_url =
        std::env::var("TF_RUNNER_URL").unwrap_or_else(|_| "http://127.0.0.1:1937".into());
    //Read the runner token from the door's OWN env. Absent => empty => the runner answers 401 rather than
    //the door silently running unauthenticated.
    let token = std::env::var("X_RUNNER_TOKEN").unwrap_or_default();

    //Log only the PRESENCE of the token (a bool), never its value.
    tracing::info!(url = %base_url, token_present = !token.is_empty(), "tf-apply-mcp starting");

    let server = TfApplyMcp::new(base_url, token);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mockito::{Matcher, Server};
    use rmcp::ServerHandler;

    fn server_at(url: &str) -> TfApplyMcp {
        TfApplyMcp::new(url.into(), "tok".into())
    }

    fn body_text(r: &CallToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("")
    }

    fn args(dir: &str, apply: bool) -> TfApplyArgs {
        TfApplyArgs { dir: dir.into(), apply }
    }

    #[test]
    fn tools_capability_enabled() {
        let info = server_at("http://127.0.0.1:1937").get_info();
        assert!(info.capabilities.tools.is_some(), "tools capability must be present");
    }

    #[test]
    fn server_name_is_tf_apply() {
        assert_eq!(server_at("http://127.0.0.1:1937").get_info().server_info.name, "tf-apply");
    }

    #[test]
    fn exposes_only_tf_apply() {
        let s = server_at("http://127.0.0.1:1937");
        assert!(s.get_tool("tf_apply").is_some(), "tf_apply must be exposed");
        for absent in ["apply", "tf_plan", "plan", "status", "health", "run"] {
            assert!(s.get_tool(absent).is_none(), "unexpected extra tool exposed: {absent}");
        }
    }

    #[test]
    fn tf_apply_schema_is_dir_required_apply_optional_no_token_field() {
        let s = server_at("http://127.0.0.1:1937");
        let tool = s.get_tool("tf_apply").expect("tf_apply listed");
        let schema = &tool.input_schema;
        let props = schema.get("properties").and_then(|p| p.as_object()).expect("properties object");
        assert!(props.contains_key("dir"), "schema must carry dir");
        assert!(props.contains_key("apply"), "schema must carry apply");
        for forbidden in ["token", "x_runner_token", "runner_token", "url", "tf_runner_url"] {
            assert!(!props.contains_key(forbidden), "schema must not carry credential/steering field {forbidden}");
        }
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(required.contains(&"dir"), "dir must be required");
        assert!(!required.contains(&"apply"), "apply must be optional (plan-only default)");
    }

    #[tokio::test]
    async fn apply_200_returns_only_run_id() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("POST", "/apply")
            .with_status(200)
            .with_body(r#"{"run_id":"11111111-2222-3333-4444-555555555555"}"#)
            .create_async()
            .await;
        let r = server_at(&srv.url()).tf_apply(Parameters(args("/tmp/tf", false))).await.unwrap();
        assert_ne!(r.is_error, Some(true), "200 must not be an error: {}", body_text(&r));
        assert!(body_text(&r).contains("11111111-2222-3333-4444-555555555555"), "run id must be returned");
        assert!(body_text(&r).contains("run_id"), "result must be the {{run_id}} object");
    }

    //Even if the runner leaks extra fields, the door forwards ONLY run_id.
    #[tokio::test]
    async fn apply_200_strips_every_field_but_run_id() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("POST", "/apply")
            .with_status(200)
            .with_body(
                r#"{"run_id":"r-42","log_tail":"SENTINEL_TF_PLAN_OUTPUT","dir":"/secret/state/path","status":"running","exit_code":0}"#,
            )
            .create_async()
            .await;
        let r = server_at(&srv.url()).tf_apply(Parameters(args("/tmp/tf", true))).await.unwrap();
        let body = body_text(&r);
        assert!(body.contains("r-42"), "run id must survive");
        assert!(!body.contains("SENTINEL_TF_PLAN_OUTPUT"), "terraform output must never be forwarded");
        assert!(!body.contains("/secret/state/path"), "dir/state path must never be forwarded");
        assert!(!body.contains("exit_code"), "no field other than run_id may be forwarded");
    }

    #[tokio::test]
    async fn apply_forwards_token_header_and_typed_body() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("POST", "/apply")
            .match_header("x-runner-token", "tok")
            .match_body(Matcher::PartialJson(json!({ "dir": "/tmp/tf", "apply": true })))
            .with_status(200)
            .with_body(r#"{"run_id":"r-1"}"#)
            .create_async()
            .await;
        let r = server_at(&srv.url()).tf_apply(Parameters(args("/tmp/tf", true))).await.unwrap();
        assert_ne!(r.is_error, Some(true), "token header + typed body must match the mock: {}", body_text(&r));
    }

    #[tokio::test]
    async fn apply_rejection_surfaces_status_not_body() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("POST", "/apply")
            .with_status(403)
            .with_body(r#"{"error":"SENTINEL_ALLOWLIST_REJECTION_DETAIL"}"#)
            .create_async()
            .await;
        let r = server_at(&srv.url()).tf_apply(Parameters(args("/etc", false))).await.unwrap();
        let body = body_text(&r);
        assert_eq!(r.is_error, Some(true), "403 must be an error result");
        assert!(body.contains("403"), "the status code must be surfaced");
        assert!(!body.contains("SENTINEL_ALLOWLIST_REJECTION_DETAIL"), "the runner body must never be forwarded");
    }

    #[tokio::test]
    async fn apply_200_missing_run_id_is_error() {
        let mut srv = Server::new_async().await;
        let _m = srv
            .mock("POST", "/apply")
            .with_status(200)
            .with_body(r#"{"status":"ok"}"#)
            .create_async()
            .await;
        let r = server_at(&srv.url()).tf_apply(Parameters(args("/tmp/tf", false))).await.unwrap();
        assert_eq!(r.is_error, Some(true), "a 200 without run_id must be an error result");
    }

    #[tokio::test]
    async fn apply_unreachable_runner_is_error() {
        let r = server_at("http://127.0.0.1:1").tf_apply(Parameters(args("/tmp/tf", false))).await.unwrap();
        assert_eq!(r.is_error, Some(true), "an unreachable runner must be an error result");
    }
}
