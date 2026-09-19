//! Section 7 for H.264 and HEVC: a unit inside a `user_data_unregistered`
//! SEI, and that SEI inside an access unit.
//!
//! The NAL cutting, the AVCC reframing and the emulation-prevention
//! removal started as `core/src/avc.rs` of ffrwd/moq (same owner,
//! Apache-2.0) and are copied rather than depended on, so this crate
//! keeps its empty dependency list. What is new here is the other
//! direction of emulation prevention, SEI messages in both spellings,
//! the two-byte HEVC header, and access unit boundaries good enough to
//! know where an SEI may go.
//!
//! Two rules decide everything below. An SEI NAL unit goes before the
//! first VCL NAL unit of its access unit, which puts it after the
//! access unit delimiter, the parameter sets and any SEI that was
//! already there. And a reader touches nothing that is not both
//! `payloadType` 5 and opened by this format's UUID: an encoder's own
//! settings SEI, captions, HDR metadata and everything else travel on
//! untouched.

use crate::message::Unit;
use crate::{Error, Result};

/// Which of the two codecs a NAL belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    H265,
}

/// `user_data_unregistered`, the SEI payload type this format uses.
pub const PAYLOAD_TYPE_USER_DATA_UNREGISTERED: u32 = 5;

/// The H.264 NAL unit type of an SEI.
pub const H264_SEI: u8 = 6;
/// The HEVC NAL unit type of a prefix SEI.
pub const H265_PREFIX_SEI: u8 = 39;
/// The HEVC NAL unit type of a suffix SEI, which this format never
/// writes and never reads a unit out of.
pub const H265_SUFFIX_SEI: u8 = 40;

impl Codec {
    /// How many bytes the NAL header takes.
    pub fn header_len(self) -> usize {
        match self {
            Codec::H264 => 1,
            Codec::H265 => 2,
        }
    }

    /// A NAL unit's type, or `None` when the NAL is too short to have
    /// one.
    pub fn nal_type(self, nal: &[u8]) -> Option<u8> {
        match self {
            Codec::H264 => nal.first().map(|byte| byte & 0x1f),
            Codec::H265 => {
                if nal.len() < 2 {
                    None
                } else {
                    Some(nal[0] >> 1 & 0x3f)
                }
            }
        }
    }

    /// Whether a type is a coded slice: the NALs an access unit is
    /// built around.
    pub fn is_vcl_type(self, kind: u8) -> bool {
        match self {
            // 1 to 5 are the slices. 19 and 20 are the auxiliary and
            // extension slices, which no encoder here writes and which
            // never open an access unit on their own.
            Codec::H264 => (1..=5).contains(&kind),
            Codec::H265 => kind <= 31,
        }
    }

    /// Whether a NAL is a coded slice.
    pub fn is_vcl(self, nal: &[u8]) -> bool {
        self.nal_type(nal)
            .is_some_and(|kind| self.is_vcl_type(kind))
    }

    /// Whether a slice type starts a random access point: the frames a
    /// file's sync samples are cut at.
    pub fn is_keyframe_type(self, kind: u8) -> bool {
        match self {
            Codec::H264 => kind == 5,
            // BLA, IDR and CRA: the IRAP range.
            Codec::H265 => (16..=23).contains(&kind),
        }
    }

    /// Whether a non-slice type opens a new access unit when it turns
    /// up after one.
    ///
    /// The types left out are the ones that belong to the access unit
    /// they follow: filler data, and the end of sequence and end of
    /// stream markers.
    pub fn starts_access_unit(self, kind: u8) -> bool {
        match self {
            Codec::H264 => matches!(kind, 6..=9 | 13..=18),
            Codec::H265 => matches!(kind, 32..=35 | 39..=47),
        }
    }

    /// The NAL header of an SEI this format writes, for an access unit
    /// at `temporal_id_plus1`.
    ///
    /// HEVC requires a prefix SEI to carry the temporal id of the
    /// access unit it sits in, so the header is not a constant: a
    /// stream with temporal sub-layers would be non-conforming with one
    /// that always said zero. H.264 keeps its temporal id somewhere
    /// else entirely and its header is the same byte every time.
    pub fn sei_header(self, temporal_id_plus1: u8) -> Vec<u8> {
        match self {
            // nal_ref_idc 0, type 6.
            Codec::H264 => vec![0x06],
            // type 39, layer 0, and the access unit's temporal id.
            Codec::H265 => vec![H265_PREFIX_SEI << 1, temporal_id_plus1.max(1) & 0x07],
        }
    }

