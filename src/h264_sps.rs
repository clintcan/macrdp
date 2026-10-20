//! H.264 SPS fix-up: add the VUI `bitstream_restriction` fields when the
//! encoder leaves them out.
//!
//! VideoToolbox's Baseline SPS (real-time, no frame reordering) has a VUI
//! but no `bitstream_restriction_flag`. Without it a decoder must assume
//! `max_num_reorder_frames` and `max_dec_frame_buffering` equal MaxDpbFrames
//! (H.264 E.2.1), so it may hold several decoded pictures before it outputs
//! one. Windows 10 mstsc then shows nothing at all: no AVC420 frame is ever
//! acknowledged and the session stays black (the same stream with the fields
//! added renders at once). The encoder never reorders frames
//! (`AllowFrameReordering = false`), so `max_num_reorder_frames = 0` and
//! `max_dec_frame_buffering = max_num_ref_frames` are true for the stream.
//!
//! The rewrite copies every SPS syntax element bit for bit and only changes
//! the VUI tail. An SPS that already has `bitstream_restriction_flag = 1`, or
//! one this parser cannot read, is left unchanged.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// Values written for the added `bitstream_restriction` fields other than
/// the two that matter. They are the values H.264 E.2.1 infers when the
/// fields are absent, so they change nothing else.
const MAX_BYTES_PER_PIC_DENOM: u32 = 2;
const MAX_BITS_PER_MB_DENOM: u32 = 1;
const LOG2_MAX_MV_LENGTH: u32 = 16;

/// Return the SPS NAL unit (header byte included, no start code) with
/// `bitstream_restriction` added, or `None` if `nal` needs no change or is
/// not an SPS this function can rewrite.
pub fn add_bitstream_restriction(nal: &[u8]) -> Option<Vec<u8>> {
    if nal.len() < 4 || nal[0] & 0x1f != 7 {
        return None;
    }
    let rbsp = unescape(&nal[1..]);
    let mut r = BitReader::new(&rbsp);
    let mut w = BitWriter::default();
    let max_num_ref_frames = copy_sps_until_vui(&mut r, &mut w)?;

    let vui_present = r.u(1)?;
    w.u(1, 1);
    if vui_present == 1 {
        if !copy_vui_until_restriction(&mut r, &mut w)? {
            return None; // already has bitstream_restriction
        }
    } else {
        // Minimal VUI: every optional part absent (aspect ratio, overscan,
        // video signal, chroma location, timing, NAL HRD, VCL HRD,
        // pic_struct): eight flags, all 0.
        for _ in 0..8 {
            w.u(1, 0);
        }
    }
    w.u(1, 1); // bitstream_restriction_flag
    w.u(1, 1); // motion_vectors_over_pic_boundaries_flag
    w.ue(MAX_BYTES_PER_PIC_DENOM);
    w.ue(MAX_BITS_PER_MB_DENOM);
    w.ue(LOG2_MAX_MV_LENGTH); // horizontal
    w.ue(LOG2_MAX_MV_LENGTH); // vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(max_num_ref_frames.max(1)); // max_dec_frame_buffering

    let mut out = Vec::with_capacity(nal.len() + 8);
    out.push(nal[0]);
    out.extend_from_slice(&escape(&w.finish_rbsp()));
    Some(out)
}

