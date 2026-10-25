//! `linkstudio mcp`: a Model Context Protocol server on stdin/stdout for
//! Claude Code and Claude Desktop. It only forwards requests to the running
//! Link Studio window over D-Bus; the window checks what the user allowed.

use std::io::{self, BufRead, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use gtk::glib::prelude::*;
use gtk::{gio, glib};
use serde_json::{json, Value};

use crate::ui::claude::{INTERFACE, OBJECT_PATH};
use crate::APP_ID;

const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const CALL_TIMEOUT_MS: i32 = 20_000;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

const INSTRUCTIONS: &str = "Link Studio controls an Insta360 Link webcam (a 4K camera on a motorized gimbal). \
Typical flow: get_status, then take_snapshot to see the picture, adjust with set_image_settings or move_gimbal, \
wait about two seconds for exposure and the gimbal to settle, and take_snapshot again to check. \
For a good video-call picture: face well lit and not backlit, natural skin tones, eyes about a third from the top, \
a little space above the head, and a calm background. Save a good result with save_profile and save_position. \
The user decides in Link Studio whether you may control the camera and, separately, whether you may see through it; \
if a tool says it is not allowed, ask the user to turn it on under Preferences → Claude instead of retrying. \
Privacy mode blocks seeing and moving the camera, and only the user can turn it off. \
Pictures are not mirrored: they show what other people see.";

fn tools() -> Value {
    let name_arg = json!({ "type": "object", "properties": { "name": { "type": "string" } }, "required": ["name"] });
    let none = json!({ "type": "object", "properties": {} });
    json!([
        { "name": "get_status", "inputSchema": none,
          "description": "Camera state: connection, permissions, privacy mode, AI tracking, gimbal position and ranges, every image setting with its current value, range and default, and the saved positions and profiles. Call this first." },
        { "name": "take_snapshot", "inputSchema": none,
          "description": "Take a picture through the camera (1280 px wide JPEG, not mirrored). Needs the user's permission to see through the camera. Wait about two seconds after changing settings or moving the gimbal before calling this." },
        { "name": "set_image_settings",
          "description": "Change one or more image settings by the names get_status reports, for example {\"exposure_compensation_ev\": 0.5, \"white_balance_auto\": false, \"white_balance_temperature\": 4800}. Values are clamped to the camera's range. Turn an automatic mode off before setting its manual value. anti_flicker takes an option label such as \"50 Hz\".",
          "inputSchema": { "type": "object", "additionalProperties": true, "properties": {
              "exposure_compensation_ev": { "type": "number", "description": "-2 (darker) to +2 (brighter)" },
              "brightness": { "type": "integer" }, "contrast": { "type": "integer" }, "saturation": { "type": "integer" },
              "hue": { "type": "integer" }, "sharpness": { "type": "integer" },
              "white_balance_auto": { "type": "boolean" }, "white_balance_temperature": { "type": "integer", "description": "Kelvin; lower is warmer" },
              "focus_auto": { "type": "boolean" }, "focus": { "type": "integer" },
              "anti_flicker": { "type": "string", "description": "Disabled, 50 Hz or 60 Hz" } } } },
        { "name": "reset_image_settings", "inputSchema": none, "description": "Put every image setting back to its default." },
        { "name": "move_gimbal",
          "description": "Point the camera. pan and tilt are in degrees (positive pan turns right, positive tilt looks up), zoom is a factor from 1.0 to 4.0. With relative=true the values are added to the current position. The gimbal physically stops at about -45° tilt (looking down), short of the range get_status reports. Turns AI tracking off.",
          "inputSchema": { "type": "object", "properties": {
              "pan": { "type": "number" }, "tilt": { "type": "number" }, "zoom": { "type": "number" },
              "relative": { "type": "boolean", "default": false } } } },
        { "name": "center_gimbal", "inputSchema": none, "description": "Move the gimbal back to its middle position." },
        { "name": "set_tracking",
          "description": "Turn AI tracking on or off and choose how much of the person stays in the picture. Tracking only works while the camera streams (Link Studio preview, virtual camera or a video call).",
          "inputSchema": { "type": "object", "properties": {
              "enabled": { "type": "boolean" },
              "framing": { "type": "string", "enum": ["head", "upper_body", "full_body"] } } } },
        { "name": "goto_position", "inputSchema": name_arg, "description": "Move the camera to a saved position by name." },
        { "name": "save_position", "inputSchema": name_arg, "description": "Save where the camera points now (pan, tilt, zoom) under a name; replaces a position with the same name." },
        { "name": "save_profile",
          "description": "Save the current image and tracking settings as a profile. apply_on_connect=true makes Link Studio apply it whenever the camera is connected.",
          "inputSchema": { "type": "object", "properties": { "name": { "type": "string" }, "apply_on_connect": { "type": "boolean" } }, "required": ["name"] } },
        { "name": "apply_profile", "inputSchema": name_arg, "description": "Apply a saved profile by name." },
        { "name": "set_virtual_camera",
          "description": "Turn the “Link Studio Camera” virtual camera on or off (for Teams, Zoom, browsers). It must have been set up by the user first.",
          "inputSchema": { "type": "object", "properties": { "enabled": { "type": "boolean" } }, "required": ["enabled"] } },
        { "name": "set_privacy",
          "description": "Turn privacy mode on: the camera points down and nothing can see through it. Only the user can turn it off again.",
          "inputSchema": { "type": "object", "properties": { "enabled": { "type": "boolean", "const": true } }, "required": ["enabled"] } }
    ])
}

fn has_owner(connection: &gio::DBusConnection) -> bool {
    connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "NameHasOwner",
            Some(&(APP_ID,).to_variant()),
            Some(glib::VariantTy::new("(b)").unwrap()),
            gio::DBusCallFlags::NONE,
            2000,
            None::<&gio::Cancellable>,
        )
        .ok()
        .and_then(|reply| reply.get::<(bool,)>())
        .is_some_and(|(owned,)| owned)
}

