//! Saved positions: the tiles under the preview and their menu.

use super::*;
use crate::debuglog::debug;

impl App {
    pub(super) fn rebuild_positions(self: &Rc<Self>) {
        self.sync_tray();
        self.positions_list.remove_all();
        let positions = self.config.borrow().positions.clone();
        self.positions_empty.set_visible(positions.is_empty());
        for (index, position) in positions.iter().enumerate() {
            let tile = self.position_tile(index, position);
            self.positions_list.append(&tile);
            // The button in the tile takes the focus, not the box around it.
            if let Some(child) = tile.parent() {
                child.set_focusable(false);
            }
        }
    }

    /// A small map of where a position points: the frame shrinks as the zoom grows.
    pub(super) fn position_map(&self, position: &Position) -> gtk::Fixed {
        const SIZE: i32 = 34;
        let (pan_min, pan_max) = self.pan_range.get();
        let (tilt_min, tilt_max) = self.tilt_range.get();
        let share = |value: i32, min: i32, max: i32| match max > min {
            true => ((value - min) as f64 / (max - min) as f64).clamp(0.0, 1.0),
            false => 0.5,
        };
        let width = (18.0 * 100.0 / position.zoom.max(100) as f64).max(6.0);
        let height = width * 9.0 / 16.0;
        let x = share(position.pan, pan_min, pan_max) * (SIZE as f64 - width);
        let y = (1.0 - share(position.tilt, tilt_min, tilt_max)) * (SIZE as f64 - height);
        let map = gtk::Fixed::builder().width_request(SIZE).height_request(SIZE).valign(gtk::Align::Center).css_classes(["position-map"]).build();
        let marker = gtk::Box::builder().width_request(width as i32).height_request(height as i32).css_classes(["position-marker"]).build();
        map.put(&marker, x, y);
        map
    }

