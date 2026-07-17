//! NVIDIA NvFBC ("Frame Buffer Capture") fallback screen capture.
//!
//! [`crate::capture`] reads the CRTC's scanout buffer straight from
//! DRM/KMS, which works everywhere except one case seen in practice: the
//! proprietary NVIDIA driver, running a classic Xorg session, that simply
//! never binds a CRTC in the kernel's DRM/KMS state at all (see README) -
//! there is no framebuffer for the DRM path to even find, tiled or
//! otherwise. NvFBC is NVIDIA's own capture API and gets the same pixels a
//! different way: straight from the X driver's internal state, bypassing
//! DRM/KMS entirely. It's only ever tried as a fallback after the DRM path
//! fails, and only matters on NVIDIA hardware.
//!
//! Officially NvFBC's system matrix is GRID/Tesla/Quadro-only; consumer
//! GeForce cards need a "magic" private-data cookie when creating the
//! session handle to unlock it, which is what every open source NvFBC
//! client (Sunshine, this one, etc.) does - see
//! <https://github.com/keylase/nvidia-patch>. This is unofficial but has
//! worked reliably across driver versions for years.
//!
//! `libnvidia-fbc.so.1` is dlopen'd at runtime, not linked, so a build
//! without it installed still works fine - this path just never succeeds.
//! The struct/constant layout below is hand-derived from NVIDIA's public
//! NvFBC.h (via the BSD-2-Clause `nvfbc-sys` crate's bindgen output, cross
//! checked against it) rather than linking that crate, since its build.rs
//! hard-links `-lnvidia-fbc` - that would break every build without the
//! NVIDIA driver installed, including plain CI containers.

use std::ffi::{CStr, c_void};
use std::io;
use std::sync::OnceLock;

use libloading::{Library, Symbol};

type NvfbcStatus = u32;
const NVFBC_SUCCESS: NvfbcStatus = 0;

const NVFBC_CAPTURE_TO_SYS: u32 = 0;
const NVFBC_TRACKING_DEFAULT: u32 = 0; // whole X screen
const NVFBC_BUFFER_FORMAT_BGRA: u32 = 5; // native - no pixel conversion needed
const NVFBC_TOSYS_GRAB_FLAGS_NOWAIT: u32 = 1; // return the latest frame, don't block
const NVFBC_FALSE: u32 = 0;

// Same cookie used by every unofficial NvFBC client to unlock GeForce
// support - see the module doc comment.
const MAGIC_PRIVATE_DATA: [u32; 4] = [0xAEF57AC5, 0x401D1A39, 0x1B856BBE, 0x9ED0CEBA];

fn nvfbc_struct_version(size: usize, version: u32) -> u32 {
    // NVFBC_VERSION_MAJOR/MINOR as of driver 550.x (NvFBC.h ships alongside
    // the driver and both have moved in lockstep for years, so this is
    // effectively pinned to what's actually installed at runtime).
    const NVFBC_VERSION_MAJOR: u32 = 1;
    const NVFBC_VERSION_MINOR: u32 = 8;
    const NVFBC_VERSION: u32 = NVFBC_VERSION_MINOR | (NVFBC_VERSION_MAJOR << 8);
    size as u32 | (version << 16) | (NVFBC_VERSION << 24)
}

