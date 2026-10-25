//! Live preview: a background thread streams MJPEG frames over V4L2 mmap
//! buffers, decodes them to RGB and hands them to the UI over a channel.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;

use crate::camera::{ioctl, iow, iowr};
use crate::i18n::{tr, trf};
use crate::vcam;

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FIELD_ANY: u32 = 0;
const PIX_FMT_MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
const BUFFER_COUNT: u32 = 4;
const POLL_TIMEOUT_MS: i32 = 200;
const MAX_DISPLAY_WIDTH: i32 = 1920;
/// Pictures for Claude: about the largest size it reads without scaling.
const SNAPSHOT_WIDTH: usize = 1280;
const SNAPSHOT_QUALITY: u8 = 85;

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

/// The kernel's `fmt` union is 200 bytes and pointer-aligned.
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

#[repr(C)]
struct RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
struct Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: [u32; 4],
    sequence: u32,
    memory: u32,
    m: u64,
    length: u32,
    reserved2: u32,
    request_fd: u32,
}

const VIDIOC_S_FMT: u64 = iowr::<Format>(b'V', 5);
const VIDIOC_REQBUFS: u64 = iowr::<RequestBuffers>(b'V', 8);
const VIDIOC_QUERYBUF: u64 = iowr::<Buffer>(b'V', 9);
const VIDIOC_QBUF: u64 = iowr::<Buffer>(b'V', 15);
const VIDIOC_DQBUF: u64 = iowr::<Buffer>(b'V', 17);
const VIDIOC_STREAMON: u64 = iow::<i32>(b'V', 18);
const VIDIOC_STREAMOFF: u64 = iow::<i32>(b'V', 19);

pub struct Frame {
    pub width: i32,
    pub height: i32,
    pub rgb: Vec<u8>,
    /// The original JPEG, kept so snapshots are saved without re-encoding.
    pub jpeg: Vec<u8>,
}

pub enum Event {
    /// The camera delivered its first frame, so it is awake and streaming.
    Started,
    Frame(Frame),
    Error(String),
    /// The stream keeps running for the preview, but the virtual camera stopped.
    VirtualCameraError(String),
    /// A picture asked for through `Options::snapshot`, unmirrored, as JPEG.
    Snapshot { jpeg: Vec<u8>, width: usize, height: usize },
}

pub struct Options {
    pub device: PathBuf,
    pub size: (u32, u32),
    /// Mirrors the preview only; the virtual camera always gets the real picture.
    pub mirror: Arc<AtomicBool>,
    /// Whether frames are sent to the preview. The stream also runs with the
    /// preview hidden while it feeds the virtual camera.
    pub show: Arc<AtomicBool>,
    pub virtual_camera: Option<PathBuf>,
    /// Set to ask for the next frame as a downscaled JPEG (for Claude).
    pub snapshot: Arc<AtomicBool>,
}

pub struct Preview {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Preview {
    pub fn start(options: Options, tx: async_channel::Sender<Event>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            if let Err(err) = capture(&options, &thread_stop, &tx) {
                // Never block here: the UI thread may be joining this thread.
                let _ = tx.force_send(Event::Error(describe(&err)));
            }
        });
        Self { stop, thread: Some(thread) }
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn describe(err: &io::Error) -> String {
    match err.raw_os_error() {
        Some(libc::EBUSY) => {
            tr("The camera is being used by another program. Close it, or try again when it is done.").into()
        }
        Some(libc::ENODEV) | Some(libc::ENOENT) => tr("The camera was disconnected.").into(),
        _ => trf("The preview stopped: {}", &[err]),
    }
}

struct Mapping {
    ptr: *mut libc::c_void,
    len: usize,
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: ptr/len come from a successful mmap of this buffer.
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

fn zeroed_buffer(index: u32) -> Buffer {
    // SAFETY: plain-old-data struct, all-zero is a valid value.
    let mut buf: Buffer = unsafe { std::mem::zeroed() };
    buf.index = index;
    buf.type_ = BUF_TYPE_VIDEO_CAPTURE;
    buf.memory = MEMORY_MMAP;
    buf
}

fn capture(options: &Options, stop: &AtomicBool, tx: &async_channel::Sender<Event>) -> io::Result<()> {
    let (width, height) = options.size;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&options.device)?;
    let fd = file.as_raw_fd();