    pub(super) fn position_tile(self: &Rc<Self>, index: usize, position: &Position) -> gtk::Button {
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).valign(gtk::Align::Center).build();
        text.append(
            &gtk::Label::builder()
                .label(position.name.as_str())
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["heading"])
                .build(),
        );
        text.append(
            &gtk::Label::builder()
                .label(format!("{}×", zoom_factor(position.zoom)))
                .xalign(0.0)
                .css_classes(["dim-label", "caption", "numeric"])
                .build(),
        );
        let inner = gtk::Box::builder().spacing(6).build();
        inner.append(&self.position_map(position));
        inner.append(&text);
        let tooltip = format!(
            "{}\n{}",
            position_summary(position),
            tr("Click to move the camera there. Right-click to replace or delete.")
        );
        let tile = gtk::Button::builder()
            .child(&inner)
            .tooltip_text(tooltip)
            .css_classes(["flat", "position-tile"])
            .build();
        let weak = Rc::downgrade(self);
        tile.connect_clicked(move |_| {
            if let Some(app) = weak.upgrade() {
                app.goto_position(index);
            }
        });
        self.attach_position_menu(&tile, index);
        tile
    }

    /// Right-click, a long press or the Menu key opens replace and delete.
    pub(super) fn attach_position_menu(self: &Rc<Self>, tile: &gtk::Button, index: usize) {
        let actions = gio::SimpleActionGroup::new();
        let replace = gio::SimpleAction::new("replace", None);
        let weak = Rc::downgrade(self);
        replace.connect_activate(move |_, _| {
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
            app.rebuild_positions_later();
            app.toast(tr("Position updated"));
        });
        actions.add_action(&replace);
        let delete = gio::SimpleAction::new("delete", None);
        let weak = Rc::downgrade(self);
        delete.connect_activate(move |_, _| {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut config = app.config.borrow_mut();
                if index < config.positions.len() {
                    config.positions.remove(index);
                }
                config.save();
            }
            app.rebuild_positions_later();
        });
        actions.add_action(&delete);
        let rename = gio::SimpleAction::new("rename", None);
        let weak = Rc::downgrade(self);
        rename.connect_activate(move |_, _| {
            if let Some(app) = weak.upgrade() {
                app.ask_rename_position(index);
            }
        });
        actions.add_action(&rename);
        // The order is also the order of Ctrl+1–9 and of the status icon menu.
        let count = self.config.borrow().positions.len();
        for (name, other, possible) in [("earlier", index.wrapping_sub(1), index > 0), ("later", index + 1, index + 1 < count)] {
            let action = gio::SimpleAction::new(name, None);
            action.set_enabled(possible);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                let Some(app) = weak.upgrade() else { return };
                {
                    let mut config = app.config.borrow_mut();
                    if index < config.positions.len() && other < config.positions.len() {
                        config.positions.swap(index, other);
                    }
                    config.save();
                }
                app.rebuild_positions_later();
            });
            actions.add_action(&action);
        }
        tile.insert_action_group("position", Some(&actions));

        let menu = gio::Menu::new();
        let edit = gio::Menu::new();
        edit.append(Some(tr("Rename…")), Some("position.rename"));
        edit.append(Some(tr("Replace with the current position")), Some("position.replace"));
        menu.append_section(None, &edit);
        let order = gio::Menu::new();
        order.append(Some(tr("Move earlier")), Some("position.earlier"));
        order.append(Some(tr("Move later")), Some("position.later"));
        menu.append_section(None, &order);
        let remove = gio::Menu::new();
        remove.append(Some(tr("Delete position")), Some("position.delete"));
        menu.append_section(None, &remove);
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(tile);
        popover.set_has_arrow(false);
        let parent = popover.clone();
        tile.connect_destroy(move |_| parent.unparent());

        let show = {
            let popover = popover.clone();
            move |x: f64, y: f64| {
                popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
                popover.popup();
            }
        };
        let right_click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let open = show.clone();
        right_click.connect_pressed(move |_, _, x, y| open(x, y));
        tile.add_controller(right_click);
        let long_press = gtk::GestureLongPress::new();
        let open = show.clone();
        long_press.connect_pressed(move |_, x, y| open(x, y));
        tile.add_controller(long_press);
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let shift_f10 = key == gdk::Key::F10 && modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            if key != gdk::Key::Menu && !shift_f10 {
                return glib::Propagation::Proceed;
            }
            show(0.0, 0.0);
            glib::Propagation::Stop
        });
        tile.add_controller(keys);
    }

    /// The menu that asks for this belongs to a tile that is about to go.
    pub(super) fn rebuild_positions_later(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(app) = weak.upgrade() {
                app.rebuild_positions();
            }
        });
    }

    pub(super) fn current_position(&self, name: String) -> Option<Position> {
        let cam = self.cam()?;
        let (pan, tilt) = gimbal_position(&cam)?;
        Some(Position {
            name,
            pan: snap_degree(pan),
            tilt: snap_degree(tilt),
            zoom: cam.get(cid::ZOOM_ABSOLUTE).ok()?,
        })
    }

    pub(super) fn goto_position(&self, index: usize) {
        let Some(cam) = self.cam() else { return };
        let Some(position) = self.config.borrow().positions.get(index).cloned() else { return };
        debug!("positions: go to “{}” (pan {}, tilt {}, zoom {})", position.name, position.pan, position.tilt, position.zoom);
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

    pub(super) fn ask_rename_position(self: &Rc<Self>, index: usize) {
        let Some(current) = self.config.borrow().positions.get(index).map(|p| p.name.clone()) else { return };
        let entry = gtk::Entry::builder().text(current.as_str()).activates_default(true).build();
        let dialog = adw::AlertDialog::new(Some(tr("Rename Position")), None);
        dialog.set_extra_child(Some(&entry));
        dialog.add_responses(&[("cancel", tr("Cancel")), ("rename", tr("Rename"))]);
        dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("rename"));
        dialog.set_close_response("cancel");

        let weak = Rc::downgrade(self);
        let name_entry = entry.clone();
        dialog.connect_response(None, move |_, response| {
            let Some(app) = weak.upgrade() else { return };
            let name = name_entry.text().trim().to_owned();
            if response != "rename" || name.is_empty() {
                return;
            }
            {
                let mut config = app.config.borrow_mut();
                // Only if it is still the same position (Claude may have changed the list).
                match config.positions.get_mut(index).filter(|p| p.name == current) {
                    Some(position) => position.name = name,
                    None => return,
                }
                config.save();
            }
            app.rebuild_positions();
        });
        dialog.present(Some(&self.window));
        entry.grab_focus();
    }

    pub(super) fn ask_save_position(self: &Rc<Self>) {
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
}
