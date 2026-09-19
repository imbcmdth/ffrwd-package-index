//! Which framing a pad's packets are in, and getting units into and
//! out of one.
//!
//! Three shapes reach a packet filter and section 7 covers all three.
//! H.264 and HEVC travel either as Annex B, start code before every
//! NAL, or length-prefixed, the framing an `avcC` or `hvcC` record
//! describes and MP4 stores samples in; which one it is, and how wide
//! the length is, is what the stream's extradata says. AV1 packets are
//! low-overhead OBUs whatever the container.
//!
//! The extradata itself is never touched. A filter may answer `init`
//! with a changed header, and the one in `weave/` has no reason to: the
//! SPS and PPS that described the pictures still describe them, and an
//! SEI added to an access unit changes nothing out of band.
//!
//! Both directions are here because both the writer and the readers
//! need them, and a packet framed one way on the way in has to be
//! framed the same way on the way out.

use ffrwd_index_core::avc::{self, Codec};
use ffrwd_index_core::obu;

/// How one pad's packets are framed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// Start codes, which is what an elementary stream and ffmpeg's own
    /// NUT carry.
    AnnexB(Codec),
    /// A length before every NAL, as an `avcC` or `hvcC` declares.
    LengthPrefixed { codec: Codec, length_size: usize },
    /// Low-overhead OBUs: no lengths outside the OBUs' own.
    Av1,
}

/// The codecs this module can be opened for, ffmpeg's names for them,
/// most preferred first.
pub const CODECS: &[&str] = &["h264", "hevc", "av1"];

/// What a pad carries, from the codec name and the out-of-band header.
///
/// An `avcC` or `hvcC` opens with a configuration version of 1; an
/// Annex B header opens with a start code, whose first byte is zero,
/// and a stream that carries no header at all is Annex B too. That is
/// the same test ffmpeg makes of the same bytes.
pub fn framing_of(codec: &str, extradata: &[u8]) -> Result<Framing, String> {
    match codec {
        "h264" | "avc1" => Ok(match configuration_record(extradata, 7) {
            true => Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: avc::avcc_length_size(extradata),
            },
            false => Framing::AnnexB(Codec::H264),
        }),
        "hevc" | "h265" | "hvc1" | "hev1" => Ok(match configuration_record(extradata, 23) {
            true => Framing::LengthPrefixed {
                codec: Codec::H265,
                length_size: avc::hvcc_length_size(extradata),
            },
            false => Framing::AnnexB(Codec::H265),
        }),
        "av1" => Ok(Framing::Av1),
        other => Err(format!(
            "weave writes {} and not {other}",
            CODECS.join(", ")
        )),
    }
}

fn configuration_record(extradata: &[u8], least: usize) -> bool {
    extradata.len() >= least && extradata[0] == 1
}

/// One packet with a unit's bytes put where section 7 says they go:
/// before the first coded slice for H.264 and HEVC, before the frame
/// header for AV1. Everything else in the packet is copied through
/// exactly as the encoder wrote it.
pub fn insert(framing: Framing, packet: &[u8], unit: &[u8]) -> Result<Vec<u8>, String> {
    match framing {
        Framing::AnnexB(codec) => {
            let sei = avc::wrap_unit_at(unit, codec, temporal_id_annexb(packet, codec));
            avc::insert_sei_annexb(packet, &sei, codec).map_err(|err| err.to_string())
        }
        Framing::LengthPrefixed { codec, length_size } => {
            let nals =
                avc::split_length_prefixed(packet, length_size).map_err(|err| err.to_string())?;
            let temporal_id_plus1 = nals
                .iter()
                .find(|nal| codec.is_vcl(nal))
                .map_or(1, |nal| codec.temporal_id_plus1(nal));
            let sei = avc::wrap_unit_at(unit, codec, temporal_id_plus1);
            avc::insert_sei_length_prefixed(packet, &sei, length_size, codec)
                .map_err(|err| err.to_string())
        }
        Framing::Av1 => {
            let metadata = obu::write_metadata_obu(unit);
            obu::insert_metadata_obu(packet, &metadata).map_err(|err| err.to_string())
        }
    }
}

/// Every unit of this format in one packet, in the order it carries
/// them. A packet this reader cannot parse answers none rather than
/// stopping the read: one broken access unit in a stream is not a
/// reason to lose the rest.
pub fn units_in(framing: Framing, packet: &[u8]) -> Vec<Vec<u8>> {
    match framing {
        Framing::AnnexB(codec) => avc::units_annexb(packet, codec),
        Framing::LengthPrefixed { codec, length_size } => {
            avc::units_length_prefixed(packet, length_size, codec).unwrap_or_default()
        }
        Framing::Av1 => obu::units_obu(packet),
    }
}

