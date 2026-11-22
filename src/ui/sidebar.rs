//! The settings sidebar: image, tracking, effects (virtual camera), profiles
//! and camera pages.

use super::*;
use crate::debuglog::debug;

impl App {
    pub(super) fn build_sidebar(self: &Rc<Self>, cam: &Rc<Camera>) {
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

    pub(super) fn build_image_page(self: &Rc<Self>, cam: &Camera) -> adw::PreferencesPage {
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
            let group = icon_group(tr(section.title), section.icon, section.description.map(tr));
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
            let group = icon_group(tr("Other Settings"), "emblem-system-symbolic", None);
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

    pub(super) fn control_row(self: &Rc<Self>, info: &CtrlInfo) -> gtk::Widget {
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
                show_reset_when_changed(&scale, &reset, default);
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

    pub(super) fn exposure_compensation_row(self: &Rc<Self>, cam: &Camera) -> Option<adw::ActionRow> {
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
        show_reset_when_changed(&scale, &reset, 0.0);
        let target = scale.clone();
        reset.connect_clicked(move |_| target.set_value(0.0));
        *self.exposure.borrow_mut() = Some(scale);
        Some(row)
    }

    pub(super) fn set_control(&self, id: u32, value: i32) {
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

    pub(super) fn reset_image_settings(&self) {
        let Some(cam) = self.cam() else { return };
        // Auto switches first, so the manual values they unlock can be written.
        for pass_integers in [false, true] {
            for bound in self.bound.borrow().iter() {
                let info = bound.info();
                if (info.kind == CtrlKind::Integer) == pass_integers {
                    if let Some(value) = reset_value(info) {
                        let _ = cam.set(info.id, value);
                    }
                }
            }
        }
        if self.exposure.borrow().is_some() {
            let _ = cam.set_exposure_compensation(0);
        }
        self.refresh(&cam);
        self.toast(tr("Image settings reset"));
    }

    pub(super) fn build_tracking_page(self: &Rc<Self>, cam: &Camera) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();

        let group = icon_group(
            tr("AI Tracking"),
            "emoji-people-symbolic",
            Some(tr("The camera uses its gimbal to keep you in the picture automatically. It turns tracking off by itself after 60 seconds with no one in the picture.")),
        );
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

        let gestures = icon_group(
            tr("Hand Gestures"),
            "input-touchpad-symbolic",
            Some(tr("The camera recognizes these gestures by itself. They are always on and cannot be turned off from Linux. Keep your face in the picture and do not cover it with your hand.")),
        );
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

    pub(super) fn build_effects_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let group = icon_group(
            tr("Virtual Camera"),
            "camera-video-symbolic",
            Some(tr("Sends the picture from Link Studio to other programs, such as Teams, Zoom or the browser, as “Link Studio Camera”. The preview and the meeting can then run at the same time.")),
        );

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
    pub(super) fn update_virtual_rows(&self) {
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

    pub(super) fn run_virtual_camera_setup(self: &Rc<Self>, remove: bool) {
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
    pub(super) fn set_virtual_camera(self: &Rc<Self>, on: bool) {
        debug!("virtual camera: {}", if on { "on" } else { "off" });
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

    pub(super) fn build_profiles_page(self: &Rc<Self>) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let group = icon_group(
            tr("Profiles"),
            "user-bookmarks-symbolic",
            Some(tr("A profile stores the image and tracking settings. Star a profile to apply it automatically every time the camera is connected.")),
        );
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

    pub(super) fn rebuild_profiles(self: &Rc<Self>) {
        self.sync_tray();
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
            let update = gtk::Button::builder()
                .icon_name("document-save-symbolic")
                .tooltip_text(tr("Replace with the current settings"))
                .valign(gtk::Align::Center)
                .css_classes(["flat", "circular"])
                .build();
            row.add_suffix(&star);
            row.add_suffix(&update);
            row.add_suffix(&delete);

            let name = profile.name.clone();
            let weak = Rc::downgrade(self);
            update.connect_clicked(move |_| {
                if let Some(app) = weak.upgrade() {
                    app.save_profile(&name);
                }
            });

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

    pub(super) fn save_profile(self: &Rc<Self>, name: &str) {
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

    pub(super) fn apply_profile(self: &Rc<Self>, profile: &Profile) {
        debug!("profiles: apply “{}”", profile.name);
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
        // Turning tracking on would end privacy mode; a profile must not do that.
        if let Some(tracking) = profile.tracking.filter(|on| !(*on && self.privacy.is_active())) {
            self.set_tracking(tracking);
        }
        self.refresh(&cam);
    }

    pub(super) fn build_camera_page(self: &Rc<Self>, cam: &Camera) -> adw::PreferencesPage {
        let page = adw::PreferencesPage::new();
        let identity = cam.identity();

        let info = icon_group(tr("Camera"), "camera-web-symbolic", None);
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

        let preview = icon_group(
            tr("Preview"),
            "image-x-generic-symbolic",
            Some(tr("Only affects the preview and photos in Link Studio. Programs such as Teams or Zoom choose their own resolution.")),
        );
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

        let gimbal = icon_group(tr("Gimbal"), "find-location-symbolic", None);
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
}
