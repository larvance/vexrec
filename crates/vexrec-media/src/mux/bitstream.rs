/// Annex-B H.264 / H.265 NALU parsing, AVCC/HVCC construction, and H.264 BitWriter for VA-API headers.

#[derive(Debug, Clone)]
pub struct NaluSpan<'a> {
    pub data: &'a [u8],
}

impl<'a> NaluSpan<'a> {
    #[inline]
    pub fn h264_type(&self) -> u8 {
        self.data.first().copied().unwrap_or(0) & 0x1f
    }

    #[inline]
    pub fn hevc_type(&self) -> u8 {
        (self.data.first().copied().unwrap_or(0) >> 1) & 0x3f
    }
}

/// Splits an Annex-B byte stream (`00 00 01` or `00 00 00 01` start codes) into individual NALU payloads.
pub fn split_annexb_nalus(buf: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let len = buf.len();
    let mut i = 0usize;
    let mut current_start: Option<usize> = None;

    while i < len {
        let sc_len = if i + 3 < len && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 0 && buf[i + 3] == 1 {
            Some(4)
        } else if i + 2 < len && buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            Some(3)
        } else {
            None
        };

        if let Some(slen) = sc_len {
            if let Some(start) = current_start {
                if i > start {
                    nalus.push(&buf[start..i]);
                }
            }
            i += slen;
            current_start = Some(i);
        } else {
            i += 1;
        }
    }

    if let Some(start) = current_start {
        if len > start {
            nalus.push(&buf[start..len]);
        }
    }
    nalus
}

/// Extracts `(sps, pps)` NALUs from an H.264 Annex-B buffer if present.
pub fn extract_h264_sps_pps(annexb: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let mut sps = None;
    let mut pps = None;
    for nalu in split_annexb_nalus(annexb) {
        let ntype = nalu.first().copied().unwrap_or(0) & 0x1f;
        match ntype {
            7 if sps.is_none() => sps = Some(nalu.to_vec()),
            8 if pps.is_none() => pps = Some(nalu.to_vec()),
            _ => {}
        }
    }
    (sps, pps)
}

/// Converts an H.264/HEVC Annex-B frame into MP4 length-prefixed AVCC/HVCC sample bytes
/// (stripping redundant SPS/PPS/AUD parameter sets when emitting into `mdat` if `strip_param_sets` is true).
pub fn annexb_to_length_prefixed(annexb: &[u8], strip_param_sets: bool) -> (Vec<u8>, bool) {
    let nalus = split_annexb_nalus(annexb);
    let mut out = Vec::with_capacity(annexb.len() + 16);
    let mut is_idr = false;

    for nalu in nalus {
        if nalu.is_empty() {
            continue;
        }
        let ntype = nalu[0] & 0x1f;
        if ntype == 5 {
            is_idr = true;
        }
        if strip_param_sets && matches!(ntype, 7 | 8 | 9) {
            continue;
        }
        let nlen = (nalu.len() as u32).to_be_bytes();
        out.extend_from_slice(&nlen);
        out.extend_from_slice(nalu);
    }
    (out, is_idr)
}

/// Builds an ISO/IEC 14496-15 `AVCDecoderConfigurationRecord` (`avcC` box payload) from raw SPS & PPS NALUs.
pub fn build_avcc_record(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let profile_idc = sps.get(1).copied().unwrap_or(100);
    let profile_compat = sps.get(2).copied().unwrap_or(0);
    let level_idc = sps.get(3).copied().unwrap_or(42);

    let mut out = Vec::with_capacity(11 + sps.len() + pps.len());
    out.push(1); // configurationVersion
    out.push(profile_idc);
    out.push(profile_compat);
    out.push(level_idc);
    out.push(0xff); // 4-byte NALU lengthSizeMinusOne = 3
    out.push(0xe1); // numOfSequenceParameterSets = 1
    out.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    out.extend_from_slice(sps);
    out.push(1); // numOfPictureParameterSets = 1
    out.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    out.extend_from_slice(pps);
    if matches!(profile_idc, 100 | 110 | 122 | 144) {
        out.extend_from_slice(&[0xfc | 1, 0xf8, 0xf8, 0x00]); // chroma_format=1 (4:2:0), 8-bit
    }
    out
}

