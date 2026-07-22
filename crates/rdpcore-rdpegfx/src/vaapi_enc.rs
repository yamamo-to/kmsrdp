//! VA-API H.264 encoder (IDR frames + Annex B SPS/PPS).
//!
//! Enabled with the `vaapi` feature. Needs `libva` / `libva-drm` at link time
//! (`libva-devel`) and a driver that exposes H.264 EncSlice or EncSliceLP.
//! Building also requires clang (cros-libva bindgen).

use std::rc::Rc;

use libva::{
    BufferType, Display, EncCodedBuffer, EncMiscParameter, EncMiscParameterRateControl,
    EncPictureParameter, EncPictureParameterBufferH264, EncSequenceParameter,
    EncSequenceParameterBufferH264, EncSliceParameter, EncSliceParameterBufferH264,
    H264EncPicFields, H264EncSeqFields, H264VuiFields, Image, MappedCodedBuffer, Picture,
    PictureH264, RcFlags, UsageHint, VAConfigAttrib, VAConfigAttribType, VAEntrypoint, VAProfile,
    VA_FOURCC_NV12, VA_INVALID_ID, VA_INVALID_SURFACE, VA_RT_FORMAT_YUV420,
};

use crate::encoder::{EncodedAu, H264Encoder, align16, bgrx_to_nv12};
use crate::h264_headers;

struct Session {
    display: Rc<Display>,
    context: Rc<libva::Context>,
    surface: libva::Surface<()>,
    coded: EncCodedBuffer,
    coded_w: u16,
    coded_h: u16,
}

/// VA-API hardware H.264 encoder.
pub struct VaapiH264Encoder {
    display: Rc<Display>,
    entrypoint: VAEntrypoint::Type,
    session: Option<Session>,
    qp: u8,
}

// Accessed only under GfxSession's Mutex.
unsafe impl Send for VaapiH264Encoder {}

impl VaapiH264Encoder {
    /// Open a DRM VA display, verify H.264 encode, and smoke-encode one frame.
    pub fn probe() -> Result<Self, String> {
        let display = Display::open().ok_or_else(|| "VAAPI: no DRM display".to_string())?;
        let entrypoint = pick_entrypoint(display.as_ref())?;
        let _ = make_config(&display, entrypoint)?;
        let mut enc = Self {
            display,
            entrypoint,
            session: None,
            qp: 22,
        };
        let pixels = vec![0u8; 64 * 64 * 4];
        enc.encode_bgrx(64, 64, 64 * 4, &pixels, true)
            .map_err(|e| format!("VAAPI smoke encode failed: {e}"))?;
        enc.reset();
        Ok(enc)
    }

    fn ensure_session(&mut self, coded_w: u16, coded_h: u16) -> Result<(), String> {
        if let Some(s) = &self.session
            && s.coded_w == coded_w
            && s.coded_h == coded_h
        {
            return Ok(());
        }
        self.session = None;
        let config = make_config(&self.display, self.entrypoint)?;
        let width = u32::from(coded_w);
        let height = u32::from(coded_h);
        let mut surfaces = self
            .display
            .create_surfaces(
                VA_RT_FORMAT_YUV420,
                None,
                width,
                height,
                Some(UsageHint::USAGE_HINT_ENCODER),
                vec![()],
            )
            .map_err(|e| format!("VAAPI create_surfaces: {e}"))?;
        let context = self
            .display
            .create_context(&config, width, height, Some(&surfaces), true)
            .map_err(|e| format!("VAAPI create_context: {e}"))?;
        let surface = surfaces
            .pop()
            .ok_or_else(|| "VAAPI: no surface".to_string())?;
        let coded_size = (usize::from(coded_w) * usize::from(coded_h)).max(64 * 1024);
        let coded = context
            .create_enc_coded(coded_size)
            .map_err(|e| format!("VAAPI coded buffer: {e}"))?;
        self.session = Some(Session {
            display: Rc::clone(&self.display),
            context,
            surface,
            coded,
            coded_w,
            coded_h,
        });
        Ok(())
    }
}

fn pick_entrypoint(display: &Display) -> Result<VAEntrypoint::Type, String> {
    let profile = VAProfile::VAProfileH264ConstrainedBaseline;
    let entrypoints = display
        .query_config_entrypoints(profile)
        .map_err(|e| format!("VAAPI query entrypoints: {e}"))?;
    if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSliceLP) {
        Ok(VAEntrypoint::VAEntrypointEncSliceLP)
    } else if entrypoints.contains(&VAEntrypoint::VAEntrypointEncSlice) {
        Ok(VAEntrypoint::VAEntrypointEncSlice)
    } else {
        Err("VAAPI: no H.264 EncSlice entrypoint".into())
    }
}

