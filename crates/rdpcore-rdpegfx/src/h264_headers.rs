//! Minimal Constrained Baseline SPS/PPS Annex B writers for HW encoders
//! that emit slice NALs without packed headers.

/// Emit Annex B SPS + PPS for Constrained Baseline 4:2:0 Progressive,
/// level 4.1, for a coded size of `coded_w`×`coded_h` (already 16-aligned).
pub fn annex_b_sps_pps(coded_w: u16, coded_h: u16) -> Vec<u8> {
    let mb_w = (u32::from(coded_w) / 16).max(1);
    let mb_h = (u32::from(coded_h) / 16).max(1);
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.push(0x67); // nal_unit_type = 7 (SPS), nal_ref_idc = 3
    out.extend(sps_rbsp(mb_w, mb_h));
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.push(0x68); // nal_unit_type = 8 (PPS)
    out.extend(pps_rbsp());
    out
}

fn sps_rbsp(mb_width: u32, mb_height: u32) -> Vec<u8> {
    let mut b = BitWriter::new();
    b.write_bits(66, 8); // profile_idc = Baseline
    b.write_bits(0b1100_0000, 8); // constraint_set0/1 + reserved
    b.write_bits(41, 8); // level_idc = 4.1
    b.write_ue(0); // seq_parameter_set_id
    b.write_ue(0); // log2_max_frame_num_minus4
    b.write_ue(0); // pic_order_cnt_type
    b.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
    b.write_ue(1); // max_num_ref_frames
    b.write_bit(0); // gaps_in_frame_num_value_allowed_flag
    b.write_ue(mb_width.saturating_sub(1));
    b.write_ue(mb_height.saturating_sub(1));
    b.write_bit(1); // frame_mbs_only_flag
    b.write_bit(1); // direct_8x8_inference_flag
    b.write_bit(0); // frame_cropping_flag
    b.write_bit(0); // vui_parameters_present_flag
    b.write_rbsp_trailing();
    b.into_bytes()
}

fn pps_rbsp() -> Vec<u8> {
    let mut b = BitWriter::new();
    b.write_ue(0); // pic_parameter_set_id
    b.write_ue(0); // seq_parameter_set_id
    b.write_bit(0); // entropy_coding_mode_flag (CAVLC)
    b.write_bit(0); // bottom_field_pic_order_in_frame_present_flag
    b.write_ue(0); // num_slice_groups_minus1
    b.write_ue(0); // num_ref_idx_l0_default_active_minus1
    b.write_ue(0); // num_ref_idx_l1_default_active_minus1
    b.write_bit(0); // weighted_pred_flag
    b.write_bits(0, 2); // weighted_bipred_idc
    b.write_se(0); // pic_init_qp_minus26
    b.write_se(0); // pic_init_qs_minus26
    b.write_se(0); // chroma_qp_index_offset
    b.write_bit(0); // deblocking_filter_control_present_flag
    b.write_bit(0); // constrained_intra_pred_flag
    b.write_bit(0); // redundant_pic_cnt_present_flag
    b.write_rbsp_trailing();
    b.into_bytes()
}

struct BitWriter {
    bytes: Vec<u8>,
    bit: u8,
    cur: u8,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bit: 0,
            cur: 0,
        }
    }

    fn write_bit(&mut self, v: u8) {
        self.cur = (self.cur << 1) | (v & 1);
        self.bit += 1;
        if self.bit == 8 {
            self.bytes.push(self.cur);
            self.bit = 0;
            self.cur = 0;
        }
    }

    fn write_bits(&mut self, mut v: u32, n: u8) {
        for i in (0..n).rev() {
            self.write_bit(((v >> i) & 1) as u8);
        }
        let _ = &mut v;
    }

    fn write_ue(&mut self, v: u32) {
        let v = v + 1;
        let zeros = 31 - v.leading_zeros();
        for _ in 0..zeros {
            self.write_bit(0);
        }
        self.write_bits(v, (zeros + 1) as u8);
    }

    fn write_se(&mut self, v: i32) {
        let mapped = if v <= 0 {
            (-2 * v) as u32
        } else {
            (2 * v - 1) as u32
        };
        self.write_ue(mapped);
    }

    fn write_rbsp_trailing(&mut self) {
        self.write_bit(1);
        while self.bit != 0 {
            self.write_bit(0);
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sps_pps_have_start_codes() {
        let h = annex_b_sps_pps(64, 64);
        assert_eq!(&h[0..5], &[0, 0, 0, 1, 0x67]);
        assert!(h.windows(5).any(|w| w == [0, 0, 0, 1, 0x68]));
    }

    #[test]
    fn sps_pps_scales_with_resolution() {
        let small = annex_b_sps_pps(64, 64);
        let large = annex_b_sps_pps(1920, 1088);
        assert_ne!(small, large);
        assert!(large.len() >= small.len());
        // Both remain Annex B with SPS then PPS
        assert_eq!(&small[0..4], &[0, 0, 0, 1]);
        assert_eq!(&large[0..4], &[0, 0, 0, 1]);
    }

    #[test]
    fn sps_pps_rejects_zero_by_clamping_macroblocks() {
        // align callers normally avoid 0; API still must not panic.
        let h = annex_b_sps_pps(0, 0);
        assert_eq!(&h[0..5], &[0, 0, 0, 1, 0x67]);
    }
}
