//! NVIDIA NVENC H.264 encoder via CUDA + the `nvenc` crate bindings.
//!
//! Enabled with the `nvenc` feature. Loads `libcuda.so.1` and
//! `libnvidia-encode.so.1` at runtime.

use std::ffi::{c_int, c_void};
use std::ptr;

use libloading::Library;
use nvenc::sys::enums::{
    NVencBufferFormat, NVencDeviceType, NVencMemoryHeap, NVencPicStruct, NVencPicType,
};
use nvenc::sys::function_table::NVencFunctionList;
use nvenc::sys::guids::{NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P3_GUID};
use nvenc::sys::structs::{
    NV_ENC_CREATE_BITSTREAM_BUFFER_VER, NV_ENC_CREATE_INPUT_BUFFER_VER,
    NV_ENC_INITIALIZE_PARAMS_VER, NV_ENC_LOCK_BITSTREAM_VER, NV_ENC_LOCK_INPUT_BUFFER_VER,
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER, NV_ENC_PIC_PARAMS_VER, NVencCreateBitstreamBuffer,
    NVencCreateInputBuffer, NVencInitializeParams, NVencLockBitStream, NVencLockInputBuffer,
    NVencOpenEncodeSessionExParams, NVencPicParams,
};
use nvenc::sys::version::NVENC_API_VERSION;
use nvenc::{NVENCLibrary, nvenc_init};

use crate::encoder::{EncodedAu, H264Encoder, align16};

struct CudaCtx {
    _lib: Library,
    ctx: *mut c_void,
    destroy: unsafe extern "C" fn(*mut c_void) -> u32,
}

impl Drop for CudaCtx {
    fn drop(&mut self) {
        if !self.ctx.is_null() {
            unsafe { (self.destroy)(self.ctx) };
            self.ctx = ptr::null_mut();
        }
    }
}

impl CudaCtx {
    fn create() -> Result<Self, String> {
        let lib = unsafe { Library::new("libcuda.so.1") }
            .map_err(|e| format!("NVENC: load libcuda.so.1: {e}"))?;
        unsafe {
            let cu_init: libloading::Symbol<unsafe extern "C" fn(u32) -> u32> =
                lib.get(b"cuInit").map_err(|e| e.to_string())?;
            if cu_init(0) != 0 {
                return Err("cuInit failed".into());
            }
            let cu_device_get: libloading::Symbol<unsafe extern "C" fn(*mut c_int, c_int) -> u32> =
                lib.get(b"cuDeviceGet").map_err(|e| e.to_string())?;
            let mut device = 0;
            if cu_device_get(&mut device, 0) != 0 {
                return Err("cuDeviceGet failed".into());
            }
            let cu_ctx_create: libloading::Symbol<
                unsafe extern "C" fn(*mut *mut c_void, u32, c_int) -> u32,
            > = lib
                .get(b"cuCtxCreate_v2")
                .or_else(|_| lib.get(b"cuCtxCreate"))
                .map_err(|e| e.to_string())?;
            let mut ctx = ptr::null_mut();
            if cu_ctx_create(&mut ctx, 0, device) != 0 {
                return Err("cuCtxCreate failed".into());
            }
            let destroy: libloading::Symbol<unsafe extern "C" fn(*mut c_void) -> u32> = lib
                .get(b"cuCtxDestroy_v2")
                .or_else(|_| lib.get(b"cuCtxDestroy"))
                .map_err(|e| e.to_string())?;
            Ok(Self {
                destroy: *destroy,
                _lib: lib,
                ctx,
            })
        }
    }
}

struct Session {
    encoder: *mut c_void,
    fl: NVencFunctionList,
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.encoder.is_null() {
            unsafe { (self.fl.nvenc_destroy_encoder)(self.encoder) };
            self.encoder = ptr::null_mut();
        }
    }
}

struct Buffers {
    input: *mut c_void,
    output: *mut c_void,
    coded_w: u16,
    coded_h: u16,
}

/// NVIDIA NVENC hardware H.264 encoder.
pub struct NvencH264Encoder {
    _cuda: CudaCtx,
    /// Kept so the dynamic library stays mapped.
    _nvenc_lib: &'static NVENCLibrary,
    session: Session,
    buffers: Option<Buffers>,
    frame_idx: u32,
    qp: u8,
}

unsafe impl Send for NvencH264Encoder {}

