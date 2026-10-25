//! Low-level access to the Insta360 Link: standard V4L2 controls plus the
//! vendor UVC extension unit (XU) that carries AI tracking and gimbal commands.
//!
//! The XU selector map comes from community reverse engineering of the
//! official Windows app (github.com/vrwallace/Insta360-Link-1-and-2-Controller-for-Linux)
//! and was checked against this camera's descriptors (unit 9, GUID faf1672d-…).

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub const VENDOR_ID: &str = "2e1a";

/// V4L2 control ids used by the UI.
pub mod cid {
    pub const BRIGHTNESS: u32 = 0x0098_0900;
    pub const CONTRAST: u32 = 0x0098_0901;
    pub const SATURATION: u32 = 0x0098_0902;
    pub const HUE: u32 = 0x0098_0903;
    pub const AUTO_WHITE_BALANCE: u32 = 0x0098_090c;
    pub const GAMMA: u32 = 0x0098_0910;
    pub const GAIN: u32 = 0x0098_0913;
    pub const POWER_LINE_FREQUENCY: u32 = 0x0098_0918;
    pub const WHITE_BALANCE_TEMPERATURE: u32 = 0x0098_091a;
    pub const SHARPNESS: u32 = 0x0098_091b;
    pub const BACKLIGHT_COMPENSATION: u32 = 0x0098_091c;
    pub const EXPOSURE_AUTO: u32 = 0x009a_0901;
    pub const EXPOSURE_ABSOLUTE: u32 = 0x009a_0902;
    pub const EXPOSURE_AUTO_PRIORITY: u32 = 0x009a_0903;
    pub const PAN_ABSOLUTE: u32 = 0x009a_0908;
    pub const TILT_ABSOLUTE: u32 = 0x009a_0909;
    pub const FOCUS_ABSOLUTE: u32 = 0x009a_090a;
    pub const FOCUS_AUTO: u32 = 0x009a_090c;
    pub const ZOOM_ABSOLUTE: u32 = 0x009a_090d;
    pub const PAN_SPEED: u32 = 0x009a_0920;
    pub const TILT_SPEED: u32 = 0x009a_0921;
}

const FLAG_DISABLED: u32 = 0x0001;
const FLAG_READ_ONLY: u32 = 0x0004;
const FLAG_INACTIVE: u32 = 0x0010;
const FLAG_NEXT_CTRL: u32 = 0x8000_0000;

const TYPE_INTEGER: u32 = 1;
const TYPE_BOOLEAN: u32 = 2;
const TYPE_MENU: u32 = 3;
const TYPE_CTRL_CLASS: u32 = 6;
const TYPE_INTEGER_MENU: u32 = 9;

const UVC_SET_CUR: u8 = 0x01;
const UVC_GET_CUR: u8 = 0x81;
const UVC_GET_LEN: u8 = 0x85;
const UVC_GET_INFO: u8 = 0x86;
const UVC_INFO_SUPPORTS_SET: u8 = 0x02;

const XU_UNIT: u8 = 9;
const XU_SEL_MODE: u8 = 2;
const XU_SEL_DEVICE_INFO: u8 = 3;
const XU_SEL_GIMBAL_RESET: u8 = 14;
const XU_SEL_FRAMING: u8 = 19;
const XU_SEL_LIVE_POSITION: u8 = 26;
const XU_SEL_EXPOSURE_COMPENSATION: u8 = 9;
/// Exposure compensation is a signed 16-bit value with 256 per EV step
/// (measured: ±256 brightens or darkens the image by about one stop).
pub const EXPOSURE_UNITS_PER_EV: f64 = 256.0;
const XU_TARGET_UNIT: u8 = 10;
const XU_TARGET_SEL: u8 = 1;
const XU_TARGET_BYTE: usize = 4;

const MODE_NORMAL: u8 = 0x00;
const MODE_TRACKING: u8 = 0x01;
const MODE_UNKNOWN: u8 = 0xff;

const fn ioc(dir: u32, ty: u8, nr: u32, size: usize) -> u64 {
    ((dir << 30) | ((size as u32) << 16) | ((ty as u32) << 8) | nr) as u64
}

pub(crate) const fn iowr<T>(ty: u8, nr: u32) -> u64 {
    ioc(3, ty, nr, std::mem::size_of::<T>())
}

pub(crate) const fn iow<T>(ty: u8, nr: u32) -> u64 {
    ioc(1, ty, nr, std::mem::size_of::<T>())
}

