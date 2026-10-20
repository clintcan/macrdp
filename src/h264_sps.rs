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
//! one this parser cannot read, is left unchanged (the latter with one
//! warning per process).

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::warn;

/// Values written for the added `bitstream_restriction` fields other than
/// the two that matter. They are the values H.264 E.2.1 infers when the
/// fields are absent, so they change nothing else.
const MAX_BYTES_PER_PIC_DENOM: u32 = 2;
const MAX_BITS_PER_MB_DENOM: u32 = 1;
const LOG2_MAX_MV_LENGTH: u32 = 16;

/// One-shot guard for the "SPS not rewritten" warning in [`fix_parameter_set`].
static UNPARSABLE_SPS_WARNED: AtomicBool = AtomicBool::new(false);

/// Result of [`rewrite`].
#[derive(Debug, PartialEq, Eq)]
enum Rewrite {
    /// The SPS with `bitstream_restriction` added.
    Done(Vec<u8>),
    /// Not an SPS, or an SPS that already has `bitstream_restriction`.
    NotNeeded,
    /// An SPS this parser cannot read.
    Unparsable,
}

/// Add `bitstream_restriction` to one parameter set from the encoder if it
/// is an SPS without it, and return the NAL unit to send. An SPS that cannot be parsed is
/// sent unchanged, with one warning per process: Windows 10 mstsc may then
/// show a black session, and the log must say why.
pub fn fix_parameter_set(nal: &[u8]) -> Vec<u8> {
    match rewrite(nal) {
        Rewrite::Done(out) => out,
        Rewrite::NotNeeded => nal.to_vec(),
        Rewrite::Unparsable => {
            if !UNPARSABLE_SPS_WARNED.swap(true, Ordering::Relaxed) {
                warn!(
                    len = nal.len(),
                    head = %hex_head(nal),
                    "H.264 SPS could not be parsed, so bitstream_restriction was not added; \
                     sending it unchanged (Windows 10 mstsc may show a black session)"
                );
            }
            nal.to_vec()
        }
    }
}

/// The first bytes of `nal` as hex, for the log.
fn hex_head(nal: &[u8]) -> String {
    nal.iter().take(24).map(|b| format!("{b:02x}")).collect()
}

/// Return the SPS NAL unit (header byte included, no start code) with
/// `bitstream_restriction` added, or `None` if `nal` needs no change or is
/// not an SPS this function can rewrite.
#[cfg(test)]
fn add_bitstream_restriction(nal: &[u8]) -> Option<Vec<u8>> {
    match rewrite(nal) {
        Rewrite::Done(out) => Some(out),
        Rewrite::NotNeeded | Rewrite::Unparsable => None,
    }
}

fn rewrite(nal: &[u8]) -> Rewrite {
    if nal.first().is_none_or(|h| h & 0x1f != 7) {
        return Rewrite::NotNeeded;
    }
    match try_rewrite(nal) {
        Some(Some(out)) => Rewrite::Done(out),
        Some(None) => Rewrite::NotNeeded,
        None => Rewrite::Unparsable,
    }
}

