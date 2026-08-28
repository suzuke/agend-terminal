//! Generic `agend-terminal tool` front end for the daemon's MCP catalogue.
//!
//! This module intentionally contains no per-tool dispatch.  Names, actions,
//! arguments, permissions, and return values remain owned by the daemon's MCP
//! registry; the CLI only composes JSON and classifies the shared wire result.

use crate::mcp_wire::{self, ResponseClass, WireError, WireErrorKind};
use serde_json::{Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

const USAGE: &str = "Usage: agend-terminal tool <NAME> [--action <A>] [--json <JSON|-|@FILE>] [--arg K=V]... [--home <DIR>] [--request-id <UUID>]";
const USAGE_FIX: &str = "Provide a tool name and valid options.";

/// Arguments for the hidden top-level `tool` command.
pub(crate) struct ToolArgs {
    pub name: Option<String>,
    pub second_name: Option<String>,
    pub action: Option<String>,
    pub json: Option<String>,
    pub args: Vec<String>,
    pub home: Option<PathBuf>,
    pub request_id: Option<String>,
}

/// Print the machine-readable malformed-invocation contract and retain a
/// human-readable diagnostic for interactive callers.
pub(crate) fn print_usage_error(error: impl Into<String>) {
    let error = error.into();
    print_json(&serde_json::json!({"error": error, "fix": USAGE_FIX}));
    eprintln!("{error}\n{USAGE}");
}

/// Run one generic tool invocation, or the live `list` / `schema` discovery
/// commands.  The returned code is already the public CLI exit code.
pub(crate) fn run(default_home: &Path, input: ToolArgs) -> i32 {
    let Some(name) = input.name.as_deref() else {
        print_usage_error("tool requires a tool name");
        return 3;
    };
    let home = input.home.as_deref().unwrap_or(default_home);
    let instance = std::env::var("AGEND_INSTANCE_NAME").unwrap_or_default();

    if name == "schema" {
        let Some(schema_name) = input.second_name.as_deref() else {
            print_usage_error("schema requires a tool name");
            return 3;
        };
        if input.action.is_some()
            || input.json.is_some()
            || !input.args.is_empty()
            || input.request_id.is_some()
        {
            print_usage_error("tool schema accepts only the tool name and --home");
            return 3;
        }
        return run_schema(home, &instance, schema_name);
    }

    if input.second_name.is_some() {
        print_usage_error("unexpected extra positional argument");
        return 3;
    }
    if name == "list" {
        if input.action.is_some()
            || input.json.is_some()
            || !input.args.is_empty()
            || input.request_id.is_some()
        {
            print_usage_error("tool list accepts only --home");
            return 3;
        }
        return run_list(home, &instance);
    }

    let arguments = match compose_arguments(input.json.as_deref(), &input.args, input.action) {
        Ok(arguments) => arguments,
        Err(error) => {
            print_usage_error(format!("tool invocation is malformed: {error}"));
            return 3;
        }
    };
    let request_id = match input.request_id.as_deref() {
        Some(value) => match uuid::Uuid::parse_str(value) {
            Ok(_) => Some(value),
            Err(_) => {
                print_usage_error("tool invocation is malformed: --request-id must be a UUID");
                return 3;
            }
        },
        None => None,
    };
    let envelope = mcp_wire::tool_envelope(&instance, name, arguments, request_id);
    let mut client = mcp_wire::Client::default();
    let response = match client.request(home, &envelope) {
        Ok(response) => response,
        Err(error) => return print_wire_error(error),
    };
    let class = mcp_wire::classify_response(&response);
    print_response_payload(&response, class);
    match class {
        ResponseClass::Ok | ResponseClass::Accepted => 0,
        ResponseClass::ToolError => 1,
        ResponseClass::Refused => 2,
        ResponseClass::Indeterminate | ResponseClass::ProtocolError => 4,
    }
}

fn run_list(home: &Path, instance: &str) -> i32 {
    let mut client = mcp_wire::Client::default();
    let result = match client.tools_list(home, instance) {
        Ok(result) => result,
        Err(error) => return print_wire_error(error),
    };
    let class = mcp_wire::classify_response(&result);
    if matches!(class, ResponseClass::ProtocolError) {
        print_response_payload(&result, class);
        return 4;
    }
    print_response_payload(&result, class);
    if matches!(class, ResponseClass::Refused) {
        2
    } else if matches!(class, ResponseClass::Indeterminate) {
        4
    } else if matches!(class, ResponseClass::ToolError) {
        1
    } else {
        0
    }
}

fn run_schema(home: &Path, instance: &str, requested: &str) -> i32 {
    let mut client = mcp_wire::Client::default();
    let result = match client.tools_list(home, instance) {
        Ok(result) => result,
        Err(error) => return print_wire_error(error),
    };
    let class = mcp_wire::classify_response(&result);
    if !matches!(class, ResponseClass::Ok) {
        print_response_payload(&result, class);
        return match class {
            ResponseClass::Refused => 2,
            ResponseClass::Indeterminate | ResponseClass::ProtocolError => 4,
            _ => 1,
        };
    }
    let Some(tools) = result
        .get("result")
        .and_then(|value| value.get("tools"))
        .and_then(Value::as_array)
    else {
        print_json(&json_error("daemon tools list omitted tools"));
        return 4;
    };
    let Some(definition) = tools
        .iter()
        .find(|tool| tool.get("name").and_then(Value::as_str) == Some(requested))
    else {
        let error = json_error(&format!("unknown tool: {requested}"));
        print_json(&error);
        eprintln!("Use `agend-terminal tool list` to discover live tools.");
        return 1;
    };
    print_json(definition);
    0
}

fn print_wire_error(error: WireError) -> i32 {
    let class = match error.kind() {
        WireErrorKind::Read | WireErrorKind::Write | WireErrorKind::Protocol => {
            ResponseClass::Indeterminate
        }
        WireErrorKind::NoDaemon | WireErrorKind::Refused | WireErrorKind::Connect => {
            ResponseClass::Refused
        }
    };
    let response = json_error(&error.to_string());
    print_response_payload(&response, class);
    match class {
        ResponseClass::Refused => 2,
        _ => {
            eprintln!("indeterminate outcome: check state with a read-only tool before resending");
            4
        }
    }
}

fn print_response_payload(response: &Value, class: ResponseClass) {
    let payload = match class {
        ResponseClass::Ok | ResponseClass::ToolError => response.get("result").unwrap_or(response),
        ResponseClass::Accepted
        | ResponseClass::Refused
        | ResponseClass::Indeterminate
        | ResponseClass::ProtocolError => response,
    };
    print_json(payload);
    if class == ResponseClass::ToolError {
        if let Some(error) = payload.get("error").and_then(Value::as_str) {
            eprintln!("tool error: {error}");
            // stringly-allow: non-authoritative UX schema-help classification of an already-returned untyped MCP error; not authorization/outcome classification.
            if error.contains("missing") || error.contains("unknown") || error.contains("required")
            {
                eprintln!("Inspect parameters with `agend-terminal tool schema <NAME>`.");
            }
        }
    } else if class == ResponseClass::Refused {
        if let Some(error) = response.get("error").and_then(Value::as_str) {
            eprintln!("tool refused: {error}");
        }
    }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

fn json_error(error: &str) -> Value {
    serde_json::json!({"error": error})
}

/// Compose the JSON arguments in the documented order: base JSON, repeated
/// string-valued `--arg` overrides, then the dedicated action override.
pub(crate) fn compose_arguments(
    json_spec: Option<&str>,
    args: &[String],
    action: Option<String>,
) -> Result<Value, String> {
    let base = match json_spec {
        None => Value::Object(Map::new()),
        Some("-") => {
            let mut input = String::new();
            std::io::stdin()
                .read_to_string(&mut input)
                .map_err(|error| format!("read --json -: {error}"))?;
            parse_json_object(&input)?
        }
        Some(spec) if spec.starts_with('@') => {
            let path = &spec[1..];
            if path.is_empty() {
                return Err("--json @FILE requires a file path".to_string());
            }
            let input = std::fs::read_to_string(path)
                .map_err(|error| format!("read --json @{path}: {error}"))?;
            parse_json_object(&input)?
        }
        Some(spec) => parse_json_object(spec)?,
    };
    let mut object = base
        .as_object()
        .cloned()
        .ok_or_else(|| "--json must contain a JSON object".to_string())?;
    for arg in args {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("--arg expects K=V, got {arg:?}"))?;
        if key.is_empty() {
            return Err("--arg key must not be empty".to_string());
        }
        object.insert(key.to_string(), Value::String(value.to_string()));
    }
    if let Some(action) = action {
        object.insert("action".to_string(), Value::String(action));
    }
    Ok(Value::Object(object))
}

fn parse_json_object(input: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(input).map_err(|error| format!("invalid JSON: {error}"))?;
    if !value.is_object() {
        return Err("JSON arguments must be an object".to_string());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_apply_string_overrides_then_action() {
        let arguments = compose_arguments(
            Some(r#"{"action":"old","count":1}"#),
            &["count=two".to_string(), "message=left=right".to_string()],
            Some("new".to_string()),
        )
        .expect("valid arguments should compose");

        assert_eq!(arguments["count"], "two");
        assert_eq!(arguments["message"], "left=right");
        assert_eq!(arguments["action"], "new");
    }

    #[test]
    fn arguments_require_a_json_object() {
        let error = compose_arguments(Some("[1, 2, 3]"), &[], None)
            .expect_err("array arguments should be rejected");
        assert_eq!(error, "JSON arguments must be an object");
    }
}
