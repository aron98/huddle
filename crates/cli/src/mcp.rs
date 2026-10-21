//! A stdio MCP server (`huddle-cli mcp`, issue #9): the same REST
//! calls as the CLI commands, exposed as agent tools. Hand-rolled
//! JSON-RPC (one JSON message per line) — see ADR 013 for why we did
//! not adopt an SDK: the protocol subset we serve is initialize,
//! tools/list, tools/call, and ping, and owning those ~200 lines beats
//! a dependency for a thin wrapper.
//!
//! Every tool call loads the stored login fresh, so an agent can run
//! `huddle-cli login` in another terminal and the already-running
//! server picks it up.

use std::{
    io::{BufRead, Write},
    path::PathBuf,
};

use serde_json::{Map, Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::client::{CliError, Client};

/// The protocol version we speak. MCP clients negotiate by sending
/// theirs; this one is the widely-supported baseline.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Serve MCP over stdin/stdout until stdin closes. Nothing but
/// JSON-RPC responses ever touches stdout; diagnostics would go to
/// stderr (currently we emit none).
pub fn serve(server: Option<String>, token_file: Option<PathBuf>) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            // Unparseable input: the JSON-RPC id is unknowable, so the
            // spec-correct move is to stay silent and keep serving.
            continue;
        };
        if let Some(response) = handle(message, &server, &token_file)
            && let Ok(text) = serde_json::to_string(&response)
        {
            let mut out = stdout.lock();
            let _ = writeln!(out, "{text}");
            let _ = out.flush();
        }
    }
}

/// Handle one JSON-RPC message. Notifications (no `id`) get no reply.
pub fn handle(
    message: Value,
    server: &Option<String>,
    token_file: &Option<PathBuf>,
) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {} },
            "serverInfo": {
                "name": "huddle-cli",
                "version": env!("CARGO_PKG_VERSION"),
            },
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => Ok(run_tool(message.get("params"), server, token_file)),
        other => Err(json!({
            "code": -32601,
            "message": format!("unknown method: {other}"),
        })),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    })
}

/// The tool catalog, straight from the issue #9 requirements.
fn tools() -> Value {
    json!([
        {
            "name": "create_meeting",
            "description": "Create a huddle meeting and return its join link and calendar (.ics) URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Meeting title." },
                    "start": {
                        "type": "string",
                        "description": "When the meeting starts: an RFC 3339 time (e.g. 2026-10-22T09:30:00Z) or a relative offset from now (e.g. '1h', '90m'). Defaults to now."
                    },
                    "duration_minutes": {
                        "type": "integer",
                        "description": "Meeting length in minutes. Default 30."
                    }
                },
                "required": ["title"]
            }
        },
        {
            "name": "list_upcoming",
            "description": "List the logged-in organizer's upcoming meetings with join links.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "get_meeting",
            "description": "Fetch one meeting by its code.",
            "inputSchema": {
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }
        },
        {
            "name": "cancel_meeting",
            "description": "Cancel a meeting by its code. Destructive: the meeting can no longer be joined.",
            "inputSchema": {
                "type": "object",
                "properties": { "code": { "type": "string" } },
                "required": ["code"]
            }
        }
    ])
}

/// Run one tools/call. Tool failures are tool results (`isError`), not
/// JSON-RPC errors — that is how an agent gets to see them.
fn run_tool(
    params: Option<&Value>,
    server: &Option<String>,
    token_file: &Option<PathBuf>,
) -> Value {
    let params = params.unwrap_or(&Value::Null);
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));
    let outcome: Result<(String, Option<Value>), CliError> = match name.as_str() {
        "create_meeting" => create(&args, server, token_file),
        "list_upcoming" => list_upcoming(server, token_file),
        "get_meeting" => get(&args, server, token_file),
        "cancel_meeting" => cancel(&args, server, token_file),
        other => {
            return tool_failure(format!("unknown tool: {other}"));
        }
    };
    match outcome {
        Ok((text, structured)) => tool_result(text, structured),
        Err(error) => tool_failure(crate::client::describe(&error)),
    }
}

fn tool_result(text: String, structured: Option<Value>) -> Value {
    let mut result = Map::new();
    result.insert(
        "content".to_owned(),
        json!([{ "type": "text", "text": text }]),
    );
    if let Some(structured) = structured {
        result.insert("structuredContent".to_owned(), structured);
    }
    Value::Object(result)
}

fn tool_failure(text: String) -> Value {
    json!({ "content": [{ "type": "text", "text": text }], "isError": true })
}

fn new_client(server: &Option<String>, token_file: &Option<PathBuf>) -> Client {
    Client::new(server.clone(), token_file.as_deref())
}

fn create(
    args: &Value,
    server: &Option<String>,
    token_file: &Option<PathBuf>,
) -> Result<(String, Option<Value>), CliError> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Usage("missing required argument: title".to_owned()))?;
    let starts_at = match args.get("start").and_then(Value::as_str) {
        Some(raw) => parse_start(raw)?,
        None => OffsetDateTime::now_utc(),
    };
    let minutes = args.get("duration_minutes").and_then(Value::as_i64);
    let duration_minutes = match minutes {
        Some(minutes) if (0..1_000_000).contains(&minutes) => minutes,
        Some(_) => {
            return Err(CliError::Usage(
                "duration_minutes must be between 1 and 999_999".to_owned(),
            ));
        }
        None => 30,
    };
    let ends_at = starts_at + std::time::Duration::from_secs((duration_minutes * 60) as u64);
    let mut client = new_client(server, token_file);
    let meeting = client.create_meeting(title, starts_at, ends_at)?;
    let base = client.require_server()?.to_owned();
    let code = meeting.meeting.code.clone();
    let join_url = format!("{base}/m/{code}");
    let ics_url = format!("{base}/api/meetings/{code}/ics");
    let text = format!(
        "Created '{}'\n  code: {code}\n  join: {join_url}\n  ics: {ics_url}",
        meeting.meeting.title
    );
    Ok((
        text,
        Some(json!({
            "meeting": meeting,
            "join_url": join_url,
            "ics_url": ics_url,
        })),
    ))
}

