//! The "Link Studio Camera" virtual camera: a v4l2loopback device that other
//! programs read from while Link Studio writes the processed picture to it.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use gtk::gio::prelude::*;
use gtk::{gio, glib};

use crate::camera::{ioctl, iowr};

pub const LABEL: &str = "Link Studio Camera";
const MODPROBE_CONF: &str = "/etc/modprobe.d/linkstudio-v4l2loopback.conf";
const SETUP_SCRIPT: &str = include_str!("../data/setup-virtual-camera.sh");
const REMOVE_SCRIPT: &str = include_str!("../data/remove-virtual-camera.sh");

const BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const FIELD_NONE: u32 = 1;
const COLORSPACE_SRGB: u32 = 8;
const PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");

/// The loopback device created by the setup script, if it is loaded.
pub fn find_device() -> Option<PathBuf> {
    let entries = fs::read_dir("/sys/class/video4linux").ok()?;
    entries.flatten().find_map(|entry| {
        let name = fs::read_to_string(entry.path().join("name")).ok()?;
        (name.trim() == LABEL).then(|| Path::new("/dev").join(entry.file_name()))
    })
}

/// Whether the setup script has been run (the device may still need a reboot).
pub fn is_set_up() -> bool {
    Path::new(MODPROBE_CONF).exists()
}

pub enum SetupError {
    /// The password dialog was dismissed.
    Cancelled,
    Failed(String),
}

/// Runs the setup or removal script as root through pkexec, which shows the
/// desktop's password dialog. The script text is embedded in the binary, so
/// nothing user-writable is executed as root. Each output line is passed to
/// `on_output` as it arrives, so slow package downloads show progress.
pub fn run_privileged(
    remove: bool,
    on_output: impl Fn(&str) + 'static,
    done: impl FnOnce(Result<(), SetupError>) + 'static,
) {
    const PKEXEC_DISMISSED: i32 = 126;
    let script = if remove { REMOVE_SCRIPT } else { SETUP_SCRIPT };
    let flags = gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_MERGE;
    let argv: [&std::ffi::OsStr; 4] = ["pkexec".as_ref(), "sh".as_ref(), "-c".as_ref(), script.as_ref()];
    let process = match gio::Subprocess::newv(&argv, flags) {
        Ok(process) => process,
        Err(err) => return done(Err(SetupError::Failed(err.to_string()))),
    };
    let Some(stdout) = process.stdout_pipe() else {
        return done(Err(SetupError::Failed("no output pipe".into())));
    };
    let reader = gio::DataInputStream::new(&stdout);
    glib::spawn_future_local(async move {
        let mut output = String::new();
        while let Ok(Some(line)) = reader.read_line_utf8_future(glib::Priority::DEFAULT).await {
            on_output(&line);
            output.push_str(&line);
            output.push('\n');
        }
        let outcome = match process.wait_future().await {
            Err(err) => Err(SetupError::Failed(err.to_string())),
            Ok(()) if process.is_successful() => Ok(()),
            Ok(()) if process.exit_status() == PKEXEC_DISMISSED => Err(SetupError::Cancelled),
            Ok(()) => Err(SetupError::Failed(output.trim().to_owned())),
        };
        done(outcome);
    });
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C, align(8))]
struct FormatUnion {
    pix: PixFormat,
    _raw: [u8; 200 - std::mem::size_of::<PixFormat>()],
}

#[repr(C)]
struct Format {
    type_: u32,
    fmt: FormatUnion,
}

const VIDIOC_S_FMT: u64 = iowr::<Format>(b'V', 5);

/// Writes RGB frames to the loopback device as YUYV, the format video-call
/// programs and browsers accept most widely.
pub struct Writer {
    file: File,
    width: usize,
    height: usize,
    yuyv: Vec<u8>,
}

impl Writer {
    pub fn open(path: &Path, width: u32, height: u32) -> io::Result<Self> {
        let file = OpenOptions::new().write(true).open(path)?;
        // SAFETY: plain-old-data struct, all-zero is a valid value.
        let mut fmt: Format = unsafe { std::mem::zeroed() };
        fmt.type_ = BUF_TYPE_VIDEO_OUTPUT;
        fmt.fmt.pix.width = width;
        fmt.fmt.pix.height = height;
        fmt.fmt.pix.pixelformat = PIX_FMT_YUYV;
        fmt.fmt.pix.field = FIELD_NONE;
        fmt.fmt.pix.bytesperline = width * 2;
        fmt.fmt.pix.sizeimage = width * height * 2;
        fmt.fmt.pix.colorspace = COLORSPACE_SRGB;
        ioctl(file.as_raw_fd(), VIDIOC_S_FMT, &mut fmt)?;
        let (width, height) = (width as usize, height as usize);
        Ok(Self { file, width, height, yuyv: vec![0; width * height * 2] })
    }

    /// Writes one frame; frames of another size are skipped.
    pub fn write(&mut self, rgb: &[u8], width: usize, height: usize) -> io::Result<()> {
        if width != self.width || height != self.height {
            return Ok(());
        }
        rgb_to_yuyv(rgb, &mut self.yuyv);
        self.file.write_all(&self.yuyv)
    }
}

/// BT.601 limited-range RGB → YUYV (4:2:2), two pixels at a time.
fn rgb_to_yuyv(rgb: &[u8], out: &mut [u8]) {
    for (px, yuyv) in rgb.chunks_exact(6).zip(out.chunks_exact_mut(4)) {
        let (r0, g0, b0) = (px[0] as i32, px[1] as i32, px[2] as i32);
        let (r1, g1, b1) = (px[3] as i32, px[4] as i32, px[5] as i32);
        let y = |r: i32, g: i32, b: i32| (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
        let (r, g, b) = ((r0 + r1) / 2, (g0 + g1) / 2, (b0 + b1) / 2);
        yuyv[0] = y(r0, g0, b0);
        yuyv[1] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
        yuyv[2] = y(r1, g1, b1);
        yuyv[3] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
    }
}
