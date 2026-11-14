//! Main window: live preview with gimbal controls and saved positions, plus a
//! settings sidebar with pages for image, tracking, profiles and camera info.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::camera::{self, cid, Camera, CtrlInfo, CtrlKind, Mode, EXPOSURE_UNITS_PER_EV};
use crate::config::{Appearance, Config, Position, Profile};
use crate::i18n::{self, n_, tr, trf};
use crate::preview::{self, Event, Frame, Preview};
use crate::{tray, vcam};
use crate::APP_ID;

pub mod claude;
mod dialogs;
mod gimbal;
mod positions;
mod sidebar;
mod stream;
mod update;

pub use claude::{register as register_claude, unregister_from_claude_code};
pub use update::start_auto_checks as start_update_checks;

const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);
/// How often a stream that another program blocks is tried again.
const STREAM_RETRY_INTERVAL: Duration = Duration::from_secs(3);
const NUDGE_INTERVAL: Duration = Duration::from_millis(90);
const ARCSEC_PER_DEGREE: i32 = 3600;
const ZOOM_MIN: f64 = 100.0;
const ZOOM_MAX: f64 = 400.0;
const ZOOM_STEP: i32 = 10;
/// Exposure compensation range offered in the UI; ±4 EV is possible but
/// beyond ±2 the image is close to black or white.
const EXPOSURE_MAX_EV: f64 = 2.0;
/// The gimbal ignores a tracking command while it is still waking up from its
/// parked position, so after turning tracking on the app checks every
/// TRACKING_CHECK whether the camera took it, resending up to TRACKING_ATTEMPTS times.
const TRACKING_CHECK: Duration = Duration::from_millis(1000);
const TRACKING_ATTEMPTS: u32 = 6;
const APPEARANCES: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

const FRAMINGS: [(u8, &str); 3] = [(1, n_("Head")), (2, n_("Upper body")), (3, n_("Full body"))];
/// Preview heights offered in the camera page, in combo row order.
const PREVIEW_HEIGHTS: [(u32, &str); 3] = [(720, "720p"), (1080, "1080p"), (2160, n_("4K (2160p)"))];

