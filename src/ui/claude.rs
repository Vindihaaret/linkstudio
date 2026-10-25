//! Claude integration, app side: a D-Bus object that `linkstudio mcp` calls.
//! Every request is checked here, in the app, against what the user allowed,
//! so the rules hold however Claude connects:
//!
//! - Nothing works until the user turns on "Allow control from Claude".
//! - Pictures need a second, separate permission.
//! - Privacy mode blocks seeing and moving the camera, and only the user can
//!   turn it off.
//! - Every picture shows a badge in the preview and a notice; changes show a notice.

use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use adw::prelude::*;
use base64::Engine;
use gtk::{gio, glib};
use serde_json::{json, Map, Value};

use super::{degrees, gimbal_position, reset_value, snap_degree, App, CURRENT, EXPOSURE_MAX_EV};
use crate::camera::{cid, CtrlKind, EXPOSURE_UNITS_PER_EV};
use crate::i18n::{tr, trf};

pub const OBJECT_PATH: &str = "/io/github/linkstudio/LinkStudio/Claude";
pub const INTERFACE: &str = "io.github.linkstudio.Claude";
const INTROSPECTION: &str = r#"<node>
  <interface name="io.github.linkstudio.Claude">
    <method name="Call">
      <arg type="s" name="method" direction="in"/>
      <arg type="s" name="arguments" direction="in"/>
      <arg type="s" name="result" direction="out"/>
    </method>
  </interface>
</node>"#;

/// How long to wait for a picture before giving up.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(8);
/// A camera that was asleep needs a moment for exposure and the gimbal to settle.
const WAKE_SETTLE: Duration = Duration::from_millis(2500);
const BADGE_DURATION: Duration = Duration::from_secs(4);
/// A stream started for Claude keeps the camera awake this long after
/// Claude's last request, so adjust–look–adjust rounds stay quick.
const CLAUDE_IDLE: Duration = Duration::from_secs(10);

/// Stable names for image controls, shared by get_status and set_image_settings.
const CONTROL_KEYS: &[(u32, &str)] = &[
    (cid::BRIGHTNESS, "brightness"),
    (cid::CONTRAST, "contrast"),
    (cid::SATURATION, "saturation"),
    (cid::HUE, "hue"),
    (cid::SHARPNESS, "sharpness"),
    (cid::GAMMA, "gamma"),
    (cid::GAIN, "gain"),
    (cid::BACKLIGHT_COMPENSATION, "backlight_compensation"),
    (cid::AUTO_WHITE_BALANCE, "white_balance_auto"),
    (cid::WHITE_BALANCE_TEMPERATURE, "white_balance_temperature"),
    (cid::FOCUS_AUTO, "focus_auto"),
    (cid::FOCUS_ABSOLUTE, "focus"),
    (cid::POWER_LINE_FREQUENCY, "anti_flicker"),
    (cid::EXPOSURE_AUTO, "exposure_mode"),
    (cid::EXPOSURE_ABSOLUTE, "exposure_time"),
    (cid::EXPOSURE_AUTO_PRIORITY, "exposure_allow_lower_frame_rate"),
];
const FRAMING_KEYS: [(u8, &str); 3] = [(1, "head"), (2, "upper_body"), (3, "full_body")];

