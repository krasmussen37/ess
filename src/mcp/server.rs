use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::tools;

const JSONRPC_VERSION: &str = "2.0";

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

pub fn run_stdio_server() -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    for line in stdin.lock().lines() {
        let line = line.context("read MCP stdin line")?;
        if line.trim().is_empty() {
            continue;
        }

        if let Some(response_line) = handle_request_line(&line) {
            writeln!(stdout, "{response_line}").context("write MCP stdout response")?;
            stdout.flush().context("flush MCP stdout response")?;
        }
    }

    Ok(())
}

fn handle_request_line(line: &str) -> Option<String> {
    let request: JsonRpcRequest = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            return Some(
                jsonrpc_error(None, -32700, "Parse error", Some(error.to_string())).to_string(),
            );
        }
    };

    if request.jsonrpc != JSONRPC_VERSION {
        return Some(
            jsonrpc_error(
                request.id,
                -32600,
                "Invalid Request",
                Some("jsonrpc must be \"2.0\"".to_string()),
            )
            .to_string(),
        );
    }

    let id = request.id.clone();
    let response = match request.method.as_str() {
        "initialize" => jsonrpc_result(
            id,
            json!({
                "serverInfo": {
                    "name": env!("CARGO_PKG_NAME"),
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": {}
                }
            }),
        ),
        "tools/list" => jsonrpc_result(
            id,
            json!({
                "tools": tools::tool_schemas()
            }),
        ),
        "tools/call" => {
            let Some(tool_name) = request.params.get("name").and_then(Value::as_str) else {
                return Some(
                    jsonrpc_error(
                        id,
                        -32602,
                        "Invalid params",
                        Some("tools/call requires params.name".to_string()),
                    )
                    .to_string(),
                );
            };

            let arguments = request
                .params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match tools::call_tool(tool_name, arguments) {
                Ok(result) => jsonrpc_result(id, result),
                Err(error) => {
                    jsonrpc_error(id, -32000, "Tool execution failed", Some(error.to_string()))
                }
            }
        }
        _ => jsonrpc_error(
            id,
            -32601,
            "Method not found",
            Some(format!("Unknown method '{}'", request.method)),
        ),
    };

    if response.get("id").is_some_and(Value::is_null) && request.id.is_none() {
        return None;
    }

    Some(response.to_string())
}

fn jsonrpc_result(id: Option<Value>, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "result": result
    })
}

fn jsonrpc_error(id: Option<Value>, code: i64, message: &str, data: Option<String>) -> Value {
    let mut error = json!({
        "code": code,
        "message": message,
    });
    if let Some(data) = data {
        error["data"] = Value::String(data);
    }

    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id.unwrap_or(Value::Null),
        "error": error
    })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::handle_request_line;

    fn parse_response(line: &str) -> Value {
        serde_json::from_str(line).expect("parse response JSON")
    }

    #[test]
    fn initialize_returns_server_info_and_capabilities() {
        let request = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let response = parse_response(&handle_request_line(request).expect("response"));
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["serverInfo"]["name"], "ess");
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_returns_tool_definitions() {
        let request = r#"{"jsonrpc":"2.0","id":"abc","method":"tools/list","params":{}}"#;
        let response = parse_response(&handle_request_line(request).expect("response"));
        assert_eq!(response["id"], "abc");
        assert!(response["result"]["tools"].is_array());
    }

    #[test]
    fn invalid_json_returns_parse_error() {
        let response = parse_response(&handle_request_line("{").expect("response"));
        assert_eq!(response["error"]["code"], -32700);
    }
}
