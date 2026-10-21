#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

//! `huddle-cli` — the huddle command line and MCP server (issue #9).
//!
//! Design constraints (from the issue, deliberately non-negotiable):
//! every command is machine-readable with `--json`, fully
//! non-interactive (destructive commands take an explicit `--yes`
//! instead of prompting), and exit codes are a contract:
//! 0 ok, 1 usage, 2 auth, 3 server-semantic error. The crate is
//! fully synchronous — `reqwest::blocking` runs its own reactor.

mod client;
mod device;
mod mcp;
mod store;

use std::io::Write;

use clap::{Parser, Subcommand, error::ErrorKind};
use client::{CliError, Client};
use serde_json::json;
use store::StoredLogin;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const EXAMPLES: &str = "\
EXAMPLES:
  huddle-cli login --server https://huddle.example.com
  huddle-cli whoami
  huddle-cli create --title \"Sync\" --in 1h
  huddle-cli create --title \"Standup\" --at 2026-10-22T09:30:00Z --duration 15m --json
  huddle-cli list --json
  huddle-cli get abcd-2345
  huddle-cli cancel abcd-2345 --yes
  huddle-cli ics abcd-2345 --out standup.ics
  huddle-cli mcp    # stdio MCP server for agents
";

const EXIT_OK: i32 = 0;
const EXIT_USAGE: i32 = 1;
const EXIT_AUTH: i32 = 2;
const EXIT_SERVER: i32 = 3;

#[derive(Parser)]
#[command(
    name = "huddle-cli",
    version,
    about = "Manage huddle meetings from the terminal, or serve them to an agent over MCP",
    after_help = EXAMPLES,
)]
struct Cli {
    /// Base URL of the huddle server (defaults to the one you logged in to)
    #[arg(long, global = true)]
    server: Option<String>,

    /// Where to keep the login (defaults to $HUDDLE_CLI_TOKEN_FILE or ~/.config/huddle/cli.json)
    #[arg(long, global = true, value_name = "PATH")]
    token_file: Option<std::path::PathBuf>,

    /// Print machine-readable JSON on stdout
    #[arg(short = 'j', long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Approve this CLI in the browser (device flow) and store the login
    Login,
    /// Revoke the stored login on the server and delete it locally
    Logout,
    /// Show which organizer the stored login belongs to
    Whoami,
    /// Create a meeting
    Create {
        /// Meeting title
        #[arg(long)]
        title: String,
        /// Start this long from now, e.g. "1h", "90m", "2h30m"
        #[arg(long = "in", value_name = "DURATION")]
        starts_in: Option<String>,
        /// Start at this RFC 3339 time, e.g. 2026-10-22T09:30:00Z
        #[arg(long, value_name = "TIME")]
        at: Option<String>,
        /// Meeting length, e.g. "30m", "1h" (default "30m")
        #[arg(long, value_name = "DURATION")]
        duration: Option<String>,
    },
    /// List your meetings
    List,
    /// Fetch one meeting by its code
    Get {
        /// The meeting code
        code: String,
    },
    /// Cancel a meeting by its code (destructive)
    Cancel {
        /// The meeting code
        code: String,
        /// Confirm the cancellation; there is no interactive prompt
        #[arg(long)]
        yes: bool,
    },
    /// Print the join link for a meeting code
    Link {
        /// The meeting code
        code: String,
    },
    /// Fetch the .ics calendar file for a meeting
    Ics {
        /// The meeting code
        code: String,
        /// Write the calendar file here instead of stdout
        #[arg(long, value_name = "PATH")]
        out: Option<std::path::PathBuf>,
    },
    /// Serve the Model Context Protocol (MCP) over stdin/stdout
    Mcp,
}

fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        // clap's own exit codes do not match our contract (it would
        // use 2 for usage), so we print and map to 1 ourselves.
        Err(err) => {
            let help = matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            err.print().unwrap_or(());
            std::process::exit(if help { EXIT_OK } else { EXIT_USAGE });
        }
    };
    match run(&cli) {
        Ok(()) => std::process::exit(EXIT_OK),
        Err(error) => {
            eprintln!("error: {}", client::describe(&error));
            std::process::exit(match error {
                CliError::Usage(_) => EXIT_USAGE,
                CliError::Auth(_) => EXIT_AUTH,
                CliError::Server(_) => EXIT_SERVER,
            });
        }
    }
}

