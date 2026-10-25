//! Status icon (StatusNotifierItem): shown by KDE Plasma, and by GNOME with
//! the AppIndicator extension. It runs on its own thread and talks to the UI
//! through a channel, so it outlives window rebuilds.

use gtk::glib;
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{CheckmarkItem, StandardItem};
use ksni::{MenuItem, ToolTip};

use crate::i18n::tr;
use crate::APP_ID;

pub enum Command {
    Show,
    ToggleVirtualCamera,
    ToggleTracking,
    Quit,
}

/// What the icon shows; the UI pushes changes with `Handle::update`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct State {
    pub connected: bool,
    pub virtual_available: bool,
    pub virtual_on: bool,
    pub tracking_on: bool,
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
                enabled: self.state.connected,
                activate: Box::new(|this: &mut Self| this.send(Command::ToggleTracking)),
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