/// Builds RFC 7845 19-byte `OpusHead` (`A_OPUS` CodecPrivate in Matroska) and MP4 `dOps` payload.
pub fn build_opus_head_matroska(channels: u8, pre_skip: u16, sample_rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(19);
    out.extend_from_slice(b"OpusHead");
    out.push(1); // Version
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&0i16.to_le_bytes()); // Output gain
    out.push(0); // Mapping family 0
    out
}

pub fn build_opus_dops_mp4(channels: u8, pre_skip: u16, sample_rate: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(11);
    out.push(0); // Version 0 in ISO BMFF Opus encapsulation
    out.push(channels);
    out.extend_from_slice(&pre_skip.to_be_bytes());
    out.extend_from_slice(&sample_rate.to_be_bytes());
    out.extend_from_slice(&0i16.to_be_bytes());
    out.push(0);
    out
}

/// Bit-level RBSP writer for H.264 packed headers (used by direct VA-API hardware encoding).
pub struct RbspWriter {
    bytes: Vec<u8>,
    bit_buf: u8,
    bits_used: u8,
}

impl RbspWriter {
    pub fn new() -> Self {
        Self {
            bytes: Vec::with_capacity(64),
            bit_buf: 0,
            bits_used: 0,
        }
    }

    pub fn write_bits(&mut self, val: u32, count: u8) {
        for i in (0..count).rev() {
            let bit = ((val >> i) & 1) as u8;
            self.bit_buf = (self.bit_buf << 1) | bit;
            self.bits_used += 1;
            if self.bits_used == 8 {
                self.bytes.push(self.bit_buf);
                self.bit_buf = 0;
                self.bits_used = 0;
            }
        }
    }

    #[inline]
    pub fn write_u1(&mut self, bit: bool) {
        self.write_bits(u32::from(bit), 1);
    }

    /// Exp-Golomb unsigned `ue(v)`
    pub fn write_ue(&mut self, val: u32) {
        let code_num = val + 1;
        let leading_zeros = (31 - code_num.leading_zeros()) as u8;
        self.write_bits(0, leading_zeros);
        self.write_bits(code_num, leading_zeros + 1);
    }

    /// Exp-Golomb signed `se(v)`
    pub fn write_se(&mut self, val: i32) {
        let code_num = if val <= 0 {
            (-2 * val) as u32
        } else {
            (2 * val - 1) as u32
        };
        self.write_ue(code_num);
    }

    /// Appends RBSP trailing stop bit and emits Annex-B NALU with emulation prevention (`00 00 03`).
    pub fn into_annexb_nalu(mut self, nal_header: u8) -> Vec<u8> {
        self.write_u1(true); // rbsp_stop_one_bit
        if self.bits_used > 0 {
            self.bit_buf <<= 8 - self.bits_used;
            self.bytes.push(self.bit_buf);
        }

        let mut out = Vec::with_capacity(self.bytes.len() + 8);
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, nal_header]);
        let mut zero_run = 0u8;
        for &b in &self.bytes {
            if zero_run >= 2 && b <= 0x03 {
                out.push(0x03); // Emulation prevention byte
                zero_run = 0;
            }
            out.push(b);
            if b == 0 {
                zero_run = zero_run.saturating_add(1);
            } else {
                zero_run = 0;
            }
        }
        out
    }

    /// Packs MSB-aligned RBSP slice header bits (without stop bit) prefixed by Annex-B start code + NAL header.
    pub fn into_packed_slice_header(mut self, nal_header: u8) -> (Vec<u8>, u32) {
        let mut tail_pad = 0u32;
        if self.bits_used > 0 {
            tail_pad = (8 - self.bits_used) as u32;
            self.bit_buf <<= tail_pad;
            self.bytes.push(self.bit_buf);
            self.bits_used = 0;
        }
        let mut out = Vec::with_capacity(self.bytes.len() + 8);
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, nal_header]);
        let mut zero_run = 0u8;
        for (idx, &b) in self.bytes.iter().enumerate() {
            let is_last = idx + 1 == self.bytes.len();
            if !is_last && zero_run >= 2 && b <= 0x03 {
                out.push(0x03);
                zero_run = 0;
            }
            out.push(b);
            if b == 0 {
                zero_run = zero_run.saturating_add(1);
            } else {
                zero_run = 0;
            }
        }
        let total_bits = (out.len() as u32 * 8).saturating_sub(tail_pad);
        (out, total_bits)
    }
}