fn run(cli: &Cli) -> Result<(), CliError> {
    let mut client = Client::new(cli.server.clone(), cli.token_file.as_deref());
    match &cli.command {
        Command::Login => login(&mut client, cli.json),
        Command::Logout => logout(&mut client, cli.json),
        Command::Whoami => whoami(&mut client, cli.json),
        Command::Create {
            title,
            starts_in,
            at,
            duration,
        } => create(
            &mut client,
            cli.json,
            title,
            starts_in.as_deref(),
            at.as_deref(),
            duration.as_deref(),
        ),
        Command::List => list(&mut client, cli.json),
        Command::Get { code } => get(&mut client, cli.json, code),
        Command::Cancel { code, yes } => cancel(&mut client, cli.json, code, *yes),
        Command::Link { code } => link(&mut client, cli.json, code),
        Command::Ics { code, out } => ics(&mut client, cli.json, code, out.as_deref()),
        Command::Mcp => {
            mcp::serve(cli.server.clone(), cli.token_file.clone());
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn login(client: &mut Client, json: bool) -> Result<(), CliError> {
    client.require_server()?;
    let view = device::start(client)?;
    // Print the code first and flush: the operator needs it before
    // this process starts its (blocking) poll loop, and e2e parses it
    // from stdout while the command is still running.
    if json {
        print_json(&json!({
            "user_code": view.user_code,
            "verification_uri": view.verification_uri,
            "verification_uri_complete": view.verification_uri_complete,
        }))?;
    } else {
        println!("Open {} and enter this code:", view.verification_uri);
        println!("  code: {}", view.user_code);
        println!("Waiting for approval…");
    }
    std::io::stdout()
        .flush()
        .map_err(|e| CliError::Server(format!("could not write output: {e}")))?;

    let token = device::wait_for_approval(client, view.device_code, view.interval)?;
    let Some(refresh_token) = token.refresh_token else {
        return Err(CliError::Server(
            "the server granted no refresh token".to_owned(),
        ));
    };
    client.set_login(StoredLogin {
        server: client.server().to_owned(),
        refresh_token,
        access_token: Some(token.access_token.clone()),
        access_expires_at: Some(client::now_unix() + token.expires_in as i64),
    });
    client.save_login()?;
    let me = client.whoami()?;
    let email = me
        .get("email")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?")
        .to_owned();
    if json {
        print_json(&json!({ "email": email }))?;
    } else {
        println!("Logged in as {email}.");
    }
    Ok(())
}

fn logout(client: &mut Client, json: bool) -> Result<(), CliError> {
    let Some(login) = client.login() else {
        if json {
            print_json(&json!({ "status": "logged_out" }))?;
        } else {
            println!("Not logged in.");
        }
        return Ok(());
    };
    // Best effort: tell the server to kill the refresh token. Even if
    // it is unreachable, wiping the local copy is the point.
    let _ = client.post_form(
        "/api/oauth/revoke",
        &[("token", login.refresh_token.clone())],
    );
    client.forget_login();
    if json {
        print_json(&json!({ "status": "logged_out" }))?;
    } else {
        println!("Logged out.");
    }
    Ok(())
}

fn whoami(client: &mut Client, json: bool) -> Result<(), CliError> {
    let me = client.whoami()?;
    if json {
        print_json(&me)?;
    } else {
        println!(
            "{}",
            me.get("email")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?")
        );
    }
    Ok(())
}

fn create(
    client: &mut Client,
    json: bool,
    title: &str,
    starts_in: Option<&str>,
    at: Option<&str>,
    duration: Option<&str>,
) -> Result<(), CliError> {
    let starts_at = resolve_start(starts_in, at)?;
    let duration_seconds = match duration {
        Some(raw) => parse_duration(raw)?,
        None => 30 * 60,
    };
    if duration_seconds <= 0 {
        return Err(CliError::Usage(
            "the duration must be at least one second".to_owned(),
        ));
    }
    let ends_at = starts_at + std::time::Duration::from_secs(duration_seconds as u64);
    let meeting = client.create_meeting(title, starts_at, ends_at)?;
    let code = meeting.meeting.code.clone();
    let join_url = client.join_url(&code)?;
    if json {
        print_json(&json!({
            "meeting": meeting,
            "join_url": join_url,
            "ics_url": format!("{}/api/meetings/{code}/ics", client.require_server()?),
        }))?;
    } else {
        println!("Created \"{}\"", meeting.meeting.title);
        println!("  code: {code}");
        println!("  join: {join_url}");
        println!(
            "  ics:  {}/api/meetings/{code}/ics",
            client.require_server()?
        );
    }
    Ok(())
}

fn list(client: &mut Client, json: bool) -> Result<(), CliError> {
    let meetings = client.list_meetings()?;
    if json {
        print_json(&meetings)?;
        return Ok(());
    }
    for meeting in &meetings {
        let starts = meeting
            .meeting
            .starts_at
            .format(&Rfc3339)
            .unwrap_or_default();
        let state = if meeting.meeting.cancelled {
            "  [cancelled]"
        } else {
            ""
        };
        println!(
            "{}  {:<16} {}{state}",
            meeting.meeting.code, starts, meeting.meeting.title
        );
    }
    Ok(())
}

fn get(client: &mut Client, json: bool, code: &str) -> Result<(), CliError> {
    let meeting = client.get_meeting(code)?;
    if json {
        print_json(&meeting)?;
        return Ok(());
    }
    let starts = meeting
        .meeting
        .starts_at
        .format(&Rfc3339)
        .unwrap_or_default();
    let ends = meeting.meeting.ends_at.format(&Rfc3339).unwrap_or_default();
    println!("{}", meeting.meeting.title);
    println!("  code: {}", meeting.meeting.code);
    println!("  starts: {starts}");
    println!("  ends: {ends}");
    if meeting.meeting.cancelled {
        println!("  cancelled");
    }
    println!("  join: {}", client.join_url(&meeting.meeting.code)?);
    Ok(())
}

fn cancel(client: &mut Client, json: bool, code: &str, yes: bool) -> Result<(), CliError> {
    if !yes {
        return Err(CliError::Usage(format!(
            "cancelling '{code}' is destructive; pass --yes to confirm"
        )));
    }
    client.cancel_meeting(code)?;
    if json {
        print_json(&json!({ "code": code, "cancelled": true }))?;
    } else {
        println!("Cancelled {code}.");
    }
    Ok(())
}

fn link(client: &mut Client, json: bool, code: &str) -> Result<(), CliError> {
    let join_url = client.join_url(code)?;
    if json {
        print_json(&json!({ "code": code, "join_url": join_url }))?;
    } else {
        println!("{join_url}");
    }
    Ok(())
}

fn ics(
    client: &mut Client,
    json: bool,
    code: &str,
    out: Option<&std::path::Path>,
) -> Result<(), CliError> {
    let body = client.fetch_ics(code)?;
    match out {
        Some(path) => {
            std::fs::write(path, &body).map_err(|e| {
                CliError::Server(format!("could not write {}: {e}", path.display()))
            })?;
            if json {
                print_json(&json!({ "code": code, "path": path.display().to_string() }))?;
            } else {
                println!("Wrote {}.", path.display());
            }
        }
        None => {
            if json {
                print_json(&json!({ "code": code, "ics": body }))?;
            } else {
                print!("{body}");
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

/// Resolve `--in`/`--at` into a concrete start time. Neither flag
/// means "start now".
fn resolve_start(starts_in: Option<&str>, at: Option<&str>) -> Result<OffsetDateTime, CliError> {
    match (starts_in, at) {
        (Some(_), Some(_)) => Err(CliError::Usage(
            "pass either --in or --at, not both".to_owned(),
        )),
        (Some(raw), None) => {
            let seconds = parse_duration(raw)?;
            Ok(OffsetDateTime::now_utc() + std::time::Duration::from_secs(seconds as u64))
        }
        (None, Some(raw)) => OffsetDateTime::parse(raw, &Rfc3339)
            .map_err(|e| CliError::Usage(format!("--at must be an RFC 3339 time: {e}"))),
        (None, None) => Ok(OffsetDateTime::now_utc()),
    }
}

/// Parse "2h30m"-style durations: a sequence of `<number><unit>` chunks
/// with units `d`/`h`/`m`/`s`. Returns seconds.
pub fn parse_duration(raw: &str) -> Result<i64, CliError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(CliError::Usage("the duration is empty".to_owned()));
    }
    let mut total: i64 = 0;
    let mut number = String::new();
    let mut parsed_any = false;
    for ch in raw.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        let unit = match ch.to_ascii_lowercase() {
            'd' => 86_400,
            'h' => 3_600,
            'm' => 60,
            's' => 1,
            _ => {
                return Err(CliError::Usage(format!(
                    "unknown duration unit '{ch}' in '{raw}' (use d, h, m, or s)"
                )));
            }
        };
        let value: i64 = number.parse().map_err(|_| {
            CliError::Usage(format!(
                "duration '{raw}' is not a number followed by a unit"
            ))
        })?;
        total = total
            .checked_add(value.checked_mul(unit).ok_or_else(|| too_big(raw))?)
            .ok_or_else(|| too_big(raw))?;
        number.clear();
        parsed_any = true;
    }
    if !number.is_empty() || !parsed_any {
        return Err(CliError::Usage(format!(
            "duration '{raw}' is missing a unit (use d, h, m, or s)"
        )));
    }
    Ok(total)
}

fn too_big(raw: &str) -> CliError {
    CliError::Usage(format!("duration '{raw}' is too large"))
}

/// Print one compact JSON value on stdout — the single emitter so
/// `--json` output stays uniform.
pub fn print_json(value: &impl serde::Serialize) -> Result<(), CliError> {
    let text = serde_json::to_string(value)
        .map_err(|e| CliError::Server(format!("could not encode JSON output: {e}")))?;
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_parse_chunk_by_chunk() {
        assert_eq!(parse_duration("1h"), Ok(3_600));
        assert_eq!(parse_duration("90m"), Ok(5_400));
        assert_eq!(parse_duration("2h30m"), Ok(9_000));
        assert_eq!(parse_duration("1d"), Ok(86_400));
        assert_eq!(parse_duration("45s"), Ok(45));
        assert_eq!(parse_duration(" 5m "), Ok(300));
    }

    #[test]
    fn garbage_durations_are_usage_errors() {
        for bad in ["", "30", "1x", "h", "10m5", "-1h"] {
            assert!(
                matches!(parse_duration(bad), Err(CliError::Usage(_))),
                "'{bad}' should be a usage error"
            );
        }
        // "10m5" — trailing digits with no unit, "-1h" is not a number.
    }

    #[test]
    fn flags_parse_into_their_commands() {
        let cli = Cli::try_parse_from([
            "huddle-cli",
            "--server",
            "http://127.0.0.1:3140",
            "--json",
            "create",
            "--title",
            "Sync",
            "--in",
            "1h",
        ])
        .expect("parse");
        assert!(cli.json);
        let Command::Create {
            title,
            starts_in,
            at,
            duration,
        } = &cli.command
        else {
            panic!("expected create");
        };
        assert_eq!(title, "Sync");
        assert_eq!(starts_in.as_deref(), Some("1h"));
        assert!(at.is_none());
        assert!(duration.is_none());

        let cli = Cli::try_parse_from(["huddle-cli", "cancel", "abcd-2345"]).expect("parse");
        let Command::Cancel { code, yes } = &cli.command else {
            panic!("expected cancel");
        };
        assert_eq!(code, "abcd-2345");
        assert!(!*yes); // enforcement happens in `run`, mapping to exit 1
    }
}
