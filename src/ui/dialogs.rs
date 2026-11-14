//! About, Preferences, uninstalling and the shortcut list.

use super::*;

impl App {
    pub(super) fn show_about(&self) {
        let about = adw::AboutDialog::builder()
            .application_name("Link Studio")
            .application_icon(APP_ID)
            .version(env!("CARGO_PKG_VERSION"))
            .comments(tr("Control the Insta360 Link webcam on Linux: image, gimbal, AI tracking and profiles."))
            .developer_name("From Norway with Love ❤️")
            .developers(["Jan Ivar Sørensen"])
            .copyright("© 2026 Jan Ivar Sørensen")
            .website("https://github.com/Vindihaaret/linkstudio")
            .issue_url("https://github.com/Vindihaaret/linkstudio/issues")
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

    pub(super) fn show_preferences(self: &Rc<Self>) -> adw::PreferencesDialog {
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

        let behaviour = adw::PreferencesGroup::builder().title(tr("Window")).build();
        let tray_available = has_tray();
        let on_close = adw::SwitchRow::builder()
            .title(tr("Keep in the status icon when the window is closed"))
            .active(self.config.borrow().close_to_tray && tray_available)
            .sensitive(tray_available)
            .build();
        on_close.set_subtitle(if tray_available {
            tr("Instead of quitting. Link Studio stays ready to use, and other programs can use the camera.")
        } else {
            tr("Needs a status icon area. On GNOME, install the AppIndicator extension.")
        });
        let login = adw::SwitchRow::builder()
            .title(tr("Start at login"))
            .subtitle(tr("Starts in the status icon, without opening the window"))
            .active(crate::autostart::is_enabled())
            .build();
        behaviour.add(&on_close);
        behaviour.add(&login);
        page.add(&behaviour);

        let privacy = adw::PreferencesGroup::builder().title(tr("Privacy")).build();
        let on_lock = adw::SwitchRow::builder()
            .title(tr("Privacy mode when the screen is locked"))
            .subtitle(tr("Points the camera down when you lock the screen, and back again when you unlock it"))
            .active(self.config.borrow().privacy_on_lock)
            .build();
        privacy.add(&on_lock);
        page.add(&privacy);
        let weak = Rc::downgrade(self);
        on_lock.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            app.config.borrow_mut().privacy_on_lock = row.is_active();
            app.config.borrow().save();
        });

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
        page.add(&self.update_preferences_group());

        // Uninstalling lives at the bottom here, not in the main menu, so it is a deliberate choice.
        let removal = adw::PreferencesGroup::new();
        let uninstall_row = adw::ActionRow::builder()
            .title(tr("Uninstall Link Studio"))
            .subtitle(tr("Removes the program and its menu entry, and optionally the settings and the virtual camera setup."))
            .build();
        let uninstall = gtk::Button::builder()
            .label(tr("Uninstall"))
            .valign(gtk::Align::Center)
            .css_classes(["destructive-action"])
            .build();
        uninstall_row.add_suffix(&uninstall);
        removal.add(&uninstall_row);
        page.add(&removal);
        dialog.add(&page);
        let weak = Rc::downgrade(self);
        let prefs = dialog.clone();
        uninstall.connect_clicked(move |_| {
            prefs.close();
            if let Some(app) = weak.upgrade() {
                app.ask_uninstall();
            }
        });
        dialog.add(&self.claude_preferences_page(&dialog));

        let weak = Rc::downgrade(self);
        on_close.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            app.config.borrow_mut().close_to_tray = row.is_active();
            app.config.borrow().save();
        });
        let weak = Rc::downgrade(self);
        login.connect_active_notify(move |row| {
            let Some(app) = weak.upgrade() else { return };
            if let Err(err) = crate::autostart::set_enabled(row.is_active()) {
                app.toast(&trf("Could not change start at login: {}", &[&err]));
            }
        });
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
        dialog
    }

    /// Recreates the window so every label picks up a new language.
    pub(super) fn rebuild_window(self: &Rc<Self>) {
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

    pub(super) fn ask_uninstall(self: &Rc<Self>) {
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

    pub(super) fn finish_uninstall(self: &Rc<Self>, purge: bool) {
        unregister_from_claude_code();
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

    pub(super) fn show_shortcuts(&self) {
        let shortcuts = [
            (tr("Ctrl + arrow keys"), tr("Move the camera")),
            (tr("Ctrl + plus / minus"), tr("Zoom in / out")),
            ("Ctrl + Home", tr("Center the camera")),
            ("Ctrl + 1–9", tr("Go to saved position")),
            ("Ctrl + T", tr("AI tracking on/off")),
            ("Ctrl + P", tr("Preview on/off")),
            ("Ctrl + S", tr("Take photo")),
            ("F9", tr("Show/hide settings")),
            ("F11", tr("Fullscreen preview")),
            ("Ctrl + Q", tr("Quit")),
        ];
        let text: Vec<String> = shortcuts.iter().map(|(keys, what)| format!("{keys}\t{what}")).collect();
        let dialog = adw::AlertDialog::new(Some(tr("Keyboard Shortcuts")), Some(&text.join("\n")));
        dialog.add_response("close", tr("Close"));
        dialog.present(Some(&self.window));
    }
}
