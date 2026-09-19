//! The layered 8-bit encoding of section 5, and the arithmetic a
//! searcher wants over it.
//!
//! A component is a sign and a seven-bit magnitude against one scale
//! for the vector, and the bits go out as planes: the signs first, then
//! the magnitudes' most significant bit, and so on. A reader with the
//! sign plane alone has a binary embedding it can rank by Hamming
//! distance; each further plane halves the error. That is why the
//! planes live in their own type rather than as a `Vec<u8>`: merging
//! what arrived on different frames, and reconstructing from whatever
//! prefix is in hand, are the two operations that matter.

use crate::{Error, Result, MAX_DIMS};

/// How many planes a full 8-bit vector has: one sign, seven magnitude.
pub const PLANE_COUNT: u8 = 8;

/// The largest magnitude a component quantizes to.
pub const MAX_MAGNITUDE: u8 = 127;

/// The bit-planes of one I8 record, whole or in part.
///
/// `dims` and the scale come from the message the planes were read
/// from; a reader merges two of these when two messages carry different
/// planes of the same record.
#[derive(Clone, Debug, PartialEq)]
pub struct Planes {
    dims: usize,
    scale_bits: u16,
    present: u8,
    /// Eight slots, `None` where the plane has not arrived.
    data: [Option<Vec<u8>>; 8],
}

impl Planes {
    /// An empty set of planes for a vector of `dims` components at
    /// `scale`, which a reader fills by merging.
    pub fn empty(dims: usize, scale_bits: u16) -> Result<Self> {
        check_dims(dims)?;
        Ok(Self {
            dims,
            scale_bits,
            present: 0,
            data: Default::default(),
        })
    }

    /// All eight planes of `vector`, quantized.
    ///
    /// The scale is the largest absolute component rounded up to the
    /// next binary16 value, so no magnitude can exceed the scale and
    /// the clamp below is only ever reached by a vector whose largest
    /// component is past binary16's own range.
    pub fn quantize(vector: &[f32]) -> Result<Self> {
        check_dims(vector.len())?;
        let mut largest = 0f32;
        for value in vector {
            if !value.is_finite() {
                return Err(Error::NotFinite);
            }
            let magnitude = value.abs();
            if magnitude > largest {
                largest = magnitude;
            }
        }
        let scale_bits = f32_to_f16_up(largest);
        let scale = f16_to_f32(scale_bits);

        let stride = plane_bytes(vector.len());
        let mut data: [Option<Vec<u8>>; 8] = Default::default();
        for plane in data.iter_mut() {
            *plane = Some(vec![0u8; stride]);
        }
        for (index, value) in vector.iter().enumerate() {
            let byte = index / 8;
            let bit = 7 - (index % 8);
            if *value < 0.0 {
                if let Some(plane) = data[0].as_mut() {
                    plane[byte] |= 1 << bit;
                }
            }
            let magnitude = if scale > 0.0 {
                let scaled = (value.abs() / scale * f32::from(MAX_MAGNITUDE)).round();
                scaled.clamp(0.0, f32::from(MAX_MAGNITUDE)) as u8
            } else {
                // An all-zero vector has no scale to divide by, and
                // every component is zero anyway.
                0
            };
            for k in 1..PLANE_COUNT {
                if magnitude >> (7 - k) & 1 == 1 {
                    if let Some(plane) = data[usize::from(k)].as_mut() {
                        plane[byte] |= 1 << bit;
                    }
                }
            }
        }
        Ok(Self {
            dims: vector.len(),
            scale_bits,
            present: 0xff,
            data,
        })
    }

    /// Components per vector.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// The scale, as the binary16 it travels as.
    pub fn scale_bits(&self) -> u16 {
        self.scale_bits
    }

    /// The scale.
    pub fn scale(&self) -> f32 {
        f16_to_f32(self.scale_bits)
    }

    /// The planes in hand, one bit each, plane 0 the low bit.
    pub fn present(&self) -> u8 {
        self.present
    }

    /// Plane `k`'s packed bits, if it is in hand.
    pub fn plane(&self, k: u8) -> Option<&[u8]> {
        self.data
            .get(usize::from(k))?
            .as_ref()
            .map(|plane| plane.as_slice())
    }