/// `None`: cannot parse. `Some(None)`: already has bitstream_restriction.
fn try_rewrite(nal: &[u8]) -> Option<Option<Vec<u8>>> {
    if nal.len() < 4 {
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
            return Some(None); // already has bitstream_restriction
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
    Some(Some(out))
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
        r2.ue().unwrap(); // seq_parameter_set_id
        if profile == 100 {
            assert_eq!(r2.ue(), Some(1), "test SPSs are 4:2:0");
            r2.ue().unwrap();
            r2.ue().unwrap();
            r2.u(1).unwrap();
            assert_eq!(r2.u(1), Some(0), "test SPSs carry no scaling matrix");
        }
        r2.ue().unwrap(); // log2_max_frame_num_minus4
        match r2.ue().unwrap() {
            0 => {
                r2.ue().unwrap();
            }
            2 => {}
            poc => panic!("test SPSs use pic_order_cnt_type 0 or 2, not {poc}"),
        }
        r2.ue().unwrap(); // max_num_ref_frames
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
            let nal_hrd = r.u(1).unwrap();
            if nal_hrd == 1 {
                skip_hrd(&mut r);
            }
            let vcl_hrd = r.u(1).unwrap();
            if vcl_hrd == 1 {
                skip_hrd(&mut r);
            }
            if nal_hrd == 1 || vcl_hrd == 1 {
                r.u(1).unwrap(); // low_delay_hrd_flag
            }
            r.u(1).unwrap(); // pic_struct_present_flag
            if r.u(1).unwrap() == 1 {
                r.u(1).unwrap();
                for _ in 0..4 {
                    r.ue().unwrap();
                }
                restriction = Some((r.ue().unwrap(), r.ue().unwrap()));
            }
        }
        // rbsp_trailing_bits must follow, then only zero bits to the end.
        assert_eq!(r.u(1).unwrap(), 1, "rbsp stop bit");
        while let Some(b) = r.bit() {
            assert_eq!(b, 0, "data after rbsp_trailing_bits");
        }
        assert!(r.pos.div_ceil(8) == rbsp.len(), "trailing bytes");
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

    /// HRD parameters (E.1.2), read with this test's own reader.
    fn skip_hrd(r: &mut BitReader<'_>) {
        let cpb_cnt = r.ue().unwrap() + 1;
        r.u(8).unwrap(); // bit_rate_scale, cpb_size_scale
        for _ in 0..cpb_cnt {
            r.ue().unwrap();
            r.ue().unwrap();
            r.u(1).unwrap();
        }
        r.u(20).unwrap();
    }

    /// x264 (libx264 via PyAV), High profile, NAL HRD (VBR), sar 7:5
    /// (aspect_ratio_idc 255), timing info, and its own
    /// bitstream_restriction. The payload has emulation-prevention bytes.
    const X264_HIGH_VBR_SAR: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xbb, 0xff, 0x00, 0x07, 0x00, 0x01,
        0x10, 0x00, 0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc6, 0x48, 0x00, 0x0f, 0x42,
        0x40, 0x01, 0xe8, 0x4a, 0x4c, 0x30, 0x07, 0x8c, 0x18, 0xcb,
    ];

    /// x264, High profile, NAL HRD (CBR), B-frames (max_num_reorder_frames 1
    /// in its own bitstream_restriction).
    const X264_HIGH_CBR: &[u8] = &[
        0x67, 0x64, 0x00, 0x1f, 0xac, 0xd9, 0x40, 0x50, 0x05, 0xba, 0x10, 0x00, 0x00, 0x03, 0x00,
        0x10, 0x00, 0x00, 0x03, 0x03, 0xc6, 0x26, 0x00, 0x0f, 0x42, 0x40, 0x01, 0xe8, 0x4e, 0x4c,
        0x30, 0x07, 0x8c, 0x18, 0xcb,
    ];

    /// The same SPS with bitstream_restriction_flag = 0 and the fields
    /// removed (built with the copy functions under test).
    fn strip_restriction(nal: &[u8]) -> Vec<u8> {
        let rbsp = unescape(&nal[1..]);
        let mut r = BitReader::new(&rbsp);
        let mut w = BitWriter::default();
        copy_sps_until_vui(&mut r, &mut w).unwrap();
        assert_eq!(r.copy_u(&mut w, 1), Some(1), "VUI present");
        assert_eq!(copy_vui_until_restriction(&mut r, &mut w), Some(false));
        w.u(1, 0);
        let mut out = vec![nal[0]];
        out.extend_from_slice(&escape(&w.finish_rbsp()));
        out
    }

    #[test]
    fn x264_sps_with_hrd_reads_to_the_end() {
        // An independent encoder's SPS with HRD and 00 00 03 escapes: the
        // copy functions must stop exactly at bitstream_restriction_flag, so
        // that the test's reader then lands exactly on the stop bit.
        for sps in [X264_HIGH_VBR_SAR, X264_HIGH_CBR] {
            let f = parse(sps);
            assert_eq!(f.profile, 100);
            assert_eq!((f.width_mbs, f.height_mbs), (80, 45));
            assert!(f.restriction.is_some());
            assert_eq!(add_bitstream_restriction(sps), None, "already has it");
        }
    }

    #[test]
    fn x264_sps_without_restriction_gets_it_back() {
        for sps in [X264_HIGH_VBR_SAR, X264_HIGH_CBR] {
            let before = parse(sps);
            let stripped = strip_restriction(sps);
            assert_eq!(parse(&stripped).restriction, None);
            let out = add_bitstream_restriction(&stripped).expect("rewritten");
            let after = parse(&out);
            assert_eq!(
                after.restriction,
                Some((0, before.max_num_ref_frames.max(1)))
            );
            assert_eq!(
                Fields {
                    restriction: None,
                    ..after
                },
                Fields {
                    restriction: None,
                    ..before
                }
            );
            // Bits before the restriction are unchanged: re-stripping gives
            // the same bytes.
            assert_eq!(strip_restriction(&out), stripped);
        }
    }

    /// A hand-built High-profile SPS (H.264 7.3.2.1.1 / E.1.1) with the
    /// syntax no encoder sample here has: SPS scaling lists (explicit 4x4
    /// and 8x8 lists, a "use default" list, a list that ends early with
    /// nextScale 0), pic_order_cnt_type 1, field coding, cropping, VUI with
    /// aspect_ratio_idc 255, colour, chroma location, timing, NAL HRD with
    /// two CPBs and VCL HRD. With `restriction`, the VUI ends with the
    /// fields `add_bitstream_restriction` writes.
    fn hand_built_sps(restriction: bool) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.u(8, 100); // profile_idc
        w.u(8, 0); // constraint flags
        w.u(8, 40); // level_idc
        w.ue(0); // seq_parameter_set_id
        w.ue(1); // chroma_format_idc 4:2:0
        w.ue(0); // bit_depth_luma_minus8
        w.ue(0); // bit_depth_chroma_minus8
        w.u(1, 0); // qpprime_y_zero_transform_bypass_flag
        w.u(1, 1); // seq_scaling_matrix_present_flag
        w.u(1, 1); // list 0 (4x4): 16 explicit deltas, all scales nonzero
        for d in [8, -1, 2, -3, 4, 0, 1, 1, -2, 3, 5, -6, 7, 1, 1, 1] {
            w.ue(se_code(d));
        }
        // List 1 (4x4): delta -8 makes nextScale 0 at j = 0, "use default".
        w.u(1, 1);
        w.ue(se_code(-8));
        // List 2: absent.
        w.u(1, 0);
        // List 3 (4x4): nextScale becomes 0 at j = 4; no more deltas.
        w.u(1, 1);
        for d in [2, 3, -1, 4, -16] {
            w.ue(se_code(d));
        }
        // Lists 4, 5: absent.
        w.u(1, 0);
        w.u(1, 0);
        // List 6 (8x8): 64 explicit deltas.
        w.u(1, 1);
        for i in 0..64 {
            w.ue(se_code(if i % 2 == 0 { 1 } else { -1 }));
        }
        // List 7: absent.
        w.u(1, 0);
        w.ue(4); // log2_max_frame_num_minus4
        w.ue(1); // pic_order_cnt_type
        w.u(1, 0); // delta_pic_order_always_zero_flag
        w.ue(se_code(-2)); // offset_for_non_ref_pic
        w.ue(se_code(1)); // offset_for_top_to_bottom_field
        w.ue(2); // num_ref_frames_in_pic_order_cnt_cycle
        w.ue(se_code(3));
        w.ue(se_code(-4));
        w.ue(4); // max_num_ref_frames
        w.u(1, 0); // gaps_in_frame_num_value_allowed_flag
        w.ue(119); // pic_width_in_mbs_minus1
        w.ue(33); // pic_height_in_map_units_minus1
        w.u(1, 0); // frame_mbs_only_flag
        w.u(1, 1); // mb_adaptive_frame_field_flag
        w.u(1, 1); // direct_8x8_inference_flag
        w.u(1, 1); // frame_cropping_flag
        for v in [0, 0, 0, 4] {
            w.ue(v);
        }
        w.u(1, 1); // vui_parameters_present_flag
        w.u(1, 1); // aspect_ratio_info_present_flag
        w.u(8, 255); // Extended_SAR
        w.u(16, 7);
        w.u(16, 5);
        w.u(1, 1); // overscan_info_present_flag
        w.u(1, 0);
        w.u(1, 1); // video_signal_type_present_flag
        w.u(3, 5);
        w.u(1, 0);
        w.u(1, 1); // colour_description_present_flag
        w.u(24, 0x01_01_01);
        w.u(1, 1); // chroma_loc_info_present_flag
        w.ue(1);
        w.ue(2);
        w.u(1, 1); // timing_info_present_flag
        w.u(32, 1001);
        w.u(32, 60_000);
        w.u(1, 1);
        // NAL HRD: two CPB specifications.
        w.u(1, 1);
        w.ue(1); // cpb_cnt_minus1
        w.u(4, 2);
        w.u(4, 3);
        for (rate, size, cbr) in [(999, 1999, 0), (4999, 9999, 1)] {
            w.ue(rate);
            w.ue(size);
            w.u(1, cbr);
        }
        w.u(5, 23); // initial_cpb_removal_delay_length_minus1
        w.u(5, 23); // cpb_removal_delay_length_minus1
        w.u(5, 23); // dpb_output_delay_length_minus1
        w.u(5, 24); // time_offset_length
        w.u(1, 1); // VCL HRD: one CPB
        w.ue(0);
        w.u(4, 1);
        w.u(4, 1);
        w.ue(500);
        w.ue(700);
        w.u(1, 0);
        w.u(20, 0xabcde);
        w.u(1, 0); // low_delay_hrd_flag
        w.u(1, 1); // pic_struct_present_flag
        if restriction {
            w.u(1, 1);
            w.u(1, 1);
            w.ue(MAX_BYTES_PER_PIC_DENOM);
            w.ue(MAX_BITS_PER_MB_DENOM);
            w.ue(LOG2_MAX_MV_LENGTH);
            w.ue(LOG2_MAX_MV_LENGTH);
            w.ue(0);
            w.ue(4); // max_dec_frame_buffering = max_num_ref_frames
        } else {
            w.u(1, 0);
        }
        let mut out = vec![0x67];
        out.extend_from_slice(&escape(&w.finish_rbsp()));
        out
    }

    /// se(v) to the codeNum that ue(v) writes (H.264 9.1.1).
    fn se_code(v: i32) -> u32 {
        if v > 0 {
            (2 * v - 1) as u32
        } else {
            (-2 * v) as u32
        }
    }

    #[test]
    fn hand_built_sps_with_scaling_lists_and_hrd() {
        let input = hand_built_sps(false);
        let out = add_bitstream_restriction(&input).expect("rewritten");
        assert_eq!(out, hand_built_sps(true), "same bits plus the restriction");
        assert_eq!(add_bitstream_restriction(&out), None);
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
    fn unparsable_sps_is_sent_unchanged() {
        // Truncated SPS (the VUI is cut off) and an SPS shorter than the
        // fixed header: both cannot be parsed.
        for bad in [&VT_SPS[..8], &[0x27, 0x42][..]] {
            assert_eq!(rewrite(bad), Rewrite::Unparsable);
            assert_eq!(fix_parameter_set(bad), bad);
        }
        assert!(UNPARSABLE_SPS_WARNED.load(Ordering::Relaxed));
        // Not an SPS, or an SPS that needs nothing: no warning case.
        assert_eq!(rewrite(&[0x28, 0xce, 0x3c, 0x80]), Rewrite::NotNeeded);
        assert_eq!(rewrite(&[]), Rewrite::NotNeeded);
        let fixed = fix_parameter_set(VT_SPS);
        assert_eq!(Some(fixed.clone()), add_bitstream_restriction(VT_SPS));
        assert_eq!(rewrite(&fixed), Rewrite::NotNeeded);
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