fn control_key(id: u32, name: &str) -> String {
    CONTROL_KEYS
        .iter()
        .find(|(cid, _)| *cid == id)
        .map(|(_, key)| (*key).to_owned())
        .unwrap_or_else(|| name.to_lowercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_"))
}

/// Something Claude asked for that has to wait until the camera is awake.
pub type WakeAction = Box<dyn FnOnce(&Rc<App>)>;

/// One reply to a D-Bus `Call`, always a JSON object with `ok`.
pub struct Reply(gio::DBusMethodInvocation);

impl Reply {
    fn send(self, value: Value) {
        self.0.return_value(Some(&(value.to_string(),).to_variant()));
    }

    fn ok(self, text: impl Into<String>) {
        self.send(json!({ "ok": true, "text": text.into() }));
    }

    fn error(self, text: impl Into<String>) {
        self.send(json!({ "ok": false, "error": text.into() }));
    }
}

/// Registers the Claude D-Bus object on the application's own bus connection.
pub fn register(application: &adw::Application) {
    let Some(connection) = application.dbus_connection() else { return };
    let Some(info) = gio::DBusNodeInfo::for_xml(INTROSPECTION).ok().and_then(|n| n.lookup_interface(INTERFACE))
    else {
        return;
    };
    let registration = connection
        .register_object(OBJECT_PATH, &info)
        .method_call(|_, _, _, _, method, params, invocation| {
            if method != "Call" {
                invocation.return_dbus_error("org.freedesktop.DBus.Error.UnknownMethod", method);
                return;
            }
            let (name, arguments) = params.get::<(String, String)>().unwrap_or_default();
            let arguments: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
            let reply = Reply(invocation);
            match CURRENT.with(|current| current.borrow().upgrade()) {
                Some(app) => app.claude_request(&name, arguments, reply),
                None => reply.error("Link Studio has no window open yet. Try again in a moment."),
            }
        })
        .build();
    if let Err(err) = registration {
        eprintln!("Claude integration unavailable: {err}");
    }
}

/// The command that registers Link Studio with Claude Code.
fn claude_code_command() -> String {
    format!("claude mcp add --scope user linkstudio -- {} mcp", executable())
}

fn claude_desktop_config() -> String {
    format!(r#"{{ "mcpServers": {{ "linkstudio": {{ "command": "{}", "args": ["mcp"] }} }} }}"#, executable())
}

/// Absolute path, so Claude Desktop finds it without ~/.local/bin on PATH.
fn executable() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_owned))
        .unwrap_or_else(|| "linkstudio".into())
}

impl App {
    fn claude_check(&self, needs_view: bool, moves_camera: bool) -> Result<(), String> {
        let config = self.config.borrow();
        if !config.claude_control {
            return Err("Claude control is turned off. Ask the user to turn on “Allow control from Claude” in Link Studio → Preferences → Claude.".into());
        }
        if needs_view && !config.claude_camera_view {
            return Err("Seeing through the camera is turned off. Ask the user to turn on “Allow Claude to see through the camera” in Link Studio → Preferences → Claude.".into());
        }
        drop(config);
        if self.cam().is_none() {
            return Err("No Insta360 Link is connected.".into());
        }
        if (needs_view || moves_camera) && self.privacy.is_active() {
            return Err("Privacy mode is on, so the camera can neither be seen through nor moved. Only the user can turn privacy mode off in Link Studio.".into());
        }
        Ok(())
    }

    /// Shows what Claude did: a toast in the window, or a desktop
    /// notification while the window is hidden.
    fn claude_notice(&self, text: &str) {
        if self.window.is_visible() {
            self.toast(text);
        } else if let Some(application) = self.window.application() {
            let notification = gio::Notification::new("Link Studio");
            notification.set_body(Some(text));
            application.send_notification(Some("claude"), &notification);
        }
    }