    /// The `nuh_temporal_id_plus1` of a NAL, which is 1 for H.264,
    /// where the field does not exist.
    pub fn temporal_id_plus1(self, nal: &[u8]) -> u8 {
        match self {
            Codec::H264 => 1,
            Codec::H265 => nal.get(1).map_or(1, |byte| (byte & 0x07).max(1)),
        }
    }

    /// Whether a NAL is an SEI a unit may sit in.
    pub fn is_prefix_sei(self, nal: &[u8]) -> bool {
        match self.nal_type(nal) {
            Some(kind) => match self {
                Codec::H264 => kind == H264_SEI,
                Codec::H265 => kind == H265_PREFIX_SEI,
            },
            None => false,
        }
    }
}

/// One NAL unit's place in an Annex B stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NalRef<'a> {
    /// Where the start code begins, which is where something spliced in
    /// before this NAL goes.
    pub code: usize,
    /// Where the NAL's own bytes begin.
    pub start: usize,
    /// Where they end.
    pub end: usize,
    /// The NAL, start code removed.
    pub bytes: &'a [u8],
}

/// The NAL units of an Annex B byte stream, with their offsets.
///
/// Both 3-byte (`00 00 01`) and 4-byte (`00 00 00 01`) start codes cut;
/// a zero byte directly before a 3-byte code belongs to the code, not
/// to the NAL before it. Bytes before the first start code are ignored,
/// as ffmpeg ignores them.
pub fn scan_nals(annexb: &[u8]) -> Vec<NalRef<'_>> {
    let mut codes: Vec<(usize, usize)> = Vec::new();
    let mut at = 0usize;
    while at + 2 < annexb.len() {
        if annexb[at] == 0 && annexb[at + 1] == 0 && annexb[at + 2] == 1 {
            let code = if at > 0 && annexb[at - 1] == 0 {
                at - 1
            } else {
                at
            };
            codes.push((code, at + 3));
            at += 3;
        } else {
            at += 1;
        }
    }
    let mut out = Vec::with_capacity(codes.len());
    for (index, (code, start)) in codes.iter().enumerate() {
        let end = match codes.get(index + 1) {
            Some((next, _)) => (*next).max(*start),
            None => annexb.len(),
        };
        out.push(NalRef {
            code: *code,
            start: *start,
            end,
            bytes: &annexb[*start..end],
        });
    }
    out
}

/// The NAL units of an Annex B byte stream, start codes removed.
pub fn split_nals(annexb: &[u8]) -> Vec<&[u8]> {
    scan_nals(annexb).into_iter().map(|nal| nal.bytes).collect()
}

/// One access unit of an Annex B stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AccessUnit {
    /// Where the access unit starts, at its first start code.
    pub start: usize,
    /// Where it ends, at the next access unit's first start code.
    pub end: usize,
    /// Where an SEI NAL goes: at the start code of the first coded
    /// slice, which is after the delimiter, the parameter sets and any
    /// SEI already there.
    pub insert_at: usize,
    /// Whether the access unit is a random access point.
    pub keyframe: bool,
    /// The `nuh_temporal_id_plus1` of its first coded slice, which an
    /// SEI put in this access unit has to repeat.
    pub temporal_id_plus1: u8,
}

/// The access units of an Annex B stream.
///
/// The boundary rule is the one a writer needs and no more: a new
/// access unit begins at a slice that says it is the first of a
/// picture, and at any parameter set, delimiter or SEI that turns up
/// after a slice. That is enough to place an SEI correctly in every
/// stream an encoder writes, which is what section 7 asks for.
pub fn access_units(annexb: &[u8], codec: Codec) -> Vec<AccessUnit> {
    let nals = scan_nals(annexb);
    let mut out: Vec<AccessUnit> = Vec::new();
    let mut seen_slice = false;
    for nal in &nals {
        let Some(kind) = codec.nal_type(nal.bytes) else {
            continue;
        };
        let slice = codec.is_vcl_type(kind);
        let boundary = if slice {
            seen_slice && first_slice_of_picture(codec, nal.bytes)
        } else {
            seen_slice && codec.starts_access_unit(kind)
        };
        if out.is_empty() || boundary {
            if let Some(last) = out.last_mut() {
                last.end = nal.code;
            }
            out.push(AccessUnit {
                start: nal.code,
                end: annexb.len(),
                insert_at: usize::MAX,
                keyframe: false,
                temporal_id_plus1: 1,
            });
            seen_slice = false;
        }
        if let Some(unit) = out.last_mut() {
            if slice {
                if unit.insert_at == usize::MAX {
                    unit.insert_at = nal.code;
                    unit.temporal_id_plus1 = codec.temporal_id_plus1(nal.bytes);
                }
                unit.keyframe |= codec.is_keyframe_type(kind);
                seen_slice = true;
            }
        }
    }
    // An access unit with no slice at all takes its own start as the
    // place to insert, which is where a slice would have gone.
    for unit in out.iter_mut() {
        if unit.insert_at == usize::MAX {
            unit.insert_at = unit.end;
        }
    }
    out
}