fn list_upcoming(
    server: &Option<String>,
    token_file: &Option<PathBuf>,
) -> Result<(String, Option<Value>), CliError> {
    let mut client = new_client(server, token_file);
    let meetings = client.list_meetings()?;
    let base = client.require_server()?.to_owned();
    let now = OffsetDateTime::now_utc();
    let upcoming: Vec<_> = meetings.iter().filter(|m| is_upcoming(m, now)).collect();
    let mut lines = Vec::new();
    let mut structured = Vec::new();
    for meeting in upcoming {
        let code = meeting.meeting.code.clone();
        let join_url = format!("{base}/m/{code}");
        let starts = meeting
            .meeting
            .starts_at
            .format(&Rfc3339)
            .unwrap_or_default();
        lines.push(format!("{starts}  {}  {join_url}", meeting.meeting.title));
        structured.push(json!({ "meeting": meeting, "join_url": join_url }));
    }
    let text = if lines.is_empty() {
        "No upcoming meetings.".to_owned()
    } else {
        lines.join("\n")
    };
    Ok((text, Some(json!({ "meetings": structured }))))
}

fn get(
    args: &Value,
    server: &Option<String>,
    token_file: &Option<PathBuf>,
) -> Result<(String, Option<Value>), CliError> {
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Usage("missing required argument: code".to_owned()))?;
    let mut client = new_client(server, token_file);
    let meeting = client.get_meeting(code)?;
    let join_url = client.join_url(&meeting.meeting.code)?;
    let starts = meeting
        .meeting
        .starts_at
        .format(&Rfc3339)
        .unwrap_or_default();
    let ends = meeting.meeting.ends_at.format(&Rfc3339).unwrap_or_default();
    let text = format!(
        "{}\n  code: {}\n  starts: {starts}\n  ends: {ends}\n  join: {join_url}",
        meeting.meeting.title, meeting.meeting.code
    );
    Ok((
        text,
        Some(json!({ "meeting": meeting, "join_url": join_url })),
    ))
}

fn cancel(
    args: &Value,
    server: &Option<String>,
    token_file: &Option<PathBuf>,
) -> Result<(String, Option<Value>), CliError> {
    let code = args
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| CliError::Usage("missing required argument: code".to_owned()))?;
    let mut client = new_client(server, token_file);
    client.cancel_meeting(code)?;
    Ok((
        format!("Cancelled {code}."),
        Some(json!({ "code": code, "cancelled": true })),
    ))
}

/// A meeting is upcoming while any of its time is still in the future
/// and it (or the series it belongs to) is not cancelled.
fn is_upcoming(meeting: &huddle_protocol::MeetingResponse, now: OffsetDateTime) -> bool {
    if meeting.meeting.cancelled {
        return false;
    }
    if meeting.meeting.ends_at > now {
        return true;
    }
    meeting
        .upcoming_occurrences
        .iter()
        .any(|o| !o.cancelled && o.ends_at > now)
}

/// An MCP `start` argument: RFC 3339, or a relative duration from now.
fn parse_start(raw: &str) -> Result<OffsetDateTime, CliError> {
    if let Ok(at) = OffsetDateTime::parse(raw, &Rfc3339) {
        return Ok(at);
    }
    let seconds = crate::parse_duration(raw)?;
    Ok(OffsetDateTime::now_utc() + std::time::Duration::from_secs(seconds as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, id: i64) -> Value {
        json!({ "jsonrpc": "2.0", "id": id, "method": method })
    }

    #[test]
    fn initialize_and_list_answer_the_protocol_shapes() {
        let no_server: Option<String> = None;
        let no_file: Option<std::path::PathBuf> = None;
        let init = handle(request("initialize", 1), &no_server, &no_file).expect("response");
        assert_eq!(
            init["result"]["protocolVersion"].as_str(),
            Some(PROTOCOL_VERSION)
        );
        let list = handle(request("tools/list", 2), &no_server, &no_file).expect("response");
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(
            names,
            vec![
                "create_meeting",
                "list_upcoming",
                "get_meeting",
                "cancel_meeting"
            ]
        );
    }

    #[test]
    fn notifications_and_unknown_methods_behave() {
        let no_server: Option<String> = None;
        let no_file: Option<std::path::PathBuf> = None;
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert!(handle(note, &no_server, &no_file).is_none());
        let unknown = handle(request("nope", 7), &no_server, &no_file).expect("response");
        assert_eq!(unknown["error"]["code"].as_i64(), Some(-32601));
        let bogus = json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": { "name": "not_a_tool", "arguments": {} }
        });
        let answer = handle(bogus, &no_server, &no_file).expect("response");
        assert_eq!(answer["result"]["isError"].as_bool(), Some(true));
    }
}