    pub(super) fn claude_request(self: &Rc<Self>, method: &str, args: Value, reply: Reply) {
        let args = match args {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        let moves = matches!(method, "move_gimbal" | "center_gimbal" | "goto_position");
        let view = method == "take_snapshot";
        let enables_tracking = method == "set_tracking" && args.get("enabled").and_then(Value::as_bool) == Some(true);
        if method != "get_status" {
            if let Err(message) = self.claude_check(view, moves || enables_tracking) {
                return reply.error(message);
            }
        } else if !self.config.borrow().claude_control {
            return reply.error(self.claude_check(false, false).unwrap_err());
        }

        if method != "get_status" {
            self.claude_keep_awake();
        }
        if method == "take_snapshot" {
            return self.claude_snapshot(reply);
        }
        // A sleeping camera parks its gimbal and ignores moves, so wake it first.
        let needs_awake = moves || method == "set_tracking";
        if needs_awake && !self.stream_live.get() {
            let method = method.to_owned();
            self.claude_when_awake(Box::new(move |app| {
                if let Err(message) = app.claude_dispatch(&method, &args) {
                    app.claude_notice(&message);
                }
            }));
            return reply.ok("The camera was asleep and is waking up; this happens in about three seconds. Wait before calling take_snapshot.");
        }
        match self.claude_dispatch(method, &args) {
            Ok(text) => reply.ok(text),
            Err(text) => reply.error(text),
        }
    }

    fn claude_dispatch(self: &Rc<Self>, method: &str, args: &Map<String, Value>) -> Result<String, String> {
        let args = args.clone();
        match method {
            "get_status" => Ok(self.claude_status().to_string()),
            "set_image_settings" => self.claude_set_image(&args),
            "reset_image_settings" => {
                self.reset_image_settings();
                self.claude_notice(tr("Claude reset the image settings"));
                Ok("All image settings are back at their defaults.".into())
            }
            "move_gimbal" => self.claude_move(&args),
            "center_gimbal" => {
                self.center();
                self.claude_notice(tr("Claude centered the camera"));
                Ok("The gimbal is moving back to its middle position.".into())
            }
            "set_tracking" => self.claude_tracking(&args),
            "goto_position" => self.claude_goto(&args),
            "save_position" => self.claude_save_position(&args),
            "save_profile" => self.claude_save_profile(&args),
            "apply_profile" => self.claude_apply_profile(&args),
            "set_virtual_camera" => self.claude_virtual_camera(&args),
            "set_privacy" => self.claude_privacy(&args),
            other => Err(format!("Unknown request “{other}”.")),
        }
    }

    /// Runs `action` once the camera streams and has settled, starting a
    /// stream for Claude if nothing else is streaming.
    fn claude_when_awake(self: &Rc<Self>, action: WakeAction) {
        if self.stream_live.get() {
            return action(self);
        }
        self.claude_after_wake.borrow_mut().push(action);
        if self.preview.borrow().is_none() {
            self.claude_stream.set(true);
            self.start_stream(self.virtual_camera_device());
        }
        // Otherwise a stream is starting; claude_stream_started runs the action.
    }

    /// Restarts the countdown after which a stream started for Claude stops.
    fn claude_keep_awake(self: &Rc<Self>) {
        if let Some(source) = self.claude_idle.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(CLAUDE_IDLE, move || {
            let Some(app) = weak.upgrade() else { return };
            app.claude_idle.borrow_mut().take();
            if app.claude_pending.borrow().is_empty() && app.claude_stream.replace(false) {
                app.restart_stream();
            }
        });
        *self.claude_idle.borrow_mut() = Some(source);
    }

    fn claude_status(&self) -> Value {
        let Some(cam) = self.cam() else { return json!({ "connected": false }) };
        let config = self.config.borrow();
        let mut image = Map::new();
        for bound in self.bound.borrow().iter() {
            let Ok(info) = cam.query(bound.info().id) else { continue };
            let value = cam.get(info.id).unwrap_or(info.default);
            let mut entry = json!({ "value": value, "default": reset_value(&info), "adjustable": info.is_writable() && !info.is_inactive() });
            match &info.kind {
                CtrlKind::Integer => {
                    entry["min"] = json!(info.min);
                    entry["max"] = json!(info.max);
                    entry["step"] = json!(info.step.max(1));
                }
                CtrlKind::Boolean => {
                    entry["value"] = json!(value != 0);
                    entry["default"] = json!(reset_value(&info) == Some(1));
                }
                CtrlKind::Menu(items) => {
                    entry["options"] = json!(items.iter().map(|(v, l)| json!({ "value": v, "label": l })).collect::<Vec<_>>());
                }
            }
            image.insert(control_key(info.id, &info.name), entry);
        }
        if let (true, Ok(value)) = (self.exposure.borrow().is_some(), cam.exposure_compensation()) {
            image.insert(
                "exposure_compensation_ev".into(),
                json!({ "value": value as f64 / EXPOSURE_UNITS_PER_EV, "min": -EXPOSURE_MAX_EV, "max": EXPOSURE_MAX_EV, "default": 0.0 }),
            );
        }
        let (pan, tilt) = gimbal_position(&cam).unwrap_or((0, 0));
        let (pan_min, pan_max) = self.pan_range.get();
        let (tilt_min, tilt_max) = self.tilt_range.get();
        let zoom = cam.get(cid::ZOOM_ABSOLUTE).unwrap_or(100);
        let framing = cam.framing().ok().and_then(|f| FRAMING_KEYS.iter().find(|(v, _)| *v == f)).map(|(_, k)| *k);
        let identity = cam.identity();
        json!({
            "connected": true,
            "camera": { "model": cam.info.name, "firmware": identity.firmware },
            "permissions": { "control": config.claude_control, "camera_view": config.claude_camera_view },
            "privacy_mode": self.privacy.is_active(),
            "preview_on": self.preview_toggle.is_active(),
            "virtual_camera": { "on": self.stream_has_virtual.get(), "set_up": crate::vcam::find_device().is_some() },
            "tracking": { "enabled": self.tracking_on.get(), "framing": framing },
            "gimbal": {
                "pan_degrees": degrees(pan),
                "tilt_degrees": degrees(tilt),
                "zoom": zoom as f64 / 100.0,
                "pan_range_degrees": [degrees(pan_min), degrees(pan_max)],
                "tilt_range_degrees": [degrees(tilt_min), degrees(tilt_max)],
                "zoom_range": [self.zoom.adjustment().lower() / 100.0, self.zoom.adjustment().upper() / 100.0],
            },
            "image": image,
            "positions": config.positions.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
            "profiles": config.profiles.iter().map(|p| json!({
                "name": p.name,
                "applied_on_connect": config.startup_profile.as_deref() == Some(p.name.as_str()),
            })).collect::<Vec<_>>(),
        })
    }

    fn claude_set_image(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let Some(cam) = self.cam() else { return Err("No camera.".into()) };
        let mut changed = Vec::new();
        let mut problems = Vec::new();
        // Auto switches and menus first, so the manual values they unlock can be written.
        let bound = self.bound.borrow();
        let mut ordered: Vec<(&String, &Value)> = args.iter().collect();
        ordered.sort_by_key(|(key, _)| {
            bound
                .iter()
                .find(|b| control_key(b.info().id, &b.info().name) == **key)
                .map_or(1, |b| u8::from(b.info().kind == CtrlKind::Integer))
        });
        for (key, value) in ordered {
            if key == "exposure_compensation_ev" {
                match (value.as_f64(), self.exposure.borrow().is_some()) {
                    (Some(ev), true) => {
                        let ev = ev.clamp(-EXPOSURE_MAX_EV, EXPOSURE_MAX_EV);
                        let units = (ev * EXPOSURE_UNITS_PER_EV).round() as i16;
                        match cam.set_exposure_compensation(units) {
                            Ok(()) => changed.push(format!("exposure_compensation_ev = {ev:+.1}")),
                            Err(err) => problems.push(format!("{key}: {err}")),
                        }
                    }
                    (None, _) => problems.push(format!("{key}: expected a number")),
                    (_, false) => problems.push(format!("{key}: not supported by this camera")),
                }
                continue;
            }
            let Some(b) = bound.iter().find(|b| control_key(b.info().id, &b.info().name) == *key) else {
                problems.push(format!("{key}: unknown setting (see get_status for the names)"));
                continue;
            };
            let info = b.info();
            let target = match (&info.kind, value) {
                (CtrlKind::Boolean, Value::Bool(on)) => Some(*on as i32),
                (CtrlKind::Boolean, Value::Number(n)) => n.as_i64().map(|n| (n != 0) as i32),
                (CtrlKind::Integer, Value::Number(n)) => n.as_f64().map(|v| {
                    let step = info.step.max(1);
                    let snapped = info.min + ((v.round() as i32 - info.min) as f64 / step as f64).round() as i32 * step;
                    snapped.clamp(info.min, info.max)
                }),
                (CtrlKind::Menu(items), Value::Number(n)) => {
                    n.as_i64().and_then(|n| items.iter().find(|(v, _)| *v as i64 == n)).map(|(v, _)| *v)
                }
                (CtrlKind::Menu(items), Value::String(label)) => items
                    .iter()
                    .find(|(_, l)| l.eq_ignore_ascii_case(label) || l.replace(' ', "").eq_ignore_ascii_case(&label.replace(' ', "")))
                    .map(|(v, _)| *v),
                _ => None,
            };
            let Some(target) = target else {
                problems.push(format!("{key}: value {value} does not fit this setting"));
                continue;
            };
            match cam.set(info.id, target) {
                Ok(()) => changed.push(format!("{key} = {target}")),
                Err(err) => problems.push(format!("{key}: {err} (an automatic mode may be on)")),
            }
        }
        drop(bound);
        if let Some(cam) = self.cam() {
            self.refresh(&cam);
        }
        if !changed.is_empty() {
            let names: Vec<&str> = changed.iter().filter_map(|c| c.split(" = ").next()).collect();
            self.claude_notice(&trf("Claude changed: {}", &[&names.join(", ")]));
        }
        let mut text = if changed.is_empty() { "Nothing was changed.".to_owned() } else { format!("Changed: {}.", changed.join(", ")) };
        if !problems.is_empty() {
            text.push_str(&format!(" Not changed: {}.", problems.join("; ")));
        }
        if changed.is_empty() && !problems.is_empty() { Err(text) } else { Ok(text) }
    }

    fn claude_move(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let Some(cam) = self.cam() else { return Err("No camera.".into()) };
        let relative = args.get("relative").and_then(Value::as_bool).unwrap_or(false);
        let mut note = String::new();
        if self.tracking_on.get() {
            self.set_tracking(false);
            note.push_str(" AI tracking was turned off so it does not move the camera back.");
        }
        let (pan, tilt) = gimbal_position(&cam).unwrap_or((0, 0));
        let to_arcsec = |deg: f64| (deg * 3600.0).round() as i32;
        if let Some(p) = args.get("pan").and_then(Value::as_f64) {
            let (min, max) = self.pan_range.get();
            let target = if relative { pan + to_arcsec(p) } else { to_arcsec(p) };
            let _ = cam.set(cid::PAN_ABSOLUTE, snap_degree(target).clamp(min, max));
        }
        if let Some(t) = args.get("tilt").and_then(Value::as_f64) {
            let (min, max) = self.tilt_range.get();
            let target = if relative { tilt + to_arcsec(t) } else { to_arcsec(t) };
            let _ = cam.set(cid::TILT_ABSOLUTE, snap_degree(target).clamp(min, max));
        }
        if let Some(z) = args.get("zoom").and_then(Value::as_f64) {
            let current = cam.get(cid::ZOOM_ABSOLUTE).unwrap_or(100) as f64 / 100.0;
            let factor = if relative { current + z } else { z };
            let adjustment = self.zoom.adjustment();
            let value = (factor * 100.0).round().clamp(adjustment.lower(), adjustment.upper());
            let _ = cam.set(cid::ZOOM_ABSOLUTE, value as i32);
        }
        self.claude_notice(tr("Claude moved the camera"));
        Ok(format!("The gimbal is moving; wait about a second before taking a snapshot.{note}"))
    }

    fn claude_tracking(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let Some(cam) = self.cam() else { return Err("No camera.".into()) };
        let mut done = Vec::new();
        if let Some(framing) = args.get("framing").and_then(Value::as_str) {
            let Some((value, _)) = FRAMING_KEYS.iter().find(|(_, k)| *k == framing) else {
                return Err(format!("Unknown framing “{framing}”; use head, upper_body or full_body."));
            };
            cam.set_framing(*value).map_err(|e| e.to_string())?;
            done.push(format!("framing = {framing}"));
        }
        if let Some(on) = args.get("enabled").and_then(Value::as_bool) {
            self.set_tracking(on);
            done.push(format!("tracking = {}", if on { "on" } else { "off" }));
        }
        if let Some(cam) = self.cam() {
            self.refresh(&cam);
        }
        self.claude_notice(tr("Claude changed AI tracking"));
        Ok(format!("{}. The camera only tracks while it is streaming (preview, virtual camera or a video call).", done.join(", ")))
    }

    fn named(args: &Map<String, Value>) -> Result<&str, String> {
        args.get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| "A name is needed.".to_owned())
    }