/// Starts Link Studio if it is not running, so the user sees what Claude does.
fn ensure_running(connection: &gio::DBusConnection) -> Result<(), String> {
    if has_owner(connection) {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    Command::new(exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Could not start Link Studio: {e}"))?;
    let start = Instant::now();
    while start.elapsed() < STARTUP_TIMEOUT {
        if has_owner(connection) {
            // Give the window a moment to connect to the camera.
            std::thread::sleep(Duration::from_secs(2));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err("Link Studio did not start. Ask the user to open it.".into())
}

fn call_app(method: &str, arguments: &Value) -> Result<Value, String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>).map_err(|e| e.to_string())?;
    ensure_running(&connection)?;
    let call = || {
        connection.call_sync(
            Some(APP_ID),
            OBJECT_PATH,
            INTERFACE,
            "Call",
            Some(&(method, arguments.to_string()).to_variant()),
            Some(glib::VariantTy::new("(s)").unwrap()),
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
    };
    // Right after start-up the object may not be registered yet.
    let mut reply = call();
    for _ in 0..10 {
        match &reply {
            Err(err) if err.message().contains("No such") || err.message().contains("UnknownObject") => {
                std::thread::sleep(Duration::from_millis(300));
                reply = call();
            }
            _ => break,
        }
    }
    let reply = reply.map_err(|e| format!("Could not reach Link Studio: {}", e.message()))?;
    let (json,) = reply.get::<(String,)>().ok_or("Unexpected reply from Link Studio")?;
    serde_json::from_str(&json).map_err(|e| e.to_string())
}

fn call_tool(params: &Value) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let known = tools().as_array().is_some_and(|t| t.iter().any(|tool| tool["name"] == name));
    if !known {
        return json!({ "isError": true, "content": [{ "type": "text", "text": format!("Unknown tool “{name}”.") }] });
    }
    match call_app(name, &arguments) {
        Ok(result) if result["ok"] == true => {
            let mut content = Vec::new();
            if let Some(image) = result.get("image").and_then(Value::as_str) {
                let mime = result.get("mime_type").and_then(Value::as_str).unwrap_or("image/jpeg");
                content.push(json!({ "type": "image", "data": image, "mimeType": mime }));
            }
            if let Some(text) = result.get("text").and_then(Value::as_str) {
                content.push(json!({ "type": "text", "text": text }));
            }
            json!({ "content": content })
        }
        Ok(result) => {
            let error = result.get("error").and_then(Value::as_str).unwrap_or("Link Studio refused the request.");
            json!({ "isError": true, "content": [{ "type": "text", "text": error }] })
        }
        Err(error) => json!({ "isError": true, "content": [{ "type": "text", "text": error }] }),
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSIONS[0]);
    let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "linkstudio", "title": "Link Studio", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

pub fn run() -> glib::ExitCode {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(message) => message,
            Err(_) => {
                let error = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } });
                let _ = writeln!(stdout, "{error}");
                let _ = stdout.flush();
                continue;
            }
        };
        // Notifications (no id) need no answer.
        let Some(id) = message.get("id").cloned() else { continue };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let response = match message.get("method").and_then(Value::as_str).unwrap_or_default() {
            "initialize" => json!({ "jsonrpc": "2.0", "id": id, "result": initialize(&params) }),
            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
            "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
            "tools/call" => json!({ "jsonrpc": "2.0", "id": id, "result": call_tool(&params) }),
            other => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": format!("Method not found: {other}") } }),
        };
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
    glib::ExitCode::SUCCESS
}