/// Builds a compliant H.264 Annex-B SPS + PPS pair with BT.709 VUI color signaling for VA-API.
pub fn build_h264_sps_pps_vaapi(
    width: u32,
    height: u32,
    fps: u32,
    profile_idc: u8,
    level_idc: u8,
) -> (Vec<u8>, Vec<u8>) {
    let mb_w = (width + 15) / 16;
    let mb_h = (height + 15) / 16;
    let crop_right = (mb_w * 16 - width) / 2;
    let crop_bottom = (mb_h * 16 - height) / 2;

    let mut sps = RbspWriter::new();
    sps.write_bits(profile_idc as u32, 8);
    sps.write_bits(0x40, 8); // constraint_set1_flag = 1 (Main compatible)
    sps.write_bits(level_idc as u32, 8);
    sps.write_ue(0); // seq_parameter_set_id

    if matches!(profile_idc, 100 | 110 | 122 | 244) {
        sps.write_ue(1); // chroma_format_idc = 1 (4:2:0)
        sps.write_ue(0); // bit_depth_luma_minus8
        sps.write_ue(0); // bit_depth_chroma_minus8
        sps.write_u1(false); // qpprime_y_zero_transform_bypass_flag
        sps.write_u1(false); // seq_scaling_matrix_present_flag
    }

    sps.write_ue(4); // log2_max_frame_num_minus4 -> MaxFrameNum = 256
    sps.write_ue(0); // pic_order_cnt_type = 0
    sps.write_ue(4); // log2_max_pic_order_cnt_lsb_minus4
    sps.write_ue(1); // max_num_ref_frames = 1
    sps.write_u1(false); // gaps_in_frame_num_value_allowed_flag
    sps.write_ue(mb_w - 1);
    sps.write_ue(mb_h - 1);
    sps.write_u1(true); // frame_mbs_only_flag = 1
    sps.write_u1(true); // direct_8x8_inference_flag

    if crop_right > 0 || crop_bottom > 0 {
        sps.write_u1(true); // frame_cropping_flag
        sps.write_ue(0);
        sps.write_ue(crop_right);
        sps.write_ue(0);
        sps.write_ue(crop_bottom);
    } else {
        sps.write_u1(false);
    }

    // VUI Parameters: BT.709 color + fixed timing
    sps.write_u1(true); // vui_parameters_present_flag
    sps.write_u1(false); // aspect_ratio_info_present_flag
    sps.write_u1(false); // overscan_info_present_flag
    sps.write_u1(true); // video_signal_type_present_flag
    sps.write_bits(5, 3); // video_format = Unspecified
    sps.write_u1(false); // video_full_range_flag = limited (standard studio range)
    sps.write_u1(true); // colour_description_present_flag
    sps.write_bits(1, 8); // colour_primaries = BT.709
    sps.write_bits(1, 8); // transfer_characteristics = BT.709
    sps.write_bits(1, 8); // matrix_coefficients = BT.709
    sps.write_u1(false); // chroma_loc_info_present_flag
    sps.write_u1(true); // timing_info_present_flag
    sps.write_bits(1, 32); // num_units_in_tick
    sps.write_bits(fps.saturating_mul(2), 32); // time_scale = 2 * fps
    sps.write_u1(true); // fixed_frame_rate_flag
    sps.write_u1(false); // nal_hrd_parameters_present_flag
    sps.write_u1(false); // vcl_hrd_parameters_present_flag
    sps.write_u1(false); // pic_struct_present_flag
    sps.write_u1(true); // bitstream_restriction_flag
    sps.write_u1(true); // motion_vectors_over_pic_boundaries_flag
    sps.write_ue(0); // max_bytes_per_pic_denom
    sps.write_ue(0); // max_bits_per_mb_denom
    sps.write_ue(11); // log2_max_mv_length_horizontal
    sps.write_ue(11); // log2_max_mv_length_vertical
    sps.write_ue(0); // max_num_reorder_frames = 0 (zero-latency!)
    sps.write_ue(1); // max_dec_frame_buffering = 1

    let mut pps = RbspWriter::new();
    pps.write_ue(0); // pic_parameter_set_id
    pps.write_ue(0); // seq_parameter_set_id
    pps.write_u1(profile_idc >= 77); // entropy_coding_mode_flag (CABAC for Main/High)
    pps.write_u1(false); // bottom_field_pic_order_in_frame_present_flag
    pps.write_ue(0); // num_slice_groups_minus1
    pps.write_ue(0); // num_ref_idx_l0_default_active_minus1
    pps.write_ue(0); // num_ref_idx_l1_default_active_minus1
    pps.write_u1(false); // weighted_pred_flag
    pps.write_bits(0, 2); // weighted_bipred_idc
    pps.write_se(0); // pic_init_qp_minus26
    pps.write_se(0); // pic_init_qs_minus26
    pps.write_se(0); // chroma_qp_index_offset
    pps.write_u1(true); // deblocking_filter_control_present_flag
    pps.write_u1(false); // constrained_intra_pred_flag
    pps.write_u1(false); // redundant_pic_cnt_present_flag

    (sps.into_annexb_nalu(0x67), pps.into_annexb_nalu(0x68))
}