    fn claude_goto(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let name = Self::named(args)?;
        let index = self.config.borrow().positions.iter().position(|p| p.name.eq_ignore_ascii_case(name));
        let Some(index) = index else { return Err(format!("No saved position called “{name}”.")) };
        self.goto_position(index);
        self.claude_notice(&trf("Claude moved the camera to “{}”", &[&name]));
        Ok(format!("Moving to “{name}”."))
    }

    fn claude_save_position(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let name = Self::named(args)?.to_owned();
        let Some(position) = self.current_position(name.clone()) else { return Err("No camera.".into()) };
        {
            let mut config = self.config.borrow_mut();
            match config.positions.iter_mut().find(|p| p.name.eq_ignore_ascii_case(&name)) {
                Some(existing) => *existing = position,
                None => config.positions.push(position),
            }
            config.save();
        }
        self.rebuild_positions();
        self.claude_notice(&trf("Claude saved the position “{}”", &[&name]));
        Ok(format!("Saved the current camera position as “{name}”."))
    }

    fn claude_save_profile(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let name = Self::named(args)?.to_owned();
        self.save_profile(&name);
        if let Some(on) = args.get("apply_on_connect").and_then(Value::as_bool) {
            let mut config = self.config.borrow_mut();
            config.startup_profile = if on { Some(name.clone()) } else { config.startup_profile.take().filter(|p| *p != name) };
            config.save();
            drop(config);
            self.rebuild_profiles();
        }
        self.claude_notice(&trf("Claude saved the profile “{}”", &[&name]));
        Ok(format!("Saved the current image and tracking settings as the profile “{name}”."))
    }