/// Whether a slice NAL says it opens a picture.
///
/// `first_mb_in_slice` in H.264 and `first_slice_segment_in_pic_flag`
/// in HEVC are both the first bit after the header, and both are one
/// for the first slice: an exp-Golomb zero is a single set bit. No
/// emulation prevention byte can land that early, because the header
/// byte before it is never zero.
fn first_slice_of_picture(codec: Codec, nal: &[u8]) -> bool {
    match nal.get(codec.header_len()) {
        Some(byte) => byte & 0x80 != 0,
        None => false,
    }
}

/// Removes emulation prevention bytes: `00 00 03` becomes `00 00`.
pub fn remove_emulation_prevention(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeros = 0u32;
    for byte in bytes {
        if zeros >= 2 && *byte == 3 {
            zeros = 0;
            continue;
        }
        zeros = if *byte == 0 { zeros + 1 } else { 0 };
        out.push(*byte);
    }
    out
}

/// Inserts emulation prevention bytes, so no `00 00 00`, `00 00 01`,
/// `00 00 02` or `00 00 03` survives into the byte stream.
///
/// This is what keeps a unit full of zeroes from growing a start code
/// in the middle of an SEI and cutting the stream in half.
pub fn insert_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 16 + 4);
    let mut zeros = 0u32;
    for byte in rbsp {
        if zeros >= 2 && *byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if *byte == 0 { zeros + 1 } else { 0 };
        out.push(*byte);
    }
    out
}

/// One SEI message: its type and its payload, the payload already free
/// of emulation prevention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeiMessage {
    pub payload_type: u32,
    pub payload: Vec<u8>,
}

/// The SEI messages of an SEI NAL unit.
///
/// Anything the NAL holds past what parses is left behind rather than
/// refused: a reader is looking for its own message, and a trailing
/// byte it does not understand is not a reason to drop the ones it
/// already read.
pub fn parse_sei(nal: &[u8], codec: Codec) -> Result<Vec<SeiMessage>> {
    if !codec.is_prefix_sei(nal) {
        return Err(Error::Malformed("the NAL unit is not a prefix SEI"));
    }
    let rbsp = remove_emulation_prevention(&nal[codec.header_len()..]);
    Ok(parse_sei_rbsp(&rbsp))
}

/// The SEI messages of an SEI RBSP, header and escapes already gone.
pub fn parse_sei_rbsp(rbsp: &[u8]) -> Vec<SeiMessage> {
    // `more_rbsp_data`: the payload ends at the stop bit, which is the
    // last byte that is not a trailing zero, and that byte is 0x80
    // because every SEI payload is a whole number of bytes.
    let mut end = rbsp.len();
    while end > 0 && rbsp[end - 1] == 0 {
        end -= 1;
    }
    if end > 0 && rbsp[end - 1] == 0x80 {
        end -= 1;
    }
    let data = &rbsp[..end];

    let mut out = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let Some((payload_type, next)) = read_ff(data, at) else {
            break;
        };
        let Some((payload_size, next)) = read_ff(data, next) else {
            break;
        };
        let size = payload_size as usize;
        if next + size > data.len() {
            break;
        }
        out.push(SeiMessage {
            payload_type,
            payload: data[next..next + size].to_vec(),
        });
        at = next + size;
    }
    out
}

/// One `ff`-extended value, and where it ended.
fn read_ff(data: &[u8], mut at: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    loop {
        let byte = *data.get(at)?;
        at += 1;
        value = value.checked_add(u32::from(byte))?;
        if byte != 0xff {
            return Some((value, at));
        }
    }
}