/// Builds a compliant H.264 Annex-B slice header (`VA_ENC_PACKED_HEADER_SLICE`) returning `(bytes, bit_length)`.
pub fn build_h264_slice_header_vaapi(
    is_idr: bool,
    frame_num: u16,
    poc: i32,
    idr_pic_id: u16,
    cabac: bool,
) -> (Vec<u8>, u32) {
    let mut w = RbspWriter::new();
    w.write_ue(0); // first_mb_in_slice = 0
    w.write_ue(if is_idr { 7 } else { 5 }); // slice_type: 7=all-I, 5=all-P
    w.write_ue(0); // pic_parameter_set_id = 0
    w.write_bits(u32::from(frame_num) & 0xff, 8); // log2_max_frame_num_minus4 + 4 = 8
    if is_idr {
        w.write_ue(u32::from(idr_pic_id));
    }
    w.write_bits((poc as u32) & 0xff, 8); // pic_order_cnt_type = 0, lsb = 8 bits
    if !is_idr {
        w.write_u1(false); // num_ref_idx_active_override_flag
        w.write_u1(false); // ref_pic_list_modification_flag_l0
    }
    if is_idr {
        w.write_u1(false); // no_output_of_prior_pics_flag
        w.write_u1(false); // long_term_reference_flag
    } else {
        w.write_u1(false); // adaptive_ref_pic_marking_mode_flag (nal_ref_idc = 2 != 0)
    }
    if cabac && !is_idr {
        w.write_ue(0); // cabac_init_idc
    }
    w.write_se(0); // slice_qp_delta
    w.write_ue(0); // disable_deblocking_filter_idc
    w.write_se(0); // slice_alpha_c0_offset_div2
    w.write_se(0); // slice_beta_offset_div2

    let nal_header = if is_idr { 0x65 } else { 0x41 };
    w.into_packed_slice_header(nal_header)
}