    // SAFETY: plain-old-data struct, all-zero is a valid value.
    let mut fmt: Format = unsafe { std::mem::zeroed() };
    fmt.type_ = BUF_TYPE_VIDEO_CAPTURE;
    fmt.fmt.pix.width = width;
    fmt.fmt.pix.height = height;
    fmt.fmt.pix.pixelformat = PIX_FMT_MJPEG;
    fmt.fmt.pix.field = FIELD_ANY;
    ioctl(fd, VIDIOC_S_FMT, &mut fmt)?;
    if fmt.fmt.pix.pixelformat != PIX_FMT_MJPEG {
        return Err(io::Error::new(io::ErrorKind::Unsupported, tr("the camera does not deliver MJPEG")));
    }

    let mut req = RequestBuffers {
        count: BUFFER_COUNT,
        type_: BUF_TYPE_VIDEO_CAPTURE,
        memory: MEMORY_MMAP,
        capabilities: 0,
        flags: 0,
        reserved: [0; 3],
    };
    ioctl(fd, VIDIOC_REQBUFS, &mut req)?;

    let mut maps = Vec::with_capacity(req.count as usize);
    for index in 0..req.count {
        let mut buf = zeroed_buffer(index);
        ioctl(fd, VIDIOC_QUERYBUF, &mut buf)?;
        // SAFETY: mapping a driver-provided buffer at the offset it reported.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                buf.length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                (buf.m & 0xffff_ffff) as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        maps.push(Mapping { ptr, len: buf.length as usize });
        ioctl(fd, VIDIOC_QBUF, &mut buf)?;
    }

    // The camera may pick a slightly different size; the virtual camera uses
    // whatever it delivers.
    let mut writer = options.virtual_camera.as_ref().and_then(|path| {
        match vcam::Writer::open(path, fmt.fmt.pix.width, fmt.fmt.pix.height) {
            Ok(writer) => Some(writer),
            Err(err) => {
                let _ = tx.force_send(Event::VirtualCameraError(err.to_string()));
                None
            }
        }
    });

    let mut kind = BUF_TYPE_VIDEO_CAPTURE as i32;
    ioctl(fd, VIDIOC_STREAMON, &mut kind)?;

    let result = stream(fd, &maps, options, &mut writer, stop, tx);

    let _ = ioctl(fd, VIDIOC_STREAMOFF, &mut kind);
    drop(maps);
    result
}

fn stream(
    fd: i32,
    maps: &[Mapping],
    options: &Options,
    writer: &mut Option<vcam::Writer>,
    stop: &AtomicBool,
    tx: &async_channel::Sender<Event>,
) -> io::Result<()> {
    let mut started = false;
    while !stop.load(Ordering::Relaxed) {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, POLL_TIMEOUT_MS) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ready == 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }

        let mut buf = zeroed_buffer(0);
        match ioctl(fd, VIDIOC_DQBUF, &mut buf) {
            Ok(()) => {}
            Err(err) if err.raw_os_error() == Some(libc::EAGAIN) => continue,
            Err(err) => return Err(err),
        }
        let map = &maps[buf.index as usize];
        let used = (buf.bytesused as usize).min(map.len);
        // SAFETY: the driver filled `used` bytes of this mapping before dequeueing it.
        let jpeg = unsafe { std::slice::from_raw_parts(map.ptr as *const u8, used) }.to_vec();
        ioctl(fd, VIDIOC_QBUF, &mut buf)?;
        if !started {
            started = true;
            let _ = tx.force_send(Event::Started);
        }

