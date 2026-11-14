//! The camera stream: preview, virtual camera, retries and photos.

use super::*;

impl App {
    /// The preview runs only while it is switched on and the window is shown.
    pub(super) fn wants_preview(&self) -> bool {
        self.preview_toggle.is_active() && self.window.is_visible()
    }

    /// The virtual camera's device, when it is turned on and set up.
    pub(super) fn virtual_camera_device(&self) -> Option<PathBuf> {
        self.config.borrow().virtual_camera.then(vcam::find_device).flatten()
    }

    /// Starts, restarts or stops the camera stream to match what is wanted:
    /// the preview, the virtual camera, both or neither.
    pub(super) fn restart_stream(self: &Rc<Self>) {
        self.cancel_stream_retry();
        self.stream_waiting.borrow_mut().take();
        let virtual_camera = self.virtual_camera_device();
        if !self.wants_preview() && virtual_camera.is_none() {
            self.stop_preview();
            self.show_preview_page();
            return;
        }
        self.start_stream(virtual_camera);
    }

    pub(super) fn start_stream(self: &Rc<Self>, virtual_camera: Option<PathBuf>) {
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
        self.stream_height.set(size.1);
        self.show_preview.store(self.wants_preview(), Ordering::Relaxed);
        let options = preview::Options {
            device: cam.info.path.clone(),
            size,
            mirror: self.mirror.clone(),
            show: self.show_preview.clone(),
            virtual_camera,
            snapshot: self.snapshot_request.clone(),
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
                    Event::Snapshot { jpeg, width, height } => app.claude_snapshot_ready(jpeg, width, height),
                    Event::Frame(frame) => app.show_frame(frame),
                    Event::Error { message, retry } => app.preview_failed(&message, retry),
                    Event::VirtualCameraError(message) => {
                        app.toast(&trf("The virtual camera stopped: {}", &[&message]));
                        app.virtual_badge.set_visible(false);
                    }
                }
            }
        });
    }

    pub(super) fn stop_preview(&self) {
        self.preview.borrow_mut().take();
        self.last_jpeg.borrow_mut().take();
        self.stream_has_virtual.set(false);
        self.stream_live.set(false);
        self.virtual_badge.set_visible(false);
        self.stream_info.set_visible(false);
    }

    /// Shows the video or the "preview is off" page, and the virtual camera badge.
    pub(super) fn show_preview_page(&self) {
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
        if let Some((title, description)) = self.stream_waiting.borrow().as_ref() {
            self.show_preview_status(title, Some(description));
        } else if streaming && self.preview_toggle.is_active() {
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
    pub(super) fn hide_to_background(self: &Rc<Self>, window: &adw::ApplicationWindow) {
        window.set_visible(false);
        // Nobody sees the preview now; keep only the virtual camera going.
        self.show_preview.store(false, Ordering::Relaxed);
        if !self.stream_has_virtual.get() {
            // Release the camera so other programs can use it directly.
            self.restart_stream();
            if self.hidden_notice_shown.replace(true) {
                return;
            }
        }
        if let Some(application) = window.application() {
            let notification = gio::Notification::new(tr("Link Studio is still running"));
            let has_tray = has_tray();
            notification.set_body(Some(if !self.stream_has_virtual.get() {
                tr("Link Studio is in the status icon, ready to use. Quit from the icon or with Ctrl+Q.")
            } else if has_tray {
                tr("The virtual camera stays on. Use the Link Studio status icon to show the window, turn the virtual camera off or quit.")
            } else {
                tr("The virtual camera stays on. Open Link Studio again to turn it off, or choose Quit in its menu.")
            }));
            application.send_notification(Some("background"), &notification);
        }
    }

    /// Closes the window for real, also while the virtual camera is running.
    /// Unlike `close()`, this neither hides the window in the status icon nor
    /// stops at an open dialog.
    pub(super) fn force_close(&self) {
        self.shutdown();
        self.window.destroy();
    }

    /// Stops the stream and every timer, so the window can go away. The
    /// refresh timer holds the last strong reference to the app.
    pub(super) fn shutdown(&self) {
        self.stop_preview();
        self.stop_nudge();
        self.cancel_stream_retry();
        for source in [&self.refresh_source, &self.tracking_retry, &self.claude_idle] {
            if let Some(source) = source.borrow_mut().take() {
                source.remove();
            }
        }
    }

    /// The camera is streaming again: turn back on tracking it dropped when
    /// the previous stream stopped. This also runs when no preview is shown.
    pub(super) fn stream_started(self: &Rc<Self>) {
        self.stream_live.set(true);
        if self.stream_waiting.borrow_mut().take().is_some() {
            self.show_preview_page();
        }
        self.claude_stream_started();
        // Privacy mode turned on while the camera slept (it ignores moves
        // then): point it down now that it is awake.
        if let (true, Some(cam)) = (self.privacy.is_active(), self.cam()) {
            self.resume_tracking.set(false);
            self.point_down(&cam);
            return;
        }
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

    pub(super) fn show_frame(&self, frame: Frame) {
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
        self.frames_shown.set(self.frames_shown.get() + 1);
        *self.last_jpeg.borrow_mut() = Some(frame.jpeg);
    }

    pub(super) fn preview_failed(self: &Rc<Self>, message: &str, retry: bool) {
        let had_virtual = self.stream_has_virtual.get();
        self.stop_preview();
        if retry && (self.wants_preview() || self.virtual_camera_device().is_some()) {
            // Keep the preview and the virtual camera switched on and start
            // again by itself once the camera is free or awake again.
            let waiting = (
                tr("Waiting for the camera").to_owned(),
                format!("{message} {}", tr("Link Studio tries again by itself.")),
            );
            self.show_preview_status(&waiting.0, Some(&waiting.1));
            let weak = Rc::downgrade(self);
            let source = glib::timeout_add_local_once(STREAM_RETRY_INTERVAL, move || {
                let Some(app) = weak.upgrade() else { return };
                app.stream_retry.borrow_mut().take();
                app.restart_stream();
                // Keep showing the waiting page until the first frame arrives.
                if app.preview.borrow().is_some() {
                    *app.stream_waiting.borrow_mut() = Some(waiting);
                    app.show_preview_page();
                }
            });
            self.cancel_stream_retry();
            *self.stream_retry.borrow_mut() = Some(source);
            return;
        }
        if had_virtual {
            self.toast(tr("The virtual camera stopped"));
        }
        self.resume_tracking.set(false);
        self.updating.set(true);
        self.preview_toggle.set_active(false);
        self.updating.set(false);
        self.show_preview_status(tr("The preview stopped"), Some(message));
    }

    pub(super) fn cancel_stream_retry(&self) {
        if let Some(source) = self.stream_retry.borrow_mut().take() {
            source.remove();
        }
    }

    pub(super) fn show_preview_status(&self, title: &str, description: Option<&str>) {
        self.preview_status.set_title(title);
        self.preview_status.set_description(Some(description.unwrap_or(tr(
            "Other programs, such as Teams or Zoom, can use the camera while the preview is off.",
        ))));
        self.preview_status_button
            .set_label(if description.is_some() { tr("Try Again") } else { tr("Start Preview") });
        self.preview_stack.set_visible_child_name("status");
    }

    pub(super) fn snapshot(&self) {
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
        // Two photos within the same second must not overwrite each other.
        let mut path = dir.join(format!("Link Studio {stamp}.jpg"));
        let mut copy = 2;
        while path.exists() {
            path = dir.join(format!("Link Studio {stamp} ({copy}).jpg"));
            copy += 1;
        }
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
}