/// Appends one `ff`-extended value.
fn put_ff(out: &mut Vec<u8>, mut value: u32) {
    while value >= 255 {
        out.push(0xff);
        value -= 255;
    }
    out.push(value as u8);
}

/// An SEI NAL unit carrying `messages`, escapes and trailing bits and
/// all, for an access unit at `temporal_id_plus1`.
pub fn write_sei(messages: &[SeiMessage], codec: Codec, temporal_id_plus1: u8) -> Vec<u8> {
    let mut rbsp = Vec::new();
    for message in messages {
        put_ff(&mut rbsp, message.payload_type);
        put_ff(&mut rbsp, message.payload.len() as u32);
        rbsp.extend_from_slice(&message.payload);
    }
    // rbsp_trailing_bits: a one bit, then zeroes to the byte.
    rbsp.push(0x80);
    let mut nal = codec.sei_header(temporal_id_plus1);
    nal.extend_from_slice(&insert_emulation_prevention(&rbsp));
    nal
}

/// A unit as an SEI NAL unit, ready to splice into an access unit at
/// `temporal_id_plus1`, which [`AccessUnit`] carries.
pub fn wrap_unit_at(unit: &[u8], codec: Codec, temporal_id_plus1: u8) -> Vec<u8> {
    write_sei(
        &[SeiMessage {
            payload_type: PAYLOAD_TYPE_USER_DATA_UNREGISTERED,
            payload: unit.to_vec(),
        }],
        codec,
        temporal_id_plus1,
    )
}

/// A unit as an SEI NAL unit for an access unit of the base temporal
/// layer, which is every access unit of a stream without sub-layers.
pub fn wrap_unit(unit: &[u8], codec: Codec) -> Vec<u8> {
    wrap_unit_at(unit, codec, 1)
}

/// The units in one SEI NAL unit: the `user_data_unregistered`
/// messages that open with this format's UUID, and no others.
pub fn units_in_nal(nal: &[u8], codec: Codec) -> Vec<Vec<u8>> {
    let Ok(messages) = parse_sei(nal, codec) else {
        return Vec::new();
    };
    messages
        .into_iter()
        .filter(|message| {
            message.payload_type == PAYLOAD_TYPE_USER_DATA_UNREGISTERED
                && Unit::is_ours(&message.payload)
        })
        .map(|message| message.payload)
        .collect()
}

/// Every unit in an Annex B stream, or in one access unit of one.
pub fn units_annexb(annexb: &[u8], codec: Codec) -> Vec<Vec<u8>> {
    scan_nals(annexb)
        .into_iter()
        .flat_map(|nal| units_in_nal(nal.bytes, codec))
        .collect()
}

/// An access unit with `sei` spliced in before its first coded slice.
///
/// The bytes on either side are copied through untouched, so a stream
/// woven this way differs from the original only by the NALs added.
pub fn insert_sei_annexb(au: &[u8], sei: &[u8], codec: Codec) -> Result<Vec<u8>> {
    let units = access_units(au, codec);
    let at = match units.first() {
        Some(unit) => unit.insert_at,
        None => return Err(Error::Malformed("the bytes hold no access unit")),
    };
    let mut out = Vec::with_capacity(au.len() + sei.len() + 4);
    out.extend_from_slice(&au[..at]);
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(sei);
    out.extend_from_slice(&au[at..]);
    Ok(out)
}

/// The NAL length prefix an `avcC` declares, in bytes. Four for
/// everything ffmpeg writes; a record too short to say is read as four.
pub fn avcc_length_size(avcc: &[u8]) -> usize {
    match avcc.get(4) {
        Some(byte) => usize::from(byte & 0x3) + 1,
        None => 4,
    }
}

/// The NAL length prefix an `hvcC` declares, in bytes.
pub fn hvcc_length_size(hvcc: &[u8]) -> usize {
    match hvcc.get(21) {
        Some(byte) => usize::from(byte & 0x3) + 1,
        None => 4,
    }
}

/// One Annex B access unit reframed with length prefixes, the framing
/// MP4 and its relatives store samples in.
pub fn annexb_to_length_prefixed(annexb: &[u8], length_size: usize) -> Result<Vec<u8>> {
    check_length_size(length_size)?;
    let mut out = Vec::with_capacity(annexb.len());
    for nal in scan_nals(annexb) {
        put_length(&mut out, nal.bytes.len(), length_size)?;
        out.extend_from_slice(nal.bytes);
    }
    Ok(out)
}