pub(crate) fn ioctl<T>(fd: i32, request: u64, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: every request number is derived from the size of `T`, so the
        // kernel reads and writes exactly one `T`.
        let r = unsafe { libc::ioctl(fd, request as _, arg as *mut T) };
        if r != -1 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

#[repr(C)]
struct QueryCtrl {
    id: u32,
    type_: u32,
    name: [u8; 32],
    minimum: i32,
    maximum: i32,
    step: i32,
    default_value: i32,
    flags: u32,
    reserved: [u32; 2],
}

#[repr(C)]
struct Control {
    id: u32,
    value: i32,
}

#[repr(C, packed)]
struct QueryMenu {
    id: u32,
    index: u32,
    name: [u8; 32],
    reserved: u32,
}

#[repr(C)]
struct XuQuery {
    unit: u8,
    selector: u8,
    query: u8,
    size: u16,
    data: *mut u8,
}

const VIDIOC_G_CTRL: u64 = iowr::<Control>(b'V', 27);
const VIDIOC_S_CTRL: u64 = iowr::<Control>(b'V', 28);
const VIDIOC_QUERYCTRL: u64 = iowr::<QueryCtrl>(b'V', 36);
const VIDIOC_QUERYMENU: u64 = iowr::<QueryMenu>(b'V', 37);
const UVCIOC_CTRL_QUERY: u64 = iowr::<XuQuery>(b'u', 0x21);

fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn read_trim(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_owned())
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub path: PathBuf,
    pub name: String,
    pub product_id: String,
}

