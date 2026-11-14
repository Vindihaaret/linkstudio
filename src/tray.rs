//! Status icon (StatusNotifierItem): shown by KDE Plasma, and by GNOME with
//! the AppIndicator extension. It runs on its own thread and talks to the UI
//! through a channel, so it outlives window rebuilds.

use gtk::glib;
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
use ksni::{MenuItem, ToolTip};

use crate::i18n::tr;
use crate::APP_ID;

pub enum Command {
    Show,
    ToggleVirtualCamera,
    ToggleTracking,
    TogglePrivacy,
    /// Index into the saved positions.
    GotoPosition(usize),
    /// Index into the saved profiles.
    ApplyProfile(usize),
    Quit,
}

/// What the icon shows; the UI pushes changes with `Handle::update`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct State {
    pub connected: bool,
    pub virtual_available: bool,
    pub virtual_on: bool,
    pub tracking_on: bool,
    /// The camera only tracks while it streams (preview or virtual camera).
    pub streaming: bool,
    pub privacy: bool,
    /// Names of the saved positions and profiles, in the app's order.
    pub positions: Vec<String>,
    pub profiles: Vec<String>,
}

/// A menu label shows a single underscore as an access key, so double it.
fn label(name: &str) -> String {
    name.replace('_', "__")
}

/// A submenu with one entry per saved name; greyed out when there are none
/// or the camera is not connected.
fn saved_menu(title: &str, names: &[String], connected: bool, command: fn(usize) -> Command) -> MenuItem<LinkTray> {
    let items = names
        .iter()
        .enumerate()
        .map(|(index, name)| {
            StandardItem {
                label: label(name),
                activate: Box::new(move |this: &mut LinkTray| this.send(command(index))),
                ..Default::default()
            }
            .into()
        })
        .collect();
    SubMenu { label: title.into(), enabled: connected && !names.is_empty(), submenu: items, ..Default::default() }.into()
}

pub struct LinkTray {
    tx: async_channel::Sender<Command>,
    icon: String,
    pub state: State,
}

impl LinkTray {
    fn send(&self, command: Command) {
        let _ = self.tx.send_blocking(command);
    }
}

impl ksni::Tray for LinkTray {
    fn id(&self) -> String {
        "linkstudio".into()
    }

    fn title(&self) -> String {
        "Link Studio".into()
    }

    fn icon_name(&self) -> String {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if !self.state.connected {
            tr("No Insta360 Link found")
        } else if self.state.virtual_on {
            tr("Virtual camera is on")
        } else {
            tr("Camera connected")
        };
        ToolTip { title: "Link Studio".into(), description: description.into(), ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(Command::Show);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            StandardItem {
                label: tr("Show Link Studio").into(),
                activate: Box::new(|this: &mut Self| this.send(Command::Show)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            saved_menu(tr("Saved Positions"), &self.state.positions, self.state.connected, Command::GotoPosition),
            saved_menu(tr("Profiles"), &self.state.profiles, self.state.connected, Command::ApplyProfile),
            MenuItem::Separator,
            CheckmarkItem {
                label: tr("Virtual camera").into(),
                checked: self.state.virtual_on,
                enabled: self.state.connected && self.state.virtual_available,
                activate: Box::new(|this: &mut Self| this.send(Command::ToggleVirtualCamera)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: tr("AI tracking").into(),
                checked: self.state.tracking_on,
                enabled: self.state.connected && self.state.streaming,
                activate: Box::new(|this: &mut Self| this.send(Command::ToggleTracking)),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: tr("Privacy").into(),
                checked: self.state.privacy,
                enabled: self.state.connected,
                activate: Box::new(|this: &mut Self| this.send(Command::TogglePrivacy)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: tr("Quit").into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|this: &mut Self| this.send(Command::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// The installed symbolic app icon, or a stock camera icon when running from
/// the build directory.
fn icon_name() -> String {
    let name = format!("{APP_ID}-symbolic");
    let installed = [glib::user_data_dir(), "/usr/share".into(), "/usr/local/share".into()]
        .iter()
        .any(|dir| dir.join(format!("icons/hicolor/symbolic/apps/{name}.svg")).exists());
    if installed { name } else { "camera-web-symbolic".into() }
}

/// Starts the status icon. Returns `None` when the desktop has no tray
/// (for example GNOME without the AppIndicator extension).
pub fn spawn(tx: async_channel::Sender<Command>) -> Option<Handle<LinkTray>> {
    let tray = LinkTray { tx, icon: icon_name(), state: State::default() };
    tray.spawn().ok()
}
