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
//!
//! A few components may go exactly instead, as escapes. One scale for
//! the whole vector is set by its largest component, and a model with
//! one component that dwarfs the rest spends most of its range on that
//! component alone; sending it as an index and a binary16 value takes
//! it out of the scale and gives the rest the range they occupy.

use crate::{Error, Result, MAX_DIMS, MAX_ESCAPES};

/// How many planes a full 8-bit vector has: one sign, seven magnitude.
pub const PLANE_COUNT: u8 = 8;

/// The largest magnitude a component quantizes to.
pub const MAX_MAGNITUDE: u8 = 127;

/// The bit-planes of one I8 record, whole or in part, and its escapes.
///
/// `dims` and the scale come from the message the planes were read
/// from; a reader merges two of these when two messages carry different
/// planes of the same record.
#[derive(Clone, Debug, PartialEq)]
pub struct Planes {
    dims: usize,
    scale_bits: u16,
    present: u8,
    /// The escaped components: each an index and a binary16 value, in
    /// the order they were read. A writer sends them ascending with no
    /// index twice, and only in the message that carries plane 0.
    escapes: Vec<(u32, u16)>,
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
            escapes: Vec::new(),
            data: Default::default(),
        })
    }

    /// All eight planes of `vector`, quantized, with its `escapes`
    /// largest components sent exactly instead.
    ///
    /// The scale is the largest absolute component that is not an
    /// escape, rounded up to the next binary16 value, so no magnitude
    /// can exceed the scale and the clamp below is only ever reached by
    /// a vector whose largest component is past binary16's own range.
    /// That is the whole point of an escape: one component that dwarfs
    /// the rest sets the scale for all of them, and taking it out of
    /// the scale gives the rest a range they actually occupy.
    ///
    /// Ties go to the earlier component, so two vectors that differ
    /// only in order do not differ in which components escape by luck.
    pub fn quantize(vector: &[f32], escapes: usize) -> Result<Self> {
        check_dims(vector.len())?;
        if escapes > MAX_ESCAPES as usize {
            return Err(Error::Escapes);
        }
        for value in vector {
            if !value.is_finite() {
                return Err(Error::NotFinite);
            }
        }

        let mut escaped = vec![false; vector.len()];
        let mut list: Vec<(u32, u16)> = Vec::new();
        let wanted = escapes.min(vector.len());
        if wanted > 0 {
            let mut order: Vec<usize> = (0..vector.len()).collect();
            order.sort_by(|a, b| {
                vector[*b]
                    .abs()
                    .partial_cmp(&vector[*a].abs())
                    .unwrap_or(core::cmp::Ordering::Equal)
                    .then(a.cmp(b))
            });
            order.truncate(wanted);
            order.sort_unstable();
            for index in order {
                escaped[index] = true;
                list.push((index as u32, f32_to_f16(vector[index])));
            }
        }

        let mut largest = 0f32;
        for (index, value) in vector.iter().enumerate() {
            if escaped[index] {
                continue;
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
            // An escaped component keeps its sign bit, so plane 0 is
            // the same with or without escapes.
            if *value < 0.0 {
                if let Some(plane) = data[0].as_mut() {
                    plane[byte] |= 1 << bit;
                }
            }
            let magnitude = if escaped[index] || scale <= 0.0 {
                // An escaped component is zero in the planes, and an
                // all-zero vector has no scale to divide by.
                0
            } else {
                let scaled = (value.abs() / scale * f32::from(MAX_MAGNITUDE)).round();
                scaled.clamp(0.0, f32::from(MAX_MAGNITUDE)) as u8
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
            escapes: list,
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

    /// The escaped components: an index and a binary16 value each.
    pub fn escapes(&self) -> &[(u32, u16)] {
        &self.escapes
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
    ///
    /// Escapes do not touch it. An escaped component is zero in every
    /// magnitude plane but keeps its sign, so a coarse search by
    /// Hamming distance reads the same bits either way.
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
    ///
    /// The escapes go with plane 0 and nowhere else, which is where
    /// section 5 tells a writer to put them: they are needed to read
    /// anything at all, and repeating them in every message would pay
    /// for them again on every carrier.
    pub fn subset(&self, mask: u8) -> Self {
        let mut data: [Option<Vec<u8>>; 8] = Default::default();
        for k in 0..PLANE_COUNT {
            if mask >> k & 1 == 1 {
                data[usize::from(k)] = self.data[usize::from(k)].clone();
            }
        }
        let escapes = if mask & 1 == 1 {
            self.escapes.clone()
        } else {
            Vec::new()
        };
        Self {
            dims: self.dims,
            scale_bits: self.scale_bits,
            present: self.present & mask,
            escapes,
            data,
        }
    }

    /// One [`Planes`] per plane in hand, in ascending order: the bodies
    /// a writer with a budget sends over several carriers. Plane 0's
    /// body carries the escapes.
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
    ///
    /// The escapes are not refused that way. They ride with plane 0, so
    /// a reader that already has a list has the one the writer meant,
    /// and a second non-empty list is either the same or damaged: the
    /// first list in hand wins and the merge goes on, because a record
    /// with planes to add is worth more than a refusal.
    pub fn merge(&mut self, other: &Self) -> Result<()> {
        if self.dims != other.dims || self.scale_bits != other.scale_bits {
            return Err(Error::Mismatch);
        }
        if self.escapes.is_empty() {
            self.escapes = other.escapes.clone();
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
    ///
    /// This is the planes' own reading. An escaped component is zero in
    /// the planes and so reads as the midpoint here; it is
    /// [`Planes::reconstruct`] that puts the escape's own value back.
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

    /// The vector, from whatever prefix of planes is in hand, with the
    /// escaped components as the escapes give them.
    ///
    /// An escape wins over the planes whatever `K` is. Two escapes of
    /// the same index are a writer's mistake, not a reader's: the list
    /// is applied in order, so the last of them stands.
    pub fn reconstruct(&self) -> Result<Vec<f32>> {
        let signs = self.signs().ok_or(Error::Planes)?;
        let magnitudes = self.magnitudes()?;
        let scale = self.scale();
        let mut out: Vec<f32> = magnitudes
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
            .collect();
        for (index, value) in &self.escapes {
            if let Some(slot) = out.get_mut(*index as usize) {
                *slot = f16_to_f32(*value);
            }
        }
        Ok(out)
    }

    /// The dot product of the reconstructed vector with a query.
    ///
    /// A record's scale is its own, so two records of few planes are
    /// not comparable by this number: rank them by [`Planes::cosine`],
    /// or by [`hamming`] over the sign planes, as section 5 says. This
    /// is the right thing for one record, or across records that a
    /// space declares unit length and a reader has normalized.
    pub fn dot(&self, query: &[f32]) -> Result<f32> {
        if query.len() != self.dims {
            return Err(Error::Mismatch);
        }
        let values = self.reconstruct()?;
        Ok(values
            .iter()
            .zip(query)
            .map(|(value, component)| value * component)
            .sum())
    }

    /// The cosine of the angle between the reconstructed vector and a
    /// query: what ranks records of few planes against each other.
    ///
    /// Zero when either side has no length, which is the only answer
    /// that says nothing about a vector that points nowhere.
    pub fn cosine(&self, query: &[f32]) -> Result<f32> {
        if query.len() != self.dims {
            return Err(Error::Mismatch);
        }
        let values = self.reconstruct()?;
        let mut dot = 0f32;
        let mut ours = 0f32;
        let mut theirs = 0f32;
        for (value, component) in values.iter().zip(query) {
            dot += value * component;
            ours += value * value;
            theirs += component * component;
        }
        let length = ours.sqrt() * theirs.sqrt();
        if length <= 0.0 {
            return Ok(0.0);
        }
        Ok(dot / length)
    }

    /// The body bytes of section 5: scale, plane set, escapes, planes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&self.scale_bits.to_le_bytes());
        out.push(self.present);
        out.push(self.escapes.len() as u8);
        for (index, value) in &self.escapes {
            crate::wire::put_varint(&mut out, *index);
            out.extend_from_slice(&value.to_le_bytes());
        }
        for k in 0..PLANE_COUNT {
            if let Some(plane) = self.data[usize::from(k)].as_ref() {
                out.extend_from_slice(plane);
            }
        }
        out
    }

    /// The planes of an I8 body, for a space of `dims` components.
    ///
    /// Section 9: more than [`MAX_ESCAPES`] escapes, or an index at or
    /// above `dims`, costs the message. The order of the list and
    /// repeated indices do not: ascending and distinct is a writer's
    /// duty, and a reader that refused a list for being out of order
    /// would throw away a record it can read.
    ///
    /// Bytes past the last plane are left alone: nothing here needs a
    /// body to end exactly, so a longer body is read, not refused.
    pub fn decode(dims: usize, bytes: &[u8]) -> Result<Self> {
        check_dims(dims)?;
        let mut reader = crate::wire::Reader::new(bytes);
        let scale_bits = reader.u16()?;
        let present = reader.u8()?;
        let count = reader.u8()?;
        if count > MAX_ESCAPES {
            return Err(Error::Escapes);
        }
        let mut escapes = Vec::with_capacity(usize::from(count));
        for _ in 0..count {
            let index = reader.varint()?;
            if index as usize >= dims {
                return Err(Error::Escapes);
            }
            escapes.push((index, reader.u16()?));
        }
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
            escapes,
            data,
        })
    }

    /// How many bytes [`Planes::encode`] will write.
    pub fn encoded_len(&self) -> usize {
        let escapes: usize = self
            .escapes
            .iter()
            .map(|(index, _)| crate::wire::varint_len(*index) + 2)
            .sum();
        4 + escapes + plane_bytes(self.dims) * usize::from(self.count())
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
        let planes = Planes::quantize(&vector, 0).expect("quantized");
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
                let full = Planes::quantize(vector, 0).expect("quantized");
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
            let planes = Planes::quantize(&vector, 0).expect("quantized");
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
        let pa = Planes::quantize(&a, 0).expect("quantized");
        let pb = Planes::quantize(&b, 0).expect("quantized");
        let pfar = Planes::quantize(&far, 0).expect("quantized");
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
        let planes = Planes::quantize(&vector, 0).expect("quantized");
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
        let planes = Planes::quantize(&vector, 0).expect("quantized");
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
        let planes = Planes::quantize(&vector, 0).expect("quantized");
        let pieces = planes.split();
        assert_eq!(pieces.len(), 8, "one piece per plane");
        let mut merged = Planes::empty(24, planes.scale_bits()).expect("an empty record");
        for piece in pieces.iter().rev() {
            merged.merge(piece).expect("a merge");
        }
        assert_eq!(merged, planes, "the pieces put back together");

        let halved: Vec<f32> = ramp(24).iter().map(|x| x * 0.5).collect();
        let other = Planes::quantize(&halved, 0).expect("quantized");
        assert_eq!(merged.merge(&other), Err(Error::Mismatch), "another scale");
        let shorter = Planes::quantize(&ramp(16), 0).expect("quantized");
        assert_eq!(merged.merge(&shorter), Err(Error::Mismatch), "other dims");
    }

    #[test]
    fn a_prefix_stops_at_the_first_missing_plane() {
        let planes = Planes::quantize(&ramp(8), 0).expect("quantized");
        assert_eq!(planes.prefix(), Some(7));
        assert_eq!(planes.subset(0b0000_1011).prefix(), Some(1));
        assert_eq!(planes.subset(0b0000_0001).prefix(), Some(0));
        let no_signs = planes.subset(0b1111_1110);
        assert_eq!(no_signs.prefix(), None);
        assert_eq!(no_signs.reconstruct(), Err(Error::Planes));
    }

    #[test]
    fn a_body_round_trips_and_a_short_one_is_refused() {
        let planes = Planes::quantize(&ramp(33), 0).expect("quantized");
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
        let planes = Planes::quantize(&[0.0; 12], 0).expect("quantized");
        assert_eq!(planes.scale(), 0.0);
        assert_eq!(planes.reconstruct().expect("zeros"), vec![0.0; 12]);
        assert_eq!(planes.dot(&[1.0; 12]).expect("a dot product"), 0.0);
    }

    #[test]
    fn dims_outside_the_range_are_refused() {
        assert_eq!(Planes::quantize(&[], 0), Err(Error::Dims(0)));
        assert!(matches!(
            Planes::empty(MAX_DIMS as usize + 1, 0),
            Err(Error::Dims(_))
        ));
        assert_eq!(Planes::quantize(&[f32::NAN, 1.0], 0), Err(Error::NotFinite));
        assert_eq!(
            Planes::quantize(&[f32::INFINITY, 1.0], 0),
            Err(Error::NotFinite)
        );
    }

    #[test]
    fn a_component_past_binary16s_range_clamps_instead_of_overflowing() {
        let planes = Planes::quantize(&[1e30, 1.0], 0).expect("quantized");
        assert_eq!(planes.scale(), 65504.0);
        let read = planes.reconstruct().expect("a reconstruction");
        assert_eq!(
            read[0], 65504.0,
            "the largest component clamps to the scale"
        );
    }

    /// A vector with one component that dwarfs the rest, which is the
    /// shape escapes exist for.
    fn dominant(dims: usize) -> Vec<f32> {
        let mut out: Vec<f32> = (0..dims)
            .map(|i| ((i as f32 * 0.77).sin()) * 0.25)
            .collect();
        out[3] = 9.5;
        out
    }

    /// The same, with a second component well above the rest.
    fn two_peaks(dims: usize) -> Vec<f32> {
        let mut out = dominant(dims);
        out[17 % dims] = -4.25;
        out
    }

    #[test]
    fn escapes_round_trip_at_every_count() {
        let vector = dominant(64);
        for count in [0usize, 1, 2, 16] {
            let planes = Planes::quantize(&vector, count).expect("quantized");
            assert_eq!(planes.escapes().len(), count, "{count} escapes asked for");
            // Ascending, no index twice, and inside the vector.
            for pair in planes.escapes().windows(2) {
                assert!(pair[0].0 < pair[1].0, "escapes are not ascending");
            }
            for (index, value) in planes.escapes() {
                assert!((*index as usize) < vector.len());
                assert_eq!(*value, f32_to_f16(vector[*index as usize]));
            }
            let body = planes.encode();
            assert_eq!(body.len(), planes.encoded_len(), "{count}");
            assert_eq!(
                Planes::decode(vector.len(), &body).expect("decoded"),
                planes,
                "{count}"
            );
            // An escaped component comes back exactly as the escape
            // says, whatever the planes hold.
            let read = planes.reconstruct().expect("a reconstruction");
            for (index, value) in planes.escapes() {
                assert_eq!(read[*index as usize], f16_to_f32(*value));
            }
            for prefix in [0u8, 1, 4] {
                let mask = ((1u16 << (prefix + 1)) - 1) as u8;
                let part = planes.subset(mask);
                let read = part.reconstruct().expect("a reconstruction");
                for (index, value) in planes.escapes() {
                    assert_eq!(
                        read[*index as usize],
                        f16_to_f32(*value),
                        "{count} escapes at prefix {prefix}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_escapes_are_the_largest_components() {
        let vector = two_peaks(32);
        let planes = Planes::quantize(&vector, 2).expect("quantized");
        let chosen: Vec<usize> = planes
            .escapes()
            .iter()
            .map(|(index, _)| *index as usize)
            .collect();
        let mut by_size: Vec<usize> = (0..vector.len()).collect();
        by_size.sort_by(|a, b| vector[*b].abs().total_cmp(&vector[*a].abs()));
        let mut wanted: Vec<usize> = by_size[..2].to_vec();
        wanted.sort_unstable();
        assert_eq!(chosen, wanted);
        // The scale is the largest component that is not an escape.
        let rest = vector
            .iter()
            .enumerate()
            .filter(|(index, _)| !chosen.contains(index))
            .map(|(_, value)| value.abs())
            .fold(0f32, f32::max);
        assert_eq!(planes.scale(), f16_to_f32(f32_to_f16_up(rest)));
        assert!(planes.scale() < 1.0, "the escapes did not free the scale");
    }

    #[test]
    fn one_escape_rescues_a_vector_with_one_dominant_component() {
        let vector = dominant(64);
        let error = |count: usize, prefix: u8| -> f32 {
            let planes = Planes::quantize(&vector, count).expect("quantized");
            let mask = ((1u16 << (prefix + 1)) - 1) as u8;
            1.0 - planes
                .subset(mask)
                .cosine(&vector)
                .expect("a cosine against the vector it came from")
        };
        // With no escape the one big component sets the scale and the
        // rest quantize into nothing.
        let without = error(0, 7);
        let with = error(1, 7);
        assert!(
            with < without / 10.0,
            "one escape took the error from {without} to {with}"
        );
        // And the same holds at two planes, which is where a coarse
        // search reads.
        let without = error(0, 1);
        let with = error(1, 1);
        assert!(
            with < without / 2.0,
            "at two planes one escape took the error from {without} to {with}"
        );
        // And with a second component above the rest, a second escape
        // is what frees the scale for the others.
        let peaks = two_peaks(64);
        let peak_error = |count: usize| -> f32 {
            1.0 - Planes::quantize(&peaks, count)
                .expect("quantized")
                .cosine(&peaks)
                .expect("a cosine")
        };
        assert!(
            peak_error(2) < peak_error(1) / 10.0,
            "the second escape took the error from {} to {}",
            peak_error(1),
            peak_error(2)
        );
    }

    #[test]
    fn plane_zero_is_the_same_with_and_without_escapes() {
        let vector = dominant(37);
        let plain = Planes::quantize(&vector, 0).expect("quantized");
        for count in [1usize, 2, 8, 16] {
            let escaped = Planes::quantize(&vector, count).expect("quantized");
            assert_eq!(
                escaped.signs().expect("signs"),
                plain.signs().expect("signs"),
                "{count} escapes changed the binary embedding"
            );
            // And an escaped component is zero in the magnitude planes.
            let magnitudes = escaped.magnitudes().expect("magnitudes");
            let _ = magnitudes;
            let eight = escaped.subset(0xff);
            let full = eight.magnitudes().expect("magnitudes");
            for (index, _) in escaped.escapes() {
                assert_eq!(
                    full[*index as usize], 0,
                    "an escape is not zero in the planes"
                );
            }
        }
    }

    #[test]
    fn escapes_travel_with_plane_zero() {
        let vector = dominant(24);
        let planes = Planes::quantize(&vector, 2).expect("quantized");
        let pieces = planes.split();
        assert_eq!(pieces.len(), 8);
        assert_eq!(pieces[0].escapes().len(), 2, "plane 0 carries them");
        for piece in &pieces[1..] {
            assert!(piece.escapes().is_empty(), "another plane carried escapes");
        }
        // The bytes of plane 0's message pay for them, which is what a
        // budget has to know.
        assert_eq!(
            pieces[0].encoded_len(),
            pieces[1].encoded_len() + 2 * (1 + 2),
            "an escape of a low index is a byte of index and two of value"
        );

        // Put back together in any order, the escapes are there once.
        let mut merged = Planes::empty(24, planes.scale_bits()).expect("an empty record");
        for piece in pieces.iter().rev() {
            merged.merge(piece).expect("a merge");
        }
        assert_eq!(merged, planes);
    }

    #[test]
    fn the_first_escape_list_in_hand_wins() {
        let vector = two_peaks(24);
        let planes = Planes::quantize(&vector, 2).expect("quantized");
        // A second message with the same planes and a different escape
        // value: the same record as far as everything else goes, so
        // only the lists are in question. The index of the first escape
        // is one byte, so its value is the two bytes after it.
        let piece = planes.subset(0b0000_0011);
        let mut body = piece.encode();
        body[5] ^= 0x40;
        let other = Planes::decode(24, &body).expect("a body of the same planes");
        assert_ne!(other.escapes(), piece.escapes(), "the lists must differ");

        // Two messages that both carry plane 0 and disagree about the
        // escapes: the one already in hand stands, and the merge does
        // not refuse the planes over it.
        let mut merged = planes.subset(0b0000_0001);
        merged.merge(&other).expect("the merge goes on");
        assert_eq!(merged.escapes(), planes.escapes());
        assert_eq!(merged.present(), 0b0000_0011);

        // A reader whose first message had none takes the list that
        // does arrive.
        let mut late = planes.subset(0b0000_0010);
        assert!(late.escapes().is_empty());
        late.merge(&planes.subset(0b0000_0001)).expect("a merge");
        assert_eq!(late.escapes(), planes.escapes());
    }

    #[test]
    fn an_escape_of_every_component_is_the_whole_vector() {
        let vector: Vec<f32> = (0..12).map(|i| (i as f32 - 5.5) * 0.7).collect();
        let planes = Planes::quantize(&vector, 16).expect("quantized");
        assert_eq!(
            planes.escapes().len(),
            12,
            "capped at the components there are"
        );
        assert_eq!(planes.scale(), 0.0, "nothing is left to set a scale");
        let read = planes.reconstruct().expect("a reconstruction");
        for (index, value) in vector.iter().enumerate() {
            assert_eq!(read[index], f16_to_f32(f32_to_f16(*value)));
        }
    }

    #[test]
    fn more_escapes_than_the_format_allows_are_refused() {
        let vector = dominant(64);
        assert_eq!(
            Planes::quantize(&vector, usize::from(MAX_ESCAPES) + 1),
            Err(Error::Escapes)
        );
        assert!(Planes::quantize(&vector, usize::from(MAX_ESCAPES)).is_ok());

        // A body that claims seventeen escapes, and one whose index is
        // not a component of the vector.
        let planes = Planes::quantize(&vector, 2).expect("quantized");
        let mut body = planes.encode();
        body[3] = MAX_ESCAPES + 1;
        assert_eq!(Planes::decode(64, &body), Err(Error::Escapes));
        let mut body = planes.encode();
        body[4] = 64; // an index at dims, one past the last component
        assert_eq!(Planes::decode(64, &body), Err(Error::Escapes));
        let mut body = planes.encode();
        body[4] = 63;
        assert!(
            Planes::decode(64, &body).is_ok(),
            "the last component is fine"
        );
    }

    #[test]
    fn an_unordered_or_repeated_escape_list_is_read_as_it_comes() {
        // Section 5 makes ascending order and distinct indices a
        // writer's duty; a reader that refused the list would throw
        // away a record it can read. Two escapes of one index are
        // applied in order, so the last of them stands.
        let mut body = Vec::new();
        body.extend_from_slice(&f32_to_f16(1.0).to_le_bytes());
        body.push(0b0000_0001); // plane 0 only
        body.push(3); // three escapes, descending and with a repeat
        for (index, value) in [(2u32, 0.5f32), (1, -0.25), (1, 0.75)] {
            crate::wire::put_varint(&mut body, index);
            body.extend_from_slice(&f32_to_f16(value).to_le_bytes());
        }
        body.push(0b0100_0000); // the sign of component 1
        let planes = Planes::decode(4, &body).expect("a body a writer wrote badly");
        assert_eq!(planes.escapes().len(), 3);
        let read = planes.reconstruct().expect("a reconstruction");
        assert_eq!(read[2], 0.5);
        assert_eq!(read[1], 0.75, "the last escape of an index stands");
        assert_eq!(planes.encode(), body, "and the list travels as it came");
    }

    #[test]
    fn a_body_with_escapes_cut_at_any_byte_is_refused() {
        let planes = Planes::quantize(&dominant(33), 3).expect("quantized");
        let body = planes.encode();
        assert_eq!(Planes::decode(33, &body).expect("decoded"), planes);
        for cut in 0..body.len() {
            assert!(
                Planes::decode(33, &body[..cut]).is_err(),
                "a body cut at {cut} was accepted"
            );
        }
    }

    #[test]
    fn random_bodies_never_panic() {
        let mut seed = 0xa5a5_1234_9876_fedcu64;
        for _ in 0..4000 {
            let mut bytes = Vec::new();
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            for _ in 0..(seed >> 40) % 48 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                bytes.push((seed >> 33) as u8);
            }
            for dims in [1usize, 4, 33, 64] {
                if let Ok(planes) = Planes::decode(dims, &bytes) {
                    assert!(planes.escapes().len() <= usize::from(MAX_ESCAPES));
                    for (index, _) in planes.escapes() {
                        assert!((*index as usize) < dims);
                    }
                    let _ = planes.reconstruct();
                    let _ = planes.cosine(&vec![0.5; dims]);
                    let _ = planes.dot(&vec![0.5; dims]);
                    assert_eq!(planes.encode().len(), planes.encoded_len());
                }
            }
        }
    }

    #[test]
    fn cosine_ranks_records_that_a_bare_dot_product_would_not() {
        // Two records of the same vector, one scaled up, are the same
        // direction: a dot product says the larger one matches better,
        // and a cosine says they are the same. Section 5 asks a reader
        // of few planes for the cosine.
        let small = dominant(32);
        let large: Vec<f32> = small.iter().map(|x| x * 8.0).collect();
        let query: Vec<f32> = small.iter().map(|x| x + 0.01).collect();
        let a = Planes::quantize(&small, 1).expect("quantized");
        let b = Planes::quantize(&large, 1).expect("quantized");
        assert!(
            b.dot(&query).expect("a dot") > a.dot(&query).expect("a dot") * 4.0,
            "the dot product did not follow the scale"
        );
        let ca = a.cosine(&query).expect("a cosine");
        let cb = b.cosine(&query).expect("a cosine");
        assert!(
            (ca - cb).abs() < 1e-3,
            "{ca} and {cb} are not the same angle"
        );
        assert!(ca > 0.99);
        assert_eq!(a.cosine(&query[..4]), Err(Error::Mismatch));
        // A vector that points nowhere has no angle to anything.
        let zero = Planes::quantize(&[0.0; 32], 0).expect("quantized");
        assert_eq!(zero.cosine(&query).expect("a cosine"), 0.0);
    }

    #[test]
    fn hamming_refuses_planes_of_different_lengths() {
        assert_eq!(hamming(&[0, 0], &[0]), Err(Error::Mismatch));
        assert_eq!(hamming(&[0b1010_0000], &[0b0110_0000]), Ok(2));
    }
}