#[repr(C)]
#[derive(Copy, Clone)]
struct CreateHandleParams {
    dw_version: u32,
    private_data: *const c_void,
    private_data_size: u32,
    b_externally_managed_context: u32,
    glx_ctx: *mut c_void,
    glx_fb_config: *mut c_void,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct DestroyHandleParams {
    dw_version: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Box_ {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct Size {
    w: u32,
    h: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct CreateCaptureSessionParams {
    dw_version: u32,
    e_capture_type: u32,
    e_tracking_type: u32,
    dw_output_id: u32,
    capture_box: Box_,
    frame_size: Size,
    b_with_cursor: u32,
    b_disable_auto_modeset_recovery: u32,
    b_round_frame_size: u32,
    dw_sampling_rate_ms: u32,
    b_push_model: u32,
    b_allow_direct_capture: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct DestroyCaptureSessionParams {
    dw_version: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct ToSysSetupParams {
    dw_version: u32,
    e_buffer_format: u32,
    pp_buffer: *mut *mut c_void,
    b_with_diff_map: u32,
    pp_diff_map: *mut *mut c_void,
    dw_diff_map_scaling_factor: u32,
    diff_map_size: Size,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct FrameGrabInfo {
    dw_width: u32,
    dw_height: u32,
    dw_byte_size: u32,
    dw_current_frame: u32,
    b_is_new_frame: u32,
    ul_timestamp_us: u64,
    dw_missed_frames: u32,
    b_required_post_processing: u32,
    b_direct_capture: u32,
}

#[repr(C)]
#[derive(Copy, Clone)]
struct ToSysGrabFrameParams {
    dw_version: u32,
    dw_flags: u32,
    p_frame_grab_info: *mut FrameGrabInfo,
    dw_timeout_ms: u32,
}

macro_rules! nvfbc_fns {
    ($($field:ident : $name:literal => fn($($arg:ty),*) $(-> $ret:ty)?;)+) => {
        struct NvfbcFns {
            $($field: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)+
        }
        impl NvfbcFns {
            unsafe fn load(lib: &Library) -> io::Result<Self> {
                $(
                    let $field = {
                        let sym: Symbol<unsafe extern "C" fn($($arg),*) $(-> $ret)?> =
                            unsafe { lib.get(concat!($name, "\0").as_bytes()) }
                                .map_err(|e| io::Error::other(format!("dlsym {}: {e}", $name)))?;
                        *sym
                    };
                )+
                Ok(NvfbcFns { $($field,)+ })
            }
        }
    };
}

nvfbc_fns! {
    create_handle: "NvFBCCreateHandle" => fn(*mut u64, *mut CreateHandleParams) -> NvfbcStatus;
    destroy_handle: "NvFBCDestroyHandle" => fn(u64, *mut DestroyHandleParams) -> NvfbcStatus;
    get_last_error_str: "NvFBCGetLastErrorStr" => fn(u64) -> *const i8;
    create_capture_session: "NvFBCCreateCaptureSession" => fn(u64, *mut CreateCaptureSessionParams) -> NvfbcStatus;
    destroy_capture_session: "NvFBCDestroyCaptureSession" => fn(u64, *mut DestroyCaptureSessionParams) -> NvfbcStatus;
    to_sys_set_up: "NvFBCToSysSetUp" => fn(u64, *mut ToSysSetupParams) -> NvfbcStatus;
    to_sys_grab_frame: "NvFBCToSysGrabFrame" => fn(u64, *mut ToSysGrabFrameParams) -> NvfbcStatus;
}

struct NvfbcCapturer {
    _lib: Library,
    fns: NvfbcFns,
    handle: u64,
    // NvFBC writes the (re)allocated buffer pointer here itself; it must
    // stay at a fixed address for the capturer's whole lifetime.
    buffer_ptr: Box<*mut c_void>,
}

impl NvfbcCapturer {
    fn last_error(&self) -> String {
        let ptr = unsafe { (self.fns.get_last_error_str)(self.handle) };
        if ptr.is_null() {
            return "(no error string)".to_string();
        }
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }

    fn check(&self, status: NvfbcStatus, what: &str) -> io::Result<()> {
        if status == NVFBC_SUCCESS {
            return Ok(());
        }
        Err(io::Error::other(format!(
            "{what} failed: status {status}: {}",
            self.last_error()
        )))
    }

    fn new() -> io::Result<Self> {
        let lib = unsafe { Library::new("libnvidia-fbc.so.1") }
            .or_else(|_| unsafe { Library::new("libnvidia-fbc.so") })
            .map_err(|e| io::Error::other(format!("failed to load libnvidia-fbc: {e}")))?;
        let fns = unsafe { NvfbcFns::load(&lib)? };

        let mut handle = 0u64;
        let mut create_params = CreateHandleParams {
            dw_version: nvfbc_struct_version(std::mem::size_of::<CreateHandleParams>(), 2),
            private_data: MAGIC_PRIVATE_DATA.as_ptr() as *const c_void,
            private_data_size: std::mem::size_of_val(&MAGIC_PRIVATE_DATA) as u32,
            b_externally_managed_context: NVFBC_FALSE,
            glx_ctx: std::ptr::null_mut(),
            glx_fb_config: std::ptr::null_mut(),
        };
        let status = unsafe { (fns.create_handle)(&mut handle, &mut create_params) };
        if status != NVFBC_SUCCESS {
            return Err(io::Error::other(format!(
                "NvFBCCreateHandle failed: status {status}"
            )));
        }

        let this = NvfbcCapturer {
            _lib: lib,
            fns,
            handle,
            buffer_ptr: Box::new(std::ptr::null_mut()),
        };

        let mut session_params = CreateCaptureSessionParams {
            dw_version: nvfbc_struct_version(std::mem::size_of::<CreateCaptureSessionParams>(), 6),
            e_capture_type: NVFBC_CAPTURE_TO_SYS,
            e_tracking_type: NVFBC_TRACKING_DEFAULT,
            dw_output_id: 0,
            capture_box: Box_ {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
            frame_size: Size { w: 0, h: 0 },
            // The RDP path renders its own client-side cursor from pointer
            // PDUs (same as the DRM primary-plane path, which never sees a
            // hardware cursor plane either) - don't also bake one into the
            // frame.
            b_with_cursor: NVFBC_FALSE,
            b_disable_auto_modeset_recovery: NVFBC_FALSE,
            b_round_frame_size: NVFBC_FALSE,
            dw_sampling_rate_ms: 16,
            b_push_model: NVFBC_FALSE,
            b_allow_direct_capture: NVFBC_FALSE,
        };
        this.check(
            unsafe { (this.fns.create_capture_session)(this.handle, &mut session_params) },
            "NvFBCCreateCaptureSession",
        )?;

        let mut setup_params = ToSysSetupParams {
            dw_version: nvfbc_struct_version(std::mem::size_of::<ToSysSetupParams>(), 3),
            e_buffer_format: NVFBC_BUFFER_FORMAT_BGRA,
            pp_buffer: this.buffer_ptr.as_ref() as *const *mut c_void as *mut *mut c_void,
            b_with_diff_map: NVFBC_FALSE,
            pp_diff_map: std::ptr::null_mut(),
            dw_diff_map_scaling_factor: 0,
            diff_map_size: Size { w: 0, h: 0 },
        };
        this.check(
            unsafe { (this.fns.to_sys_set_up)(this.handle, &mut setup_params) },
            "NvFBCToSysSetUp",
        )?;

        Ok(this)
    }

    fn grab(&self) -> io::Result<(u32, u32, Vec<u8>)> {
        let mut info: FrameGrabInfo = unsafe { std::mem::zeroed() };
        let mut params = ToSysGrabFrameParams {
            dw_version: nvfbc_struct_version(std::mem::size_of::<ToSysGrabFrameParams>(), 2),
            dw_flags: NVFBC_TOSYS_GRAB_FLAGS_NOWAIT,
            p_frame_grab_info: &mut info,
            dw_timeout_ms: 0,
        };
        self.check(
            unsafe { (self.fns.to_sys_grab_frame)(self.handle, &mut params) },
            "NvFBCToSysGrabFrame",
        )?;

        // Safety: NvFBC wrote a fresh buffer pointer through `buffer_ptr`
        // during setup/grab, valid for `info.dw_byte_size` bytes until the
        // next grab call.
        let data = unsafe {
            std::slice::from_raw_parts((*self.buffer_ptr) as *const u8, info.dw_byte_size as usize)
        }
        .to_vec();

        Ok((info.dw_width, info.dw_height, data))
    }
}

impl Drop for NvfbcCapturer {
    fn drop(&mut self) {
        let mut destroy_session = DestroyCaptureSessionParams {
            dw_version: nvfbc_struct_version(std::mem::size_of::<DestroyCaptureSessionParams>(), 1),
        };
        unsafe { (self.fns.destroy_capture_session)(self.handle, &mut destroy_session) };
        let mut destroy_handle = DestroyHandleParams {
            dw_version: nvfbc_struct_version(std::mem::size_of::<DestroyHandleParams>(), 1),
        };
        unsafe { (self.fns.destroy_handle)(self.handle, &mut destroy_handle) };
    }
}

type GrabReply = io::Result<(u32, u32, Vec<u8>)>;

// NvFBC binds its internal OpenGL context to whichever OS thread creates
// the capture session, and every later call (including grab) must come
// from that exact thread - a different thread gets back "the context is
// bound to a different thread" (confirmed against real hardware). Our
// caller is a tokio blocking task, which can land on a different pool
// thread every time, so the whole capturer instead lives on one dedicated
// thread we spawn ourselves, and every request is proxied to it.
static WORKER: OnceLock<std::sync::mpsc::Sender<std::sync::mpsc::Sender<GrabReply>>> =
    OnceLock::new();

fn worker() -> &'static std::sync::mpsc::Sender<std::sync::mpsc::Sender<GrabReply>> {
    WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<std::sync::mpsc::Sender<GrabReply>>();
        std::thread::spawn(move || {
            let capturer = NvfbcCapturer::new();
            for reply_tx in rx {
                let result = match &capturer {
                    Ok(c) => c.grab(),
                    Err(e) => Err(io::Error::other(format!("NvFBC init failed: {e}"))),
                };
                let _ = reply_tx.send(result);
            }
        });
        tx
    })
}

/// Grab the current frame via NvFBC as tightly-packed BGRX8888 bytes
/// (`stride == width * 4`) - a fallback for when [`crate::capture`]'s
/// DRM/KMS path can't find a bound CRTC at all (the proprietary NVIDIA
/// driver under Xorg, in practice).
pub fn capture_bgrx() -> GrabReply {
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    worker()
        .send(reply_tx)
        .map_err(|_| io::Error::other("NvFBC worker thread is gone"))?;
    reply_rx
        .recv()
        .map_err(|_| io::Error::other("NvFBC worker thread dropped the reply"))?
}
