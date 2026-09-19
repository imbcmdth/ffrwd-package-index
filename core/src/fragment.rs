//! Section 6: slicing one VECTOR message across carriers, and putting
//! the slices back.
//!
//! What is sliced is the VECTOR message's whole value, offsets and all,
//! so the reassembled bytes are exactly what a reader would have read
//! had the message fitted. The FRAGMENT's own `space_id` and
//! `record_id` repeat what the first slice already carries, which is
//! what lets a reader file a slice it receives before the first one.

use crate::message::{Fragment, VectorRecord};
use crate::wire::varint_len;
use crate::{Error, Result};

/// The largest VECTOR value this crate will reassemble.
///
/// A reader allocates `total` bytes the moment it sees a first slice,
/// and `total` is a varint a stranger wrote, so it needs a ceiling.
/// This one is four times the largest body section 3 allows: 65536
/// components of F32.
pub const MAX_VALUE_BYTES: usize = 1 << 20;

/// Slices `value`, a VECTOR message's value, into FRAGMENT messages
/// each of which encodes to at most `budget` bytes.
///
/// The budget counts the whole message, type byte and length included,
/// because that is what a carrier's room is measured in.
pub fn fragment(
    space_id: u8,
    record_id: u16,
    value: &[u8],
    budget: usize,
) -> Result<Vec<Fragment>> {
    fragment_with_first(space_id, record_id, value, budget, budget)
}

/// Slices as [`fragment`] does, with a smaller budget for the first
/// slice.
///
/// A writer that has already put something else on this carrier has
/// less room for the slice at offset 0, and that slice has to go out
/// here: section 6 hangs the record's offsets on the carrier of the
/// slice at offset 0, so putting it off to the next carrier would make
/// the offsets already written wrong.
pub fn fragment_with_first(
    space_id: u8,
    record_id: u16,
    value: &[u8],
    first: usize,
    budget: usize,
) -> Result<Vec<Fragment>> {
    if value.is_empty() || value.len() > u32::MAX as usize {
        return Err(Error::TooLarge);
    }
    let total = value.len() as u32;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < value.len() {
        let allowed = if offset == 0 { first } else { budget };
        let room = slice_room(record_id, total, offset as u32, allowed).ok_or(Error::TooLarge)?;
        let end = (offset + room).min(value.len());
        out.push(Fragment {
            space_id,
            record_id,
            total,
            offset: offset as u32,
            bytes: value[offset..end].to_vec(),
        });
        offset = end;
    }
    Ok(out)
}

/// The largest slice that still fits `budget` bytes of encoded message
/// at this offset, or `None` when even one byte will not fit.
pub fn slice_room(record_id: u16, total: u32, offset: u32, budget: usize) -> Option<usize> {
    // The one byte is the FRAGMENT's own space_id.
    let head = 1 + varint_len(u32::from(record_id)) + varint_len(total) + varint_len(offset);
    // The message is a type byte, the value's length as a varint, and
    // the value. The length's own width depends on how much of the
    // value goes in, so start from the widest slice a one-byte length
    // would allow and give a byte back until the whole message fits.
    let mut slice = budget.saturating_sub(2 + head);
    while slice > 0 && 1 + varint_len((head + slice) as u32) + head + slice > budget {
        slice -= 1;
    }
    if slice == 0 {
        return None;
    }
    Some(slice)
}

/// One record's slices, gathered.
///
/// A reader keeps one of these per record it has seen a slice of, and
/// drops it whole if any slice never arrives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reassembly {
    total: u32,
    bytes: Vec<u8>,
    filled: Vec<bool>,
    have: usize,
}

impl Reassembly {
    /// An empty reassembly for a value of `total` bytes.
    pub fn new(total: u32) -> Result<Self> {
        if total == 0 || total as usize > MAX_VALUE_BYTES {
            return Err(Error::FragmentBounds);
        }
        Ok(Self {
            total,
            bytes: vec![0; total as usize],
            filled: vec![false; total as usize],
            have: 0,
        })
    }

    /// Files one slice.
    ///
    /// A slice that disagrees about `total`, or that falls outside it,
    /// is refused; a slice that repeats bytes already in hand must
    /// repeat them exactly.
    pub fn push(&mut self, fragment: &Fragment) -> Result<()> {
        if fragment.total != self.total {
            return Err(Error::Mismatch);
        }
        let start = fragment.offset as usize;
        let end = start
            .checked_add(fragment.bytes.len())
            .ok_or(Error::FragmentBounds)?;
        if end > self.bytes.len() {
            return Err(Error::FragmentBounds);
        }
        for (index, byte) in fragment.bytes.iter().enumerate() {
            let at = start + index;
            if self.filled[at] {
                if self.bytes[at] != *byte {
                    return Err(Error::Mismatch);
                }
                continue;
            }
            self.bytes[at] = *byte;
            self.filled[at] = true;
            self.have += 1;
        }
        Ok(())
    }

    /// Whether every byte of the value has arrived.
    pub fn complete(&self) -> bool {
        self.have == self.total as usize
    }

    /// How many bytes are still missing.
    pub fn missing(&self) -> usize {
        self.total as usize - self.have
    }