impl NvencH264Encoder {
    /// Load CUDA + NVENC and open an encode session that supports H.264.
    pub fn probe() -> Result<Self, String> {
        let cuda = CudaCtx::create()?;
        let nvenc_lib = nvenc_init().map_err(|e| format!("NVENC init: {e}"))?;
        let fl = nvenc_lib
            .create_instance()
            .map_err(|e| format!("NVENC CreateInstance: {e:?}"))?;

        let mut params: NVencOpenEncodeSessionExParams = unsafe { std::mem::zeroed() };
        params.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        params.device_type = NVencDeviceType::Cuda;
        params.device = cuda.ctx;
        params.api_version = NVENC_API_VERSION;
        let mut encoder = ptr::null_mut();
        unsafe { (fl.nvenc_open_encode_session_ex)(&mut params, &mut encoder) }
            .into_error()
            .map_err(|e| format!("NVENC open session: {e:?}"))?;
        if encoder.is_null() {
            return Err("NVENC open session returned null".into());
        }

        let session = Session { encoder, fl };
        // Confirm H.264 is advertised.
        let mut count = 0u32;
        unsafe { (session.fl.nvenc_get_encoder_guid_count)(session.encoder, &mut count) }
            .into_error()
            .map_err(|e| format!("NVENC guid count: {e:?}"))?;
        let mut guids = vec![nvenc::sys::structs::Guid::default(); count as usize];
        unsafe {
            (session.fl.nvenc_get_encoder_guids)(
                session.encoder,
                guids.as_mut_ptr(),
                count,
                &mut count,
            )
        }
        .into_error()
        .map_err(|e| format!("NVENC guids: {e:?}"))?;
        guids.truncate(count as usize);
        if !guids.contains(&NV_ENC_CODEC_H264_GUID) {
            return Err("NVENC: H.264 codec not available".into());
        }

        Ok(Self {
            _cuda: cuda,
            _nvenc_lib: nvenc_lib,
            session,
            buffers: None,
            frame_idx: 0,
            qp: 22,
        })
    }

    fn destroy_buffers(&mut self) {
        if let Some(b) = self.buffers.take() {
            let _ = unsafe {
                (self.session.fl.nvenc_destroy_input_buffer)(self.session.encoder, b.input)
            };
            let _ = unsafe {
                (self.session.fl.nvenc_destory_bit_stream_buffer)(self.session.encoder, b.output)
            };
        }
    }

    fn ensure_buffers(&mut self, coded_w: u16, coded_h: u16) -> Result<(), String> {
        if let Some(b) = &self.buffers
            && b.coded_w == coded_w
            && b.coded_h == coded_h
        {
            return Ok(());
        }
        self.destroy_buffers();

        let mut init: NVencInitializeParams = unsafe { std::mem::zeroed() };
        init.version = NV_ENC_INITIALIZE_PARAMS_VER;
        init.encode_guid = NV_ENC_CODEC_H264_GUID;
        init.preset_guid = NV_ENC_PRESET_P3_GUID;
        init.encode_width = u32::from(coded_w);
        init.encode_height = u32::from(coded_h);
        init.dar_width = u32::from(coded_w);
        init.dar_height = u32::from(coded_h);
        init.frame_rate_num = 30;
        init.frame_rate_den = 1;
        init.enable_ptd = 1;
        unsafe { (self.session.fl.nvenc_initialize_encoder)(self.session.encoder, &mut init) }
            .into_error()
            .map_err(|e| format!("NVENC initialize: {e:?}"))?;

        let mut input = NVencCreateInputBuffer {
            version: NV_ENC_CREATE_INPUT_BUFFER_VER,
            width: u32::from(coded_w),
            height: u32::from(coded_h),
            memory_heap: NVencMemoryHeap::AutoSelect,
            buffer_fmt: NVencBufferFormat::ABGR,
            ..unsafe { std::mem::zeroed() }
        };
        unsafe { (self.session.fl.nvenc_create_input_buffer)(self.session.encoder, &mut input) }
            .into_error()
            .map_err(|e| format!("NVENC create input: {e:?}"))?;

        let mut output: NVencCreateBitstreamBuffer = unsafe { std::mem::zeroed() };
        output.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
        unsafe {
            (self.session.fl.nvenc_create_bit_stream_buffer)(self.session.encoder, &mut output)
        }
        .into_error()
        .map_err(|e| format!("NVENC create bitstream: {e:?}"))?;

        self.buffers = Some(Buffers {
            input: input.input_buffer,
            output: output.bitstream_buffer,
            coded_w,
            coded_h,
        });
        Ok(())
    }
}

