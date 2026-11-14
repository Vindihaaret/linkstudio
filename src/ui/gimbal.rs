//! Gimbal moves, zoom, AI tracking and privacy mode.

use super::*;

impl App {
    /// Moves the gimbal one step. `resync` starts from where the gimbal really
    /// points (it may have been moved by tracking); repeats while a button is
    /// held continue from the last target instead, since the motor lags behind.
    pub(super) fn nudge(&self, dpan: i32, dtilt: i32, resync: bool) {
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

    pub(super) fn zoom_by(&self, delta: i32) {
        if self.privacy.is_active() {
            return;
        }
        let value = (self.zoom.value() + delta as f64).clamp(self.zoom.adjustment().lower(), self.zoom.adjustment().upper());
        self.zoom.set_value(value);
    }

    pub(super) fn center(&self) {
        let Some(cam) = self.cam() else { return };
        if self.privacy.is_active() {
            self.leave_privacy();
        }
        // The vendor reset is not obeyed from every position (for example
        // after privacy mode), so set pan and tilt as well.
        let _ = cam.reset_gimbal();
        let _ = cam.set(cid::PAN_ABSOLUTE, 0);
        let _ = cam.set(cid::TILT_ABSOLUTE, 0);
    }

    pub(super) fn set_tracking(self: &Rc<Self>, on: bool) {
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
    pub(super) fn schedule_tracking_retry(self: &Rc<Self>) {
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

    pub(super) fn set_privacy(&self, on: bool) {
        let Some(cam) = self.cam() else { return };
        if on {
            let (pan, tilt) = gimbal_position(&cam).unwrap_or((0, 0));
            let saved = (snap_degree(pan), snap_degree(tilt), cam.get(cid::ZOOM_ABSOLUTE).unwrap_or(100));
            self.privacy_restore.set(Some(saved));
            self.resume_tracking.set(false);
            if self.tracking_on.get() && cam.set_tracking(false).is_ok() {
                self.tracking_on.set(false);
            }
            self.point_down(&cam);
        } else if let Some((pan, tilt, zoom)) = self.privacy_restore.take() {
            let _ = cam.set(cid::PAN_ABSOLUTE, pan);
            let _ = cam.set(cid::TILT_ABSOLUTE, tilt);
            let _ = cam.set(cid::ZOOM_ABSOLUTE, zoom);
        }
        self.save_privacy();
        self.refresh(&cam);
    }

    /// "Privacy mode when the screen is locked": on when the screen locks, and
    /// off again on unlock unless privacy mode was already on before.
    pub(super) fn screen_locked(&self, locked: bool) {
        if locked {
            let wanted = self.config.borrow().privacy_on_lock;
            if wanted && self.cam().is_some() && !self.privacy.is_active() {
                self.lock_privacy.set(true);
                self.privacy.set_active(true);
            }
        } else if self.lock_privacy.take() && self.privacy.is_active() {
            self.privacy.set_active(false);
        }
    }

    pub(super) fn point_down(&self, cam: &Camera) {
        let _ = cam.set(cid::ZOOM_ABSOLUTE, 100);
        let _ = cam.set(cid::PAN_ABSOLUTE, 0);
        let _ = cam.set(cid::TILT_ABSOLUTE, self.tilt_range.get().0);
    }

    pub(super) fn save_privacy(&self) {
        let mut config = self.config.borrow_mut();
        config.privacy = self.privacy.is_active();
        config.privacy_restore = self.privacy_restore.get();
        config.save();
    }

    /// Puts privacy mode back on after a restart or reconnect.
    pub(super) fn restore_privacy(&self, cam: &Camera) {
        let (on, restore) = {
            let config = self.config.borrow();
            (config.privacy, config.privacy_restore)
        };
        if !on {
            return;
        }
        self.privacy_restore.set(restore);
        self.updating.set(true);
        self.privacy.set_active(true);
        self.updating.set(false);
        self.point_down(cam);
    }

    /// Ends privacy mode without moving back, for actions that move the camera anyway.
    pub(super) fn leave_privacy(&self) {
        self.lock_privacy.set(false);
        self.privacy_restore.set(None);
        self.updating.set(true);
        self.privacy.set_active(false);
        self.updating.set(false);
        self.save_privacy();
    }
}