/// Copy seq_parameter_set_data() up to (not including)
/// vui_parameters_present_flag. Returns max_num_ref_frames.
fn copy_sps_until_vui(r: &mut BitReader<'_>, w: &mut BitWriter) -> Option<u32> {
    let profile_idc = r.copy_u(w, 8)?;
    r.copy_u(w, 8)?; // constraint flags + reserved
    r.copy_u(w, 8)?; // level_idc
    r.copy_ue(w)?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.copy_ue(w)?;
        if chroma_format_idc == 3 {
            r.copy_u(w, 1)?; // separate_colour_plane_flag
        }
        r.copy_ue(w)?; // bit_depth_luma_minus8
        r.copy_ue(w)?; // bit_depth_chroma_minus8
        r.copy_u(w, 1)?; // qpprime_y_zero_transform_bypass_flag
        if r.copy_u(w, 1)? == 1 {
            // seq_scaling_matrix_present_flag
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.copy_u(w, 1)? == 1 {
                    copy_scaling_list(r, w, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    r.copy_ue(w)?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = r.copy_ue(w)?;
    match pic_order_cnt_type {
        0 => {
            r.copy_ue(w)?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.copy_u(w, 1)?; // delta_pic_order_always_zero_flag
            r.copy_se(w)?; // offset_for_non_ref_pic
            r.copy_se(w)?; // offset_for_top_to_bottom_field
            let n = r.copy_ue(w)?;
            for _ in 0..n {
                r.copy_se(w)?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.copy_ue(w)?;
    r.copy_u(w, 1)?; // gaps_in_frame_num_value_allowed_flag
    r.copy_ue(w)?; // pic_width_in_mbs_minus1
    r.copy_ue(w)?; // pic_height_in_map_units_minus1
    if r.copy_u(w, 1)? == 0 {
        // frame_mbs_only_flag
        r.copy_u(w, 1)?; // mb_adaptive_frame_field_flag
    }
    r.copy_u(w, 1)?; // direct_8x8_inference_flag
    if r.copy_u(w, 1)? == 1 {
        // frame_cropping_flag
        for _ in 0..4 {
            r.copy_ue(w)?;
        }
    }
    Some(max_num_ref_frames)
}

fn copy_scaling_list(r: &mut BitReader<'_>, w: &mut BitWriter, size: usize) -> Option<()> {
    let mut last = 8i64;
    let mut next = 8i64;
    for _ in 0..size {
        if next != 0 {
            let delta = r.copy_se(w)?;
            next = (last + i64::from(delta) + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

/// Copy vui_parameters() up to (not including) bitstream_restriction_flag.
/// Returns false if the SPS already sets bitstream_restriction_flag.
fn copy_vui_until_restriction(r: &mut BitReader<'_>, w: &mut BitWriter) -> Option<bool> {
    if r.copy_u(w, 1)? == 1 {
        // aspect_ratio_info_present_flag
        if r.copy_u(w, 8)? == 255 {
            r.copy_u(w, 16)?; // sar_width
            r.copy_u(w, 16)?; // sar_height
        }
    }
    if r.copy_u(w, 1)? == 1 {
        r.copy_u(w, 1)?; // overscan_appropriate_flag
    }
    if r.copy_u(w, 1)? == 1 {
        // video_signal_type_present_flag
        r.copy_u(w, 3)?; // video_format
        r.copy_u(w, 1)?; // video_full_range_flag
        if r.copy_u(w, 1)? == 1 {
            r.copy_u(w, 24)?; // colour_primaries, transfer, matrix
        }
    }
    if r.copy_u(w, 1)? == 1 {
        r.copy_ue(w)?; // chroma_sample_loc_type_top_field
        r.copy_ue(w)?; // chroma_sample_loc_type_bottom_field
    }
    if r.copy_u(w, 1)? == 1 {
        // timing_info_present_flag
        r.copy_u(w, 32)?;
        r.copy_u(w, 32)?;
        r.copy_u(w, 1)?;
    }
    let nal_hrd = r.copy_u(w, 1)?;
    if nal_hrd == 1 {
        copy_hrd(r, w)?;
    }
    let vcl_hrd = r.copy_u(w, 1)?;
    if vcl_hrd == 1 {
        copy_hrd(r, w)?;
    }
    if nal_hrd == 1 || vcl_hrd == 1 {
        r.copy_u(w, 1)?; // low_delay_hrd_flag
    }
    r.copy_u(w, 1)?; // pic_struct_present_flag
    Some(r.u(1)? == 0)
}

fn copy_hrd(r: &mut BitReader<'_>, w: &mut BitWriter) -> Option<()> {
    let cpb_cnt_minus1 = r.copy_ue(w)?;
    r.copy_u(w, 4)?; // bit_rate_scale
    r.copy_u(w, 4)?; // cpb_size_scale
    for _ in 0..=cpb_cnt_minus1 {
        r.copy_ue(w)?; // bit_rate_value_minus1
        r.copy_ue(w)?; // cpb_size_value_minus1
        r.copy_u(w, 1)?; // cbr_flag
    }
    r.copy_u(w, 20)?; // four 5-bit length fields
    Some(())
}

/// Remove emulation-prevention bytes (00 00 03 -> 00 00).
fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Insert emulation-prevention bytes where 00 00 is followed by 00..=03.
fn escape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4);
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - (self.pos % 8))) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            v = (v << 1) | self.bit()?;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0u32;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        let rest = self.u(zeros)?;
        ((1u64 << zeros) - 1 + u64::from(rest)).try_into().ok()
    }

    fn copy_u(&mut self, w: &mut BitWriter, n: u32) -> Option<u32> {
        let v = self.u(n)?;
        w.u(n, v);
        Some(v)
    }

    fn copy_ue(&mut self, w: &mut BitWriter) -> Option<u32> {
        let v = self.ue()?;
        w.ue(v);
        Some(v)
    }

    fn copy_se(&mut self, w: &mut BitWriter) -> Option<i32> {
        let k = self.copy_ue(w)?;
        let v = i64::from(k.div_ceil(2));
        (if k % 2 == 1 { v } else { -v }).try_into().ok()
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    bits: u32,
}

impl BitWriter {
    fn push(&mut self, bit: u32) {
        if self.bits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if bit != 0 {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 0x80 >> (self.bits % 8);
        }
        self.bits += 1;
    }

    fn u(&mut self, n: u32, v: u32) {
        for i in (0..n).rev() {
            self.push((v >> i) & 1);
        }
    }

    fn ue(&mut self, v: u32) {
        let v1 = u64::from(v) + 1;
        let len = 64 - v1.leading_zeros(); // bits in v+1
        for _ in 1..len {
            self.push(0);
        }
        for i in (0..len).rev() {
            self.push(((v1 >> i) & 1) as u32);
        }
    }

    /// rbsp_trailing_bits: a 1 bit, then zero bits to the byte boundary.
    fn finish_rbsp(mut self) -> Vec<u8> {
        self.push(1);
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS from VideoToolbox (Baseline, level 3.1, 1024x768, VUI with BT.709
    /// colour, no bitstream_restriction), as captured from a live session.
    const VT_SPS: &[u8] = &[
        0x27, 0x42, 0x00, 0x1f, 0xab, 0x40, 0x20, 0x03, 0x0d, 0x37, 0x01, 0x01, 0x01, 0x02,
    ];

    /// Parsed view of the fields the tests check.
    #[derive(Debug, PartialEq)]
    struct Fields {
        profile: u32,
        level: u32,
        max_num_ref_frames: u32,
        width_mbs: u32,
        height_mbs: u32,
        colour: Option<(u32, u32, u32)>,
        restriction: Option<(u32, u32)>, // (max_num_reorder_frames, max_dec_frame_buffering)
    }

    fn parse(nal: &[u8]) -> Fields {
        let rbsp = unescape(&nal[1..]);
        let mut r = BitReader::new(&rbsp);
        let mut sink = BitWriter::default();
        let profile = rbsp[0].into();
        let level = rbsp[2].into();
        let max_num_ref_frames = copy_sps_until_vui(&mut r, &mut sink).unwrap();
        // Re-read the size fields: they sit just before the VUI flag.
        let rbsp2 = unescape(&nal[1..]);
        let mut r2 = BitReader::new(&rbsp2);
        r2.u(24).unwrap();
        r2.ue().unwrap();
        r2.ue().unwrap();
        let poc = r2.ue().unwrap();
        assert_eq!(poc, 0, "test SPSs use pic_order_cnt_type 0");
        r2.ue().unwrap();
        r2.ue().unwrap();
        r2.u(1).unwrap();
        let width_mbs = r2.ue().unwrap() + 1;
        let height_mbs = r2.ue().unwrap() + 1;

        let mut colour = None;
        let mut restriction = None;
        if r.u(1).unwrap() == 1 {
            if r.u(1).unwrap() == 1 && r.u(8).unwrap() == 255 {
                r.u(32).unwrap();
            }
            if r.u(1).unwrap() == 1 {
                r.u(1).unwrap();
            }
            if r.u(1).unwrap() == 1 {
                r.u(4).unwrap();
                if r.u(1).unwrap() == 1 {
                    colour = Some((r.u(8).unwrap(), r.u(8).unwrap(), r.u(8).unwrap()));
                }
            }
            if r.u(1).unwrap() == 1 {
                r.ue().unwrap();
                r.ue().unwrap();
            }
            if r.u(1).unwrap() == 1 {
                r.u(32).unwrap();
                r.u(32).unwrap();
                r.u(1).unwrap();
            }
            assert_eq!(r.u(2).unwrap(), 0, "test SPSs carry no HRD");
            r.u(1).unwrap(); // pic_struct_present_flag
            if r.u(1).unwrap() == 1 {
                r.u(1).unwrap();
                for _ in 0..4 {
                    r.ue().unwrap();
                }
                restriction = Some((r.ue().unwrap(), r.ue().unwrap()));
            }
        }
        // rbsp_trailing_bits must follow.
        assert_eq!(r.u(1).unwrap(), 1, "rbsp stop bit");
        Fields {
            profile,
            level,
            max_num_ref_frames,
            width_mbs,
            height_mbs,
            colour,
            restriction,
        }
    }

    #[test]
    fn adds_restriction_to_videotoolbox_sps() {
        let before = parse(VT_SPS);
        assert_eq!(before.restriction, None);
        let out = add_bitstream_restriction(VT_SPS).expect("rewritten");
        let after = parse(&out);
        assert_eq!(out[0], VT_SPS[0], "NAL header kept");
        assert_eq!(after.restriction, Some((0, 1)));
        assert_eq!(
            Fields {
                restriction: None,
                ..after
            },
            before,
            "all other fields kept"
        );
    }

    #[test]
    fn leaves_sps_with_restriction_alone() {
        let out = add_bitstream_restriction(VT_SPS).unwrap();
        assert_eq!(add_bitstream_restriction(&out), None);
    }

    #[test]
    fn adds_vui_when_absent() {
        // Same SPS with vui_parameters_present_flag = 0: rebuild it by hand.
        let rbsp = unescape(&VT_SPS[1..]);
        let mut r = BitReader::new(&rbsp);
        let mut w = BitWriter::default();
        copy_sps_until_vui(&mut r, &mut w).unwrap();
        w.u(1, 0);
        let mut no_vui = vec![VT_SPS[0]];
        no_vui.extend_from_slice(&escape(&w.finish_rbsp()));
        assert_eq!(parse(&no_vui).colour, None);

        let out = add_bitstream_restriction(&no_vui).expect("rewritten");
        let f = parse(&out);
        assert_eq!(f.restriction, Some((0, 1)));
        assert_eq!((f.width_mbs, f.height_mbs), (64, 48));
    }

    #[test]
    fn ignores_non_sps_and_garbage() {
        assert_eq!(add_bitstream_restriction(&[0x28, 0xce, 0x3c, 0x80]), None); // PPS
        assert_eq!(add_bitstream_restriction(&[0x27, 0x42]), None);
        assert_eq!(
            add_bitstream_restriction(&[0x27, 0x42, 0x00, 0x1f, 0x00, 0x00]),
            None
        );
    }

    #[test]
    fn escape_round_trip() {
        let raw = [0u8, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4];
        let esc = escape(&raw);
        assert!(!esc.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
        assert_eq!(unescape(&esc), raw);
    }

    #[test]
    fn exp_golomb_round_trip() {
        let mut w = BitWriter::default();
        for v in [0u32, 1, 2, 7, 16, 255, 65_535] {
            w.ue(v);
        }
        let bytes = w.finish_rbsp();
        let mut r = BitReader::new(&bytes);
        for v in [0u32, 1, 2, 7, 16, 255, 65_535] {
            assert_eq!(r.ue(), Some(v));
        }
    }
}