/// One length-prefixed sample back as Annex B, 4-byte start codes.
pub fn length_prefixed_to_annexb(sample: &[u8], length_size: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(sample.len() + 8);
    for nal in split_length_prefixed(sample, length_size)? {
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    Ok(out)
}

/// The NAL units of a length-prefixed sample.
pub fn split_length_prefixed(sample: &[u8], length_size: usize) -> Result<Vec<&[u8]>> {
    check_length_size(length_size)?;
    let mut out = Vec::new();
    let mut at = 0usize;
    while at < sample.len() {
        let header = sample
            .get(at..at + length_size)
            .ok_or(Error::Malformed("the sample ends inside a NAL length"))?;
        let length = header
            .iter()
            .fold(0usize, |value, byte| (value << 8) | usize::from(*byte));
        at += length_size;
        let nal = sample
            .get(at..at + length)
            .ok_or(Error::Malformed("a NAL overruns the sample"))?;
        at += length;
        out.push(nal);
    }
    Ok(out)
}

/// A length-prefixed sample with `sei` spliced in before its first
/// coded slice.
pub fn insert_sei_length_prefixed(
    sample: &[u8],
    sei: &[u8],
    length_size: usize,
    codec: Codec,
) -> Result<Vec<u8>> {
    let nals = split_length_prefixed(sample, length_size)?;
    let at = nals
        .iter()
        .position(|nal| codec.is_vcl(nal))
        .unwrap_or(nals.len());
    let mut out = Vec::with_capacity(sample.len() + sei.len() + length_size);
    for (index, nal) in nals.iter().enumerate() {
        if index == at {
            put_length(&mut out, sei.len(), length_size)?;
            out.extend_from_slice(sei);
        }
        put_length(&mut out, nal.len(), length_size)?;
        out.extend_from_slice(nal);
    }
    if at == nals.len() {
        put_length(&mut out, sei.len(), length_size)?;
        out.extend_from_slice(sei);
    }
    Ok(out)
}

/// Every unit in a length-prefixed sample.
pub fn units_length_prefixed(
    sample: &[u8],
    length_size: usize,
    codec: Codec,
) -> Result<Vec<Vec<u8>>> {
    Ok(split_length_prefixed(sample, length_size)?
        .into_iter()
        .flat_map(|nal| units_in_nal(nal, codec))
        .collect())
}

fn check_length_size(length_size: usize) -> Result<()> {
    if !(1..=4).contains(&length_size) {
        return Err(Error::Malformed("a NAL length prefix is 1 to 4 bytes"));
    }
    Ok(())
}

fn put_length(out: &mut Vec<u8>, length: usize, length_size: usize) -> Result<()> {
    if length_size < 4 && length >= 1 << (length_size * 8) {
        return Err(Error::TooLarge);
    }
    for shift in (0..length_size).rev() {
        out.push((length >> (shift * 8)) as u8);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::UUID;

    /// A short Annex B stream shaped like what x264 writes: parameter
    /// sets, the encoder's own SEI, an IDR, then two more pictures.
    fn a_stream() -> Vec<u8> {
        let mut out = Vec::new();
        let mut nal = |payload: &[u8], four: bool| {
            out.extend_from_slice(if four { &[0, 0, 0, 1] } else { &[0, 0, 1] });
            out.extend_from_slice(payload);
        };
        nal(&[0x67, 0x64, 0x00, 0x1f, 0xac], true); // SPS
        nal(&[0x68, 0xeb, 0xec, 0xb2], true); // PPS
        nal(&x264_sei(), false); // the encoder's settings
        nal(&[0x65, 0x88, 0x84, 0x00], false); // IDR slice
        nal(&[0x41, 0x9a, 0x01], false); // a P slice
        nal(&[0x41, 0x9a, 0x02], false); // another picture
        out
    }

    /// An SEI of payload type 5 with somebody else's UUID, which is
    /// what an x264 stream opens with.
    fn x264_sei() -> Vec<u8> {
        let mut payload = vec![0xdcu8, 0x45, 0xe9, 0xbd, 0xe6, 0xd9, 0x48, 0xb7];
        payload.extend_from_slice(&[0x96, 0x2c, 0xd8, 0x20, 0xd9, 0x23, 0xee, 0xef]);
        payload.extend_from_slice(b"x264 - core 164 - options: cabac=1");
        write_sei(
            &[SeiMessage {
                payload_type: 5,
                payload,
            }],
            Codec::H264,
            1,
        )
    }

    fn a_unit() -> Vec<u8> {
        let mut unit = UUID.to_vec();
        unit.push(1);
        unit.extend_from_slice(&[0x02, 0x04, 0x01, 0x00, 0x00, 0x00]);
        unit
    }

    #[test]
    fn emulation_prevention_round_trips_the_worst_payloads() {
        let patterns: [&[u8]; 8] = [
            &[0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 0, 1, 0, 0, 1, 0, 0, 1],
            &[0, 0, 3, 0, 0, 3, 0, 0, 3],
            &[0, 0, 2],
            &[1, 0, 0, 0, 1, 0, 0, 0, 1],
            &[0xff, 0, 0, 0xff],
            &[0],
            &[],
        ];
        for pattern in patterns {
            let escaped = insert_emulation_prevention(pattern);
            assert_eq!(
                remove_emulation_prevention(&escaped),
                pattern,
                "pattern {pattern:?}"
            );
            assert!(
                !escaped.windows(3).any(|w| w == [0, 0, 1]),
                "an escaped payload grew a start code: {escaped:?}"
            );
        }
    }

    #[test]
    fn a_unit_full_of_start_codes_survives_the_wrap() {
        let mut unit = UUID.to_vec();
        unit.push(1);
        for _ in 0..40 {
            unit.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 3, 0, 0, 2]);
        }
        let nal = wrap_unit(&unit, Codec::H264);
        assert!(
            !nal.windows(3).any(|w| w == [0, 0, 1]),
            "the SEI NAL holds a start code"
        );
        assert_eq!(units_in_nal(&nal, Codec::H264), vec![unit]);
    }

    #[test]
    fn a_payload_of_every_size_around_the_ff_boundary_round_trips() {
        for size in [0usize, 1, 254, 255, 256, 509, 510, 511, 1000] {
            let mut unit = UUID.to_vec();
            unit.push(1);
            unit.resize(17 + size, 0x5a);
            for codec in [Codec::H264, Codec::H265] {
                let nal = wrap_unit(&unit, codec);
                assert_eq!(units_in_nal(&nal, codec), vec![unit.clone()], "{size}");
                let messages = parse_sei(&nal, codec).expect("messages");
                assert_eq!(messages.len(), 1);
                assert_eq!(messages[0].payload.len(), unit.len());
            }
        }
    }

    #[test]
    fn an_sei_that_is_not_ours_is_left_alone() {
        let nal = x264_sei();
        assert!(units_in_nal(&nal, Codec::H264).is_empty());
        let messages = parse_sei(&nal, Codec::H264).expect("messages");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].payload_type, 5);
        assert!(messages[0].payload.starts_with(&[0xdc, 0x45]));
    }

    #[test]
    fn other_sei_types_in_the_same_nal_are_kept_and_ours_is_found() {
        let nal = write_sei(
            &[
                SeiMessage {
                    payload_type: 1,
                    payload: vec![0x11, 0x22],
                },
                SeiMessage {
                    payload_type: 5,
                    payload: a_unit(),
                },
                SeiMessage {
                    payload_type: 137,
                    payload: vec![0x33],
                },
            ],
            Codec::H264,
            1,
        );
        let messages = parse_sei(&nal, Codec::H264).expect("messages");
        assert_eq!(
            messages.iter().map(|m| m.payload_type).collect::<Vec<_>>(),
            vec![1, 5, 137]
        );
        assert_eq!(units_in_nal(&nal, Codec::H264), vec![a_unit()]);
    }

    #[test]
    fn the_hevc_header_is_the_two_bytes_of_a_prefix_sei() {
        let nal = wrap_unit(&a_unit(), Codec::H265);
        assert_eq!(&nal[..2], &[0x4e, 0x01]);
        assert_eq!(Codec::H265.nal_type(&nal), Some(H265_PREFIX_SEI));
        assert!(Codec::H265.is_prefix_sei(&nal));
        // The same bytes read as H.264 are a different type entirely,
        // so a reader that has the codec wrong finds nothing.
        assert!(units_in_nal(&nal, Codec::H264).is_empty());
    }

    #[test]
    fn an_hevc_sei_repeats_its_access_units_temporal_id() {
        // A stream of two HEVC pictures, the second on temporal
        // sub-layer 2, which its slice header says in the NAL header's
        // second byte.
        let mut stream = Vec::new();
        for (kind, temporal_id_plus1) in [(19u8, 1u8), (1, 3)] {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(&[kind << 1, temporal_id_plus1, 0x80, 0x00]);
        }
        let units = access_units(&stream, Codec::H265);
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].temporal_id_plus1, 1);
        assert_eq!(units[1].temporal_id_plus1, 3);
        assert!(units[0].keyframe, "type 19 is an IDR");

        let sei = wrap_unit_at(&a_unit(), Codec::H265, units[1].temporal_id_plus1);
        assert_eq!(sei[0] >> 1 & 0x3f, H265_PREFIX_SEI);
        assert_eq!(sei[1] & 0x07, 3, "the SEI is on the same sub-layer");
        assert_eq!(units_in_nal(&sei, Codec::H265), vec![a_unit()]);
        // The base layer is what a stream without sub-layers gets.
        assert_eq!(wrap_unit(&a_unit(), Codec::H265)[1], 1);
    }

    #[test]
    fn a_suffix_sei_is_not_where_a_unit_lives() {
        let mut nal = wrap_unit(&a_unit(), Codec::H265);
        nal[0] = H265_SUFFIX_SEI << 1;
        assert!(units_in_nal(&nal, Codec::H265).is_empty());
    }

    #[test]
    fn access_units_cut_where_the_pictures_do() {
        let stream = a_stream();
        let units = access_units(&stream, Codec::H264);
        assert_eq!(units.len(), 3, "three pictures");
        assert!(units[0].keyframe, "the first is an IDR");
        assert!(!units[1].keyframe);
        assert_eq!(units[0].start, 0);
        assert_eq!(units.last().expect("a unit").end, stream.len());
        // The first access unit inserts before its IDR slice, which is
        // after the parameter sets and after x264's SEI.
        let nals = scan_nals(&stream);
        let idr = nals
            .iter()
            .find(|nal| Codec::H264.nal_type(nal.bytes) == Some(5))
            .expect("an IDR");
        assert_eq!(units[0].insert_at, idr.code);
    }

    #[test]
    fn a_woven_stream_differs_only_by_the_sei() {
        let stream = a_stream();
        let sei = wrap_unit(&a_unit(), Codec::H264);
        let units = access_units(&stream, Codec::H264);
        let mut woven = Vec::new();
        let mut at = 0usize;
        for unit in &units {
            woven.extend_from_slice(&stream[at..unit.insert_at]);
            woven.extend_from_slice(&[0, 0, 0, 1]);
            woven.extend_from_slice(&sei);
            at = unit.insert_at;
        }
        woven.extend_from_slice(&stream[at..]);

        assert_eq!(units_annexb(&woven, Codec::H264), vec![a_unit(); 3]);
        // Every NAL of the original is still there, in order.
        let original: Vec<&[u8]> = split_nals(&stream);
        let after: Vec<&[u8]> = split_nals(&woven);
        let kept: Vec<&[u8]> = after
            .into_iter()
            .filter(|nal| units_in_nal(nal, Codec::H264).is_empty())
            .collect();
        assert_eq!(kept, original);
        assert_eq!(access_units(&woven, Codec::H264).len(), units.len());
    }

    #[test]
    fn an_sei_goes_in_before_the_first_slice_either_framing() {
        let stream = a_stream();
        let au = &stream[..access_units(&stream, Codec::H264)[0].end];
        let sei = wrap_unit(&a_unit(), Codec::H264);

        let woven = insert_sei_annexb(au, &sei, Codec::H264).expect("a woven access unit");
        let kinds: Vec<Option<u8>> = split_nals(&woven)
            .iter()
            .map(|nal| Codec::H264.nal_type(nal))
            .collect();
        assert_eq!(
            kinds,
            vec![Some(7), Some(8), Some(6), Some(6), Some(5)],
            "ours goes last of the non-slices"
        );
        assert_eq!(units_annexb(&woven, Codec::H264), vec![a_unit()]);

        for length_size in 1..=4usize {
            let sample = annexb_to_length_prefixed(au, length_size).expect("a sample");
            let woven =
                insert_sei_length_prefixed(&sample, &sei, length_size, Codec::H264).expect("woven");
            assert_eq!(
                units_length_prefixed(&woven, length_size, Codec::H264).expect("units"),
                vec![a_unit()]
            );
            let back = length_prefixed_to_annexb(&woven, length_size).expect("annex b");
            assert_eq!(units_annexb(&back, Codec::H264), vec![a_unit()]);
            let kinds: Vec<Option<u8>> = split_nals(&back)
                .iter()
                .map(|nal| Codec::H264.nal_type(nal))
                .collect();
            assert_eq!(kinds, vec![Some(7), Some(8), Some(6), Some(6), Some(5)]);
        }
    }

    #[test]
    fn the_two_framings_round_trip() {
        let stream = a_stream();
        for length_size in 1..=4usize {
            let framed = annexb_to_length_prefixed(&stream, length_size).expect("framed");
            let back = length_prefixed_to_annexb(&framed, length_size).expect("annex b");
            assert_eq!(split_nals(&back), split_nals(&stream));
        }
        assert!(annexb_to_length_prefixed(&stream, 0).is_err());
        assert!(annexb_to_length_prefixed(&stream, 5).is_err());
        assert_eq!(avcc_length_size(&[1, 0x64, 0, 0x1f, 0xff]), 4);
        assert_eq!(avcc_length_size(&[]), 4);
        assert_eq!(hvcc_length_size(&[0; 22]), 1);
        assert_eq!(hvcc_length_size(&[]), 4);
    }

    #[test]
    fn a_nal_too_long_for_its_prefix_is_refused() {
        let mut annexb = vec![0, 0, 0, 1, 0x65];
        annexb.extend(std::iter::repeat_n(0x5a, 300));
        assert_eq!(annexb_to_length_prefixed(&annexb, 1), Err(Error::TooLarge));
        assert!(annexb_to_length_prefixed(&annexb, 2).is_ok());
    }

    #[test]
    fn a_truncated_sample_is_refused_not_panicked() {
        let stream = a_stream();
        let sample = annexb_to_length_prefixed(&stream, 4).expect("a sample");
        for cut in 1..sample.len() {
            let _ = split_length_prefixed(&sample[..cut], 4);
            let _ = units_length_prefixed(&sample[..cut], 4, Codec::H264);
        }
        assert!(split_length_prefixed(&sample[..3], 4).is_err());
    }

    #[test]
    fn truncated_and_random_nals_never_panic() {
        let nal = wrap_unit(&a_unit(), Codec::H264);
        for cut in 0..nal.len() {
            for codec in [Codec::H264, Codec::H265] {
                let _ = units_in_nal(&nal[..cut], codec);
                let _ = parse_sei(&nal[..cut], codec);
                let _ = access_units(&nal[..cut], codec);
            }
        }
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..3000 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 48 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 33) as u8);
            }
            for codec in [Codec::H264, Codec::H265] {
                let _ = units_in_nal(&bytes, codec);
                let _ = units_annexb(&bytes, codec);
                let _ = access_units(&bytes, codec);
                let _ = insert_sei_annexb(&bytes, &nal, codec);
                for length_size in 1..=4 {
                    let _ = units_length_prefixed(&bytes, length_size, codec);
                }
            }
            let _ = parse_sei_rbsp(&bytes);
            let _ = remove_emulation_prevention(&bytes);
            assert_eq!(
                remove_emulation_prevention(&insert_emulation_prevention(&bytes)),
                bytes
            );
        }
    }

    #[test]
    fn an_sei_whose_size_overruns_the_nal_gives_up_quietly() {
        // A payload size of 200 with ten bytes behind it.
        let mut rbsp = vec![5u8, 200];
        rbsp.extend_from_slice(&[0xaa; 10]);
        rbsp.push(0x80);
        let mut nal = vec![0x06u8];
        nal.extend_from_slice(&rbsp);
        assert!(parse_sei(&nal, Codec::H264).expect("messages").is_empty());
        assert!(units_in_nal(&nal, Codec::H264).is_empty());
    }

    #[test]
    fn a_stream_with_no_slices_still_takes_an_sei() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&[0, 0, 0, 1]);
        stream.extend_from_slice(&[0x67, 0x64, 0x00]);
        let sei = wrap_unit(&a_unit(), Codec::H264);
        let woven = insert_sei_annexb(&stream, &sei, Codec::H264).expect("woven");
        assert_eq!(units_annexb(&woven, Codec::H264), vec![a_unit()]);
    }
}