fn make_config(
    display: &Rc<Display>,
    entrypoint: VAEntrypoint::Type,
) -> Result<libva::Config, String> {
    let profile = VAProfile::VAProfileH264ConstrainedBaseline;
    let mut attrs = vec![VAConfigAttrib {
        type_: VAConfigAttribType::VAConfigAttribRTFormat,
        value: 0,
    }];
    display
        .get_config_attributes(profile, entrypoint, &mut attrs)
        .map_err(|e| format!("VAAPI get_config_attributes: {e}"))?;
    if attrs[0].value & VA_RT_FORMAT_YUV420 == 0 {
        return Err("VAAPI: YUV420 RT format unsupported".into());
    }
    attrs[0].value = VA_RT_FORMAT_YUV420;
    display
        .create_config(attrs, profile, entrypoint)
        .map_err(|e| format!("VAAPI create_config: {e}"))
}

fn upload_nv12(
    display: &Rc<Display>,
    surface: &libva::Surface<()>,
    width: u32,
    height: u32,
    nv12: &[u8],
) -> Result<(), String> {
    let mut image = match Image::derive_from(surface, (width, height)) {
        Ok(img) => img,
        Err(_) => {
            let image_fmts = display
                .query_image_formats()
                .map_err(|e| format!("VAAPI image formats: {e}"))?;
            let image_fmt = image_fmts
                .into_iter()
                .find(|f| f.fourcc == VA_FOURCC_NV12)
                .ok_or_else(|| "VAAPI: no NV12 image format".to_string())?;
            Image::create_from(surface, image_fmt, (width, height), (width, height))
                .map_err(|e| format!("VAAPI create image: {e}"))?
        }
    };
    let va_image = *image.image();
    let dest = image.as_mut();
    let w = width as usize;
    let h = height as usize;
    let mut src = nv12;
    let mut dst = &mut dest[va_image.offsets[0] as usize..];
    for _ in 0..h {
        dst[..w].copy_from_slice(&src[..w]);
        dst = &mut dst[va_image.pitches[0] as usize..];
        src = &src[w..];
    }
    let mut src = &nv12[w * h..];
    let mut dst = &mut dest[va_image.offsets[1] as usize..];
    for _ in 0..(h / 2) {
        dst[..w].copy_from_slice(&src[..w]);
        dst = &mut dst[va_image.pitches[1] as usize..];
        src = &src[w..];
    }
    drop(image);
    surface
        .sync()
        .map_err(|e| format!("VAAPI surface sync: {e}"))?;
    Ok(())
}