    /// The packed sign bits: a binary embedding, ready for
    /// [`hamming`], or `None` when plane 0 has not arrived.
    pub fn signs(&self) -> Option<&[u8]> {
        self.plane(0)
    }

    /// The highest `K` for which planes 0 to `K` are all in hand.
    ///
    /// The midpoint rule of section 5 needs a prefix, so a record
    /// holding planes 0, 1 and 3 is read with 0 and 1: plane 3's bits
    /// say nothing without plane 2's above them.
    pub fn prefix(&self) -> Option<u8> {
        if self.present & 1 == 0 {
            return None;
        }
        let mut k = 0u8;
        while k + 1 < PLANE_COUNT && self.present >> (k + 1) & 1 == 1 {
            k += 1;
        }
        Some(k)
    }

    /// Only the planes named by `mask`, for a writer splitting a record
    /// across carriers.
    pub fn subset(&self, mask: u8) -> Self {
        let mut data: [Option<Vec<u8>>; 8] = Default::default();
        for k in 0..PLANE_COUNT {
            if mask >> k & 1 == 1 {
                data[usize::from(k)] = self.data[usize::from(k)].clone();
            }
        }
        Self {
            dims: self.dims,
            scale_bits: self.scale_bits,
            present: self.present & mask,
            data,
        }
    }

    /// One [`Planes`] per plane in hand, in ascending order: the bodies
    /// a writer with a budget sends over several carriers.
    pub fn split(&self) -> Vec<Self> {
        (0..PLANE_COUNT)
            .filter(|k| self.present >> k & 1 == 1)
            .map(|k| self.subset(1 << k))
            .collect()
    }

    /// Takes `other`'s planes into this one.
    ///
    /// Two messages of the same record must agree about the vector they
    /// describe; disagreeing dims, scale or bits are corruption, not a
    /// later revision, and the merge refuses rather than mixing them.
    pub fn merge(&mut self, other: &Self) -> Result<()> {
        if self.dims != other.dims || self.scale_bits != other.scale_bits {
            return Err(Error::Mismatch);
        }
        for k in 0..PLANE_COUNT {
            let slot = usize::from(k);
            let Some(incoming) = other.data[slot].as_ref() else {
                continue;
            };
            match self.data[slot].as_ref() {
                Some(held) if held != incoming => return Err(Error::Mismatch),
                Some(_) => {}
                None => {
                    self.data[slot] = Some(incoming.clone());
                    self.present |= 1 << k;
                }
            }
        }
        Ok(())
    }

    /// Each component's seven-bit magnitude, the unknown low bits
    /// filled in with their midpoint.
    pub fn magnitudes(&self) -> Result<Vec<u8>> {
        let prefix = self.prefix().ok_or(Error::Planes)?;
        let mut out = vec![0u8; self.dims];
        for k in 1..=prefix {
            let plane = self.plane(k).ok_or(Error::Planes)?;
            for (index, magnitude) in out.iter_mut().enumerate() {
                if bit_at(plane, index) {
                    *magnitude |= 1 << (7 - k);
                }
            }
        }
        if prefix < 7 {
            let midpoint = 1u8 << (6 - prefix);
            for magnitude in out.iter_mut() {
                *magnitude |= midpoint;
            }
        }
        Ok(out)
    }

    /// The vector, from whatever prefix of planes is in hand.
    pub fn reconstruct(&self) -> Result<Vec<f32>> {
        let signs = self.signs().ok_or(Error::Planes)?;
        let magnitudes = self.magnitudes()?;
        let scale = self.scale();
        Ok(magnitudes
            .iter()
            .enumerate()
            .map(|(index, magnitude)| {
                let value = f32::from(*magnitude) / f32::from(MAX_MAGNITUDE) * scale;
                if bit_at(signs, index) {
                    -value
                } else {
                    value
                }
            })
            .collect())
    }

    /// The dot product of the reconstructed vector with a query.
    ///
    /// The searcher's inner loop: it reconstructs into the product
    /// rather than into a vector, so ranking a stream of records
    /// allocates nothing per record.
    pub fn dot(&self, query: &[f32]) -> Result<f32> {
        if query.len() != self.dims {
            return Err(Error::Mismatch);
        }
        let signs = self.signs().ok_or(Error::Planes)?;
        let magnitudes = self.magnitudes()?;
        let scale = self.scale() / f32::from(MAX_MAGNITUDE);
        let mut total = 0f32;
        for (index, component) in query.iter().enumerate() {
            let value = f32::from(magnitudes[index]) * scale;
            total += if bit_at(signs, index) {
                -value * component
            } else {
                value * component
            };
        }
        Ok(total)
    }