impl Drop for NvencH264Encoder {
    fn drop(&mut self) {
        self.destroy_buffers();
    }
}

impl H264Encoder for NvencH264Encoder {
    fn encode_bgrx(
        &mut self,
        width: u16,
        height: u16,
        stride: usize,
        pixels: &[u8],
        force_idr: bool,
    ) -> Result<EncodedAu, String> {
        if width == 0 || height == 0 {
            return Err("empty frame".into());
        }
        let coded_w = align16(width).max(16);
        let coded_h = align16(height).max(16);
        self.ensure_buffers(coded_w, coded_h)?;
        let buffers = self.buffers.as_ref().ok_or("NVENC buffers missing")?;

        let mut lock: NVencLockInputBuffer = unsafe { std::mem::zeroed() };
        lock.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
        lock.input_buffer = buffers.input;
        unsafe { (self.session.fl.nvenc_lock_input_buffer)(self.session.encoder, &mut lock) }
            .into_error()
            .map_err(|e| format!("NVENC lock input: {e:?}"))?;

        let pitch = lock.pitch as usize;
        let dst = lock.buffer_data_ptr as *mut u8;
        let w = usize::from(width);
        let h = usize::from(height);
        unsafe {
            for row in 0..h {
                let src_row = pixels.as_ptr().add(row * stride);
                let dst_row = dst.add(row * pitch);
                for col in 0..w {
                    let s = src_row.add(col * 4);
                    let d = dst_row.add(col * 4);
                    // BGRX → ABGR
                    *d = *s.add(2);
                    *d.add(1) = *s.add(1);
                    *d.add(2) = *s;
                    *d.add(3) = 0xff;
                }
            }
            for row in h..usize::from(coded_h) {
                ptr::write_bytes(dst.add(row * pitch), 0, usize::from(coded_w) * 4);
            }
        }
        unsafe { (self.session.fl.nvenc_unlock_input_buffer)(self.session.encoder, buffers.input) }
            .into_error()
            .map_err(|e| format!("NVENC unlock input: {e:?}"))?;

        let mut pic = std::mem::MaybeUninit::<NVencPicParams>::zeroed();
        unsafe {
            let p = &mut *pic.as_mut_ptr();
            p.version = NV_ENC_PIC_PARAMS_VER;
            p.input_width = u32::from(coded_w);
            p.input_height = u32::from(coded_h);
            p.input_pitch = lock.pitch;
            if force_idr {
                p.encode_pic_flags = 0x1; // NV_ENC_PIC_FLAG_FORCEIDR
            }
            p.frame_idx = self.frame_idx;
            p.input_buffer = buffers.input;
            p.output_bitstream = buffers.output;
            p.buffer_format = NVencBufferFormat::ABGR;
            p.picture_struct = NVencPicStruct::Frame;
            p.picture_type = if force_idr {
                NVencPicType::IDR
            } else {
                NVencPicType::P
            };
        }

        unsafe { (self.session.fl.nvenc_encode_picture)(self.session.encoder, &mut pic) }
            .into_error()
            .map_err(|e| format!("NVENC encode: {e:?}"))?;

        let mut bs = std::mem::MaybeUninit::<NVencLockBitStream>::zeroed();
        unsafe {
            let b = &mut *bs.as_mut_ptr();
            b.version = NV_ENC_LOCK_BITSTREAM_VER;
            b.output_bit_stream = buffers.output;
        }
        unsafe { (self.session.fl.nvenc_lock_bit_stream)(self.session.encoder, &mut bs) }
            .into_error()
            .map_err(|e| format!("NVENC lock bitstream: {e:?}"))?;
        let bs = unsafe { bs.assume_init() };

        let mut annex_b = Vec::new();
        if !bs.bitstream_buffer.is_null() && bs.bitstream_size_in_bytes > 0 {
            let slice = unsafe {
                std::slice::from_raw_parts(
                    bs.bitstream_buffer as *const u8,
                    bs.bitstream_size_in_bytes as usize,
                )
            };
            annex_b.extend_from_slice(slice);
        }
        let _ = unsafe {
            (self.session.fl.nvenc_unlock_bit_stream)(self.session.encoder, buffers.output)
        };
        self.frame_idx = self.frame_idx.wrapping_add(1);

        if annex_b.is_empty() {
            return Err("NVENC produced empty bitstream".into());
        }
        Ok(EncodedAu {
            annex_b,
            qp: self.qp,
        })
    }

    fn reset(&mut self) {
        self.destroy_buffers();
        self.frame_idx = 0;
    }
}