/// The `nuh_temporal_id_plus1` an SEI in this access unit has to
/// repeat, which is its first coded slice's. H.264 has no such field
/// and the wrapper ignores what it is given.
fn temporal_id_annexb(packet: &[u8], codec: Codec) -> u8 {
    avc::access_units(packet, codec)
        .first()
        .map_or(1, |unit| unit.temporal_id_plus1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_index_core::message::{Encoding, Message, Space, Unit};

    fn unit() -> Vec<u8> {
        let mut space = Space::new(0, 4, Encoding::I8);
        space.model = "test:model".into();
        Unit::new(vec![Message::Space(space)]).encode()
    }

    /// A one-slice h264 access unit in Annex B: an SPS, a PPS and an IDR
    /// slice whose first bit says it opens the picture.
    fn annexb_h264() -> Vec<u8> {
        let mut out = Vec::new();
        for nal in [
            vec![0x67u8, 0x42, 0x00, 0x0a, 0x96],
            vec![0x68, 0xce, 0x3c, 0x80],
            vec![0x65, 0x88, 0x84, 0x00, 0x21],
        ] {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&nal);
        }
        out
    }

    #[test]
    fn extradata_tells_the_framing_and_the_length_width() {
        assert_eq!(
            framing_of("h264", &[]).expect("a framing"),
            Framing::AnnexB(Codec::H264)
        );
        assert_eq!(
            framing_of("h264", &[0, 0, 0, 1, 0x67, 0x42, 0x00]).expect("a framing"),
            Framing::AnnexB(Codec::H264)
        );
        // An avcC: version, profile, compatibility, level, then the
        // length size less one in the low two bits.
        let avcc = [1u8, 0x42, 0x00, 0x0a, 0xff, 0xe1, 0x00];
        assert_eq!(
            framing_of("h264", &avcc).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 4
            }
        );
        let two_byte = [1u8, 0x42, 0x00, 0x0a, 0xfd, 0xe1, 0x00];
        assert_eq!(
            framing_of("h264", &two_byte).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H264,
                length_size: 2
            }
        );
        // An hvcC is longer, and its length size sits at byte 21.
        let mut hvcc = vec![1u8; 23];
        hvcc[21] = 0xf3;
        assert_eq!(
            framing_of("hevc", &hvcc).expect("a framing"),
            Framing::LengthPrefixed {
                codec: Codec::H265,
                length_size: 4
            }
        );
        assert_eq!(
            framing_of("av1", &[0x81, 0x05]).expect("a framing"),
            Framing::Av1
        );
        assert!(framing_of("vp9", &[]).unwrap_err().contains("weave writes"));
    }

    #[test]
    fn what_goes_in_comes_back_out_of_every_framing() {
        let packet = annexb_h264();
        let annexb = Framing::AnnexB(Codec::H264);
        let woven = insert(annexb, &packet, &unit()).expect("woven");
        assert_eq!(units_in(annexb, &woven), vec![unit()]);
        assert!(
            units_in(annexb, &packet).is_empty(),
            "a unit came from nowhere"
        );

        let sample = avc::annexb_to_length_prefixed(&packet, 4).expect("a sample");
        let prefixed = Framing::LengthPrefixed {
            codec: Codec::H264,
            length_size: 4,
        };
        let woven = insert(prefixed, &sample, &unit()).expect("woven");
        assert_eq!(units_in(prefixed, &woven), vec![unit()]);

        let obus = vec![0x12u8, 0x00, 0x0a, 0x01, 0x00, 0x32, 0x02, 0x00, 0x00];
        let woven = insert(Framing::Av1, &obus, &unit()).expect("woven");
        assert_eq!(units_in(Framing::Av1, &woven), vec![unit()]);

        // Bytes that are not the framing they were said to be answer
        // nothing rather than stopping a read.
        assert!(units_in(prefixed, &[0xff, 0xff, 0xff, 0xff, 1]).is_empty());
    }

    #[test]
    fn a_unit_goes_in_before_the_first_slice_and_moves_nothing() {
        let packet = annexb_h264();
        let woven = insert(Framing::AnnexB(Codec::H264), &packet, &unit()).expect("woven");
        assert!(woven.len() > packet.len());
        // The parameter sets are still first, the slice still last, and
        // the bytes of both are the bytes that arrived.
        assert!(woven.starts_with(&packet[..packet.len() - 9]));
        assert!(woven.ends_with(&packet[packet.len() - 9..]));
        let read = avc::units_annexb(&woven, Codec::H264);
        assert_eq!(read.len(), 1, "one unit, and it is ours");
        assert_eq!(read[0], unit());
    }

    #[test]
    fn a_length_prefixed_sample_keeps_its_framing() {
        let sample = avc::annexb_to_length_prefixed(&annexb_h264(), 4).expect("a sample");
        let framing = Framing::LengthPrefixed {
            codec: Codec::H264,
            length_size: 4,
        };
        let woven = insert(framing, &sample, &unit()).expect("woven");
        let nals = avc::split_length_prefixed(&woven, 4).expect("the NALs");
        assert_eq!(nals.len(), 4, "three NALs and the SEI");
        assert_eq!(nals[2][0] & 0x1f, 6, "the SEI is before the slice");
        let read = avc::units_length_prefixed(&woven, 4, Codec::H264).expect("units");
        assert_eq!(read, vec![unit()]);
    }

    #[test]
    fn an_av1_temporal_unit_takes_a_metadata_obu() {
        // A temporal delimiter, a sequence header and a frame.
        let packet = vec![0x12u8, 0x00, 0x0a, 0x01, 0x00, 0x32, 0x02, 0x00, 0x00];
        let woven = insert(Framing::Av1, &packet, &unit()).expect("woven");
        assert!(woven.len() > packet.len());
        assert_eq!(obu::units_obu(&woven), vec![unit()]);
    }
}