    /// The body bytes of section 5: scale, plane set, plane data.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(3 + plane_bytes(self.dims) * usize::from(self.count()));
        out.extend_from_slice(&self.scale_bits.to_le_bytes());
        out.push(self.present);
        for k in 0..PLANE_COUNT {
            if let Some(plane) = self.data[usize::from(k)].as_ref() {
                out.extend_from_slice(plane);
            }
        }
        out
    }

    /// The planes of an I8 body, for a space of `dims` components.
    ///
    /// Bytes past the last plane are left alone: section 3 reserves the
    /// tail of a SPACE message for later versions and nothing here
    /// needs a body to end exactly, so a longer body is read, not
    /// refused.
    pub fn decode(dims: usize, bytes: &[u8]) -> Result<Self> {
        check_dims(dims)?;
        let mut reader = crate::wire::Reader::new(bytes);
        let scale_bits = reader.u16()?;
        let present = reader.u8()?;
        let stride = plane_bytes(dims);
        let mut data: [Option<Vec<u8>>; 8] = Default::default();
        for k in 0..PLANE_COUNT {
            if present >> k & 1 == 1 {
                data[usize::from(k)] = Some(reader.take(stride)?.to_vec());
            }
        }
        Ok(Self {
            dims,
            scale_bits,
            present,
            data,
        })
    }

    /// How many bytes [`Planes::encode`] will write.
    pub fn encoded_len(&self) -> usize {
        3 + plane_bytes(self.dims) * usize::from(self.count())
    }

    /// How many planes are in hand.
    pub fn count(&self) -> u8 {
        self.present.count_ones() as u8
    }
}

/// Bytes one plane of `dims` components takes.
pub fn plane_bytes(dims: usize) -> usize {
    dims.div_ceil(8)
}

/// Component `index`'s bit within a packed plane.
fn bit_at(plane: &[u8], index: usize) -> bool {
    match plane.get(index / 8) {
        Some(byte) => byte >> (7 - (index % 8)) & 1 == 1,
        None => false,
    }
}

/// The sign bits of a vector, packed as plane 0: set where the
/// component is negative.
pub fn pack_signs(vector: &[f32]) -> Vec<u8> {
    let mut out = vec![0u8; plane_bytes(vector.len())];
    for (index, value) in vector.iter().enumerate() {
        if *value < 0.0 {
            out[index / 8] |= 1 << (7 - (index % 8));
        }
    }
    out
}

/// A packed sign plane back as plus and minus one, `dims` long.
pub fn unpack_signs(plane: &[u8], dims: usize) -> Vec<i8> {
    (0..dims)
        .map(|index| if bit_at(plane, index) { -1 } else { 1 })
        .collect()
}

/// The Hamming distance between two packed sign planes.
///
/// The planes must be the same length: the padding bits of the last
/// byte are zero in both, so they never add to the count.
pub fn hamming(a: &[u8], b: &[u8]) -> Result<u32> {
    if a.len() != b.len() {
        return Err(Error::Mismatch);
    }
    Ok(a.iter()
        .zip(b)
        .map(|(x, y)| (x ^ y).count_ones())
        .sum::<u32>())
}

/// Refuses a dimensionality section 9 says to refuse.
pub fn check_dims(dims: usize) -> Result<()> {
    if dims == 0 || dims > MAX_DIMS as usize {
        return Err(Error::Dims(dims.min(u32::MAX as usize) as u32));
    }
    Ok(())
}

