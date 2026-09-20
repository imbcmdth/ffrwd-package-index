//! Section 7, in this format's own words.
//!
//! Every byte of the work is `ffrwd-nal`'s: cutting a stream into NAL
//! units and OBUs, emulation prevention, building and reading a
//! `user_data_unregistered` SEI or a metadata OBU, and finding the
//! boundaries of a stream that is still being written. What this module
//! adds is the two constants that make those calls this format's rather
//! than anyone else's, [`crate::UUID`] and [`crate::METADATA_TYPE`], so
//! that the rest of the workspace says "units" where it means units.
//!
//! Anything here that does not bind one of those constants is not here.
//! A caller that wants access units, temporal units, reframing or the
//! length an `avcC` declares calls `ffrwd_nal` for it directly, because
//! a wrapper that only renames a function is a second name to keep in
//! step with the first.

use ffrwd_nal::feed::{Feed, StreamKind};
use ffrwd_nal::obu::ObuRef;
use ffrwd_nal::{obu, sei, Codec};

use crate::{Result, METADATA_TYPE, SELECT, UUID};

/// A unit as an SEI NAL unit, ready to splice into an access unit at
/// `temporal_id_plus1`, which `ffrwd_nal::h26x::AccessUnit` carries.
pub fn wrap_unit_at(unit: &[u8], codec: Codec, temporal_id_plus1: u8) -> Vec<u8> {
    sei::write_user_data_at(unit, codec, temporal_id_plus1)
}

/// A unit as an SEI NAL unit for an access unit of the base temporal
/// layer, which is every access unit of a stream without sub-layers.
pub fn wrap_unit(unit: &[u8], codec: Codec) -> Vec<u8> {
    sei::write_user_data(unit, codec)
}

/// The units in one SEI NAL unit: the `user_data_unregistered` messages
/// that open with this format's UUID, and no others.
pub fn units_in_nal(nal: &[u8], codec: Codec) -> Vec<Vec<u8>> {
    sei::user_data_in_nal(nal, codec, &UUID)
}

/// Every unit in an Annex B stream, or in one access unit of one.
pub fn units_annexb(annexb: &[u8], codec: Codec) -> Vec<Vec<u8>> {
    sei::user_data_annexb(annexb, codec, &UUID)
}

/// Every unit in a length-prefixed sample.
pub fn units_length_prefixed(
    sample: &[u8],
    length_size: usize,
    codec: Codec,
) -> Result<Vec<Vec<u8>>> {
    Ok(sei::user_data_length_prefixed(
        sample,
        length_size,
        codec,
        &UUID,
    )?)
}

/// The unit in one OBU, if it is a metadata OBU of this format's type
/// whose payload opens with this format's UUID.
pub fn unit_in_obu(obu: &ObuRef<'_>) -> Option<Vec<u8>> {
    obu::metadata_payload(obu, METADATA_TYPE)
        .filter(|payload| payload.starts_with(&UUID))
        .map(<[u8]>::to_vec)
}

/// Every unit in a low-overhead AV1 stream, temporal unit or sample.
pub fn units_obu(bytes: &[u8]) -> Vec<Vec<u8>> {
    obu::user_data(bytes, METADATA_TYPE, &UUID)
}

/// A feed that cuts a growing stream into carriers and takes this
/// format's units off each of them.
pub fn feed(kind: StreamKind) -> Feed {
    Feed::new(kind, SELECT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Encoding, Message, Space, Unit};

    fn unit() -> Vec<u8> {
        let mut space = Space::new(1, 4, Encoding::F32);
        space.model = "test:model".into();
        Unit::new(vec![Message::Space(space)]).encode()
    }

    /// Somebody else's `user_data_unregistered` SEI, which is what an
    /// x264 stream opens with, and somebody else's metadata OBU of the
    /// same private type this format uses.
    #[test]
    fn a_payload_that_is_not_this_formats_is_left_where_it_is() {
        let mut theirs = vec![0xdcu8, 0x45, 0xe9, 0xbd, 0xe6, 0xd9, 0x48, 0xb7];
        theirs.extend_from_slice(&[0x96, 0x2c, 0xd8, 0x20, 0xd9, 0x23, 0xee, 0xef]);
        theirs.extend_from_slice(b"x264 - core 164");
        let nal = sei::write_user_data(&theirs, Codec::H264);
        assert!(units_in_nal(&nal, Codec::H264).is_empty());
        assert!(units_annexb(&[&[0, 0, 0, 1][..], &nal].concat(), Codec::H264).is_empty());

        let obu_bytes = obu::write_metadata(METADATA_TYPE, &theirs);
        assert!(units_obu(&obu_bytes).is_empty());
    }

    /// A unit goes in under this format's UUID and metadata type, and
    /// comes back out under the same two, whichever spelling carries it.
    #[test]
    fn a_unit_goes_out_and_comes_back_in_every_spelling() {
        for codec in [Codec::H264, Codec::H265] {
            let nal = wrap_unit(&unit(), codec);
            assert_eq!(units_in_nal(&nal, codec), vec![unit()]);
            assert_eq!(wrap_unit_at(&unit(), codec, 1), nal);

            let mut annexb = vec![0, 0, 0, 1];
            annexb.extend_from_slice(&nal);
            assert_eq!(units_annexb(&annexb, codec), vec![unit()]);

            let sample = ffrwd_nal::annexb::annexb_to_length_prefixed(&annexb, 4)
                .expect("a length-prefixed sample");
            assert_eq!(
                units_length_prefixed(&sample, 4, codec).expect("units"),
                vec![unit()]
            );
        }

        let obu_bytes = obu::write_metadata(METADATA_TYPE, &unit());
        assert_eq!(units_obu(&obu_bytes), vec![unit()]);
        let obus = obu::scan_obus(&obu_bytes).expect("obus");
        assert_eq!(unit_in_obu(&obus[0]), Some(unit()));
    }

    /// The feed is a feed of this format: it hands back the units the
    /// carriers carried, not every payload in them.
    #[test]
    fn the_feed_answers_this_formats_units_and_no_others() {
        let mut stream = Vec::new();
        for slice in [0x65u8, 0x41] {
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(&wrap_unit(&unit(), Codec::H264));
            stream.extend_from_slice(&[0, 0, 0, 1]);
            stream.extend_from_slice(&sei::write_user_data(&[0xab; 20], Codec::H264));
            stream.extend_from_slice(&[0, 0, 0, 1, slice, 0x88, 0x84, 0x00]);
        }
        let mut feed = feed(StreamKind::Nal(Codec::H264));
        let mut carried = feed.push(&stream).expect("a push");
        carried.extend(feed.finish().expect("the last carrier"));
        assert_eq!(carried.len(), 2);
        assert!(carried[0].keyframe);
        for carrier in &carried {
            assert_eq!(carrier.payloads, vec![unit()]);
        }
    }
}
