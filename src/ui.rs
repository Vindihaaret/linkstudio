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

const REFRESH_INTERVAL: Duration = Duration::from_millis(1000);
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
.preview-video { background-color: black; border-radius: 12px; }
.preview-status { background-color: alpha(currentColor, 0.05); border-radius: 12px; }
.badge {
  border-radius: 999px;
  padding: 4px 12px;
  font-weight: 700;
  font-size: 0.85em;
  color: white;
  background-color: alpha(@accent_bg_color, 0.92);
}
.badge.virtual { background-color: alpha(@success_bg_color, 0.92); }
.badge.privacy { background-color: alpha(@warning_bg_color, 0.95); color: @warning_fg_color; }
.ptz-pad button { min-width: 46px; min-height: 46px; }
.control-card { padding: 14px 18px; }
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
                (_, None) => {}
            }
        }
    });
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
    description: Option<&'static str>,
    ids: &'static [u32],
}

const IMAGE_SECTIONS: &[Section] = &[
    Section {
        title: n_("Image"),
        description: None,
        ids: &[cid::BRIGHTNESS, cid::CONTRAST, cid::SATURATION, cid::HUE, cid::SHARPNESS, cid::GAMMA],
    },
    Section {
        title: n_("Exposure"),
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
        description: None,
        ids: &[cid::AUTO_WHITE_BALANCE, cid::WHITE_BALANCE_TEMPERATURE],
    },
    Section { title: n_("Focus"), description: None, ids: &[cid::FOCUS_AUTO, cid::FOCUS_ABSOLUTE] },
    Section {
        title: n_("Flicker"),
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
    trf("Pan {}° · Tilt {}° · Zoom {}×", &[&degrees(p.pan), &degrees(p.tilt), &zoom_factor(p.zoom)])
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
    tracking_badge: gtk::Label,
    privacy_badge: gtk::Label,
    pad: gtk::Grid,
    pad_buttons: [gtk::Button; 5],
    zoom: gtk::Scale,
    zoom_out: gtk::Button,
    zoom_in: gtk::Button,
    position_label: gtk::Label,
    privacy: gtk::ToggleButton,
    positions_list: gtk::ListBox,
    save_position: gtk::Button,
    mirror: Arc<AtomicBool>,
    show_preview: Arc<AtomicBool>,
    virtual_badge: gtk::Label,

    camera: RefCell<Option<Rc<Camera>>>,
    /// Whether the running stream also feeds the virtual camera.
    stream_has_virtual: Cell<bool>,
    /// Whether the running stream has delivered frames (the camera is awake).
    stream_live: Cell<bool>,
    virtual_rows: RefCell<Option<VirtualCameraRows>>,
    quality_row: RefCell<Option<adw::ComboRow>>,
    /// Last state pushed to the status icon, to avoid needless D-Bus updates.
    tray_state: Cell<Option<tray::State>>,
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
        menu.append(Some(tr("About Link Studio")), Some("win.about"));
        let removal = gio::Menu::new();
        removal.append(Some(tr("Uninstall Link Studio…")), Some("win.uninstall"));
        removal.append(Some(tr("Quit")), Some("app.quit"));
        menu.append_section(None, &removal);
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
        let tracking_badge = gtk::Label::builder()
            .label(tr("AI tracking"))
            .visible(false)
            .css_classes(["badge"])
            .build();
        let privacy_badge = gtk::Label::builder()
            .label(tr("Privacy"))
            .visible(false)
            .css_classes(["badge", "privacy"])
            .build();
        let virtual_badge = gtk::Label::builder()
            .label(tr("Virtual camera"))
            .visible(false)
            .css_classes(["badge", "virtual"])
            .build();
        badges.append(&virtual_badge);
        badges.append(&tracking_badge);
        badges.append(&privacy_badge);
        overlay.add_overlay(&badges);

        let osd = gtk::Box::builder()
            .spacing(6)
            .halign(gtk::Align::End)
            .valign(gtk::Align::End)
            .margin_bottom(12)
            .margin_end(12)
            .build();
        let mirror_button = gtk::ToggleButton::builder()
            .icon_name("object-flip-horizontal-symbolic")
            .tooltip_text(tr("Mirror the preview (does not affect other programs)"))
            .css_classes(["osd", "circular"])
            .active(config.preview_mirror)
            .build();
        let snapshot_button = gtk::Button::builder()
            .icon_name("camera-photo-symbolic")
            .tooltip_text(tr("Take photo (Ctrl+S)"))
            .css_classes(["osd", "circular"])
            .build();
        osd.append(&mirror_button);
        osd.append(&snapshot_button);
        overlay.add_overlay(&osd);

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
        for b in [&up, &down, &left, &right, &home] {
            b.add_css_class("circular");
        }
        let pad = gtk::Grid::builder()
            .row_spacing(6)
            .column_spacing(6)
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
        zoom.set_width_request(180);
        zoom.set_draw_value(true);
        zoom.set_value_pos(gtk::PositionType::Right);
        zoom.set_format_value_func(|_, v| format!("{:.1}×", v / 100.0));
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

        let position_label = gtk::Label::builder()
            .label(pan_tilt_label(0, 0))
            .halign(gtk::Align::Start)
            .css_classes(["dim-label", "caption", "numeric"])
            .build();
        let privacy = gtk::ToggleButton::builder()
            .child(&adw::ButtonContent::builder().icon_name("view-conceal-symbolic").label(tr("Privacy")).build())
            .tooltip_text(tr("Point the camera down. Turn off to return to the previous position."))
            .halign(gtk::Align::Start)
            .build();

        let gimbal_side = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        gimbal_side.append(&gtk::Label::builder().label(tr("Zoom")).halign(gtk::Align::Start).css_classes(["heading"]).build());
        gimbal_side.append(&zoom_row);
        gimbal_side.append(&position_label);
        gimbal_side.append(&privacy);

        let gimbal_inner = gtk::Box::builder().spacing(24).build();
        gimbal_inner.append(&pad);
        gimbal_inner.append(&gimbal_side);
        let gimbal_card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .css_classes(["card", "control-card"])
            .width_request(440)
            .build();
        gimbal_card.append(
            &gtk::Label::builder()
                .label(tr("Camera Control"))
                .halign(gtk::Align::Start)
                .css_classes(["title-4"])
                .build(),
        );
        gimbal_card.append(&gimbal_inner);

        // Saved positions.
        let positions_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let save_position = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label(tr("Save Position")).build())
            .css_classes(["flat"])
            .build();
        let positions_group = adw::PreferencesGroup::builder()
            .title(tr("Saved Positions"))
            .description(tr("Click to move the camera there"))
            .header_suffix(&save_position)
            .hexpand(true)
            .build();
        let positions_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .max_content_height(260)
            .child(&positions_list)
            .build();
        positions_group.add(&positions_scroll);

        let bottom = gtk::Box::builder().spacing(18).build();
        bottom.append(&gimbal_card);
        bottom.append(&positions_group);

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
            privacy_badge,
            pad,
            pad_buttons: [up, down, left, right, home],
            zoom,
            zoom_out,
            zoom_in,
            position_label,
            privacy,
            positions_list,
            save_position,
            mirror: Arc::new(AtomicBool::new(config.preview_mirror)),
            show_preview: Arc::new(AtomicBool::new(false)),
            virtual_badge,
            camera: RefCell::new(None),
            stream_has_virtual: Cell::new(false),
            stream_live: Cell::new(false),
            virtual_rows: RefCell::new(None),
            quality_row: RefCell::new(None),
            tray_state: Cell::new(None),
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
        });
        CURRENT.with(|current| *current.borrow_mut() = Rc::downgrade(&app));
        app.connect_signals();
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

    /// Development aid: render the window to a PNG and quit, since GNOME on
    /// Wayland does not let other programs capture it.
    fn schedule_screenshot(self: &Rc<Self>, path: String, page: Option<String>) {
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_secs(5), move || {
            let Some(app) = weak.upgrade() else { return };
            if page.as_deref() == Some("preferences") {
                app.show_preferences();
            } else if let Some(page) = page {
                if let Some(stack) = app.sidebar.content().and_downcast::<adw::ViewStack>() {
                    stack.set_visible_child_name(&page);
                }
            }
            let window = app.window.clone();
            let weak = Rc::downgrade(&app);
            glib::timeout_add_local_once(Duration::from_millis(800), move || {
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
        };
        if self.tray_state.replace(Some(state)) == Some(state) {
            return;
        }
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
                if app.stream_has_virtual.get() {
                    app.hide_to_background(window);
                    return glib::Propagation::Stop;
                }
                app.stop_preview();
                app.stop_nudge();
                for source in [&app.refresh_source, &app.tracking_retry] {
                    if let Some(source) = source.borrow_mut().take() {
                        source.remove();
                    }
                }
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
        let uninstall = gio::ActionEntry::builder("uninstall")
            .activate(move |_: &adw::ApplicationWindow, _, _| {
                if let Some(app) = weak.upgrade() {
                    app.ask_uninstall();
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
        self.window.add_action_entries([preferences, about, shortcuts, uninstall]);
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
            Some(cam) => self.refresh(&cam),
        }
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
        self.title.set_subtitle(&trf("{} · connected", &[&cam.info.name]));
        *self.camera.borrow_mut() = Some(cam.clone());
        self.build_sidebar(&cam);
        self.set_connected_ui(true);

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
        self.stop_preview();
        self.stop_nudge();
        *self.camera.borrow_mut() = None;
        self.bound.borrow_mut().clear();
        self.exposure.borrow_mut().take();
        *self.tracking.borrow_mut() = None;
        self.privacy_restore.set(None);
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
        let settling = self.resume_tracking.get() || self.tracking_retry.borrow().is_some();
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

    // ----- Settings sidebar -----

    fn build_sidebar(self: &Rc<Self>, cam: &Rc<Camera>) {
        self.bound.borrow_mut().clear();
        self.exposure.borrow_mut().take();
        let stack = adw::ViewStack::new();

        let image = self.build_image_page(cam);
        stack.add_titled_with_icon(&image, Some("image"), tr("Image"), "display-brightness-symbolic");
        let tracking = self.build_tracking_page(cam);
        stack.add_titled_with_icon(&tracking, Some("tracking"), tr("Tracking"), "emoji-people-symbolic");
        let effects = self.build_effects_page();
        stack.add_titled_with_icon(&effects, Some("effects"), tr("Effects"), "applications-graphics-symbolic");
        let profiles = self.build_profiles_page();
        stack.add_titled_with_icon(&profiles, Some("profiles"), tr("Profiles"), "user-bookmarks-symbolic");
        let info = self.build_camera_page(cam);
        stack.add_titled_with_icon(&info, Some("camera"), tr("Camera"), "camera-web-symbolic");

        self.switcher.set_stack(Some(&stack));
        self.sidebar.set_content(Some(&stack));
    }

    fn build_image_page(self: &Rc<Self>, cam: &Camera) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let controls: Vec<CtrlInfo> = cam.controls().into_iter().filter(|c| !PTZ_IDS.contains(&c.id)).collect();

        for section in IMAGE_SECTIONS {
            let infos: Vec<&CtrlInfo> = section
                .ids
                .iter()
                .filter_map(|id| controls.iter().find(|c| c.id == *id))
                .collect();
            // The Link has no standard exposure controls, only the vendor one.
            let exposure_row = match section.ids.contains(&cid::EXPOSURE_AUTO) {
                true => self.exposure_compensation_row(cam),
                false => None,
            };
            if infos.is_empty() && exposure_row.is_none() {
                continue;
            }
            let group = adw::PreferencesGroup::builder().title(tr(section.title)).build();
            if let Some(description) = section.description {
                group.set_description(Some(tr(description)));
            }
            if let Some(row) = exposure_row {
                group.add(&row);
            }
            for info in infos {
                group.add(&self.control_row(info));
            }
            page.add(&group);
        }

        let known: Vec<u32> = IMAGE_SECTIONS.iter().flat_map(|s| s.ids.iter().copied()).collect();
        let others: Vec<&CtrlInfo> = controls.iter().filter(|c| !known.contains(&c.id)).collect();
        if !others.is_empty() {
            let group = adw::PreferencesGroup::builder().title(tr("Other Settings")).build();
            for info in others {
                group.add(&self.control_row(info));
            }
            page.add(&group);
        }

        let reset_group = adw::PreferencesGroup::new();
        let reset = gtk::Button::builder()
            .label(tr("Reset All Image Settings"))
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        let weak = Rc::downgrade(self);
        reset.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.reset_image_settings();
            }
        });
        reset_group.add(&reset);
        page.add(&reset_group);
        page
    }

    fn control_row(self: &Rc<Self>, info: &CtrlInfo) -> gtk::Widget {
        let (title, subtitle) = control_title(info);
        let id = info.id;
        let bound = match &info.kind {
            CtrlKind::Integer => {
                let row = adw::ActionRow::builder().title(title.as_str()).build();
                if let Some(subtitle) = subtitle {
                    row.set_subtitle(subtitle);
                }
                let scale = gtk::Scale::with_range(
                    gtk::Orientation::Horizontal,
                    info.min as f64,
                    info.max as f64,
                    info.step.max(1) as f64,
                );
                scale.set_digits(0);
                scale.set_round_digits(0);
                scale.set_draw_value(true);
                scale.set_value_pos(gtk::PositionType::Right);
                // Same width on every row, so the sliders line up.
                scale.set_width_request(200);
                scale.set_valign(gtk::Align::Center);
                if id == cid::WHITE_BALANCE_TEMPERATURE {
                    scale.set_format_value_func(|_, v| format!("{v:.0} K"));
                }
                if info.min < info.default && info.default < info.max {
                    scale.add_mark(info.default as f64, gtk::PositionType::Bottom, None);
                }
                let reset = gtk::Button::builder()
                    .icon_name("edit-undo-symbolic")
                    .tooltip_text(tr("Reset to default"))
                    .valign(gtk::Align::Center)
                    .css_classes(["flat", "circular"])
                    .build();
                row.add_suffix(&scale);
                row.add_suffix(&reset);

                let weak = Rc::downgrade(self);
                scale.connect_value_changed(move |scale| {
                    if let Some(app) = weak.upgrade() {
                        app.set_control(id, scale.value().round() as i32);
                    }
                });
                let default = info.default as f64;
                let target = scale.clone();
                reset.connect_clicked(move |_| target.set_value(default));
                Bound::Scale { info: info.clone(), row, scale }
            }
            CtrlKind::Boolean => {
                let row = adw::SwitchRow::builder().title(title.as_str()).build();
                if let Some(subtitle) = subtitle {
                    row.set_subtitle(subtitle);
                }
                let weak = Rc::downgrade(self);
                row.connect_active_notify(move |row| {
                    if let Some(app) = weak.upgrade() {
                        app.set_control(id, row.is_active() as i32);
                    }
                });
                Bound::Switch { info: info.clone(), row }
            }
            CtrlKind::Menu(items) => {
                let labels: Vec<String> = items.iter().map(|(_, name)| menu_label(name)).collect();
                let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
                let values: Vec<i32> = items.iter().map(|(v, _)| *v).collect();
                let row = adw::ComboRow::builder()
                    .title(title.as_str())
                    .model(&gtk::StringList::new(&labels))
                    .build();
                if let Some(subtitle) = subtitle {
                    row.set_subtitle(subtitle);
                }
                let weak = Rc::downgrade(self);
                let menu_values = values.clone();
                row.connect_selected_notify(move |row| {
                    let Some(app) = weak.upgrade() else { return };
                    if let Some(&value) = menu_values.get(row.selected() as usize) {
                        app.set_control(id, value);
                    }
                });
                Bound::Combo { info: info.clone(), row, values }
            }
        };
        let widget = bound.row();
        self.bound.borrow_mut().push(bound);
        widget
    }

    fn exposure_compensation_row(self: &Rc<Self>, cam: &Camera) -> Option<adw::ActionRow> {
        cam.exposure_compensation().ok()?;
        let row = adw::ActionRow::builder()
            .title(tr("Exposure compensation"))
            .subtitle(tr("Brighter or darker image, in EV steps"))
            .build();
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, -EXPOSURE_MAX_EV, EXPOSURE_MAX_EV, 0.1);
        scale.set_digits(1);
        scale.set_round_digits(1);
        scale.set_draw_value(true);
        scale.set_value_pos(gtk::PositionType::Right);
        scale.set_width_request(200);
        scale.set_valign(gtk::Align::Center);
        scale.set_format_value_func(|_, ev| format!("{ev:+.1} EV"));
        scale.add_mark(0.0, gtk::PositionType::Bottom, None);
        let reset = gtk::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text(tr("Reset to default"))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        row.add_suffix(&scale);
        row.add_suffix(&reset);
        let weak = Rc::downgrade(self);
        scale.connect_value_changed(move |scale| {
            let Some(app) = weak.upgrade() else { return };
            if app.updating.get() {
                return;
            }
            if let Some(cam) = app.cam() {
                let value = (scale.value() * EXPOSURE_UNITS_PER_EV).round() as i16;
                app.report(n_("Could not change the setting: {}"), cam.set_exposure_compensation(value));
            }
        });
        let target = scale.clone();
        reset.connect_clicked(move |_| target.set_value(0.0));
        *self.exposure.borrow_mut() = Some(scale);
        Some(row)
    }

    fn set_control(&self, id: u32, value: i32) {
        if self.updating.get() {
            return;
        }
        let Some(cam) = self.cam() else { return };
        if let Err(err) = cam.set(id, value) {
            self.toast(&trf("Could not change the setting: {}", &[&err]));
        }
        // Auto modes enable or disable their manual counterparts.
        for bound in self.bound.borrow().iter() {
            if let Ok(info) = cam.query(bound.info().id) {
                bound.row().set_sensitive(info.is_writable() && !info.is_inactive());
            }
        }
    }

    fn reset_image_settings(&self) {
        let Some(cam) = self.cam() else { return };
        // Auto switches first, so the manual values they unlock can be written.
        for pass_integers in [false, true] {
            for bound in self.bound.borrow().iter() {
                let info = bound.info();
                if (info.kind == CtrlKind::Integer) == pass_integers {
                    let _ = cam.set(info.id, info.default);
                }
            }
        }
        if self.exposure.borrow().is_some() {
            let _ = cam.set_exposure_compensation(0);
        }
        self.refresh(&cam);
        self.toast(tr("Image settings reset"));
    }

    fn build_tracking_page(self: &Rc<Self>, cam: &Camera) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();

        let group = adw::PreferencesGroup::builder()
            .title(tr("AI Tracking"))
            .description(tr("The camera uses its gimbal to keep you in the picture automatically. It turns tracking off by itself after 60 seconds with no one in the picture."))
            .build();
        let follow = adw::SwitchRow::builder()
            .title(tr("Follow Me"))
            .subtitle(tr("Ctrl+T turns tracking on and off"))
            .build();
        let framing_labels: Vec<&str> = FRAMINGS.iter().map(|(_, l)| tr(l)).collect();
        let framing = adw::ComboRow::builder()
            .title(tr("Framing"))
            .subtitle(tr("How much of you stays in the picture"))
            .model(&gtk::StringList::new(&framing_labels))
            .build();
        let target = adw::ComboRow::builder()
            .title(tr("Who to Follow"))
            .model(&gtk::StringList::new(&[tr("One person"), tr("Group")]))
            .visible(cam.can_set_group_tracking())
            .build();
        let other_mode = adw::ActionRow::builder()
            .title(tr("A special mode is active"))
            .subtitle(tr("DeskView, whiteboard or overhead view – often started by a hand gesture"))
            .visible(false)
            .build();
        other_mode.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        let normal = gtk::Button::builder()
            .label(tr("Normal View"))
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        other_mode.add_suffix(&normal);
        group.add(&follow);
        group.add(&framing);
        group.add(&target);
        group.add(&other_mode);
        page.add(&group);

        let gestures = adw::PreferencesGroup::builder()
            .title(tr("Hand Gestures"))
            .description(tr("The camera recognizes these gestures by itself. They are always on and cannot be turned off from Linux. Keep your face in the picture and do not cover it with your hand."))
            .build();
        // From the Insta360 Link manual (onlinemanual.insta360.com/link).
        let gesture_rows = [
            ("✋", tr("Open palm"), tr("Turns AI tracking on or off. Hold your palm toward the camera next to your face until the light flashes green.")),
            ("L", tr("L sign"), tr("Zoom. Hold the L until the light flashes blue, then move your hand up to zoom in or down to zoom out.")),
            ("✌", tr("Peace sign"), tr("Turns whiteboard mode on or off. Needs the recognition markers on the corners of the whiteboard.")),
        ];
        for (symbol, title, subtitle) in gesture_rows {
            let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
            row.add_prefix(
                &gtk::Label::builder()
                    .label(symbol)
                    .width_chars(2)
                    .css_classes(["title-2"])
                    .build(),
            );
            gestures.add(&row);
        }
        let other_mode_hint = adw::ActionRow::builder()
            .title(tr("Ended up in another mode?"))
            .subtitle(tr("Use “Normal View” above, which appears when the camera is in DeskView or whiteboard mode."))
            .build();
        other_mode_hint.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
        gestures.add(&other_mode_hint);
        page.add(&gestures);

        let weak = Rc::downgrade(self);
        follow.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            if !app.updating.get() {
                app.set_tracking(row.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        framing.connect_selected_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            if app.updating.get() {
                return;
            }
            if let (Some(cam), Some((value, _))) = (app.cam(), FRAMINGS.get(row.selected() as usize)) {
                app.report(n_("Could not change the framing: {}"), cam.set_framing(*value));
            }
        });
        let weak = Rc::downgrade(self);
        target.connect_selected_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            if app.updating.get() {
                return;
            }
            if let Some(cam) = app.cam() {
                app.report(n_("Could not change who to follow: {}"), cam.set_group_tracking(row.selected() == 1));
            }
        });
        let weak = Rc::downgrade(self);
        normal.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.set_tracking(false);
            }
        });

        *self.tracking.borrow_mut() = Some(TrackingRows { follow, framing, group: target, other_mode });
        page
    }

    fn build_effects_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
            .title(tr("Virtual Camera"))
            .description(tr("Sends the picture from Link Studio to other programs, such as Teams, Zoom or the browser, as “Link Studio Camera”. The preview and the meeting can then run at the same time."))
            .build();

        let setup = adw::ActionRow::builder().title(tr("Set up the virtual camera")).build();
        setup.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
        let setup_button = gtk::Button::builder()
            .label(tr("Set Up…"))
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        setup.add_suffix(&setup_button);

        let enabled = adw::SwitchRow::builder()
            .title(tr("Virtual Camera"))
            .subtitle(tr("Keeps running when the window is closed. Quit from the menu to stop it."))
            .active(self.config.borrow().virtual_camera)
            .build();
        let labels: Vec<&str> = VIRTUAL_HEIGHTS.iter().map(|(_, l)| *l).collect();
        let current = self.config.borrow().virtual_camera_height;
        let resolution = adw::ComboRow::builder()
            .title(tr("Resolution"))
            .model(&gtk::StringList::new(&labels))
            .selected(VIRTUAL_HEIGHTS.iter().position(|(h, _)| *h == current).unwrap_or(0) as u32)
            .build();

        let remove = adw::ActionRow::builder()
            .title(tr("Remove the virtual camera setup"))
            .subtitle(tr("Needs the administrator password"))
            .build();
        let remove_button = gtk::Button::builder()
            .label(tr("Remove…"))
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        remove.add_suffix(&remove_button);

        group.add(&setup);
        group.add(&enabled);
        group.add(&resolution);
        group.add(&remove);
        page.add(&group);

        let weak = Rc::downgrade(self);
        setup_button.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.run_virtual_camera_setup(false);
            }
        });
        let weak = Rc::downgrade(self);
        remove_button.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.run_virtual_camera_setup(true);
            }
        });
        let weak = Rc::downgrade(self);
        enabled.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            if app.updating.get() {
                return;
            }
            app.set_virtual_camera(row.is_active());
        });
        let weak = Rc::downgrade(self);
        resolution.connect_selected_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            let Some((height, _)) = VIRTUAL_HEIGHTS.get(row.selected() as usize) else { return };
            app.config.borrow_mut().virtual_camera_height = *height;
            app.config.borrow().save();
            if app.stream_has_virtual.get() {
                app.restart_stream();
            }
        });

        *self.virtual_rows.borrow_mut() = Some(VirtualCameraRows { setup, setup_button, enabled, resolution, remove });
        self.update_virtual_rows();
        page
    }

    /// Shows the setup row until the loopback device exists, and enables the
    /// switch only when there is a device to write to.
    fn update_virtual_rows(&self) {
        let rows = self.virtual_rows.borrow();
        let Some(rows) = rows.as_ref() else { return };
        let device = vcam::find_device();
        let configured = vcam::is_set_up();
        rows.setup.set_visible(device.is_none());
        rows.setup_button.set_visible(!configured);
        rows.setup.set_subtitle(if configured {
            tr("Restart the computer to finish the setup.")
        } else {
            tr("Installs the v4l2loopback driver. Needs the administrator password.")
        });
        rows.enabled.set_sensitive(device.is_some());
        rows.resolution.set_sensitive(device.is_some());
        rows.remove.set_visible(configured);
    }

    fn run_virtual_camera_setup(self: &Rc<Self>, remove: bool) {
        if remove {
            // The driver cannot be unloaded while Link Studio writes to it.
            self.set_virtual_camera(false);
        }

        // Package downloads can take a while, so show what the script prints.
        let title = if remove { tr("Removing the Virtual Camera") } else { tr("Setting Up the Virtual Camera") };
        let dialog = adw::AlertDialog::new(
            Some(title),
            Some(tr("Enter the administrator password when asked. Downloading the driver can take a few minutes.")),
        );
        let spinner = gtk::Spinner::builder().spinning(true).build();
        let status = gtk::Label::builder()
            .label(tr("Waiting for the administrator password…"))
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .xalign(0.0)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();
        let status_row = gtk::Box::builder().spacing(10).build();
        status_row.append(&spinner);
        status_row.append(&status);
        let log = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .build();
        let details = gtk::Expander::builder()
            .label(tr("Details"))
            .child(
                &gtk::ScrolledWindow::builder()
                    .min_content_height(160)
                    .hscrollbar_policy(gtk::PolicyType::Never)
                    .child(&log)
                    .build(),
            )
            .build();
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
        content.append(&status_row);
        content.append(&details);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("close", tr("Close"));
        dialog.set_response_enabled("close", false);
        dialog.set_can_close(false);
        dialog.present(Some(&self.window));

        let buffer = log.buffer();
        let log_buffer = buffer.clone();
        let status_label = status.clone();
        let log_view = log.clone();
        let on_output = move |line: &str| {
            let line = line.trim();
            if line.is_empty() {
                return;
            }
            status_label.set_label(line);
            let mut end = buffer.end_iter();
            buffer.insert(&mut end, &format!("{line}\n"));
            log_view.scroll_to_iter(&mut buffer.end_iter(), 0.0, false, 0.0, 1.0);
        };

        let weak = Rc::downgrade(self);
        vcam::run_privileged(remove, on_output, move |result| {
            spinner.set_spinning(false);
            spinner.set_visible(false);
            dialog.set_response_enabled("close", true);
            dialog.set_can_close(true);
            let Some(app) = weak.upgrade() else { return };
            match result {
                Ok(()) if remove => {
                    dialog.set_heading(Some(tr("The virtual camera setup has been removed")));
                    dialog.set_body("");
                    status.set_visible(false);
                }
                Ok(()) => {
                    let ready = vcam::find_device().is_some();
                    dialog.set_heading(Some(if ready {
                        tr("The virtual camera is ready")
                    } else {
                        tr("Restart the computer to finish the setup.")
                    }));
                    dialog.set_body(if ready {
                        tr("Choose “Link Studio Camera” in Teams, Zoom or your browser.")
                    } else {
                        ""
                    });
                    status.set_visible(false);
                    if ready {
                        app.set_virtual_camera(true);
                    }
                }
                // The password dialog was dismissed; nothing happened.
                Err(vcam::SetupError::Cancelled) => {
                    dialog.force_close();
                }
                Err(vcam::SetupError::Failed(message)) => {
                    // Errors before the script printed anything (e.g. no pkexec).
                    if log_buffer.char_count() == 0 {
                        log_buffer.set_text(&message);
                    }
                    dialog.set_heading(Some(tr("The setup did not finish")));
                    dialog.set_body(tr("See the details below."));
                    details.set_expanded(true);
                }
            }
            app.update_virtual_rows();
        });
    }

    /// Turns the virtual camera on or off, from the Effects page or the status icon.
    fn set_virtual_camera(self: &Rc<Self>, on: bool) {
        self.config.borrow_mut().virtual_camera = on;
        self.config.borrow().save();
        if let Some(rows) = self.virtual_rows.borrow().as_ref() {
            self.updating.set(true);
            rows.enabled.set_active(on);
            self.updating.set(false);
        }
        self.restart_stream();
        self.update_virtual_rows();
        self.sync_tray();
        // Hidden and nothing left to do: without a status icon there is no way
        // back to the window, so close for real.
        let has_tray = TRAY.with(|tray| tray.borrow().is_some());
        if !on && !self.window.is_visible() && !has_tray {
            self.force_close();
        }
    }

    fn build_profiles_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder()
            .title(tr("Profiles"))
            .description(tr("A profile stores the image and tracking settings. Star a profile to apply it automatically every time the camera is connected."))
            .build();
        let entry = adw::EntryRow::builder()
            .title(tr("Save current settings as…"))
            .show_apply_button(true)
            .build();
        let weak = Rc::downgrade(self);
        entry.connect_apply(move |entry| {
            let Some(app) = weak.upgrade() else { return };
            let name = entry.text().trim().to_owned();
            if name.is_empty() {
                return;
            }
            app.save_profile(&name);
            entry.set_text("");
        });
        group.add(&entry);
        page.add(&group);
        *self.profiles_group.borrow_mut() = Some(group);
        self.profile_rows.borrow_mut().clear();
        self.rebuild_profiles();
        page
    }

    fn rebuild_profiles(self: &Rc<Self>) {
        let Some(group) = self.profiles_group.borrow().clone() else { return };
        for row in self.profile_rows.borrow_mut().drain(..) {
            group.remove(&row);
        }
        let config = self.config.borrow();
        for profile in &config.profiles {
            let is_startup = config.startup_profile.as_deref() == Some(profile.name.as_str());
            let row = adw::ActionRow::builder()
                .title(profile.name.as_str())
                .subtitle(if is_startup { tr("Applied automatically on connect") } else { tr("Click to apply") })
                .activatable(true)
                .build();
            let star = gtk::Button::builder()
                .icon_name(if is_startup { "starred-symbolic" } else { "non-starred-symbolic" })
                .tooltip_text(tr("Apply automatically when the camera is connected"))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            let delete = gtk::Button::builder()
                .icon_name("edit-delete-symbolic")
                .tooltip_text(tr("Delete profile"))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            row.add_suffix(&star);
            row.add_suffix(&delete);

            let name = profile.name.clone();
            let weak = Rc::downgrade(self);
            row.connect_activated(move |_| {
                let Some(app) = weak.upgrade() else { return };
                let profile = app.config.borrow().profiles.iter().find(|p| p.name == name).cloned();
                if let Some(profile) = profile {
                    app.apply_profile(&profile);
                    app.toast(&trf("Profile “{}” applied", &[&profile.name]));
                }
            });
            let name = profile.name.clone();
            let weak = Rc::downgrade(self);
            star.connect_clicked(move |_| {
                let Some(app) = weak.upgrade() else { return };
                {
                    let mut config = app.config.borrow_mut();
                    config.startup_profile =
                        if config.startup_profile.as_deref() == Some(name.as_str()) { None } else { Some(name.clone()) };
                    config.save();
                }
                app.rebuild_profiles();
            });
            let name = profile.name.clone();
            let weak = Rc::downgrade(self);
            delete.connect_clicked(move |_| {
                let Some(app) = weak.upgrade() else { return };
                {
                    let mut config = app.config.borrow_mut();
                    config.profiles.retain(|p| p.name != name);
                    if config.startup_profile.as_deref() == Some(name.as_str()) {
                        config.startup_profile = None;
                    }
                    config.save();
                }
                app.rebuild_profiles();
                app.toast(&trf("Profile “{}” deleted", &[&name]));
            });

            group.add(&row);
            self.profile_rows.borrow_mut().push(row.upcast());
        }
    }

    fn save_profile(self: &Rc<Self>, name: &str) {
        let Some(cam) = self.cam() else { return };
        let mut profile = Profile { name: name.to_owned(), ..Default::default() };
        for bound in self.bound.borrow().iter() {
            let id = bound.info().id;
            if let Ok(value) = cam.get(id) {
                profile.controls.insert(format!("{id:#010x}"), value);
            }
        }
        profile.tracking = Some(self.tracking_on.get());
        profile.framing = cam.framing().ok();
        if self.exposure.borrow().is_some() {
            profile.exposure_compensation = cam.exposure_compensation().ok();
        }
        if cam.can_set_group_tracking() {
            profile.group_tracking = cam.group_tracking().ok();
        }
        {
            let mut config = self.config.borrow_mut();
            match config.profiles.iter_mut().find(|p| p.name == name) {
                Some(existing) => *existing = profile,
                None => config.profiles.push(profile),
            }
            config.save();
        }
        self.rebuild_profiles();
        self.toast(&trf("Profile “{}” saved", &[&name]));
    }

    fn apply_profile(self: &Rc<Self>, profile: &Profile) {
        let Some(cam) = self.cam() else { return };
        let values: Vec<(u32, i32)> = profile
            .controls
            .iter()
            .filter_map(|(key, value)| parse_id(key).map(|id| (id, *value)))
            .collect();
        // Auto switches and menus first, so manual values are writable afterwards.
        for pass_integers in [false, true] {
            for &(id, value) in &values {
                let is_integer = cam.query(id).map(|info| info.kind == CtrlKind::Integer).unwrap_or(true);
                if is_integer == pass_integers {
                    let _ = cam.set(id, value);
                }
            }
        }
        if let Some(framing) = profile.framing {
            let _ = cam.set_framing(framing);
        }
        if let (Some(value), true) = (profile.exposure_compensation, self.exposure.borrow().is_some()) {
            let _ = cam.set_exposure_compensation(value);
        }
        if let (Some(group), true) = (profile.group_tracking, cam.can_set_group_tracking()) {
            let _ = cam.set_group_tracking(group);
        }
        if let Some(tracking) = profile.tracking {
            self.set_tracking(tracking);
        }
        self.refresh(&cam);
    }

    fn build_camera_page(self: &Rc<Self>, cam: &Camera) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let identity = cam.identity();

        let info = adw::PreferencesGroup::builder().title(tr("Camera")).build();
        let rows = [
            (tr("Model"), Some(cam.info.name.clone())),
            (tr("Serial number"), identity.serial),
            (tr("Firmware"), identity.firmware),
            (tr("Device"), Some(cam.info.path.display().to_string())),
            (tr("USB ID"), Some(format!("{}:{}", camera::VENDOR_ID, cam.info.product_id))),
        ];
        for (title, value) in rows {
            let row = adw::ActionRow::builder()
                .title(title)
                .subtitle(value.as_deref().unwrap_or(tr("Unknown")))
                .subtitle_selectable(true)
                .css_classes(["property"])
                .build();
            info.add(&row);
        }
        page.add(&info);

        let preview = adw::PreferencesGroup::builder()
            .title(tr("Preview"))
            .description(tr("Only affects the preview and photos in Link Studio. Programs such as Teams or Zoom choose their own resolution."))
            .build();
        let labels: Vec<&str> = PREVIEW_HEIGHTS.iter().map(|(_, l)| tr(l)).collect();
        let current = self.config.borrow().preview_height;
        let resolution = adw::ComboRow::builder()
            .title(tr("Quality"))
            .subtitle(tr("Higher quality uses more processing power"))
            .model(&gtk::StringList::new(&labels))
            .selected(PREVIEW_HEIGHTS.iter().position(|(h, _)| *h == current).unwrap_or(0) as u32)
            .build();
        let autostart = adw::SwitchRow::builder()
            .title(tr("Start Automatically"))
            .subtitle(tr("Start the preview when the camera is connected"))
            .active(self.config.borrow().preview_autostart)
            .build();
        preview.add(&resolution);
        preview.add(&autostart);
        *self.quality_row.borrow_mut() = Some(resolution.clone());
        page.add(&preview);

        let weak = Rc::downgrade(self);
        resolution.connect_selected_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            let Some((height, _)) = PREVIEW_HEIGHTS.get(row.selected() as usize) else { return };
            app.config.borrow_mut().preview_height = *height;
            app.config.borrow().save();
            if app.preview.borrow().is_some() && !app.stream_has_virtual.get() {
                app.restart_stream();
            }
        });
        let weak = Rc::downgrade(self);
        autostart.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            app.config.borrow_mut().preview_autostart = row.is_active();
            app.config.borrow().save();
        });

        let gimbal = adw::PreferencesGroup::builder().title(tr("Gimbal")).build();
        let center_row = adw::ActionRow::builder()
            .title(tr("Center the Camera"))
            .subtitle(tr("Moves the gimbal back to its middle position"))
            .build();
        let center = gtk::Button::builder().label(tr("Center")).valign(gtk::Align::Center).build();
        center_row.add_suffix(&center);
        gimbal.add(&center_row);
        page.add(&gimbal);
        let weak = Rc::downgrade(self);
        center.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.center();
            }
        });
        page
    }

    // ----- Gimbal -----

    /// Moves the gimbal one step. `resync` starts from where the gimbal really
    /// points (it may have been moved by tracking); repeats while a button is
    /// held continue from the last target instead, since the motor lags behind.
    fn nudge(&self, dpan: i32, dtilt: i32, resync: bool) {
        let Some(cam) = self.cam() else { return };
        if self.tracking_on.get() || self.privacy.is_active() {
            return;
        }
        let start = if resync {
            gimbal_position(&cam)
        } else {
            cam.get(cid::PAN_ABSOLUTE).ok().zip(cam.get(cid::TILT_ABSOLUTE).ok())
        };
        let Some((pan, tilt)) = start else { return };
        // Smaller steps when zoomed in, so the movement feels the same on screen.
        let zoom = cam.get(cid::ZOOM_ABSOLUTE).unwrap_or(100).max(100);
        let step = (300 / zoom).max(1) * ARCSEC_PER_DEGREE;
        if dpan != 0 {
            let (min, max) = self.pan_range.get();
            let _ = cam.set(cid::PAN_ABSOLUTE, snap_degree(pan + dpan * step).clamp(min, max));
        }
        if dtilt != 0 {
            let (min, max) = self.tilt_range.get();
            let _ = cam.set(cid::TILT_ABSOLUTE, snap_degree(tilt + dtilt * step).clamp(min, max));
        }
    }

    fn zoom_by(&self, delta: i32) {
        if self.privacy.is_active() {
            return;
        }
        let value = (self.zoom.value() + delta as f64).clamp(self.zoom.adjustment().lower(), self.zoom.adjustment().upper());
        self.zoom.set_value(value);
    }

    fn center(&self) {
        let Some(cam) = self.cam() else { return };
        if self.privacy.is_active() {
            self.leave_privacy();
        }
        if cam.reset_gimbal().is_err() {
            let _ = cam.set(cid::PAN_ABSOLUTE, 0);
            let _ = cam.set(cid::TILT_ABSOLUTE, 0);
        }
    }

    fn set_tracking(self: &Rc<Self>, on: bool) {
        let Some(cam) = self.cam() else { return };
        if on && self.privacy.is_active() {
            self.leave_privacy();
        }
        self.resume_tracking.set(false);
        match cam.set_tracking(on) {
            Ok(()) => {
                self.tracking_on.set(on);
                if on && self.preview.borrow().is_some() && !self.stream_live.get() {
                    // Our stream is still waking the camera, which ignores the
                    // command; send it again once frames arrive.
                    self.resume_tracking.set(true);
                } else if on {
                    self.schedule_tracking_retry();
                }
            }
            Err(err) => self.toast(&trf("Could not change AI tracking: {}", &[&err])),
        }
        self.refresh(&cam);
    }

    /// Makes sure a tracking command took effect: 0x01 means tracking runs,
    /// 0x00 means it was ignored (resend), 0xff means "command received" with
    /// no more detail (keep watching).
    fn schedule_tracking_retry(self: &Rc<Self>) {
        if let Some(source) = self.tracking_retry.borrow_mut().take() {
            source.remove();
        }
        let weak = Rc::downgrade(self);
        let attempts = Cell::new(0);
        let source = glib::timeout_add_local(TRACKING_CHECK, move || {
            let Some(app) = weak.upgrade() else { return glib::ControlFlow::Break };
            let (true, Some(cam)) = (app.tracking_on.get(), app.cam()) else {
                app.tracking_retry.borrow_mut().take();
                return glib::ControlFlow::Break;
            };
            attempts.set(attempts.get() + 1);
            let mode = cam.mode().ok().flatten();
            let done = mode == Some(Mode::Tracking) || attempts.get() >= TRACKING_ATTEMPTS;
            if mode == Some(Mode::Normal) && !done {
                let _ = cam.set_tracking(true);
            }
            if done {
                app.tracking_retry.borrow_mut().take();
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
        *self.tracking_retry.borrow_mut() = Some(source);
    }

    fn set_privacy(&self, on: bool) {
        let Some(cam) = self.cam() else { return };
        if on {
            let (pan, tilt) = gimbal_position(&cam).unwrap_or((0, 0));
            let saved = (snap_degree(pan), snap_degree(tilt), cam.get(cid::ZOOM_ABSOLUTE).unwrap_or(100));
            self.privacy_restore.set(Some(saved));
            self.resume_tracking.set(false);
            if self.tracking_on.get() && cam.set_tracking(false).is_ok() {
                self.tracking_on.set(false);
            }
            let _ = cam.set(cid::ZOOM_ABSOLUTE, 100);
            let _ = cam.set(cid::PAN_ABSOLUTE, 0);
            let _ = cam.set(cid::TILT_ABSOLUTE, self.tilt_range.get().0);
        } else if let Some((pan, tilt, zoom)) = self.privacy_restore.take() {
            let _ = cam.set(cid::PAN_ABSOLUTE, pan);
            let _ = cam.set(cid::TILT_ABSOLUTE, tilt);
            let _ = cam.set(cid::ZOOM_ABSOLUTE, zoom);
        }
        self.refresh(&cam);
    }

    /// Ends privacy mode without moving back, for actions that move the camera anyway.
    fn leave_privacy(&self) {
        self.privacy_restore.set(None);
        self.updating.set(true);
        self.privacy.set_active(false);
        self.updating.set(false);
    }

    // ----- Saved positions -----

    fn rebuild_positions(self: &Rc<Self>) {
        // remove_all() drops the placeholder too, so it is set again each time.
        self.positions_list.remove_all();
        self.positions_list.set_placeholder(Some(
            &gtk::Label::builder()
                .label(tr("No saved positions yet"))
                .css_classes(["dim-label"])
                .margin_top(18)
                .margin_bottom(18)
                .build(),
        ));
        let positions = self.config.borrow().positions.clone();
        for (index, position) in positions.iter().enumerate() {
            let row = adw::ActionRow::builder()
                .title(position.name.as_str())
                .subtitle(position_summary(position))
                .activatable(true)
                .build();
            if index < 9 {
                row.add_prefix(
                    &gtk::Label::builder()
                        .label(format!("{}", index + 1))
                        .width_chars(2)
                        .css_classes(["dim-label", "numeric"])
                        .build(),
                );
            }
            let update = gtk::Button::builder()
                .icon_name("document-save-symbolic")
                .tooltip_text(tr("Replace with the current position"))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            let delete = gtk::Button::builder()
                .icon_name("edit-delete-symbolic")
                .tooltip_text(tr("Delete position"))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            row.add_suffix(&update);
            row.add_suffix(&delete);

            let weak = Rc::downgrade(self);
            row.connect_activated(move |_| {
                if let Some(app) = weak.upgrade() {
                    app.goto_position(index);
                }
            });
            let weak = Rc::downgrade(self);
            update.connect_clicked(move |_| {
                let Some(app) = weak.upgrade() else { return };
                let Some(current) = app.current_position(String::new()) else { return };
                {
                    let mut config = app.config.borrow_mut();
                    if let Some(p) = config.positions.get_mut(index) {
                        p.pan = current.pan;
                        p.tilt = current.tilt;
                        p.zoom = current.zoom;
                    }
                    config.save();
                }
                app.rebuild_positions();
                app.toast(tr("Position updated"));
            });
            let weak = Rc::downgrade(self);
            delete.connect_clicked(move |_| {
                let Some(app) = weak.upgrade() else { return };
                {
                    let mut config = app.config.borrow_mut();
                    if index < config.positions.len() {
                        config.positions.remove(index);
                    }
                    config.save();
                }
                app.rebuild_positions();
            });
            self.positions_list.append(&row);
        }
    }

    fn current_position(&self, name: String) -> Option<Position> {
        let cam = self.cam()?;
        let (pan, tilt) = gimbal_position(&cam)?;
        Some(Position {
            name,
            pan: snap_degree(pan),
            tilt: snap_degree(tilt),
            zoom: cam.get(cid::ZOOM_ABSOLUTE).ok()?,
        })
    }

    fn goto_position(&self, index: usize) {
        let Some(cam) = self.cam() else { return };
        let Some(position) = self.config.borrow().positions.get(index).cloned() else { return };
        self.resume_tracking.set(false);
        if self.tracking_on.get() && cam.set_tracking(false).is_ok() {
            self.tracking_on.set(false);
            self.toast(tr("AI tracking was turned off to move the camera"));
        }
        if self.privacy.is_active() {
            self.leave_privacy();
        }
        let _ = cam.set(cid::ZOOM_ABSOLUTE, position.zoom);
        let _ = cam.set(cid::PAN_ABSOLUTE, position.pan);
        let _ = cam.set(cid::TILT_ABSOLUTE, position.tilt);
        self.refresh(&cam);
    }

    fn ask_save_position(self: &Rc<Self>) {
        if self.cam().is_none() {
            return;
        }
        let count = self.config.borrow().positions.len();
        let default_name = trf("Position {}", &[&(count + 1)]);
        let entry = gtk::Entry::builder().text(default_name.as_str()).activates_default(true).build();
        let dialog = adw::AlertDialog::new(Some(tr("Save Position")), Some(tr("Give the current camera position a name.")));
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", tr("Cancel")), ("save", tr("Save"))]);
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(self);
        let name_entry = entry.clone();
        dialog.connect_response(None, move |_, response| {
            let Some(app) = weak.upgrade() else { return };
            if response != "save" {
                return;
            }
            let name = name_entry.text().trim().to_owned();
            let name = if name.is_empty() { default_name.clone() } else { name };
            let Some(position) = app.current_position(name.clone()) else { return };
            {
                let mut config = app.config.borrow_mut();
                config.positions.push(position);
                config.save();
            }
            app.rebuild_positions();
            app.toast(&trf("“{}” saved", &[&name]));
        });
        dialog.present(Some(&self.window));
        entry.grab_focus();
    }

    // ----- Preview -----

    /// The virtual camera's device, when it is turned on and set up.
    /// The preview runs only while it is switched on and the window is shown.
    fn wants_preview(&self) -> bool {
        self.preview_toggle.is_active() && self.window.is_visible()
    }

    fn virtual_camera_device(&self) -> Option<PathBuf> {
        self.config.borrow().virtual_camera.then(vcam::find_device).flatten()
    }

    /// Starts, restarts or stops the camera stream to match what is wanted:
    /// the preview, the virtual camera, both or neither.
    fn restart_stream(self: &Rc<Self>) {
        let virtual_camera = self.virtual_camera_device();
        if !self.wants_preview() && virtual_camera.is_none() {
            self.stop_preview();
            self.show_preview_page();
            return;
        }
        self.start_stream(virtual_camera);
    }

    fn start_stream(self: &Rc<Self>, virtual_camera: Option<PathBuf>) {
        let Some(cam) = self.cam() else { return };
        // Restarting the stream drops AI tracking in the camera; turn it back
        // on once the new stream delivers frames.
        if self.tracking_on.get() {
            self.resume_tracking.set(true);
        }
        self.stop_preview();
        let (tx, rx) = async_channel::bounded(2);
        let config = self.config.borrow();
        let size = if virtual_camera.is_some() { config.virtual_camera_size() } else { config.preview_size() };
        drop(config);
        self.stream_has_virtual.set(virtual_camera.is_some());
        self.show_preview.store(self.wants_preview(), Ordering::Relaxed);
        let options = preview::Options {
            device: cam.info.path.clone(),
            size,
            mirror: self.mirror.clone(),
            show: self.show_preview.clone(),
            virtual_camera,
        };
        *self.preview.borrow_mut() = Some(Preview::start(options, tx));
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.show_preview_page();

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = rx.recv().await {
                let Some(app) = weak.upgrade() else { break };
                match event {
                    Event::Started => app.stream_started(),
                    Event::Frame(frame) => app.show_frame(frame),
                    Event::Error(message) => app.preview_failed(&message),
                    Event::VirtualCameraError(message) => {
                        app.toast(&trf("The virtual camera stopped: {}", &[&message]));
                        app.virtual_badge.set_visible(false);
                    }
                }
            }
        });
    }

    fn stop_preview(&self) {
        self.preview.borrow_mut().take();
        self.last_jpeg.borrow_mut().take();
        self.stream_has_virtual.set(false);
        self.stream_live.set(false);
        self.virtual_badge.set_visible(false);
    }

    /// Shows the video or the "preview is off" page, and the virtual camera badge.
    fn show_preview_page(&self) {
        let streaming = self.preview.borrow().is_some();
        // While the virtual camera runs, the stream uses its resolution.
        if let Some(row) = self.quality_row.borrow().as_ref() {
            let overridden = streaming && self.stream_has_virtual.get();
            row.set_sensitive(!overridden);
            row.set_subtitle(if overridden {
                tr("Uses the virtual camera's resolution while it is on")
            } else {
                tr("Higher quality uses more processing power")
            });
        }
        self.virtual_badge.set_visible(streaming && self.stream_has_virtual.get());
        self.snapshot_button.set_sensitive(streaming && self.preview_toggle.is_active());
        if streaming && self.preview_toggle.is_active() {
            self.preview_stack.set_visible_child_name("video");
        } else if streaming {
            self.show_preview_status(
                tr("Preview is off"),
                Some(tr("The virtual camera is still running, so other programs keep getting the picture.")),
            );
            self.preview_status_button.set_label(tr("Start Preview"));
        } else {
            self.show_preview_status(tr("Preview is off"), None);
        }
    }

    /// Hides the window instead of closing it, so the virtual camera keeps running.
    fn hide_to_background(&self, window: &adw::ApplicationWindow) {
        window.set_visible(false);
        // Nobody sees the preview now; keep only the virtual camera going.
        self.show_preview.store(false, Ordering::Relaxed);
        if let Some(application) = window.application() {
            let notification = gio::Notification::new(tr("Link Studio is still running"));
            let has_tray = TRAY.with(|tray| tray.borrow().is_some());
            notification.set_body(Some(if has_tray {
                tr("The virtual camera stays on. Use the Link Studio status icon to show the window, turn the virtual camera off or quit.")
            } else {
                tr("The virtual camera stays on. Open Link Studio again to turn it off, or choose Quit in its menu.")
            }));
            application.send_notification(Some("background"), &notification);
        }
    }

    /// Closes the window for real, also while the virtual camera is running.
    fn force_close(&self) {
        self.stop_preview();
        self.window.close();
    }

    /// The camera is streaming again: turn back on tracking it dropped when
    /// the previous stream stopped. This also runs when no preview is shown.
    fn stream_started(self: &Rc<Self>) {
        self.stream_live.set(true);
        if !self.resume_tracking.take() {
            return;
        }
        if let Some(cam) = self.cam() {
            match cam.set_tracking(true) {
                Ok(()) => self.schedule_tracking_retry(),
                Err(_) => self.tracking_on.set(false),
            }
        }
    }

    fn show_frame(&self, frame: Frame) {
        if self.preview.borrow().is_none() {
            return;
        }
        let bytes = glib::Bytes::from_owned(frame.rgb);
        let texture = gdk::MemoryTexture::new(
            frame.width,
            frame.height,
            gdk::MemoryFormat::R8g8b8,
            &bytes,
            frame.width as usize * 3,
        );
        self.picture.set_paintable(Some(&texture));
        *self.last_jpeg.borrow_mut() = Some(frame.jpeg);
    }

    fn preview_failed(&self, message: &str) {
        let had_virtual = self.stream_has_virtual.get();
        self.stop_preview();
        if had_virtual {
            self.toast(tr("The virtual camera stopped"));
        }
        self.resume_tracking.set(false);
        self.updating.set(true);
        self.preview_toggle.set_active(false);
        self.updating.set(false);
        self.show_preview_status(tr("The preview stopped"), Some(message));
    }

    fn show_preview_status(&self, title: &str, description: Option<&str>) {
        self.preview_status.set_title(title);
        self.preview_status.set_description(Some(description.unwrap_or(tr(
            "Other programs, such as Teams or Zoom, can use the camera while the preview is off.",
        ))));
        self.preview_status_button
            .set_label(if description.is_some() { tr("Try Again") } else { tr("Start Preview") });
        self.preview_stack.set_visible_child_name("status");
    }

    fn snapshot(&self) {
        let Some(jpeg) = self.last_jpeg.borrow().clone() else {
            self.toast(tr("Start the preview to take a photo"));
            return;
        };
        let dir = glib::user_special_dir(glib::UserDirectory::Pictures)
            .unwrap_or_else(glib::home_dir)
            .join("Link Studio");
        let stamp = glib::DateTime::now_local()
            .and_then(|now| now.format("%Y-%m-%d %H.%M.%S"))
            .map(|s| s.to_string())
            .unwrap_or_else(|_| "photo".into());
        let path = dir.join(format!("Link Studio {stamp}.jpg"));
        let result = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &jpeg));
        match result {
            Ok(()) => {
                let toast = adw::Toast::builder().title(tr("Photo saved")).button_label(tr("Open")).build();
                let window = self.window.clone();
                toast.connect_button_clicked(move |_| {
                    gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).launch(
                        Some(&window),
                        None::<&gio::Cancellable>,
                        |_| {},
                    );
                });
                self.toasts.add_toast(toast);
            }
            Err(err) => self.toast(&trf("Could not save the photo: {}", &[&err])),
        }
    }

    // ----- Dialogs -----

    fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Link Studio")
            .application_icon(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments(tr("Control the Insta360 Link webcam on Linux: image, gimbal, AI tracking and profiles."))
            .license_type(gtk::License::Gpl30)
            .build();
        // Translators: put your name(s) here to be credited in the About dialog.
        let credits = tr("translator-credits");
        if credits != "translator-credits" {
            about.set_translator_credits(credits);
        }
        about.add_credit_section(
            Some(tr("Camera command mapping")),
            &["vrwallace https://github.com/vrwallace/Insta360-Link-1-and-2-Controller-for-Linux"],
        );
        about.present(Some(&self.window));
    }

    fn show_preferences(self: &Rc<Self>) {
        let dialog = adw::PreferencesDialog::new();
        let page = adw::PreferencesPage::builder()
            .title(tr("General"))
            .icon_name("preferences-system-symbolic")
            .build();

        let look = adw::PreferencesGroup::builder().title(tr("Appearance")).build();
        let current = self.config.borrow().appearance;
        let style = adw::ComboRow::builder()
            .title(tr("Style"))
            .model(&gtk::StringList::new(&[tr("Follow system"), tr("Light"), tr("Dark")]))
            .selected(APPEARANCES.iter().position(|a| *a == current).unwrap_or(0) as u32)
            .build();
        look.add(&style);
        page.add(&look);

        let languages = i18n::available_languages();
        let mut labels = vec![tr("System default")];
        labels.extend(languages.iter().map(|(_, name)| *name));
        let current = self.config.borrow().language.clone();
        let selected = current
            .as_deref()
            .and_then(|code| languages.iter().position(|(c, _)| *c == code))
            .map_or(0, |i| i + 1);
        let language_group = adw::PreferencesGroup::builder()
            .title(tr("Language"))
            .description(tr("“System default” uses the language of your desktop."))
            .build();
        let language = adw::ComboRow::builder()
            .title(tr("Language"))
            .model(&gtk::StringList::new(&labels))
            .selected(selected as u32)
            .build();
        language_group.add(&language);
        page.add(&language_group);
        dialog.add(&page);

        let weak = Rc::downgrade(self);
        style.connect_selected_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            let appearance = APPEARANCES.get(row.selected() as usize).copied().unwrap_or_default();
            app.config.borrow_mut().appearance = appearance;
            app.config.borrow().save();
            apply_appearance(appearance);
        });
        let weak = Rc::downgrade(self);
        let prefs = dialog.clone();
        language.connect_selected_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            let code = match row.selected() as usize {
                0 => None,
                i => languages.get(i - 1).map(|(code, _)| *code),
            };
            app.config.borrow_mut().language = code.map(str::to_owned);
            app.config.borrow().save();
            i18n::set_language(code);
            prefs.close();
            app.rebuild_window();
        });
        dialog.present(Some(&self.window));
    }

    /// Recreates the window so every label picks up a new language.
    fn rebuild_window(self: &Rc<Self>) {
        let Some(application) = self.window.application().and_downcast::<adw::Application>() else {
            return;
        };
        let carry = Carry {
            size: (self.window.width(), self.window.height()),
            maximized: self.window.is_maximized(),
            tracking: self.tracking_on.get(),
            page: self
                .sidebar
                .content()
                .and_downcast::<adw::ViewStack>()
                .and_then(|stack| stack.visible_child_name()),
        };
        // Keep the application running between closing this window (which
        // releases the camera stream) and opening the new one.
        let _hold = application.hold();
        self.force_close();
        App::new(&application, Some(carry)).present();
    }

    fn ask_uninstall(self: &Rc<Self>) {
        let dialog = adw::AlertDialog::new(
            Some(tr("Uninstall Link Studio?")),
            Some(tr("The program and its entry in the app menu will be removed. Photos you have taken stay in your Pictures folder.")),
        );
        let purge = gtk::CheckButton::builder()
            .label(tr("Also delete saved positions, profiles and settings"))
            .build();
        let remove_virtual = gtk::CheckButton::builder()
            .label(tr("Also remove the virtual camera setup (needs the administrator password)"))
            .active(true)
            .visible(vcam::is_set_up())
            .build();
        let options = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
        options.append(&purge);
        options.append(&remove_virtual);
        dialog.set_extra_child(Some(&options));
        dialog.add_responses(&[("cancel", tr("Cancel")), ("uninstall", tr("Uninstall"))]);
        dialog.set_response_appearance("uninstall", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            let Some(app) = weak.upgrade() else { return };
            if response != "uninstall" {
                return;
            }
            let purge = purge.is_active();
            if remove_virtual.is_visible() && remove_virtual.is_active() {
                // The driver can only be unloaded once nothing writes to it.
                app.set_virtual_camera(false);
                let weak = Rc::downgrade(&app);
                vcam::run_privileged(true, |_| {}, move |result| {
                    let Some(app) = weak.upgrade() else { return };
                    if let Err(vcam::SetupError::Failed(message)) = result {
                        app.toast(&trf("Could not remove the virtual camera setup: {}", &[&message]));
                    }
                    app.finish_uninstall(purge);
                });
            } else {
                app.finish_uninstall(purge);
            }
        });
        dialog.present(Some(&self.window));
    }

    fn finish_uninstall(self: &Rc<Self>, purge: bool) {
        if let Err(err) = crate::uninstall::uninstall(purge) {
            self.toast(&trf("Uninstalling failed: {}", &[&err]));
            return;
        }
        let body = if purge {
            tr("Link Studio and all its settings have been removed. The window will now close.")
        } else {
            tr("Link Studio has been removed. The window will now close.")
        };
        let done = adw::AlertDialog::new(Some(tr("Uninstalled")), Some(body));
        done.add_response("close", tr("Close"));
        let weak = Rc::downgrade(self);
        done.connect_response(None, move |_, _| {
            if let Some(app) = weak.upgrade() {
                app.force_close();
            }
        });
        done.present(Some(&self.window));
    }

    fn show_shortcuts(&self) {
        let shortcuts = [
            (tr("Ctrl + arrow keys"), tr("Move the camera")),
            (tr("Ctrl + plus / minus"), tr("Zoom in / out")),
            ("Ctrl + Home", tr("Center the camera")),
            ("Ctrl + 1–9", tr("Go to saved position")),
            ("Ctrl + T", tr("AI tracking on/off")),
            ("Ctrl + P", tr("Preview on/off")),
            ("Ctrl + S", tr("Take photo")),
            ("F9", tr("Show/hide settings")),
            ("Ctrl + Q", tr("Quit")),
        ];
        let text: Vec<String> = shortcuts.iter().map(|(keys, what)| format!("{keys}\t{what}")).collect();
        let dialog = adw::AlertDialog::new(Some(tr("Keyboard Shortcuts")), Some(&text.join("\n")));
        dialog.add_response("close", tr("Close"));
        dialog.present(Some(&self.window));
    }
}
