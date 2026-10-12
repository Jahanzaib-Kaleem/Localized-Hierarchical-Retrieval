//! Admin-only shell running in a separate network-enabled sandbox container.
//! This MCP process only queues requests on a shared control volume.
//! The LHR database, host root filesystem, and Docker socket are NOT mounted in the sandbox.

use super::McpState;
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_PENDING_REQUESTS: usize = 256;
const MAX_TIMEOUT_SECONDS: u64 = 3600;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxArguments {
    action: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    request_id: Option<String>,
}

pub(super) fn tool_schema() -> Value {
    json!({
        "type":"object",
        "required":["action"],
        "properties":{
            "action":{"type":"string","enum":["info","run","status"]},
            "command":{"type":"string","description":"Bash script to run inside the isolated sandbox container for action=run. Bash, Python, curl, git and coreutils are available."},
            "cwd":{"type":"string","description":"Absolute path INSIDE the sandbox, default /workspace. Optional read-only host directory appears at /imports if separately mounted by the VPS administrator."},
            "timeout_seconds":{"type":"integer","minimum":1,"maximum":3600,"default":60},
            "request_id":{"type":"string","description":"32-character request ID for action=status"}
        },
        "additionalProperties":false
    })
}

fn control_dir(root: &Path) -> PathBuf {
    root.join("control").join("sandbox")
}

fn enabled(root: &Path) -> bool {
    control_dir(root).join("enabled").is_file()
}

fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .as_bytes()
            .iter()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

fn new_id() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn validate_run(args: &SandboxArguments) -> Result<(&str, u64), String> {
    let command = args.command.as_deref().ok_or("command is required for action=run")?;
    if command.trim().is_empty() || command.len() > MAX_COMMAND_BYTES || command.contains('\0') {
        return Err(format!("command must be 1..={MAX_COMMAND_BYTES} bytes without NUL"));
    }
    let timeout = args.timeout_seconds.unwrap_or(60);
    if timeout == 0 || timeout > MAX_TIMEOUT_SECONDS {
        return Err(format!("timeout_seconds must be 1..={MAX_TIMEOUT_SECONDS}"));
    }
    if let Some(cwd) = args.cwd.as_deref() {
        if cwd.is_empty() || !cwd.starts_with('/') || cwd.contains('\0') || cwd.len() > 4096 {
            return Err("cwd must be an absolute path inside the sandbox container".into());
        }
    }
    Ok((command, timeout))
}

fn write_request(dir: &Path, request: &Value) -> Result<String, String> {
    let requests = dir.join("requests");
    if !requests.is_dir() {
        return Err("sandbox sidecar not installed; run the LHR setup installer on the VPS".into());
    }
    let outstanding = fs::read_dir(&requests)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .count();
    if outstanding >= MAX_PENDING_REQUESTS {
        return Err("sandbox request queue is full; existing requests must complete".into());
    }

    for _ in 0..4 {
        let id = new_id();
        let mut payload = request.clone();
        payload["request_id"] = json!(id);
        let bytes = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
        let tmp = requests.join(format!(".{id}.tmp"));
        let dest = requests.join(format!("{id}.json"));
        if dest.exists() {
            continue;
        }
        let file = OpenOptions::new().write(true).create_new(true).open(&tmp);
        let mut file = match file {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.to_string()),
        };
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        file.sync_all().map_err(|e| e.to_string())?;
        drop(file);
        fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
        return Ok(id);
    }
    Err("could not allocate a unique sandbox request ID".into())
}

pub(super) fn call(state: &McpState, arguments: Value) -> Result<Value, String> {
    let args: SandboxArguments = serde_json::from_value(arguments)
        .map_err(|e| format!("invalid sandbox arguments: {e}"))?;
    let dir = control_dir(&state.root);

    if args.action == "info" {
        return Ok(json!({
            "enabled":enabled(&state.root),
            "tool":"sandbox",
            "environment":"separate persistent Docker container, outbound network enabled",
            "workspace":"/workspace",
            "imports":"/imports (optional explicit read-only host mount)",
            "host_root_access":false,
            "host_filesystem_mounted":false,
            "actions":["info","run","status"],
            "max_timeout_seconds":MAX_TIMEOUT_SECONDS
        }));
    }
    if !enabled(&state.root) {
        return Err("sandbox sidecar is not enabled; run the latest LHR setup installer on the VPS".into());
    }

    match args.action.as_str() {
        "run" => {
            let (command, timeout) = validate_run(&args)?;
            let request = json!({
                "command":command,
                "cwd":args.cwd,
                "timeout_seconds":timeout
            });
            let id = write_request(&dir, &request)?;
            Ok(json!({
                "request_id":id,
                "status":"queued",
                "note":"Retrieve output with sandbox(action=status,request_id=<id>). Commands run in the dedicated sandbox container, not on the VPS host."
            }))
        }
        "status" => {
            let id = args.request_id.as_deref().ok_or("request_id required for action=status")?;
            if !valid_id(id) {
                return Err("invalid request_id".into());
            }
            let response_path = dir.join("responses").join(format!("{id}.json"));
            let request_path = dir.join("requests").join(format!("{id}.json"));
            if response_path.is_file() {
                let bytes = fs::read(&response_path).map_err(|e| e.to_string())?;
                if bytes.len() > 256 * 1024 {
                    return Err("sandbox response exceeds safety limit".into());
                }
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())
            } else if request_path.is_file() {
                Ok(json!({"request_id":id,"status":"queued"}))
            } else {
                Err("unknown or expired sandbox request ID".into())
            }
        }
        _ => Err("action must be info, run or status".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_exposes_one_shell_tool() {
        let schema = tool_schema();
        assert_eq!(schema["properties"]["timeout_seconds"]["maximum"], 3600);
        assert_eq!(schema["properties"]["action"]["enum"][1], "run");
    }

    #[test]
    fn rejects_invalid_command_or_working_dir() {
        let mut args = SandboxArguments {
            action:"run".into(), command:Some("pwd".into()), cwd:Some("../../etc".into()),
            timeout_seconds:Some(60), request_id:None
        };
        assert!(validate_run(&args).is_err());
        args.cwd = Some("/workspace".into());
        assert!(validate_run(&args).is_ok());
        args.timeout_seconds = Some(3601);
        assert!(validate_run(&args).is_err());
    }

    #[test]
    fn rejects_path_traversal_as_request_id() {
        assert!(valid_id("1234567890abcdef1234567890abcdef"));
        assert!(!valid_id("../../etc/passwd"));
        assert!(!valid_id("ABCDEF1234567890abcdef1234567890"));
    }
}