        // The virtual camera needs every frame; the preview drops frames
        // while the UI is still busy with earlier ones.
        let to_preview = options.show.load(Ordering::Relaxed) && !tx.is_full();
        let to_snapshot = options.snapshot.load(Ordering::Relaxed);
        if jpeg.is_empty() || (writer.is_none() && !to_preview && !to_snapshot) {
            continue;
        }
        let Some((mut width, mut height, mut rgb)) = decode(&jpeg) else {
            continue;
        };
        if let Some(out) = writer.as_mut() {
            if let Err(err) = out.write(&rgb, width as usize, height as usize) {
                let _ = tx.force_send(Event::VirtualCameraError(err.to_string()));
                *writer = None;
            }
        }
        if to_snapshot && options.snapshot.swap(false, Ordering::Relaxed) {
            if let Some(event) = snapshot(&rgb, width as usize, height as usize) {
                let _ = tx.force_send(event);
            }
        }
        if !to_preview {
            continue;
        }
        // The preview is never shown larger than a window, so 4K frames are
        // halved here instead of making the UI thread upload 24 MB per frame.
        // Snapshots still use the full-size JPEG.
        while width > MAX_DISPLAY_WIDTH {
            rgb = halve(&rgb, width as usize, height as usize);
            width /= 2;
            height /= 2;
        }
        if options.mirror.load(Ordering::Relaxed) {
            flip_horizontal(&mut rgb, width as usize);
        }
        if tx.try_send(Event::Frame(Frame { width, height, rgb, jpeg })).is_err() && tx.is_closed() {
            break;
        }
    }
    Ok(())
}

fn decode(jpeg: &[u8]) -> Option<(i32, i32, Vec<u8>)> {
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
    let mut decoder = JpegDecoder::new_with_options(jpeg, options);
    let pixels = decoder.decode().ok()?;
    let (width, height) = decoder.dimensions()?;
    Some((width as i32, height as i32, pixels))
}

fn snapshot(rgb: &[u8], width: usize, height: usize) -> Option<Event> {
    let (pixels, width, height) = if width > SNAPSHOT_WIDTH {
        let target_height = (height * SNAPSHOT_WIDTH + width / 2) / width;
        (resize(rgb, width, height, SNAPSHOT_WIDTH, target_height), SNAPSHOT_WIDTH, target_height)
    } else {
        (rgb.to_vec(), width, height)
    };
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, SNAPSHOT_QUALITY)
        .encode(&pixels, width as u16, height as u16, jpeg_encoder::ColorType::Rgb)
        .ok()?;
    Some(Event::Snapshot { jpeg, width, height })
}

/// Area-average downscale of an RGB image to any smaller size.
fn resize(rgb: &[u8], width: usize, height: usize, out_w: usize, out_h: usize) -> Vec<u8> {
    let mut out = vec![0u8; out_w * out_h * 3];
    for oy in 0..out_h {
        let (y0, y1) = (oy * height / out_h, ((oy + 1) * height / out_h).max(oy * height / out_h + 1));
        for ox in 0..out_w {
            let (x0, x1) = (ox * width / out_w, ((ox + 1) * width / out_w).max(ox * width / out_w + 1));
            let mut sum = [0u32; 3];
            for y in y0..y1 {
                for px in rgb[(y * width + x0) * 3..(y * width + x1) * 3].as_chunks::<3>().0 {
                    sum[0] += px[0] as u32;
                    sum[1] += px[1] as u32;
                    sum[2] += px[2] as u32;
                }
            }
            let count = ((y1 - y0) * (x1 - x0)) as u32;
            let i = (oy * out_w + ox) * 3;
            out[i] = (sum[0] / count) as u8;
            out[i + 1] = (sum[1] / count) as u8;
            out[i + 2] = (sum[2] / count) as u8;
        }
    }
    out
}

/// 2×2 box filter: halves an RGB image in both directions.
fn halve(rgb: &[u8], width: usize, height: usize) -> Vec<u8> {
    let (w, h) = (width / 2, height / 2);
    let mut out = vec![0u8; w * h * 3];
    for y in 0..h {
        let top = &rgb[(2 * y) * width * 3..];
        let bottom = &rgb[(2 * y + 1) * width * 3..];
        let row = &mut out[y * w * 3..(y + 1) * w * 3];
        for x in 0..w {
            for c in 0..3 {
                let i = 6 * x + c;
                let sum = top[i] as u16 + top[i + 3] as u16 + bottom[i] as u16 + bottom[i + 3] as u16;
                row[3 * x + c] = (sum / 4) as u8;
            }
        }
    }
    out
}

fn flip_horizontal(rgb: &mut [u8], width: usize) {
    for row in rgb.chunks_exact_mut(width * 3) {
        let (mut left, mut right) = (0, width - 1);
        while left < right {
            for c in 0..3 {
                row.swap(left * 3 + c, right * 3 + c);
            }
            left += 1;
            right -= 1;
        }
    }
}
