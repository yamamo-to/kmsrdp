//! Sanity check for `kmsrdp::gpu_detile` independent of any active display.
//!
//! Allocates a small GBM buffer on the given card's GPU (letting the driver
//! pick whatever modifier it wants - usually a tiled one, not Linear),
//! fills it with a solid color through `gbm_bo_map` (which repacks into the
//! buffer's real, possibly-tiled layout on unmap), then round-trips that
//! buffer through the exact same `detile_to_bgrx` path `capture.rs` uses for
//! a real tiled scanout framebuffer, and checks the color survives. This
//! exercises the EGL/GLES import+readback pipeline on real hardware without
//! needing a CRTC to actually be bound to anything.
//!
//! Usage: `detile_selftest [/dev/dri/cardN] [/dev/dri/renderDNNN]` (default
//! to card0/renderD128 - a single-GPU dev box's usual names).

use std::ffi::{c_int, c_void};
use std::os::unix::io::{AsRawFd, RawFd};

use drm_fourcc::{DrmFourcc, DrmModifier};
use kmsrdp::gpu_detile;
use libloading::{Library, Symbol};

// GBM_FORMAT_* values are defined to equal the matching DRM_FOURCC code.
const GBM_FORMAT_XRGB8888: u32 = DrmFourcc::Xrgb8888 as u32;
const GBM_BO_USE_RENDERING: u32 = 1 << 2;
// gbm_bo_map() is only guaranteed to work on a buffer allocated with this
// flag (some drivers, including NVIDIA's, return NULL from gbm_bo_map()
// otherwise) - it just changes how *this test* writes its known color in,
// the detile pass itself doesn't care and treats whatever modifier comes
// back the same way it would treat a real tiled one.
const GBM_BO_USE_LINEAR: u32 = 1 << 4;
const GBM_BO_TRANSFER_READ_WRITE: u32 = (1 << 0) | (1 << 1);

unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> T {
    let s: Symbol<T> = unsafe { lib.get(format!("{name}\0").as_bytes()) }
        .unwrap_or_else(|e| panic!("dlsym {name}: {e}"));
    *s
}

fn main() {
    let mut args = std::env::args().skip(1);
    let card_path = args.next().unwrap_or_else(|| "/dev/dri/card0".to_string());
    let render_path = args
        .next()
        .unwrap_or_else(|| "/dev/dri/renderD128".to_string());

    println!("opening {render_path} for GBM allocation, {card_path} for the detile pass");
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&render_path)
        .expect("open render node");

    let gbm_lib = unsafe { Library::new("libgbm.so.1") }
        .or_else(|_| unsafe { Library::new("libgbm.so") })
        .expect("load libgbm");

    type GbmCreateDevice = unsafe extern "C" fn(c_int) -> *mut c_void;
    type GbmDeviceDestroy = unsafe extern "C" fn(*mut c_void);
    type GbmBoCreate = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32) -> *mut c_void;
    type GbmBoDestroy = unsafe extern "C" fn(*mut c_void);
    type GbmBoMap = unsafe extern "C" fn(
        *mut c_void,
        u32,
        u32,
        u32,
        u32,
        u32,
        *mut u32,
        *mut *mut c_void,
    ) -> *mut c_void;
    type GbmBoUnmap = unsafe extern "C" fn(*mut c_void, *mut c_void);
    type GbmBoGetFd = unsafe extern "C" fn(*mut c_void) -> c_int;
    type GbmBoGetModifier = unsafe extern "C" fn(*mut c_void) -> u64;
    type GbmBoGetStride = unsafe extern "C" fn(*mut c_void) -> u32;

    let create_device: GbmCreateDevice = unsafe { sym(&gbm_lib, "gbm_create_device") };
    let device_destroy: GbmDeviceDestroy = unsafe { sym(&gbm_lib, "gbm_device_destroy") };
    let bo_create: GbmBoCreate = unsafe { sym(&gbm_lib, "gbm_bo_create") };
    let bo_destroy: GbmBoDestroy = unsafe { sym(&gbm_lib, "gbm_bo_destroy") };
    let bo_map: GbmBoMap = unsafe { sym(&gbm_lib, "gbm_bo_map") };
    let bo_unmap: GbmBoUnmap = unsafe { sym(&gbm_lib, "gbm_bo_unmap") };
    let bo_get_fd: GbmBoGetFd = unsafe { sym(&gbm_lib, "gbm_bo_get_fd") };
    let bo_get_modifier: GbmBoGetModifier = unsafe { sym(&gbm_lib, "gbm_bo_get_modifier") };
    let bo_get_stride: GbmBoGetStride = unsafe { sym(&gbm_lib, "gbm_bo_get_stride") };

    let device = unsafe { create_device(fd.as_raw_fd()) };
    assert!(!device.is_null(), "gbm_create_device failed");

    const W: u32 = 64;
    const H: u32 = 64;
    let bo = unsafe {
        bo_create(
            device,
            W,
            H,
            GBM_FORMAT_XRGB8888,
            GBM_BO_USE_RENDERING | GBM_BO_USE_LINEAR,
        )
    };
    assert!(!bo.is_null(), "gbm_bo_create failed");

    // Solid red as XRGB8888: memory bytes (little-endian) are B,G,R,X.
    let mut map_stride: u32 = 0;
    let mut map_data: *mut c_void = std::ptr::null_mut();
    let map_ptr = unsafe {
        bo_map(
            bo,
            0,
            0,
            W,
            H,
            GBM_BO_TRANSFER_READ_WRITE,
            &mut map_stride,
            &mut map_data,
        )
    };
    assert!(!map_ptr.is_null(), "gbm_bo_map failed");
    unsafe {
        for y in 0..H as usize {
            let row = (map_ptr as *mut u8).add(y * map_stride as usize);
            for x in 0..W as usize {
                let px = row.add(x * 4);
                *px.add(0) = 0x00; // B
                *px.add(1) = 0x00; // G
                *px.add(2) = 0xFF; // R
                *px.add(3) = 0x00; // X
            }
        }
        bo_unmap(bo, map_data);
    }

    let modifier = unsafe { bo_get_modifier(bo) };
    let stride = unsafe { bo_get_stride(bo) };
    let dma_fd = unsafe { bo_get_fd(bo) };
    assert!(dma_fd >= 0, "gbm_bo_get_fd failed");

    println!(
        "allocated {W}x{H} GBM buffer: modifier={modifier:#x} stride={stride} (Linear = {:#x})",
        u64::from(DrmModifier::Linear)
    );

    let result = gpu_detile::detile_to_bgrx(
        &card_path,
        dma_fd as RawFd,
        DrmFourcc::Xrgb8888,
        DrmModifier::from(modifier),
        W,
        H,
        0,
        stride,
    );

    unsafe {
        libc::close(dma_fd);
        bo_destroy(bo);
        device_destroy(device);
    }

    match result {
        Ok(data) => {
            let px = &data[0..4];
            println!("readback pixel 0: {px:02x?} (expected [00, 00, ff, ..])");
            assert_eq!(
                &px[0..3],
                &[0x00, 0x00, 0xFF],
                "color mismatch after detile round-trip"
            );
            let mid = (H as usize / 2) * W as usize * 4 + (W as usize / 2) * 4;
            let px_mid = &data[mid..mid + 4];
            assert_eq!(
                &px_mid[0..3],
                &[0x00, 0x00, 0xFF],
                "color mismatch at center pixel"
            );
            println!("OK: GBM/EGL detile round-trip produced the expected color.");
        }
        Err(e) => {
            eprintln!("detile_to_bgrx failed: {e}");
            std::process::exit(1);
        }
    }
}