/// Lists the capture nodes of every connected Insta360 camera.
pub fn find_devices() -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/video4linux") else {
        return out;
    };
    for entry in entries.flatten() {
        let sys = entry.path();
        // Each UVC camera exposes a metadata node as well; index 0 is the capture node.
        if read_trim(&sys.join("index")).as_deref() != Some("0") {
            continue;
        }
        let Ok(interface) = fs::canonicalize(sys.join("device")) else {
            continue;
        };
        let Some(usb) = interface.parent() else {
            continue;
        };
        if read_trim(&usb.join("idVendor")).as_deref() != Some(VENDOR_ID) {
            continue;
        }
        let name = read_trim(&sys.join("name")).unwrap_or_default();
        let name = name.rsplit(": ").next().unwrap_or(&name).to_owned();
        out.push(DeviceInfo {
            path: Path::new("/dev").join(entry.file_name()),
            name,
            product_id: read_trim(&usb.join("idProduct")).unwrap_or_default(),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[derive(Clone, Debug, PartialEq)]
pub enum CtrlKind {
    Integer,
    Boolean,
    Menu(Vec<(i32, String)>),
}

#[derive(Clone, Debug)]
pub struct CtrlInfo {
    pub id: u32,
    pub name: String,
    pub kind: CtrlKind,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
    pub flags: u32,
}

impl CtrlInfo {
    pub fn is_inactive(&self) -> bool {
        self.flags & FLAG_INACTIVE != 0
    }

    pub fn is_writable(&self) -> bool {
        self.flags & (FLAG_READ_ONLY | FLAG_DISABLED) == 0
    }
}

/// The camera's working mode as reported by XU selector 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Tracking,
    /// DeskView, whiteboard or overhead, usually entered by a hand gesture.
    Other(u8),
}

#[derive(Clone, Debug, Default)]
pub struct Identity {
    pub serial: Option<String>,
    pub firmware: Option<String>,
}

pub struct Camera {
    file: File,
    pub info: DeviceInfo,
}

impl Camera {
    pub fn open(info: DeviceInfo) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&info.path)?;
        Ok(Self { file, info })
    }

    fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    /// True once the camera has gone away: unplugged, or reset and
    /// re-enumerated. After a reset the device node comes back quickly, but
    /// this file handle stays dead and every ioctl fails with ENODEV.
    pub fn is_gone(&self) -> bool {
        if !self.info.path.exists() {
            return true;
        }
        let mut probe = Control { id: cid::BRIGHTNESS, value: 0 };
        matches!(ioctl(self.fd(), VIDIOC_G_CTRL, &mut probe), Err(err) if err.raw_os_error() == Some(libc::ENODEV))
    }

    pub fn controls(&self) -> Vec<CtrlInfo> {
        let mut out = Vec::new();
        let mut id = FLAG_NEXT_CTRL;
        loop {
            // SAFETY: plain-old-data struct, all-zero is a valid value.
            let mut q: QueryCtrl = unsafe { std::mem::zeroed() };
            q.id = id;
            if ioctl(self.fd(), VIDIOC_QUERYCTRL, &mut q).is_err() {
                break;
            }
            id = q.id | FLAG_NEXT_CTRL;
            if q.type_ == TYPE_CTRL_CLASS || q.flags & FLAG_DISABLED != 0 {
                continue;
            }
            if let Some(info) = self.ctrl_info(&q) {
                out.push(info);
            }
        }
        out
    }

    pub fn query(&self, id: u32) -> io::Result<CtrlInfo> {
        // SAFETY: plain-old-data struct, all-zero is a valid value.
        let mut q: QueryCtrl = unsafe { std::mem::zeroed() };
        q.id = id;
        ioctl(self.fd(), VIDIOC_QUERYCTRL, &mut q)?;
        self.ctrl_info(&q)
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "unsupported control type"))
    }

    fn ctrl_info(&self, q: &QueryCtrl) -> Option<CtrlInfo> {
        let kind = match q.type_ {
            TYPE_INTEGER => CtrlKind::Integer,
            TYPE_BOOLEAN => CtrlKind::Boolean,
            TYPE_MENU | TYPE_INTEGER_MENU => CtrlKind::Menu(self.menu_items(q)),
            _ => return None,
        };
        Some(CtrlInfo {
            id: q.id,
            name: c_string(&q.name),
            kind,
            min: q.minimum,
            max: q.maximum,
            step: q.step,
            default: q.default_value,
            flags: q.flags,
        })
    }

    fn menu_items(&self, q: &QueryCtrl) -> Vec<(i32, String)> {
        let mut items = Vec::new();
        for index in q.minimum..=q.maximum {
            let mut m = QueryMenu { id: q.id, index: index as u32, name: [0; 32], reserved: 0 };
            if ioctl(self.fd(), VIDIOC_QUERYMENU, &mut m).is_ok() {
                let name = if q.type_ == TYPE_INTEGER_MENU {
                    let mut raw = [0u8; 8];
                    raw.copy_from_slice(&m.name[..8]);
                    i64::from_le_bytes(raw).to_string()
                } else {
                    c_string(&m.name)
                };
                items.push((index, name));
            }
        }
        items
    }

    pub fn get(&self, id: u32) -> io::Result<i32> {
        let mut c = Control { id, value: 0 };
        ioctl(self.fd(), VIDIOC_G_CTRL, &mut c)?;
        Ok(c.value)
    }

    pub fn set(&self, id: u32, value: i32) -> io::Result<()> {
        let mut c = Control { id, value };
        ioctl(self.fd(), VIDIOC_S_CTRL, &mut c)
    }

    fn xu_query(&self, unit: u8, selector: u8, query: u8, buf: &mut [u8]) -> io::Result<()> {
        let mut q = XuQuery {
            unit,
            selector,
            query,
            size: buf.len() as u16,
            data: buf.as_mut_ptr(),
        };
        ioctl(self.fd(), UVCIOC_CTRL_QUERY, &mut q)
    }

    fn xu_len(&self, unit: u8, selector: u8) -> io::Result<usize> {
        let mut len = [0u8; 2];
        self.xu_query(unit, selector, UVC_GET_LEN, &mut len)?;
        Ok(u16::from_le_bytes(len) as usize)
    }

    pub fn xu_get(&self, unit: u8, selector: u8) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; self.xu_len(unit, selector)?];
        self.xu_query(unit, selector, UVC_GET_CUR, &mut buf)?;
        Ok(buf)
    }

    /// Writes an XU control. The payload is padded to the length the camera
    /// reports, because a mismatched length can stall the firmware.
    pub fn xu_set(&self, unit: u8, selector: u8, data: &[u8]) -> io::Result<()> {
        let len = self.xu_len(unit, selector)?;
        if data.len() > len {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "XU payload too long"));
        }
        let mut buf = vec![0u8; len];
        buf[..data.len()].copy_from_slice(data);
        self.xu_query(unit, selector, UVC_SET_CUR, &mut buf)
    }

    /// Current mode, or `None` when the camera does not say. After a mode
    /// command, firmware 1.4.5.8 reads back 0xff whatever the mode is.
    pub fn mode(&self) -> io::Result<Option<Mode>> {
        let buf = self.xu_get(XU_UNIT, XU_SEL_MODE)?;
        Ok(match buf.first().copied().unwrap_or(MODE_UNKNOWN) {
            MODE_UNKNOWN => None,
            MODE_NORMAL => Some(Mode::Normal),
            MODE_TRACKING => Some(Mode::Tracking),
            other => Some(Mode::Other(other)),
        })
    }

    /// Turns AI tracking on, or returns the camera to normal mode. Turning it
    /// off also leaves DeskView, whiteboard and overhead modes.
    pub fn set_tracking(&self, on: bool) -> io::Result<()> {
        let mode = if on { MODE_TRACKING } else { MODE_NORMAL };
        self.xu_set(XU_UNIT, XU_SEL_MODE, &[mode, 0x00])
    }

    /// Framing used while tracking: 1 = head, 2 = upper body, 3 = full body.
    pub fn framing(&self) -> io::Result<u8> {
        Ok(self.xu_get(XU_UNIT, XU_SEL_FRAMING)?.first().copied().unwrap_or(2))
    }

    pub fn set_framing(&self, framing: u8) -> io::Result<()> {
        self.xu_set(XU_UNIT, XU_SEL_FRAMING, &[framing])
    }

    /// Whether tracking follows a group instead of a single person.
    pub fn group_tracking(&self) -> io::Result<bool> {
        let buf = self.xu_get(XU_TARGET_UNIT, XU_TARGET_SEL)?;
        Ok(buf.get(XU_TARGET_BYTE).copied().unwrap_or(0) != 0)
    }

    /// Group tracking is read-only on some firmware (1.4.5.8 on the Link).
    pub fn can_set_group_tracking(&self) -> bool {
        let mut info = [0u8];
        self.xu_query(XU_TARGET_UNIT, XU_TARGET_SEL, UVC_GET_INFO, &mut info).is_ok()
            && info[0] & UVC_INFO_SUPPORTS_SET != 0
    }

    pub fn set_group_tracking(&self, group: bool) -> io::Result<()> {
        let mut buf = self.xu_get(XU_TARGET_UNIT, XU_TARGET_SEL)?;
        let Some(byte) = buf.get_mut(XU_TARGET_BYTE) else {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "unexpected XU length"));
        };
        *byte = group as u8;
        self.xu_set(XU_TARGET_UNIT, XU_TARGET_SEL, &buf)
    }

    /// Where the gimbal actually points, as (pan, tilt) in arcsec. The V4L2
    /// pan/tilt values are cached by uvcvideo and miss moves made by AI
    /// tracking or the camera itself; XU selector 26 is read from the camera.
    pub fn live_position(&self) -> io::Result<(i32, i32)> {
        let buf = self.xu_get(XU_UNIT, XU_SEL_LIVE_POSITION)?;
        if buf.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "unexpected XU length"));
        }
        let tilt = i32::from_le_bytes(buf[0..4].try_into().unwrap());
        let pan = i32::from_le_bytes(buf[4..8].try_into().unwrap());
        Ok((pan, tilt))
    }

    /// Exposure compensation in camera units (see `EXPOSURE_UNITS_PER_EV`).
    /// The camera keeps it across stream restarts.
    pub fn exposure_compensation(&self) -> io::Result<i16> {
        let buf = self.xu_get(XU_UNIT, XU_SEL_EXPOSURE_COMPENSATION)?;
        match buf[..] {
            [lo, hi] => Ok(i16::from_le_bytes([lo, hi])),
            _ => Err(io::Error::new(io::ErrorKind::Unsupported, "unexpected XU length")),
        }
    }

    pub fn set_exposure_compensation(&self, value: i16) -> io::Result<()> {
        self.xu_set(XU_UNIT, XU_SEL_EXPOSURE_COMPENSATION, &value.to_le_bytes())
    }

    pub fn reset_gimbal(&self) -> io::Result<()> {
        self.xu_set(XU_UNIT, XU_SEL_GIMBAL_RESET, &[0x01])
    }

    /// Serial number and firmware version from XU selector 3.
    pub fn identity(&self) -> Identity {
        let Ok(buf) = self.xu_get(XU_UNIT, XU_SEL_DEVICE_INFO) else {
            return Identity::default();
        };
        let runs: Vec<String> = buf
            .split(|b| !b.is_ascii_graphic())
            .filter(|run| run.len() >= 6)
            .map(|run| String::from_utf8_lossy(run).into_owned())
            .collect();
        Identity {
            serial: runs.first().cloned(),
            firmware: runs.iter().find(|r| r.starts_with('v') && r.contains('.')).cloned(),
        }
    }
}