/// A binary16 as a binary32. Exact: every binary16 is a binary32.
pub fn f16_to_f32(bits: u16) -> f32 {
    let negative = bits & 0x8000 != 0;
    let exponent = u32::from(bits >> 10 & 0x1f);
    let mantissa = u32::from(bits & 0x03ff);
    let value = match exponent {
        // Zero and the subnormals, which are mantissa times 2^-24.
        0 => mantissa as f32 * f32::from_bits(0x3380_0000),
        // Infinity and the NaNs.
        31 => {
            if mantissa == 0 {
                f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => f32::from_bits((exponent + 112) << 23 | mantissa << 13),
    };
    if negative {
        -value
    } else {
        value
    }
}

/// A binary32 as the nearest binary16, ties to even.
pub fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x007f_ffff;
    if exponent == 0xff {
        return if mantissa != 0 {
            sign | 0x7e00
        } else {
            sign | 0x7c00
        };
    }
    let unbiased = exponent - 127;
    if unbiased > 15 {
        return sign | 0x7c00;
    }
    if unbiased >= -14 {
        let mut rounded = round_shift(mantissa, 13) as u16;
        let mut field = (unbiased + 15) as u16;
        // Rounding the mantissa up out of its ten bits carries into the
        // exponent, which can carry all the way to infinity.
        if rounded & 0x400 != 0 {
            rounded = 0;
            field += 1;
            if field >= 0x1f {
                return sign | 0x7c00;
            }
        }
        return sign | field << 10 | rounded;
    }
    // Below 2^-14 binary16 has only subnormals, which are an integer
    // count of 2^-24; below half of that there is nothing to round to.
    if unbiased < -25 {
        return sign;
    }
    let full = mantissa | 0x0080_0000;
    sign | round_shift(full, (-unbiased - 1) as u32) as u16
}

/// A non-negative binary32 as the nearest binary16 at or above it.
///
/// This is what section 5's "rounded up" has to mean for the scale: a
/// scale below the largest component would quantize that component past
/// 127. A vector whose largest component is past binary16's own range
/// gets binary16's largest value and the clamp in `quantize` takes the
/// rest.
pub fn f32_to_f16_up(value: f32) -> u16 {
    if !value.is_finite() {
        return 0x7bff;
    }
    if value <= 0.0 {
        return 0;
    }
    let nearest = f32_to_f16(value);
    if nearest >= 0x7c00 {
        return 0x7bff;
    }
    if f16_to_f32(nearest) >= value {
        return nearest;
    }
    let next = nearest + 1;
    // 0x7c00 is infinity; the format has no room for it in a scale.
    if next >= 0x7c00 {
        0x7bff
    } else {
        next
    }
}

/// `value >> shift`, rounded to nearest with ties to even.
fn round_shift(value: u32, shift: u32) -> u32 {
    let truncated = value >> shift;
    let half = 1u32 << (shift - 1);
    let remainder = value & ((1u32 << shift) - 1);
    if remainder > half || (remainder == half && truncated & 1 == 1) {
        truncated + 1
    } else {
        truncated
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(dims: usize) -> Vec<f32> {
        (0..dims)
            .map(|i| {
                let x = (i as f32 * 0.37).sin() * 2.5;
                if i % 3 == 0 {
                    -x
                } else {
                    x
                }
            })
            .collect()
    }

    #[test]
    fn f16_round_trips_the_values_it_can_hold() {
        for bits in 0u16..=u16::MAX {
            let exponent = bits >> 10 & 0x1f;
            let mantissa = bits & 0x3ff;
            if exponent == 31 {
                continue; // infinities and NaNs are not scales
            }
            let value = f16_to_f32(bits);
            let back = f32_to_f16(value);
            // Negative zero comes back as the bits it went in as.
            assert_eq!(back, bits, "binary16 {bits:#06x} ({mantissa})");
        }
    }

    #[test]
    fn rounding_up_never_lands_below_the_value() {
        let mut value = 1e-7f32;
        while value < 70000.0 {
            let up = f32_to_f16_up(value);
            assert!(
                f16_to_f32(up) >= value,
                "{value} rounded up to {}",
                f16_to_f32(up)
            );
            value *= 1.37;
        }
        assert_eq!(f32_to_f16_up(0.0), 0);
        assert_eq!(f16_to_f32(f32_to_f16_up(70000.0)), 65504.0);
    }

    #[test]
    fn eight_planes_reproduce_the_int8_value_exactly() {
        let vector = ramp(37);
        let planes = Planes::quantize(&vector).expect("quantized");
        let scale = planes.scale();
        let magnitudes = planes.magnitudes().expect("magnitudes");
        for (index, value) in vector.iter().enumerate() {
            let want = (value.abs() / scale * 127.0).round().clamp(0.0, 127.0) as u8;
            assert_eq!(magnitudes[index], want, "component {index}");
        }
    }

    /// A deterministic pseudo-random vector, so the error numbers below
    /// are the same on every machine and every run.
    fn noise(seed: &mut u64, dims: usize) -> Vec<f32> {
        (0..dims)
            .map(|_| {
                *seed = seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let unit = ((*seed >> 33) as f64 / (1u64 << 31) as f64) as f32;
                (unit - 0.5) * 3.0
            })
            .collect()
    }

    #[test]
    fn each_plane_shrinks_the_error() {
        // Per component the midpoint rule can be unlucky: a component
        // the midpoint happened to hit exactly gets worse when the next
        // plane moves it. Averaged over vectors it halves, which is the
        // claim section 5 makes, so that is what is asserted.
        let mut seed = 0x5eed_1234u64;
        let vectors: Vec<Vec<f32>> = (0..200).map(|_| noise(&mut seed, 64)).collect();
        let mut previous = f32::INFINITY;
        for prefix in 0..PLANE_COUNT {
            let mask = ((1u16 << (prefix + 1)) - 1) as u8;
            let mut total = 0f32;
            for vector in &vectors {
                let full = Planes::quantize(vector).expect("quantized");
                let read = full.subset(mask).reconstruct().expect("a reconstruction");
                let worst = vector
                    .iter()
                    .zip(&read)
                    .map(|(a, b)| (a - b).abs() / full.scale())
                    .fold(0f32, f32::max);
                // No component may ever be further off than the
                // midpoint rule's own bound.
                // The midpoint's own half-step, plus the half step the
                // float lost when it became an integer magnitude.
                let bound = if prefix < 7 {
                    (f32::from(1u8 << (6 - prefix)) + 0.5) / 127.0
                } else {
                    0.5 / 127.0
                };
                assert!(worst <= bound + 1e-6, "prefix {prefix}: {worst} > {bound}");
                total += worst;
            }
            let mean = total / vectors.len() as f32;
            assert!(
                mean < previous * 0.7,
                "prefix {prefix} averaged {mean}, no better than {previous}"
            );
            previous = mean;
        }
    }

    #[test]
    fn eight_planes_are_the_int8_vector_itself() {
        let mut seed = 0xfeed_9876u64;
        for _ in 0..50 {
            let vector = noise(&mut seed, 31);
            let planes = Planes::quantize(&vector).expect("quantized");
            let read = planes.reconstruct().expect("a reconstruction");
            let magnitudes = planes.magnitudes().expect("magnitudes");
            for (index, value) in vector.iter().enumerate() {
                let sign = if *value < 0.0 { -1.0 } else { 1.0 };
                let want = sign * f32::from(magnitudes[index]) / 127.0 * planes.scale();
                assert_eq!(read[index], want, "component {index}");
            }
        }
    }

    #[test]
    fn the_sign_plane_alone_ranks_by_hamming_distance() {
        let a = ramp(96);
        let b: Vec<f32> = a.iter().map(|x| x + 0.01).collect();
        let far: Vec<f32> = a.iter().map(|x| -x).collect();
        let pa = Planes::quantize(&a).expect("quantized");
        let pb = Planes::quantize(&b).expect("quantized");
        let pfar = Planes::quantize(&far).expect("quantized");
        let near = hamming(pa.signs().expect("signs"), pb.signs().expect("signs")).expect("near");
        let away = hamming(pa.signs().expect("signs"), pfar.signs().expect("signs")).expect("far");
        assert!(near < away, "{near} is not nearer than {away}");
        // The negated vector differs in every component that is not zero.
        let nonzero = a.iter().filter(|x| **x != 0.0).count() as u32;
        assert_eq!(away, nonzero);
    }

    #[test]
    fn sign_bits_agree_with_the_components() {
        let vector = ramp(19);
        let planes = Planes::quantize(&vector).expect("quantized");
        let packed = pack_signs(&vector);
        assert_eq!(planes.signs().expect("signs"), &packed[..]);
        for (index, sign) in unpack_signs(&packed, vector.len()).iter().enumerate() {
            let want = if vector[index] < 0.0 { -1 } else { 1 };
            assert_eq!(*sign, want, "component {index}");
        }
    }

    #[test]
    fn a_dot_product_agrees_with_reconstructing_first() {
        let vector = ramp(48);
        let query: Vec<f32> = (0..48).map(|i| (i as f32 * 0.11).cos()).collect();
        let planes = Planes::quantize(&vector).expect("quantized");
        for mask in [0x01u8, 0x03, 0x0f, 0xff] {
            let part = planes.subset(mask);
            let read = part.reconstruct().expect("a reconstruction");
            let want: f32 = read.iter().zip(&query).map(|(a, b)| a * b).sum();
            let got = part.dot(&query).expect("a dot product");
            assert!((want - got).abs() < 1e-3, "mask {mask:#04x}: {want} {got}");
        }
        assert_eq!(planes.dot(&query[..4]), Err(Error::Mismatch));
    }

    #[test]
    fn planes_merge_and_refuse_to_mix_records() {
        let vector = ramp(24);
        let planes = Planes::quantize(&vector).expect("quantized");
        let pieces = planes.split();
        assert_eq!(pieces.len(), 8, "one piece per plane");
        let mut merged = Planes::empty(24, planes.scale_bits()).expect("an empty record");
        for piece in pieces.iter().rev() {
            merged.merge(piece).expect("a merge");
        }
        assert_eq!(merged, planes, "the pieces put back together");

        let other = Planes::quantize(&ramp(24).iter().map(|x| x * 0.5).collect::<Vec<_>>())
            .expect("quantized");
        assert_eq!(merged.merge(&other), Err(Error::Mismatch), "another scale");
        let shorter = Planes::quantize(&ramp(16)).expect("quantized");
        assert_eq!(merged.merge(&shorter), Err(Error::Mismatch), "other dims");
    }

    #[test]
    fn a_prefix_stops_at_the_first_missing_plane() {
        let planes = Planes::quantize(&ramp(8)).expect("quantized");
        assert_eq!(planes.prefix(), Some(7));
        assert_eq!(planes.subset(0b0000_1011).prefix(), Some(1));
        assert_eq!(planes.subset(0b0000_0001).prefix(), Some(0));
        let no_signs = planes.subset(0b1111_1110);
        assert_eq!(no_signs.prefix(), None);
        assert_eq!(no_signs.reconstruct(), Err(Error::Planes));
    }

    #[test]
    fn a_body_round_trips_and_a_short_one_is_refused() {
        let planes = Planes::quantize(&ramp(33)).expect("quantized");
        let body = planes.encode();
        assert_eq!(body.len(), planes.encoded_len());
        assert_eq!(Planes::decode(33, &body).expect("decoded"), planes);
        for cut in 0..body.len() {
            assert!(
                Planes::decode(33, &body[..cut]).is_err(),
                "a body cut at {cut} was accepted"
            );
        }
    }

    #[test]
    fn the_zero_vector_quantizes_and_comes_back_zero() {
        let planes = Planes::quantize(&[0.0; 12]).expect("quantized");
        assert_eq!(planes.scale(), 0.0);
        assert_eq!(planes.reconstruct().expect("zeros"), vec![0.0; 12]);
        assert_eq!(planes.dot(&[1.0; 12]).expect("a dot product"), 0.0);
    }

    #[test]
    fn dims_outside_the_range_are_refused() {
        assert_eq!(Planes::quantize(&[]), Err(Error::Dims(0)));
        assert!(matches!(
            Planes::empty(MAX_DIMS as usize + 1, 0),
            Err(Error::Dims(_))
        ));
        assert_eq!(Planes::quantize(&[f32::NAN, 1.0]), Err(Error::NotFinite));
        assert_eq!(
            Planes::quantize(&[f32::INFINITY, 1.0]),
            Err(Error::NotFinite)
        );
    }

    #[test]
    fn a_component_past_binary16s_range_clamps_instead_of_overflowing() {
        let planes = Planes::quantize(&[1e30, 1.0]).expect("quantized");
        assert_eq!(planes.scale(), 65504.0);
        let read = planes.reconstruct().expect("a reconstruction");
        assert_eq!(
            read[0], 65504.0,
            "the largest component clamps to the scale"
        );
    }

    #[test]
    fn hamming_refuses_planes_of_different_lengths() {
        assert_eq!(hamming(&[0, 0], &[0]), Err(Error::Mismatch));
        assert_eq!(hamming(&[0b1010_0000], &[0b0110_0000]), Ok(2));
    }
}