    fn claude_apply_profile(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let name = Self::named(args)?;
        let profile = self.config.borrow().profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name)).cloned();
        let Some(profile) = profile else { return Err(format!("No profile called “{name}”.")) };
        self.apply_profile(&profile);
        self.claude_notice(&trf("Claude applied the profile “{}”", &[&profile.name]));
        Ok(format!("Applied “{}”.", profile.name))
    }

    fn claude_virtual_camera(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        let on = args.get("enabled").and_then(Value::as_bool).ok_or("“enabled” must be true or false.")?;
        if on && crate::vcam::find_device().is_none() {
            return Err("The virtual camera is not set up. The user can set it up in Link Studio → Effects (needs the administrator password).".into());
        }
        self.set_virtual_camera(on);
        self.claude_notice(if on { tr("Claude turned the virtual camera on") } else { tr("Claude turned the virtual camera off") });
        Ok(format!("The virtual camera is {}.", if on { "on" } else { "off" }))
    }

    fn claude_privacy(self: &Rc<Self>, args: &Map<String, Value>) -> Result<String, String> {
        if args.get("enabled").and_then(Value::as_bool) != Some(true) {
            return Err("Only the user can turn privacy mode off, in Link Studio.".into());
        }
        if !self.privacy.is_active() {
            self.privacy.set_active(true);
        }
        self.claude_notice(tr("Claude turned privacy mode on"));
        Ok("Privacy mode is on: the camera points down. Only the user can turn it off.".into())
    }

    /// Asks the stream for one picture. If the camera is asleep, the stream is
    /// started first and the picture taken once exposure has settled.
    fn claude_snapshot(self: &Rc<Self>, reply: Reply) {
        let id = self.claude_next_id.get();
        self.claude_next_id.set(id + 1);
        self.claude_pending.borrow_mut().push((id, reply));
        // Each request has its own deadline, so an old one cannot time out a newer one.
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(SNAPSHOT_TIMEOUT, move || {
            let Some(app) = weak.upgrade() else { return };
            let expired = {
                let mut pending = app.claude_pending.borrow_mut();
                pending.iter().position(|(i, _)| *i == id).map(|index| pending.remove(index).1)
            };
            if let Some(reply) = expired {
                reply.error("The camera did not deliver a picture in time. Try again.");
                if app.claude_pending.borrow().is_empty() {
                    app.finish_claude_snapshot();
                }
            }
        });
        self.claude_when_awake(Box::new(|app| app.snapshot_request.store(true, Ordering::Relaxed)));
    }

    /// Called when a stream goes live: a waiting snapshot is taken after the
    /// camera has had time to wake up.
    pub(super) fn claude_stream_started(self: &Rc<Self>) {
        if self.claude_after_wake.borrow().is_empty() {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(WAKE_SETTLE, move || {
            let Some(app) = weak.upgrade() else { return };
            let actions: Vec<_> = app.claude_after_wake.borrow_mut().drain(..).collect();
            for action in actions {
                action(&app);
            }
        });
    }

    pub(super) fn claude_snapshot_ready(self: &Rc<Self>, jpeg: Vec<u8>, width: usize, height: usize) {
        let pending: Vec<Reply> = self.claude_pending.borrow_mut().drain(..).map(|(_, reply)| reply).collect();
        if pending.is_empty() {
            return;
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&jpeg);
        let text = format!(
            "Picture {width}×{height}, as other people see it (not mirrored). Call get_status for the current settings."
        );
        for reply in pending {
            reply.send(json!({ "ok": true, "text": text, "image": data, "mime_type": "image/jpeg" }));
        }
        self.show_claude_badge();
        self.claude_notice(tr("Claude took a picture through the camera"));
        self.finish_claude_snapshot();
    }

    /// Stops a stream that was only started for a snapshot.
    fn finish_claude_snapshot(self: &Rc<Self>) {
        self.snapshot_request.store(false, Ordering::Relaxed);
        self.claude_keep_awake();
    }

    fn show_claude_badge(self: &Rc<Self>) {
        self.claude_badge.set_visible(true);
        if let Some(source) = self.claude_badge_timeout.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(BADGE_DURATION, move || {
            if let Some(app) = weak.upgrade() {
                app.claude_badge_timeout.borrow_mut().take();
                app.claude_badge.set_visible(false);
            }
        });
        *self.claude_badge_timeout.borrow_mut() = Some(source);
    }

    /// The Claude page in Preferences.
    pub(super) fn claude_preferences_page(self: &Rc<Self>, dialog: &adw::PreferencesDialog) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::builder()
            .name("claude")
            .title(tr("Claude"))
            .icon_name("user-available-symbolic")
            .build();

        let access = adw::PreferencesGroup::builder()
            .title(tr("Claude Integration (MCP)"))
            .description(tr("Lets Claude, in Claude Code or Claude Desktop, read and change the camera settings, move the gimbal and manage positions and profiles for you. Everything is off until you turn it on here, and you can turn it off at any time."))
            .build();
        let control = adw::SwitchRow::builder()
            .title(tr("Allow control from Claude"))
            .subtitle(tr("Settings, gimbal, AI tracking, positions, profiles and the virtual camera"))
            .active(self.config.borrow().claude_control)
            .build();
        let view = adw::SwitchRow::builder()
            .title(tr("Allow Claude to see through the camera"))
            .subtitle(tr("Claude can take pictures to judge lighting and framing. The pictures are sent to Anthropic to be analyzed, like an image you paste into a chat. A badge in the preview shows each time."))
            .active(self.config.borrow().claude_camera_view)
            .sensitive(self.config.borrow().claude_control)
            .build();
        let privacy = adw::ActionRow::builder()
            .title(tr("Privacy mode always wins"))
            .subtitle(tr("While privacy mode is on, Claude can neither see through nor move the camera. Claude can turn privacy mode on, but only you can turn it off."))
            .build();
        privacy.add_prefix(&gtk::Image::from_icon_name("view-conceal-symbolic"));
        access.add(&control);
        access.add(&view);
        access.add(&privacy);
        page.add(&access);

        let connect = adw::PreferencesGroup::builder()
            .title(tr("Connect Claude"))
            .description(tr("Start a new Claude session after connecting, then ask for example “Make me look good for my meeting”."))
            .build();
        let code = adw::ActionRow::builder()
            .title(tr("Claude Code"))
            .subtitle(claude_code_command())
            .subtitle_selectable(true)
            .build();
        let has_claude = glib::find_program_in_path("claude").is_some();
        let add = gtk::Button::builder().label(tr("Add")).valign(gtk::Align::Center).visible(has_claude).build();
        let copy_code = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(tr("Copy"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        code.add_suffix(&add);
        code.add_suffix(&copy_code);
        let desktop = adw::ActionRow::builder()
            .title(tr("Claude Desktop"))
            .subtitle(tr("Add this to claude_desktop_config.json"))
            .build();
        let copy_desktop = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text(tr("Copy"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        desktop.add_suffix(&copy_desktop);
        connect.add(&code);
        connect.add(&desktop);
        page.add(&connect);

        let weak = Rc::downgrade(self);
        let view_row = view.clone();
        control.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            app.config.borrow_mut().claude_control = row.is_active();
            app.config.borrow().save();
            view_row.set_sensitive(row.is_active());
        });
        let weak = Rc::downgrade(self);
        view.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            app.config.borrow_mut().claude_camera_view = row.is_active();
            app.config.borrow().save();
        });
        let copy_to = |dialog: &adw::PreferencesDialog, text: String| {
            dialog.clipboard().set_text(&text);
            dialog.add_toast(adw::Toast::new(tr("Copied")));
        };
        let prefs = dialog.clone();
        copy_code.connect_clicked(move |_| copy_to(&prefs, claude_code_command()));
        let prefs = dialog.clone();
        copy_desktop.connect_clicked(move |_| copy_to(&prefs, claude_desktop_config()));
        let prefs = dialog.clone();
        add.connect_clicked(move |button| {
            button.set_sensitive(false);
            let argv = ["claude", "mcp", "add", "--scope", "user", "linkstudio", "--", &executable(), "mcp"];
            let argv: Vec<&std::ffi::OsStr> = argv.iter().map(|a| a.as_ref()).collect();
            let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE;
            let Ok(process) = gio::Subprocess::newv(&argv, flags) else {
                prefs.add_toast(adw::Toast::new(tr("Could not run the claude command")));
                return;
            };
            let prefs = prefs.clone();
            let button = button.clone();
            let child = process.clone();
            process.communicate_utf8_async(None, None::<&gio::Cancellable>, move |result| {
                button.set_sensitive(true);
                let message = match result {
                    Ok(_) if child.is_successful() => tr("Link Studio was added to Claude Code").to_owned(),
                    Ok((output, _)) => {
                        let output = output.map(|o| o.trim().to_owned()).unwrap_or_default();
                        if output.contains("already exists") {
                            tr("Link Studio is already added to Claude Code").to_owned()
                        } else {
                            trf("Could not add it: {}", &[&output])
                        }
                    }
                    Err(err) => trf("Could not add it: {}", &[&err]),
                };
                prefs.add_toast(adw::Toast::new(&message));
            });
        });
        page
    }
}

/// Removes the Claude Code registration when uninstalling, if `claude` exists.
pub fn unregister_from_claude_code() {
    if glib::find_program_in_path("claude").is_some() {
        let _ = std::process::Command::new("claude")
            .args(["mcp", "remove", "--scope", "user", "linkstudio"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}