const CSS: &str = r#"
.preview-video { background-color: black; border-radius: 16px; }
.preview-status { background-color: alpha(currentColor, 0.05); border-radius: 16px; }
.preview-stack { border-radius: 16px; box-shadow: 0 8px 24px alpha(black, 0.22); }
.chip {
  border-radius: 999px;
  padding: 3px 10px;
  font-weight: 700;
  font-size: 12px;
  color: white;
  background-color: alpha(black, 0.6);
}
.chip-dot { min-width: 7px; min-height: 7px; border-radius: 999px; background-color: #8ff0a4; }
.chip-dot.accent { background-color: #62a0ea; }
.chip-dot.warning { background-color: #f8e45c; }
.chip-dot.claude { background-color: #d97757; }
.stream-info { font-weight: 400; font-feature-settings: "tnum"; }
.preview-tools { padding: 4px; border-radius: 999px; background-color: alpha(black, 0.66); }
.preview-tools button { color: white; min-width: 36px; min-height: 36px; padding: 0; }
.preview-tools separator { margin: 8px 4px; background-color: alpha(white, 0.22); }
.ptz-pad {
  padding: 2px;
  border-radius: 999px;
  background-color: alpha(currentColor, 0.05);
  box-shadow: inset 0 0 0 1px alpha(currentColor, 0.1);
}
.ptz-pad button { min-width: 46px; min-height: 46px; padding: 0; }
.ptz-pad .ptz-home { background-color: alpha(currentColor, 0.1); }
.big-number { font-size: 30px; font-weight: 700; letter-spacing: -0.5px; font-feature-settings: "tnum"; }
.control-card { padding: 14px 18px; }
.position-tile { padding: 6px; border-radius: 12px; font-weight: 400; background-color: alpha(currentColor, 0.05); }
.position-tile:hover { background-color: alpha(currentColor, 0.1); }
.position-map {
  border-radius: 9px;
  background-color: alpha(currentColor, 0.05);
  box-shadow: inset 0 0 0 1px alpha(currentColor, 0.1);
}
.position-marker { border-radius: 3px; background-color: @accent_bg_color; }
.status-hero {
  padding: 13px 16px;
  border-radius: 14px;
  box-shadow: inset 0 0 0 1px alpha(currentColor, 0.08);
  background-image: linear-gradient(110deg, alpha(@warning_color, 0.17), alpha(currentColor, 0.04));
}
.status-hero.control { background-image: linear-gradient(110deg, alpha(@success_color, 0.17), alpha(currentColor, 0.04)); }
.status-hero.view { background-image: linear-gradient(110deg, alpha(#d97757, 0.24), alpha(currentColor, 0.04)); }
.status-hero-icon { min-width: 36px; min-height: 36px; border-radius: 999px; background-color: alpha(currentColor, 0.1); }

/* The main window: layered cards. Dialogs stay outside and keep the plain look. */
.link-main headerbar { background: none; box-shadow: none; }
.link-main .control-card, .link-main list.boxed-list {
  background-color: alpha(white, 0.065);
  border-radius: 16px;
  box-shadow: inset 0 1px alpha(white, 0.09), inset 0 0 0 1px alpha(white, 0.07), 0 6px 20px alpha(black, 0.16);
}
.link-main.link-light .control-card, .link-main.link-light list.boxed-list {
  background-color: alpha(white, 0.8);
  box-shadow: inset 0 0 0 1px alpha(black, 0.06), 0 6px 18px alpha(black, 0.07);
}
.link-main list.boxed-list > row:first-child { border-top-left-radius: 16px; border-top-right-radius: 16px; }
.link-main list.boxed-list > row:last-child { border-bottom-left-radius: 16px; border-bottom-right-radius: 16px; }
"#;

pub fn load_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

thread_local! {
    /// The status icon belongs to the application, not to a window, so it
    /// survives the window being rebuilt for a language change.
    static TRAY: RefCell<Option<ksni::blocking::Handle<tray::LinkTray>>> = const { RefCell::new(None) };
    /// The window that status icon commands act on.
    static CURRENT: RefCell<std::rc::Weak<App>> = const { RefCell::new(std::rc::Weak::new()) };
}

fn has_tray() -> bool {
    TRAY.with(|tray| tray.borrow().is_some())
}

/// Opens the window, or brings back the hidden one (from the app menu or a
/// second launch). With `minimized` (at login) it stays in the status icon.
pub fn activate(application: &adw::Application, minimized: bool) {
    if let Some(app) = CURRENT.with(|current| current.borrow().upgrade()) {
        app.present();
        return;
    }
    let app = App::new(application, None);
    if !(minimized && has_tray()) {
        app.present();
    }
}

/// Starts the status icon and routes its menu commands to the current window.
pub fn start_tray(application: &adw::Application) {
    let (tx, rx) = async_channel::unbounded();
    let Some(handle) = tray::spawn(tx) else { return };
    TRAY.with(|tray| *tray.borrow_mut() = Some(handle));
    let application = application.downgrade();
    glib::spawn_future_local(async move {
        while let Ok(command) = rx.recv().await {
            let Some(application) = application.upgrade() else { break };
            let app = CURRENT.with(|current| current.borrow().upgrade());
            match (command, app) {
                (tray::Command::Quit, _) => application.quit(),
                (tray::Command::Show, Some(app)) => app.present(),
                (tray::Command::Show, None) => application.activate(),
                (tray::Command::ToggleVirtualCamera, Some(app)) => {
                    let on = !app.config.borrow().virtual_camera;
                    app.set_virtual_camera(on);
                }
                (tray::Command::ToggleTracking, Some(app)) => {
                    let on = !app.tracking_on.get();
                    app.set_tracking(on);
                }
                (tray::Command::TogglePrivacy, Some(app)) => app.privacy.set_active(!app.privacy.is_active()),
                (tray::Command::GotoPosition(index), Some(app)) => {
                    // With the window hidden the camera sleeps and ignores moves:
                    // wake it the way Claude's requests do, and let it sleep again.
                    app.claude_keep_awake();
                    app.claude_when_awake(Box::new(move |app| app.goto_position(index)));
                }
                (tray::Command::ApplyProfile(index), Some(app)) => {
                    let profile = app.config.borrow().profiles.get(index).cloned();
                    if let Some(profile) = profile {
                        app.apply_profile(&profile);
                    }
                }
                (_, None) => {}
            }
        }
    });
}

/// Follows the desktop's screen lock, for "privacy mode when the screen is
/// locked". GNOME and KDE both announce it with an `ActiveChanged` signal.
pub fn watch_screen_lock(application: &adw::Application) {
    let Some(connection) = application.dbus_connection() else { return };
    for interface in ["org.gnome.ScreenSaver", "org.freedesktop.ScreenSaver"] {
        // The subscription lasts as long as the application.
        let _ = connection.signal_subscribe(
            None,
            Some(interface),
            Some("ActiveChanged"),
            None,
            None,
            gio::DBusSignalFlags::NONE,
            |_, _, _, _, _, parameters| {
                let Some((locked,)) = parameters.get::<(bool,)>() else { return };
                if let Some(app) = CURRENT.with(|current| current.borrow().upgrade()) {
                    app.screen_locked(locked);
                }
            },
        );
    }
}

pub fn apply_appearance(appearance: Appearance) {
    let scheme = match appearance {
        Appearance::System => adw::ColorScheme::Default,
        Appearance::Light => adw::ColorScheme::ForceLight,
        Appearance::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

/// Window state handed over when the window is rebuilt for a new language.
pub struct Carry {
    size: (i32, i32),
    maximized: bool,
    tracking: bool,
    page: Option<glib::GString>,
}

struct Section {
    title: &'static str,
    icon: &'static str,
    description: Option<&'static str>,
    ids: &'static [u32],
}

const IMAGE_SECTIONS: &[Section] = &[
    Section {
        title: n_("Image"),
        icon: "display-brightness-symbolic",
        description: None,
        ids: &[cid::BRIGHTNESS, cid::CONTRAST, cid::SATURATION, cid::HUE, cid::SHARPNESS, cid::GAMMA],
    },
    Section {
        title: n_("Exposure"),
        icon: "weather-clear-symbolic",
        description: None,
        ids: &[
            cid::EXPOSURE_AUTO,
            cid::EXPOSURE_ABSOLUTE,
            cid::EXPOSURE_AUTO_PRIORITY,
            cid::GAIN,
            cid::BACKLIGHT_COMPENSATION,
        ],
    },
    Section {
        title: n_("White balance"),
        icon: "color-select-symbolic",
        description: None,
        ids: &[cid::AUTO_WHITE_BALANCE, cid::WHITE_BALANCE_TEMPERATURE],
    },
    Section { title: n_("Focus"), icon: "find-location-symbolic", description: None, ids: &[cid::FOCUS_AUTO, cid::FOCUS_ABSOLUTE] },
    Section {
        title: n_("Flicker"),
        icon: "emblem-system-symbolic",
        description: Some(n_("Use 50 Hz in Europe and most of the world, 60 Hz in North America.")),
        ids: &[cid::POWER_LINE_FREQUENCY],
    },
];

/// Gimbal controls live under the preview, not in the settings pages or profiles.
const PTZ_IDS: &[u32] = &[
    cid::PAN_ABSOLUTE,
    cid::TILT_ABSOLUTE,
    cid::ZOOM_ABSOLUTE,
    cid::PAN_SPEED,
    cid::TILT_SPEED,
];

fn control_title(info: &CtrlInfo) -> (String, Option<&'static str>) {
    let (title, subtitle) = match info.id {
        cid::BRIGHTNESS => (n_("Brightness"), None),
        cid::CONTRAST => (n_("Contrast"), None),
        cid::SATURATION => (n_("Saturation"), None),
        cid::HUE => (n_("Hue"), None),
        cid::SHARPNESS => (n_("Sharpness"), None),
        cid::GAMMA => (n_("Gamma"), None),
        cid::EXPOSURE_AUTO => (n_("Exposure mode"), None),
        cid::EXPOSURE_ABSOLUTE => (n_("Exposure time"), None),
        cid::EXPOSURE_AUTO_PRIORITY => (n_("Allow lower frame rate"), Some(n_("Gives a brighter image in dark rooms"))),
        cid::GAIN => (n_("Gain"), None),
        cid::BACKLIGHT_COMPENSATION => (n_("Backlight compensation"), None),
        cid::AUTO_WHITE_BALANCE => (n_("Automatic white balance"), None),
        cid::WHITE_BALANCE_TEMPERATURE => (n_("Color temperature"), Some(n_("Low is warmer, high is cooler"))),
        cid::FOCUS_AUTO => (n_("Autofocus"), None),
        cid::FOCUS_ABSOLUTE => (n_("Focus"), Some(n_("Low is near, high is far away"))),
        cid::POWER_LINE_FREQUENCY => (n_("Anti-flicker"), None),
        _ => return (info.name.clone(), None),
    };
    (tr(title).to_owned(), subtitle.map(tr))
}

/// Menu entries come from the camera driver in English; translate the common ones.
fn menu_label(name: &str) -> String {
    let known = match name {
        "Disabled" => n_("Off"),
        "Manual Mode" => n_("Manual"),
        "Aperture Priority Mode" | "Auto Mode" => n_("Automatic"),
        "Shutter Priority Mode" => n_("Shutter priority"),
        other => return other.to_owned(),
    };
    tr(known).to_owned()
}

fn degrees(arcsec: i32) -> i32 {
    (arcsec as f64 / ARCSEC_PER_DEGREE as f64).round() as i32
}

/// Pan and tilt only accept whole degrees (3600 arcsec steps).
fn snap_degree(arcsec: i32) -> i32 {
    (arcsec as f64 / ARCSEC_PER_DEGREE as f64).round() as i32 * ARCSEC_PER_DEGREE
}

/// Where the gimbal really points, falling back to the last value set over
/// V4L2 on cameras without the live-position control.
fn gimbal_position(cam: &Camera) -> Option<(i32, i32)> {
    cam.live_position()
        .ok()
        .or_else(|| cam.get(cid::PAN_ABSOLUTE).ok().zip(cam.get(cid::TILT_ABSOLUTE).ok()))
}

fn zoom_factor(zoom: i32) -> String {
    format!("{:.1}", zoom as f64 / 100.0)
}

fn pan_tilt_label(pan: i32, tilt: i32) -> String {
    trf("Pan {}° · Tilt {}°", &[&degrees(pan), &degrees(tilt)])
}

fn position_summary(p: &Position) -> String {
    format!("{}° · {}° · {}×", degrees(p.pan), degrees(p.tilt), zoom_factor(p.zoom))
}

/// The value "reset" puts a control back to. Automatic modes are turned on
/// even where the driver's default is off (the Link reports autofocus off),
/// and a menu default that is not one of its options is skipped.
fn reset_value(info: &CtrlInfo) -> Option<i32> {
    match info.id {
        cid::FOCUS_AUTO | cid::AUTO_WHITE_BALANCE => return Some(1),
        // Aperture priority is the automatic exposure mode on UVC cameras.
        cid::EXPOSURE_AUTO => return Some(3),
        _ => {}
    }
    match &info.kind {
        CtrlKind::Menu(items) => items.iter().any(|(v, _)| *v == info.default).then_some(info.default),
        _ => Some(info.default),
    }
}

fn parse_id(key: &str) -> Option<u32> {
    u32::from_str_radix(key.trim_start_matches("0x"), 16).ok()
}

/// A settings widget bound to one V4L2 control.
enum Bound {
    Scale { info: CtrlInfo, row: adw::ActionRow, scale: gtk::Scale },
    Switch { info: CtrlInfo, row: adw::SwitchRow },
    Combo { info: CtrlInfo, row: adw::ComboRow, values: Vec<i32> },
}

impl Bound {
    fn info(&self) -> &CtrlInfo {
        match self {
            Bound::Scale { info, .. } | Bound::Switch { info, .. } | Bound::Combo { info, .. } => info,
        }
    }

    fn row(&self) -> gtk::Widget {
        match self {
            Bound::Scale { row, .. } => row.clone().upcast(),
            Bound::Switch { row, .. } => row.clone().upcast(),
            Bound::Combo { row, .. } => row.clone().upcast(),
        }
    }

    fn show_value(&self, value: i32) {
        match self {
            Bound::Scale { scale, .. } => {
                if (scale.value() - value as f64).abs() > 0.5 {
                    scale.set_value(value as f64);
                }
            }
            Bound::Switch { row, .. } => row.set_active(value != 0),
            Bound::Combo { row, values, .. } => {
                let pos = values.iter().position(|&v| v == value);
                row.set_selected(pos.map_or(gtk::INVALID_LIST_POSITION, |p| p as u32));
            }
        }
    }
}

struct VirtualCameraRows {
    setup: adw::ActionRow,
    setup_button: gtk::Button,
    enabled: adw::SwitchRow,
    resolution: adw::ComboRow,
    remove: adw::ActionRow,
}

/// Virtual camera sizes offered, in combo row order.
const VIRTUAL_HEIGHTS: [(u32, &str); 2] = [(1080, "1080p"), (720, "720p")];

struct TrackingRows {
    follow: adw::SwitchRow,
    framing: adw::ComboRow,
    group: adw::ComboRow,
    other_mode: adw::ActionRow,
}

pub struct App {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    title: adw::WindowTitle,
    split: adw::OverlaySplitView,
    sidebar_toggle: gtk::ToggleButton,
    sidebar: adw::ToolbarView,
    switcher: adw::ViewSwitcher,
    main_stack: gtk::Stack,
    preview_stack: gtk::Stack,
    preview_status: adw::StatusPage,
    preview_status_button: gtk::Button,
    preview_toggle: gtk::ToggleButton,
    picture: gtk::Picture,
    mirror_button: gtk::ToggleButton,
    snapshot_button: gtk::Button,
    tracking_badge: gtk::Box,
    tracking_badge_label: gtk::Label,
    privacy_badge: gtk::Box,
    stream_info: gtk::Label,
    grid_button: gtk::ToggleButton,
    thirds: gtk::DrawingArea,
    fullscreen_button: gtk::Button,
    bottom: gtk::Box,
    content: adw::ToolbarView,
    camera_page: gtk::Box,
    pad: gtk::Grid,
    pad_buttons: [gtk::Button; 5],
    zoom: gtk::Scale,
    zoom_out: gtk::Button,
    zoom_in: gtk::Button,
    position_label: gtk::Label,
    privacy: gtk::ToggleButton,
    positions_list: gtk::FlowBox,
    positions_empty: gtk::Label,
    save_position: gtk::Button,
    mirror: Arc<AtomicBool>,
    show_preview: Arc<AtomicBool>,
    virtual_badge: gtk::Box,

    camera: RefCell<Option<Rc<Camera>>>,
    /// Whether the running stream also feeds the virtual camera.
    stream_has_virtual: Cell<bool>,
    /// Whether the running stream has delivered frames (the camera is awake).
    stream_live: Cell<bool>,
    /// "Still running" was already shown once for closing to the status icon.
    hidden_notice_shown: Cell<bool>,
    virtual_rows: RefCell<Option<VirtualCameraRows>>,
    quality_row: RefCell<Option<adw::ComboRow>>,
    /// Snapshot requests from Claude waiting for a frame.
    claude_pending: RefCell<Vec<(u64, claude::Reply)>>,
    claude_next_id: Cell<u64>,
    snapshot_request: Arc<AtomicBool>,
    /// The running stream was started only to take a picture for Claude.
    claude_stream: Cell<bool>,
    claude_badge: gtk::Box,
    claude_badge_timeout: RefCell<Option<glib::SourceId>>,
    /// Claude actions waiting for the camera to wake up.
    claude_after_wake: RefCell<Vec<claude::WakeAction>>,
    claude_idle: RefCell<Option<glib::SourceId>>,
    /// Last state pushed to the status icon, to avoid needless D-Bus updates.
    tray_state: RefCell<Option<tray::State>>,
    bound: RefCell<Vec<Bound>>,
    exposure: RefCell<Option<gtk::Scale>>,
    tracking: RefCell<Option<TrackingRows>>,
    profiles_group: RefCell<Option<adw::PreferencesGroup>>,
    profile_rows: RefCell<Vec<gtk::Widget>>,
    config: RefCell<Config>,
    preview: RefCell<Option<Preview>>,
    last_jpeg: RefCell<Option<Vec<u8>>>,
    updating: Cell<bool>,
    tracking_on: Cell<bool>,
    /// The camera drops AI tracking whenever its video stream restarts; this
    /// is set while a restarted preview waits for its first frame.
    resume_tracking: Cell<bool>,
    privacy_restore: Cell<Option<(i32, i32, i32)>>,
    pan_range: Cell<(i32, i32)>,
    tilt_range: Cell<(i32, i32)>,
    nudge_source: RefCell<Option<glib::SourceId>>,
    refresh_source: RefCell<Option<glib::SourceId>>,
    tracking_retry: RefCell<Option<glib::SourceId>>,
    /// Pending restart of a stream that stopped because another program had
    /// the camera, or because it got stuck.
    stream_retry: RefCell<Option<glib::SourceId>>,
    /// While a retried stream has not delivered a frame yet, the "waiting"
    /// page stays up (title and description) instead of a blank preview.
    stream_waiting: RefCell<Option<(String, String)>>,
    /// Tracking was on when the camera was disconnected; turn it back on when
    /// it comes back.
    reconnect_tracking: Cell<bool>,
    /// Height of the running stream and frames shown since the last tick, for
    /// the label on the preview.
    stream_height: Cell<u32>,
    frames_shown: Cell<u32>,
    /// Whether the settings sidebar was open before going fullscreen.
    sidebar_before_fullscreen: Cell<bool>,
    /// Privacy mode was turned on by the screen lock, so unlocking ends it.
    lock_privacy: Cell<bool>,
}

/// A small status label for the preview, with a colored dot.
fn status_chip(text: &str, dot: &str) -> (gtk::Box, gtk::Label) {
    let chip = gtk::Box::builder().spacing(6).visible(false).css_classes(["chip"]).build();
    chip.append(&gtk::Box::builder().valign(gtk::Align::Center).css_classes(["chip-dot", dot]).build());
    let label = gtk::Label::new(Some(text));
    chip.append(&label);
    (chip, label)
}

/// A settings group whose title has an icon. Libadwaita has no such title, so
/// the header suffix takes its place.
fn icon_group(title: &str, icon: &str, description: Option<&str>) -> adw::PreferencesGroup {
    let header = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).build();
    let line = gtk::Box::builder().spacing(8).build();
    line.append(&gtk::Image::builder().icon_name(icon).css_classes(["dim-label"]).build());
    line.append(&gtk::Label::builder().label(title).xalign(0.0).css_classes(["heading"]).build());
    header.append(&line);
    if let Some(description) = description {
        // A wrapping label asks for the width of its whole text otherwise.
        header.append(&gtk::Label::builder().label(description).wrap(true).max_width_chars(1).hexpand(true).xalign(0.0).css_classes(["dim-label"]).build());
    }
    let group = adw::PreferencesGroup::new();
    group.set_header_suffix(Some(&header));
    // The empty title beside it would otherwise keep half the width.
    if let Some(title) = header.prev_sibling() {
        title.set_visible(false);
    }
    group
}

/// The reset button only shows when the value differs from the default.
fn show_reset_when_changed(scale: &gtk::Scale, reset: &gtk::Button, default: f64) {
    let sync = {
        let reset = reset.clone();
        move |scale: &gtk::Scale| {
            let changed = (scale.value() - default).abs() > 0.05;
            reset.set_opacity(if changed { 1.0 } else { 0.0 });
            reset.set_sensitive(changed);
        }
    };
    sync(scale);
    scale.connect_value_changed(sync);
}

/// Rule-of-thirds lines over the 16:9 picture, inside any black bars.
fn draw_thirds(_: &gtk::DrawingArea, cr: &gtk::cairo::Context, width: i32, height: i32) {
    let (area_w, area_h) = (width as f64, height as f64);
    let (w, h) = if area_w / area_h > 16.0 / 9.0 { (area_h * 16.0 / 9.0, area_h) } else { (area_w, area_w * 9.0 / 16.0) };
    let (x, y) = ((area_w - w) / 2.0, (area_h - h) / 2.0);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.45);
    cr.set_line_width(1.0);
    for part in [1.0, 2.0] {
        cr.move_to(x + w * part / 3.0, y);
        cr.line_to(x + w * part / 3.0, y + h);
        cr.move_to(x, y + h * part / 3.0);
        cr.line_to(x + w, y + h * part / 3.0);
    }
    let _ = cr.stroke();
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).valign(gtk::Align::Center).build()
}

impl App {
    pub fn new(application: &adw::Application, carry: Option<Carry>) -> Rc<Self> {
        let config = Config::load();

        let (width, height) = carry.as_ref().map_or((1280, 820), |c| c.size);
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Link Studio")
            .default_width(width)
            .default_height(height)
            .maximized(carry.as_ref().is_some_and(|c| c.maximized))
            .width_request(360)
            .height_request(520)
            .build();

        // Content pane header.
        let title = adw::WindowTitle::new("Link Studio", tr("Looking for camera…"));
        let content_header = adw::HeaderBar::new();
        content_header.set_title_widget(Some(&title));
        let preview_toggle = gtk::ToggleButton::builder()
            .icon_name("camera-video-symbolic")
            .tooltip_text(tr("Preview on/off (Ctrl+P)"))
            .build();
        content_header.pack_start(&preview_toggle);
        let menu = gio::Menu::new();
        menu.append(Some(tr("Preferences")), Some("win.preferences"));
        menu.append(Some(tr("Keyboard Shortcuts")), Some("win.shortcuts"));
        menu.append(Some(tr("Check for Updates…")), Some("win.check-update"));
        menu.append(Some(tr("About Link Studio")), Some("win.about"));
        let quit = gio::Menu::new();
        quit.append(Some(tr("Quit")), Some("app.quit"));
        menu.append_section(None, &quit);
        let menu_button = gtk::MenuButton::builder()
            .icon_name("open-menu-symbolic")
            .menu_model(&menu)
            .primary(true)
            .tooltip_text(tr("Main Menu"))
            .build();
        let sidebar_toggle = gtk::ToggleButton::builder()
            .icon_name("sidebar-show-right-symbolic")
            .tooltip_text(tr("Show settings (F9)"))
            .build();
        content_header.pack_end(&menu_button);
        content_header.pack_end(&sidebar_toggle);

        // Preview.
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&picture));
        overlay.add_css_class("preview-video");
        overlay.set_overflow(gtk::Overflow::Hidden);

        let badges = gtk::Box::builder()
            .spacing(6)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_start(12)
            .build();
        let (tracking_badge, tracking_badge_label) = status_chip(tr("AI tracking"), "accent");
        let (privacy_badge, _) = status_chip(tr("Privacy"), "warning");
        let (virtual_badge, _) = status_chip(tr("Virtual camera"), "live");
        let (claude_badge, _) = status_chip(tr("Claude is looking"), "claude");
        badges.append(&claude_badge);
        badges.append(&virtual_badge);
        badges.append(&tracking_badge);
        badges.append(&privacy_badge);
        overlay.add_overlay(&badges);

        let stream_info = gtk::Label::builder()
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_end(12)
            .visible(false)
            .css_classes(["chip", "stream-info"])
            .build();
        overlay.add_overlay(&stream_info);

        let thirds = gtk::DrawingArea::builder().can_target(false).visible(config.preview_grid).build();
        thirds.set_draw_func(draw_thirds);
        overlay.add_overlay(&thirds);

        let tools = gtk::Box::builder()
            .spacing(2)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(12)
            .css_classes(["preview-tools"])
            .build();
        let grid_button = gtk::ToggleButton::builder()
            .icon_name("view-grid-symbolic")
            .tooltip_text(tr("Rule-of-thirds lines (only in the preview)"))
            .css_classes(["flat", "circular"])
            .active(config.preview_grid)
            .build();
        let mirror_button = gtk::ToggleButton::builder()
            .icon_name("object-flip-horizontal-symbolic")
            .tooltip_text(tr("Mirror the preview (does not affect other programs)"))
            .css_classes(["flat", "circular"])
            .active(config.preview_mirror)
            .build();
        let snapshot_button = gtk::Button::builder()
            .icon_name("camera-photo-symbolic")
            .tooltip_text(tr("Take photo (Ctrl+S)"))
            .css_classes(["flat", "circular"])
            .build();
        let fullscreen_button = gtk::Button::builder()
            .icon_name("view-fullscreen-symbolic")
            .tooltip_text(tr("Fullscreen (F11)"))
            .css_classes(["flat", "circular"])
            .build();
        tools.append(&grid_button);
        tools.append(&mirror_button);
        tools.append(&gtk::Separator::new(gtk::Orientation::Vertical));
        tools.append(&snapshot_button);
        tools.append(&fullscreen_button);
        overlay.add_overlay(&tools);

        let preview_status_button = gtk::Button::builder()
            .label(tr("Start Preview"))
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let preview_status = adw::StatusPage::builder()
            .icon_name("camera-web-symbolic")
            .title(tr("Preview is off"))
            .description(tr(
                "Other programs, such as Teams or Zoom, can use the camera while the preview is off.",
            ))
            .child(&preview_status_button)
            .build();
        preview_status.add_css_class("preview-status");
        let preview_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .height_request(220)
            .css_classes(["preview-stack"])
            .build();
        preview_stack.add_named(&overlay, Some("video"));
        preview_stack.add_named(&preview_status, Some("status"));
        preview_stack.set_visible_child_name("status");

        // Gimbal controls.
        let up = icon_button("go-up-symbolic", tr("Tilt up (Ctrl+↑)"));
        let down = icon_button("go-down-symbolic", tr("Tilt down (Ctrl+↓)"));
        let left = icon_button("go-previous-symbolic", tr("Pan left (Ctrl+←)"));
        let right = icon_button("go-next-symbolic", tr("Pan right (Ctrl+→)"));
        let home = icon_button("go-home-symbolic", tr("Center the camera (Ctrl+Home)"));
        for b in [&up, &down, &left, &right] {
            b.add_css_class("flat");
        }
        for b in [&up, &down, &left, &right, &home] {
            b.add_css_class("circular");
        }
        home.add_css_class("ptz-home");
        let pad = gtk::Grid::builder()
            .row_spacing(4)
            .column_spacing(4)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["ptz-pad"])
            .build();
        pad.attach(&up, 1, 0, 1, 1);
        pad.attach(&left, 0, 1, 1, 1);
        pad.attach(&home, 1, 1, 1, 1);
        pad.attach(&right, 2, 1, 1, 1);
        pad.attach(&down, 1, 2, 1, 1);

        let zoom = gtk::Scale::with_range(gtk::Orientation::Horizontal, ZOOM_MIN, ZOOM_MAX, 1.0);
        zoom.set_hexpand(true);
        zoom.set_width_request(120);
        zoom.set_draw_value(false);
        for z in [100.0, 200.0, 300.0, 400.0] {
            zoom.add_mark(z, gtk::PositionType::Bottom, Some(&format!("{}×", z / 100.0)));
        }
        let zoom_out = icon_button("zoom-out-symbolic", tr("Zoom out (Ctrl+−)"));
        let zoom_in = icon_button("zoom-in-symbolic", tr("Zoom in (Ctrl++)"));
        zoom_out.add_css_class("flat");
        zoom_in.add_css_class("flat");
        let zoom_row = gtk::Box::builder().spacing(6).build();
        zoom_row.append(&zoom_out);
        zoom_row.append(&zoom);
        zoom_row.append(&zoom_in);

        let zoom_value = gtk::Label::builder()
            .label("1.0×")
            .halign(gtk::Align::Start)
            .css_classes(["big-number"])
            .build();
        let shown = zoom_value.clone();
        zoom.connect_value_changed(move |scale| shown.set_label(&format!("{:.1}×", scale.value() / 100.0)));

        let position_label = gtk::Label::builder()
            .label(pan_tilt_label(0, 0))
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption", "numeric"])
            .build();
        let privacy = gtk::ToggleButton::builder()
            .child(&adw::ButtonContent::builder().icon_name("view-conceal-symbolic").label(tr("Privacy")).build())
            .tooltip_text(tr("Point the camera down. Turn off to return to the previous position."))
            .valign(gtk::Align::Center)
            .build();

        let gimbal_side = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        let zoom_heading = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        zoom_heading.append(&gtk::Label::builder().label(tr("Zoom")).halign(gtk::Align::Start).css_classes(["caption-heading", "dim-label"]).build());
        zoom_heading.append(&zoom_value);
        let gimbal_header = gtk::Box::builder().spacing(12).build();
        gimbal_header.append(&zoom_heading);
        gimbal_header.append(&privacy);
        gimbal_side.append(&gimbal_header);
        gimbal_side.append(&zoom_row);
        gimbal_side.append(&position_label);

        let gimbal_card = gtk::Box::builder().spacing(16).css_classes(["card", "control-card"]).build();
        gimbal_card.append(&pad);
        gimbal_card.append(&gimbal_side);

        // Saved positions.
        let positions_list = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .min_children_per_line(2)
            .max_children_per_line(3)
            .column_spacing(8)
            .row_spacing(8)
            .valign(gtk::Align::Start)
            .build();
        let positions_empty = gtk::Label::builder()
            .label(tr("No saved positions yet"))
            .css_classes(["dim-label"])
            .margin_top(18)
            .margin_bottom(18)
            .build();
        let save_position = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label(tr("Save Position")).build())
            .build();
        let positions_header = gtk::Box::builder().spacing(12).build();
        positions_header.append(
            &gtk::Label::builder()
                .label(tr("Saved Positions"))
                .hexpand(true)
                .xalign(0.0)
                .css_classes(["caption-heading", "dim-label"])
                .build(),
        );
        positions_header.append(&save_position);
        let positions_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(170)
            .child(&positions_list)
            .build();
        let positions_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .hexpand(true)
            .css_classes(["card", "control-card"])
            .build();
        positions_card.append(&positions_header);
        positions_card.append(&positions_empty);
        positions_card.append(&positions_scroll);

        let bottom = gtk::Box::builder().spacing(18).build();
        bottom.append(&gimbal_card);
        bottom.append(&positions_card);

        let camera_page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(18)
            .margin_top(6)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        camera_page.append(&preview_stack);
        camera_page.append(&bottom);

        let missing_page = adw::StatusPage::builder()
            .icon_name("camera-web-symbolic")
            .title(tr("No Insta360 Link Found"))
            .description(tr("Connect the camera with USB. Link Studio connects automatically."))
            .build();

        let main_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();
        main_stack.add_named(&camera_page, Some("camera"));
        main_stack.add_named(&missing_page, Some("missing"));
        main_stack.set_visible_child_name("missing");

        let content = adw::ToolbarView::new();
        content.add_top_bar(&content_header);
        content.set_content(Some(&main_stack));

        // Settings sidebar.
        let switcher = adw::ViewSwitcher::builder().policy(adw::ViewSwitcherPolicy::Narrow).build();
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.set_title_widget(Some(&switcher));
        let sidebar = adw::ToolbarView::new();
        sidebar.add_top_bar(&sidebar_header);

        let split = adw::OverlaySplitView::builder()
            .css_classes(["link-main"])
            .sidebar_position(gtk::PackType::End)
            .min_sidebar_width(450.0)
            .max_sidebar_width(500.0)
            .sidebar_width_fraction(0.36)
            .content(&content)
            .sidebar(&sidebar)
            .show_sidebar(false)
            .build();
        split
            .bind_property("show-sidebar", &sidebar_toggle, "active")
            .bidirectional()
            .sync_create()
            .build();

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split));
        window.set_content(Some(&toasts));

        let medium = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 1000sp").unwrap());
        medium.add_setter(&split, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(medium);
        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 720sp").unwrap());
        narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
        narrow.add_setter(&bottom, "orientation", Some(&gtk::Orientation::Vertical.to_value()));
        narrow.add_setter(&gimbal_card, "width-request", Some(&(-1).to_value()));
        window.add_breakpoint(narrow);

        let app = Rc::new(Self {
            window,
            toasts,
            title,
            split,
            sidebar_toggle,
            sidebar,
            switcher,
            main_stack,
            preview_stack,
            preview_status,
            preview_status_button,
            preview_toggle,
            picture,
            mirror_button,
            snapshot_button,
            tracking_badge,
            tracking_badge_label,
            privacy_badge,
            stream_info,
            grid_button,
            thirds,
            fullscreen_button,
            bottom,
            content,
            camera_page,
            pad,
            pad_buttons: [up, down, left, right, home],
            zoom,
            zoom_out,
            zoom_in,
            position_label,
            privacy,
            positions_list,
            positions_empty,
            save_position,
            mirror: Arc::new(AtomicBool::new(config.preview_mirror)),
            show_preview: Arc::new(AtomicBool::new(false)),
            virtual_badge,
            camera: RefCell::new(None),
            stream_has_virtual: Cell::new(false),
            stream_live: Cell::new(false),
            hidden_notice_shown: Cell::new(false),
            virtual_rows: RefCell::new(None),
            quality_row: RefCell::new(None),
            claude_pending: RefCell::new(Vec::new()),
            claude_next_id: Cell::new(0),
            snapshot_request: Arc::new(AtomicBool::new(false)),
            claude_stream: Cell::new(false),
            claude_badge,
            claude_badge_timeout: RefCell::new(None),
            claude_after_wake: RefCell::new(Vec::new()),
            claude_idle: RefCell::new(None),
            tray_state: RefCell::new(None),
            bound: RefCell::new(Vec::new()),
            exposure: RefCell::new(None),
            tracking: RefCell::new(None),
            profiles_group: RefCell::new(None),
            profile_rows: RefCell::new(Vec::new()),
            config: RefCell::new(config),
            preview: RefCell::new(None),
            last_jpeg: RefCell::new(None),
            updating: Cell::new(false),
            tracking_on: Cell::new(false),
            resume_tracking: Cell::new(false),
            privacy_restore: Cell::new(None),
            pan_range: Cell::new((-522_000, 522_000)),
            tilt_range: Cell::new((-324_000, 360_000)),
            nudge_source: RefCell::new(None),
            refresh_source: RefCell::new(None),
            tracking_retry: RefCell::new(None),
            stream_retry: RefCell::new(None),
            stream_waiting: RefCell::new(None),
            reconnect_tracking: Cell::new(false),
            stream_height: Cell::new(0),
            frames_shown: Cell::new(0),
            sidebar_before_fullscreen: Cell::new(false),
            lock_privacy: Cell::new(false),
        });
        CURRENT.with(|current| *current.borrow_mut() = Rc::downgrade(&app));
        app.connect_signals();
        app.follow_style();
        app.rebuild_positions();
        app.set_connected_ui(false);
        // Restarting the preview resumes tracking that was on in the old window.
        if carry.as_ref().is_some_and(|c| c.tracking) {
            app.tracking_on.set(true);
        }
        app.tick();
        if let Some(page) = carry.and_then(|c| c.page) {
            if let Some(stack) = app.sidebar.content().and_downcast::<adw::ViewStack>() {
                stack.set_visible_child_name(&page);
            }
        }

        // Holds the app alive; removed when the window closes.
        let strong = app.clone();
        let source = glib::timeout_add_local(REFRESH_INTERVAL, move || {
            strong.tick();
            glib::ControlFlow::Continue
        });
        *app.refresh_source.borrow_mut() = Some(source);
        if let Ok(path) = std::env::var("LINKSTUDIO_SCREENSHOT") {
            app.schedule_screenshot(path, std::env::var("LINKSTUDIO_PAGE").ok());
        }
        app
    }

    /// The layered cards need other colors on the light background.
    fn follow_style(&self) {
        let sync = {
            let split = self.split.clone();
            move |style: &adw::StyleManager| {
                if style.is_dark() {
                    split.remove_css_class("link-light");
                } else {
                    split.add_css_class("link-light");
                }
            }
        };
        let style = adw::StyleManager::default();
        sync(&style);
        style.connect_dark_notify(sync);
    }

    /// Development aid: render the window to a PNG and quit, since GNOME on
    /// Wayland does not let other programs capture it.
    fn schedule_screenshot(self: &Rc<Self>, path: String, page: Option<String>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(5), move || {
            let Some(app) = weak.upgrade() else { return };
            if page.as_deref() == Some("about") {
                app.show_about();
            } else if page.as_deref() == Some("update-check") {
                app.check_for_updates(true);
            } else if let Some(prefs_page) = page.as_deref().and_then(|p| p.strip_prefix("preferences")) {
                let dialog = app.show_preferences();
                if let Some(name) = prefs_page.strip_prefix('-') {
                    dialog.set_visible_page_name(name);
                }
            } else if let Some(page) = page {
                if let Some(stack) = app.sidebar.content().and_downcast::<adw::ViewStack>() {
                    stack.set_visible_child_name(&page);
                }
            }
            let window = app.window.clone();
            let weak = Rc::downgrade(&app);
            glib::timeout_add_local_once(Duration::from_millis(2500), move || {
                let paintable = gtk::WidgetPaintable::new(Some(&window));
                let snapshot = gtk::Snapshot::new();
                paintable.snapshot(&snapshot, window.width() as f64, window.height() as f64);
                match (snapshot.to_node(), window.renderer()) {
                    (Some(node), Some(renderer)) => {
                        if let Err(err) = renderer.render_texture(node, None).save_to_png(&path) {
                            eprintln!("screenshot: {err}");
                        }
                    }
                    (node, renderer) => {
                        eprintln!("screenshot: node={} renderer={}", node.is_some(), renderer.is_some())
                    }
                }
                // Really quit, even while the virtual camera would keep it hidden.
                if let Some(app) = weak.upgrade() {
                    app.force_close();
                }
            });
        });
    }

    /// Shows the window, and resumes the preview if it was hidden in the background.
    pub fn present(self: &Rc<Self>) {
        self.window.present();
        if self.stream_has_virtual.get() {
            self.show_preview.store(self.wants_preview(), Ordering::Relaxed);
        } else if self.wants_preview() && self.preview.borrow().is_none() && self.cam().is_some() {
            self.restart_stream();
        }
    }

    /// Pushes connection, virtual camera and tracking state to the status icon.
    fn sync_tray(&self) {
        let state = tray::State {
            connected: self.camera.borrow().is_some(),
            virtual_available: vcam::find_device().is_some(),
            virtual_on: self.stream_has_virtual.get(),
            tracking_on: self.tracking_on.get(),
            streaming: self.preview.borrow().is_some(),
            privacy: self.privacy.is_active(),
            positions: self.config.borrow().positions.iter().map(|p| p.name.clone()).collect(),
            profiles: self.config.borrow().profiles.iter().map(|p| p.name.clone()).collect(),
        };
        if self.tray_state.borrow().as_ref() == Some(&state) {
            return;
        }
        self.tray_state.replace(Some(state.clone()));
        TRAY.with(|tray| {
            if let Some(handle) = tray.borrow().as_ref() {
                handle.update(|tray| tray.state = state);
            }
        });
    }

    fn cam(&self) -> Option<Rc<Camera>> {
        self.camera.borrow().clone()
    }

    fn toast(&self, message: &str) {
        self.toasts.add_toast(adw::Toast::new(message));
    }

    /// Shows `what` (a translatable "…: {}" message) when `result` failed.
    fn report(&self, what: &'static str, result: std::io::Result<()>) {
        if let Err(err) = result {
            self.toast(&trf(what, &[&err]));
        }
    }

    // ----- Signal wiring -----

    fn connect_signals(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.preview_toggle.connect_toggled(move |_| {
            let Some(app) = weak.upgrade() else { return };
            if app.updating.get() {
                return;
            }
            if app.stream_has_virtual.get() {
                // The stream keeps feeding the virtual camera; only the preview changes.
                app.show_preview.store(app.wants_preview(), Ordering::Relaxed);
                app.show_preview_page();
            } else {
                app.restart_stream();
            }
        });

        let toggle = self.preview_toggle.clone();
        self.preview_status_button.connect_clicked(move |_| {
            if toggle.is_active() {
                toggle.set_active(false);
            }
            toggle.set_active(true);
        });

        let weak = Rc::downgrade(self);
        self.grid_button.connect_toggled(move |button| {
            let Some(app) = weak.upgrade() else { return };
            app.thirds.set_visible(button.is_active());
            app.config.borrow_mut().preview_grid = button.is_active();
            app.config.borrow().save();
        });
        let weak = Rc::downgrade(self);
        self.fullscreen_button.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.toggle_fullscreen();
            }
        });
        let weak = Rc::downgrade(self);
        self.window.connect_fullscreened_notify(move |window| {
            if let Some(app) = weak.upgrade() {
                app.fullscreen_changed(window.is_fullscreen());
            }
        });
        let weak = Rc::downgrade(self);
        self.mirror_button.connect_toggled(move |button| {
            let Some(app) = weak.upgrade() else { return };
            app.mirror.store(button.is_active(), Ordering::Relaxed);
            app.config.borrow_mut().preview_mirror = button.is_active();
            app.config.borrow().save();
        });

        let weak = Rc::downgrade(self);
        self.snapshot_button.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.snapshot();
            }
        });

        let [up, down, left, right, home] = &self.pad_buttons;
        self.attach_nudge(up, 0, 1);
        self.attach_nudge(down, 0, -1);
        self.attach_nudge(left, -1, 0);
        self.attach_nudge(right, 1, 0);
        let weak = Rc::downgrade(self);
        home.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.center();
            }
        });

        let weak = Rc::downgrade(self);
        self.zoom.connect_value_changed(move |scale| {
            let Some(app) = weak.upgrade() else { return };
            if app.updating.get() {
                return;
            }
            if let Some(cam) = app.cam() {
                app.report(n_("Could not zoom: {}"), cam.set(cid::ZOOM_ABSOLUTE, scale.value().round() as i32));
            }
        });
        let weak = Rc::downgrade(self);
        self.zoom_out.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.zoom_by(-ZOOM_STEP);
            }
        });
        let weak = Rc::downgrade(self);
        self.zoom_in.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.zoom_by(ZOOM_STEP);
            }
        });

        let weak = Rc::downgrade(self);
        self.privacy.connect_toggled(move |button| {
            let Some(app) = weak.upgrade() else { return };
            if !app.updating.get() {
                app.set_privacy(button.is_active());
            }
        });

        let weak = Rc::downgrade(self);
        self.save_position.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.ask_save_position();
            }
        });

        let weak = Rc::downgrade(self);
        self.window.connect_close_request(move |window| {
            if let Some(app) = weak.upgrade() {
                if app.stream_has_virtual.get() || (app.config.borrow().close_to_tray && has_tray()) {
                    app.hide_to_background(window);
                    return glib::Propagation::Stop;
                }
                app.shutdown();
            }
            glib::Propagation::Proceed
        });

        self.install_actions();
        self.install_shortcuts();
    }

    fn install_actions(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        let about = gio::ActionEntry::builder("about")
            .activate(move |_: &adw::ApplicationWindow, _, _| {
                if let Some(app) = weak.upgrade() {
                    app.show_about();
                }
            })
            .build();
        let weak = Rc::downgrade(self);
        let shortcuts = gio::ActionEntry::builder("shortcuts")
            .activate(move |_: &adw::ApplicationWindow, _, _| {
                if let Some(app) = weak.upgrade() {
                    app.show_shortcuts();
                }
            })
            .build();
        let weak = Rc::downgrade(self);
        let check_update = gio::ActionEntry::builder("check-update")
            .activate(move |_: &adw::ApplicationWindow, _, _| {
                if let Some(app) = weak.upgrade() {
                    app.check_for_updates(true);
                }
            })
            .build();
        let weak = Rc::downgrade(self);
        let preferences = gio::ActionEntry::builder("preferences")
            .activate(move |_: &adw::ApplicationWindow, _, _| {
                if let Some(app) = weak.upgrade() {
                    app.show_preferences();
                }
            })
            .build();
        self.window.add_action_entries([preferences, about, shortcuts, check_update]);
    }

    fn add_shortcut(self: &Rc<Self>, controller: &gtk::ShortcutController, accel: &str, action: impl Fn(&Rc<App>) + 'static) {
        let weak = Rc::downgrade(self);
        let callback = gtk::CallbackAction::new(move |_, _| {
            if let Some(app) = weak.upgrade() {
                action(&app);
            }
            glib::Propagation::Stop
        });
        controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string(accel), Some(callback)));
    }

    fn install_shortcuts(self: &Rc<Self>) {
        let controller = gtk::ShortcutController::new();
        controller.set_scope(gtk::ShortcutScope::Global);
        self.add_shortcut(&controller, "<Control>Left", |app| app.nudge(-1, 0, true));
        self.add_shortcut(&controller, "<Control>Right", |app| app.nudge(1, 0, true));
        self.add_shortcut(&controller, "<Control>Up", |app| app.nudge(0, 1, true));
        self.add_shortcut(&controller, "<Control>Down", |app| app.nudge(0, -1, true));
        self.add_shortcut(&controller, "<Control>plus|<Control>equal|<Control>KP_Add", |app| app.zoom_by(ZOOM_STEP));
        self.add_shortcut(&controller, "<Control>minus|<Control>KP_Subtract", |app| app.zoom_by(-ZOOM_STEP));
        self.add_shortcut(&controller, "<Control>Home", |app| app.center());
        self.add_shortcut(&controller, "<Control>p", |app| {
            app.preview_toggle.set_active(!app.preview_toggle.is_active());
        });
        self.add_shortcut(&controller, "<Control>s", |app| app.snapshot());
        self.add_shortcut(&controller, "<Control>t", |app| {
            let on = !app.tracking_on.get();
            app.set_tracking(on);
        });
        self.add_shortcut(&controller, "F9", |app| app.split.set_show_sidebar(!app.split.shows_sidebar()));
        self.add_shortcut(&controller, "F11", |app| app.toggle_fullscreen());
        // Escape is only taken while fullscreen, so it keeps closing menus and dialogs.
        let weak = Rc::downgrade(self);
        let leave_fullscreen = gtk::CallbackAction::new(move |_, _| match weak.upgrade() {
            Some(app) if app.window.is_fullscreen() => {
                app.window.unfullscreen();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        controller.add_shortcut(gtk::Shortcut::new(gtk::ShortcutTrigger::parse_string("Escape"), Some(leave_fullscreen)));
        for n in 1..=9usize {
            self.add_shortcut(&controller, &format!("<Control>{n}"), move |app| app.goto_position(n - 1));
        }
        self.window.add_controller(controller);
    }

    /// Press-and-hold on a pad button keeps the gimbal moving.
    fn attach_nudge(self: &Rc<Self>, button: &gtk::Button, dpan: i32, dtilt: i32) {
        let gesture = gtk::GestureClick::new();
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = Rc::downgrade(self);
        gesture.connect_pressed(move |_, _, _, _| {
            let Some(app) = weak.upgrade() else { return };
            app.nudge(dpan, dtilt, true);
            let repeat = Rc::downgrade(&app);
            let source = glib::timeout_add_local(NUDGE_INTERVAL, move || match repeat.upgrade() {
                Some(app) => {
                    app.nudge(dpan, dtilt, false);
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
            app.stop_nudge();
            *app.nudge_source.borrow_mut() = Some(source);
        });
        let weak = Rc::downgrade(self);
        gesture.connect_released(move |_, _, _, _| {
            if let Some(app) = weak.upgrade() {
                app.stop_nudge();
            }
        });
        let weak = Rc::downgrade(self);
        gesture.connect_stopped(move |_| {
            if let Some(app) = weak.upgrade() {
                app.stop_nudge();
            }
        });
        button.add_controller(gesture);
    }

    fn stop_nudge(&self) {
        if let Some(source) = self.nudge_source.borrow_mut().take() {
            source.remove();
        }
    }

    // ----- Connection handling -----

    fn tick(self: &Rc<Self>) {
        match self.cam() {
            None => self.try_connect(),
            Some(cam) if cam.is_gone() => self.disconnect(),
            Some(cam) => {
                self.refresh(&cam);
                self.update_stream_info();
            }
        }
    }

    fn update_stream_info(&self) {
        let frames = self.frames_shown.replace(0);
        self.stream_info.set_visible(frames > 0);
        if frames > 0 {
            let height = self.stream_height.get();
            let size = if height == 2160 { "4K".to_owned() } else { format!("{height}p") };
            self.stream_info.set_label(&format!("{size} · {frames} fps"));
        }
    }

    fn toggle_fullscreen(&self) {
        if self.window.is_fullscreen() {
            self.window.unfullscreen();
        } else {
            self.window.fullscreen();
        }
    }

    /// Fullscreen shows only the preview.
    fn fullscreen_changed(&self, fullscreen: bool) {
        if fullscreen {
            self.sidebar_before_fullscreen.set(self.split.shows_sidebar());
            self.split.set_show_sidebar(false);
        } else {
            self.split.set_show_sidebar(self.sidebar_before_fullscreen.get());
        }
        self.content.set_reveal_top_bars(!fullscreen);
        self.bottom.set_visible(!fullscreen);
        let margin = if fullscreen { 0 } else { 18 };
        self.camera_page.set_margin_start(margin);
        self.camera_page.set_margin_end(margin);
        self.camera_page.set_margin_bottom(margin);
        self.camera_page.set_margin_top(if fullscreen { 0 } else { 6 });
        self.fullscreen_button.set_icon_name(if fullscreen { "view-restore-symbolic" } else { "view-fullscreen-symbolic" });
        self.fullscreen_button.set_tooltip_text(Some(if fullscreen { tr("Leave fullscreen (Esc)") } else { tr("Fullscreen (F11)") }));
    }

    fn try_connect(self: &Rc<Self>) {
        let Some(info) = camera::find_devices().into_iter().next() else {
            return;
        };
        let cam = match Camera::open(info) {
            Ok(cam) => Rc::new(cam),
            Err(err) => {
                self.title.set_subtitle(&trf("Cannot open the camera: {}", &[&err]));
                return;
            }
        };
        if let Ok(pan) = cam.query(cid::PAN_ABSOLUTE) {
            self.pan_range.set((pan.min, pan.max));
        }
        if let Ok(tilt) = cam.query(cid::TILT_ABSOLUTE) {
            self.tilt_range.set((tilt.min, tilt.max));
        }
        if let Ok(zoom) = cam.query(cid::ZOOM_ABSOLUTE) {
            self.zoom.set_range(zoom.min as f64, zoom.max as f64);
        }
        self.rebuild_positions();
        self.title.set_subtitle(&trf("{} · connected", &[&cam.info.name]));
        *self.camera.borrow_mut() = Some(cam.clone());
        self.build_sidebar(&cam);
        self.set_connected_ui(true);
        self.restore_privacy(&cam);
        // The stream below turns tracking back on once it delivers frames;
        // never in privacy mode, where tracking would turn the camera up.
        let tracking = self.reconnect_tracking.take() && !self.config.borrow().privacy;
        self.tracking_on.set(tracking);

        // Start the stream before the startup profile, so tracking in the
        // profile can wait for the camera to wake up.
        if self.config.borrow().preview_autostart {
            self.preview_toggle.set_active(true);
        } else {
            self.restart_stream();
        }

        let startup = self.config.borrow().startup_profile.clone();
        if let Some(name) = startup {
            let profile = self.config.borrow().profiles.iter().find(|p| p.name == name).cloned();
            if let Some(profile) = profile {
                self.apply_profile(&profile);
                self.toast(&trf("Profile “{}” applied", &[&profile.name]));
            }
        }
        self.refresh(&cam);
    }

    fn disconnect(self: &Rc<Self>) {
        self.cancel_stream_retry();
        self.stream_waiting.borrow_mut().take();
        self.stop_preview();
        self.stop_nudge();
        *self.camera.borrow_mut() = None;
        self.bound.borrow_mut().clear();
        self.exposure.borrow_mut().take();
        *self.tracking.borrow_mut() = None;
        self.privacy_restore.set(None);
        self.reconnect_tracking.set(self.tracking_on.get());
        self.tracking_on.set(false);
        self.resume_tracking.set(false);
        self.updating.set(true);
        self.preview_toggle.set_active(false);
        self.privacy.set_active(false);
        self.updating.set(false);
        self.title.set_subtitle(tr("Not connected"));
        self.set_connected_ui(false);
        self.sync_tray();
        self.toast(tr("The camera was disconnected"));
    }

    fn set_connected_ui(&self, connected: bool) {
        self.main_stack.set_visible_child_name(if connected { "camera" } else { "missing" });
        self.preview_toggle.set_sensitive(connected);
        self.sidebar_toggle.set_sensitive(connected);
        if !connected {
            self.split.set_show_sidebar(false);
            self.tracking_badge.set_visible(false);
            self.privacy_badge.set_visible(false);
            self.stream_info.set_visible(false);
        } else if !self.split.is_collapsed() {
            self.split.set_show_sidebar(true);
        }
    }

    /// Reads every control back from the camera so the UI follows changes made
    /// by the camera itself (auto modes, AI tracking, hand gestures).
    fn refresh(&self, cam: &Camera) {
        self.updating.set(true);
        for bound in self.bound.borrow().iter() {
            let id = bound.info().id;
            if let Ok(info) = cam.query(id) {
                bound.row().set_sensitive(info.is_writable() && !info.is_inactive());
            }
            if let Ok(value) = cam.get(id) {
                bound.show_value(value);
            }
        }

        if let (Some(scale), Ok(value)) = (self.exposure.borrow().as_ref(), cam.exposure_compensation()) {
            let ev = value as f64 / EXPOSURE_UNITS_PER_EV;
            if (scale.value() - ev).abs() > 0.05 {
                scale.set_value(ev);
            }
        }

        // The camera reads back 0xff after a mode command (then we keep what we
        // set), and resets to normal when its stream stops. While a restarted
        // preview is waiting to resume tracking, the reset is expected.
        let mode = cam.mode().ok().flatten();
        let settling = self.resume_tracking.get()
            || self.tracking_retry.borrow().is_some()
            || self.stream_retry.borrow().is_some();
        if let (Some(mode), false) = (mode, settling) {
            self.tracking_on.set(mode == Mode::Tracking);
        }
        let tracking = self.tracking_on.get();
        if let Some(rows) = self.tracking.borrow().as_ref() {
            rows.follow.set_active(tracking);
            if mode.is_some() {
                rows.other_mode.set_visible(matches!(mode, Some(Mode::Other(_))));
            }
            if let Ok(framing) = cam.framing() {
                if let Some(pos) = FRAMINGS.iter().position(|(v, _)| *v == framing) {
                    rows.framing.set_selected(pos as u32);
                    self.tracking_badge_label.set_label(&format!("{} · {}", tr("AI tracking"), tr(FRAMINGS[pos].1)));
                }
            }
            if rows.group.is_visible() {
                if let Ok(group) = cam.group_tracking() {
                    rows.group.set_selected(group as u32);
                }
            }
        }
        self.tracking_badge.set_visible(tracking);
        let privacy = self.privacy.is_active();
        self.privacy_badge.set_visible(privacy);
        self.pad.set_sensitive(!tracking && !privacy);
        for widget in [self.zoom.upcast_ref::<gtk::Widget>(), self.zoom_in.upcast_ref(), self.zoom_out.upcast_ref()] {
            widget.set_sensitive(!privacy);
        }

        if let Some((pan, tilt)) = gimbal_position(cam) {
            self.position_label.set_label(&pan_tilt_label(pan, tilt));
        }
        if let Ok(zoom) = cam.get(cid::ZOOM_ABSOLUTE) {
            if (self.zoom.value() - zoom as f64).abs() > 0.5 {
                self.zoom.set_value(zoom as f64);
            }
        }
        self.updating.set(false);
        self.sync_tray();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn degrees_and_ids() {
        assert_eq!(snap_degree(5399), 3600);
        assert_eq!(snap_degree(-5401), -7200);
        assert_eq!(degrees(-162_000), -45);
        assert_eq!(parse_id("0x00980900"), Some(cid::BRIGHTNESS));
        assert_eq!(parse_id("brightness"), None);
        assert_eq!(zoom_factor(150), "1.5");
    }

    #[test]
    fn reset_turns_automatic_modes_on() {
        let info = |id, kind, default| CtrlInfo { id, name: String::new(), kind, min: 0, max: 3, step: 1, default, flags: 0 };
        assert_eq!(reset_value(&info(cid::FOCUS_AUTO, CtrlKind::Boolean, 0)), Some(1));
        assert_eq!(reset_value(&info(cid::BRIGHTNESS, CtrlKind::Integer, 2)), Some(2));
        let menu = CtrlKind::Menu(vec![(1, "50 Hz".into()), (2, "60 Hz".into())]);
        assert_eq!(reset_value(&info(cid::POWER_LINE_FREQUENCY, menu.clone(), 2)), Some(2));
        assert_eq!(reset_value(&info(cid::POWER_LINE_FREQUENCY, menu, 0)), None);
    }
}