    /// The value, once it is whole.
    pub fn take(self) -> Option<Vec<u8>> {
        if self.complete() {
            Some(self.bytes)
        } else {
            None
        }
    }

    /// How many bytes this reassembly is holding on to.
    pub fn footprint(&self) -> usize {
        self.bytes.len() * 2
    }
}

/// Slices back into the VECTOR value they were cut from.
pub fn reassemble(fragments: &[Fragment]) -> Result<Vec<u8>> {
    let first = fragments.first().ok_or(Error::FragmentBounds)?;
    let mut out = Reassembly::new(first.total)?;
    for fragment in fragments {
        out.push(fragment)?;
    }
    out.take().ok_or(Error::FragmentBounds)
}

/// A VECTOR message value that is too big for one carrier, as slices.
///
/// The convenience a writer wants: it hands over the record it would
/// have sent whole.
pub fn fragment_record(record: &VectorRecord, budget: usize) -> Result<Vec<Fragment>> {
    fragment(record.space_id, record.record_id, &record.encode(), budget)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    fn a_record(body: usize) -> VectorRecord {
        VectorRecord {
            space_id: 2,
            record_id: 1234,
            start_off: -4000,
            end_off: -1000,
            body: (0..body).map(|i| (i % 251) as u8).collect(),
        }
    }

    #[test]
    fn slices_reassemble_into_the_value_they_came_from() {
        for budget in [16usize, 17, 31, 64, 200, 1000] {
            let record = a_record(500);
            let value = record.encode();
            let slices = fragment_record(&record, budget).expect("slices");
            for slice in &slices {
                let message = Message::Fragment(slice.clone());
                assert!(
                    message.encoded_len() <= budget,
                    "a slice of {} bytes overran a budget of {budget}",
                    message.encoded_len()
                );
            }
            assert_eq!(reassemble(&slices).expect("the value"), value);
            assert_eq!(
                VectorRecord::decode(&reassemble(&slices).expect("the value")).expect("a record"),
                record
            );
        }
    }

    #[test]
    fn slices_arriving_out_of_order_still_reassemble() {
        let record = a_record(300);
        let mut slices = fragment_record(&record, 64).expect("slices");
        slices.reverse();
        assert_eq!(reassemble(&slices).expect("the value"), record.encode());
    }

    #[test]
    fn a_missing_slice_drops_the_record() {
        let record = a_record(300);
        let slices = fragment_record(&record, 64).expect("slices");
        assert!(slices.len() > 2);
        let mut held = Reassembly::new(slices[0].total).expect("a reassembly");
        for slice in slices.iter().skip(1) {
            held.push(slice).expect("a slice");
        }
        assert!(!held.complete());
        assert_eq!(held.missing(), slices[0].bytes.len());
        assert!(held.take().is_none());
    }

    #[test]
    fn slices_that_disagree_are_refused() {
        let record = a_record(120);
        let slices = fragment_record(&record, 48).expect("slices");
        let mut held = Reassembly::new(slices[0].total).expect("a reassembly");
        held.push(&slices[0]).expect("a slice");
        let mut wrong = slices[0].clone();
        wrong.bytes[0] ^= 0xff;
        assert_eq!(held.push(&wrong), Err(Error::Mismatch));
        let mut other_total = slices[0].clone();
        other_total.total += 1;
        assert_eq!(held.push(&other_total), Err(Error::Mismatch));
        let mut past_the_end = slices[0].clone();
        past_the_end.offset = slices[0].total - 1;
        assert_eq!(held.push(&past_the_end), Err(Error::FragmentBounds));
    }

    #[test]
    fn a_budget_too_small_for_any_slice_is_refused() {
        // Seven bytes is the header of a FRAGMENT of this record with
        // no room left for a byte of the value.
        let record = a_record(40);
        assert_eq!(fragment_record(&record, 7), Err(Error::TooLarge));
        assert_eq!(fragment(1, 1, &[], 100), Err(Error::TooLarge));
        // One byte more and it goes, a byte of value at a time.
        let crawling = fragment_record(&record, 8).expect("slices");
        assert!(crawling.iter().all(|slice| slice.bytes.len() == 1));
    }

    #[test]
    fn a_reassembly_larger_than_the_ceiling_is_refused() {
        assert!(Reassembly::new(MAX_VALUE_BYTES as u32).is_ok());
        assert_eq!(
            Reassembly::new(MAX_VALUE_BYTES as u32 + 1),
            Err(Error::FragmentBounds)
        );
        assert_eq!(Reassembly::new(0), Err(Error::FragmentBounds));
    }

    #[test]
    fn a_budget_at_a_varint_boundary_still_fits() {
        // 0x80 bytes of value is where the length varint widens; the
        // slice has to shrink by one to pay for it.
        let record = a_record(1000);
        for budget in 120..160usize {
            let slices = fragment_record(&record, budget).expect("slices");
            let widest = slices
                .iter()
                .map(|s| Message::Fragment(s.clone()).encoded_len())
                .max()
                .expect("a slice");
            assert!(widest <= budget, "budget {budget} produced {widest}");
        }
    }
}