impl H264Encoder for VaapiH264Encoder {
    fn encode_bgrx(
        &mut self,
        width: u16,
        height: u16,
        stride: usize,
        pixels: &[u8],
        _force_idr: bool,
    ) -> Result<EncodedAu, String> {
        if width == 0 || height == 0 {
            return Err("empty frame".into());
        }
        let coded_w = align16(width).max(16);
        let coded_h = align16(height).max(16);
        let nv12 = bgrx_to_nv12(width, height, stride, pixels, coded_w, coded_h)?;
        self.ensure_session(coded_w, coded_h)?;

        let session = self.session.as_mut().ok_or("VAAPI session missing")?;
        upload_nv12(
            &session.display,
            &session.surface,
            u32::from(coded_w),
            u32::from(coded_h),
            &nv12,
        )?;

        let surface_id = session.surface.id();
        let mb_w = u32::from(coded_w) / 16;
        let mb_h = u32::from(coded_h) / 16;
        let num_mbs = mb_w * mb_h;

        let seq_fields = H264EncSeqFields::new(1, 1, 0, 0, 0, 1, 0, 2, 0);
        let sps = session
            .context
            .create_buffer(BufferType::EncSequenceParameter(EncSequenceParameter::H264(
                EncSequenceParameterBufferH264::new(
                    0,
                    10,
                    10,
                    30,
                    1,
                    0,
                    1,
                    mb_w as u16,
                    mb_h as u16,
                    &seq_fields,
                    0,
                    0,
                    0,
                    0,
                    0,
                    [0; 256],
                    None,
                    Some(H264VuiFields::new(1, 1, 0, 0, 0, 1, 0, 0)),
                    255,
                    1,
                    1,
                    1,
                    60,
                ),
            )))
            .map_err(|e| format!("VAAPI SPS buffer: {e}"))?;

        let invalid_pic = || PictureH264::new(VA_INVALID_ID, 0, VA_INVALID_SURFACE, 0, 0);
        let ref_frames: [PictureH264; 16] = std::array::from_fn(|_| invalid_pic());

        let bps = (u32::from(coded_w) * u32::from(coded_h) / 2).max(500_000);
        let rc = session
            .context
            .create_buffer(BufferType::EncMiscParameter(EncMiscParameter::RateControl(
                EncMiscParameterRateControl::new(
                    bps,
                    50,
                    1000,
                    26,
                    1,
                    0,
                    RcFlags::new(0, 0, 0, 0, 0, 0, 0, 0, 0),
                    0,
                    51,
                    0,
                    0,
                ),
            )))
            .map_err(|e| format!("VAAPI RC buffer: {e}"))?;

        let pps = session
            .context
            .create_buffer(BufferType::EncPictureParameter(EncPictureParameter::H264(
                EncPictureParameterBufferH264::new(
                    PictureH264::new(surface_id, 0, 0, 0, 0),
                    ref_frames,
                    session.coded.id(),
                    0,
                    0,
                    0,
                    0,
                    26,
                    0,
                    0,
                    0,
                    0,
                    &H264EncPicFields::new(1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0),
                ),
            )))
            .map_err(|e| format!("VAAPI PPS buffer: {e}"))?;

        let ref_list_0: [PictureH264; 32] = std::array::from_fn(|_| invalid_pic());
        let ref_list_1: [PictureH264; 32] = std::array::from_fn(|_| invalid_pic());
        let slice = session
            .context
            .create_buffer(BufferType::EncSliceParameter(EncSliceParameter::H264(
                EncSliceParameterBufferH264::new(
                    0,
                    num_mbs,
                    VA_INVALID_ID,
                    2, // I-slice
                    0,
                    1,
                    0,
                    0,
                    [0, 0],
                    1,
                    0,
                    0,
                    0,
                    ref_list_0,
                    ref_list_1,
                    0,
                    0,
                    0,
                    [0; 32],
                    [0; 32],
                    0,
                    [[0; 2]; 32],
                    [[0; 2]; 32],
                    0,
                    [0; 32],
                    [0; 32],
                    0,
                    [[0; 2]; 32],
                    [[0; 2]; 32],
                    0,
                    0,
                    0,
                    2,
                    2,
                ),
            )))
            .map_err(|e| format!("VAAPI slice buffer: {e}"))?;

        let Session {
            display,
            context,
            surface,
            coded,
            coded_w,
            coded_h,
        } = self.session.take().ok_or("VAAPI session missing")?;

        let mut picture = Picture::new(0, Rc::clone(&context), surface);
        picture.add_buffer(sps);
        picture.add_buffer(pps);
        picture.add_buffer(slice);
        picture.add_buffer(rc);
        let picture = picture
            .begin()
            .map_err(|e| format!("VAAPI begin: {e}"))?;
        let picture = picture
            .render()
            .map_err(|e| format!("VAAPI render: {e}"))?;
        let picture = picture.end().map_err(|e| format!("VAAPI end: {e}"))?;
        let picture = picture
            .sync()
            .map_err(|(e, _)| format!("VAAPI sync: {e}"))?;
        let surface = picture
            .take_surface()
            .map_err(|_| "VAAPI: surface still shared".to_string())?;

        let mapped = MappedCodedBuffer::new(&coded).map_err(|e| format!("VAAPI map coded: {e}"))?;
        let mut annex_b = h264_headers::annex_b_sps_pps(coded_w, coded_h);
        for segment in mapped.iter() {
            if segment.buf.is_empty() {
                continue;
            }
            if !(segment.buf.starts_with(&[0, 0, 0, 1]) || segment.buf.starts_with(&[0, 0, 1])) {
                annex_b.extend_from_slice(&[0, 0, 0, 1]);
            }
            annex_b.extend_from_slice(segment.buf);
        }
        drop(mapped);

        self.session = Some(Session {
            display,
            context,
            surface,
            coded,
            coded_w,
            coded_h,
        });

        if annex_b.len() <= 16 {
            return Err("VAAPI produced empty bitstream".into());
        }
        Ok(EncodedAu {
            annex_b,
            qp: self.qp,
        })
    }

    fn reset(&mut self) {
        self.session = None;
    }
}
