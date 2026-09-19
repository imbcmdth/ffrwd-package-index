//! Section 7's placement policies, as a state machine with no I/O.
//!
//! A writer sees access units go by and records turn up, and has to
//! decide which carrier each record rides on, declare its spaces on
//! every keyframe so a reader that joined late has them, and turn
//! absolute spans into offsets from whichever carrier it picked. That
//! decision is the whole of this module, so the packet filter that will
//! do the weaving inside a pipeline is left with nothing but moving
//! bytes.
//!
//! Feed it [`Planner::carrier`] for every access unit in presentation
//! order and it hands back the messages for that access unit's unit.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::fragment::{fragment_with_first, slice_room};
use crate::message::{Message, Space, VectorRecord};
use crate::quant::Planes;
use crate::{Error, Result};

/// Where records go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Whole records ride on keyframes, each on the first keyframe at
    /// or after the end of its span. The policy for files.
    Keyframe,
    /// A record rides on the first carrier after it exists. The policy
    /// for live streams, where a watcher should hear of a match at once
    /// and the next keyframe may be seconds away.
    Next,
    /// As `Next`, with a byte budget per carrier, filled with planes
    /// and fragments, most significant first.
    Spread { budget_bytes: usize },
}

/// One access unit, as much of it as placement cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carrier {
    pub pts_ms: i64,
    pub keyframe: bool,
}

/// What one access unit takes, and whether the writer must keep it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Placed {
    /// The messages for this access unit's unit, which may be empty.
    pub messages: Vec<Message>,
    /// Whether this access unit may still be asked to take more.
    ///
    /// Section 7 puts a record whose span ends after the LAST keyframe
    /// on that keyframe, with a positive `end_off`, so that the
    /// keyframes of a file hold all of its records. A writer streaming
    /// past does not know which keyframe is the last one until the next
    /// one arrives, so under [`Placement::Keyframe`] every keyframe is
    /// open from here until the one after it, and the writer holds it
    /// and everything behind it until then. The access unit still open
    /// when the stream ends is the one [`Planner::finish`] is handed.
    ///
    /// Under `next` and `spread` nothing is ever open: those policies
    /// never look back.
    pub open: bool,
}

/// A record waiting for a carrier.
#[derive(Clone, Debug, PartialEq)]
pub struct Pending {
    pub space_id: u8,
    pub record_id: u16,
    /// The span the record describes, in presentation time.
    pub start_ms: i64,
    pub end_ms: i64,
    /// When the writer had the record in hand. A record cannot ride a
    /// carrier that has already gone past.
    pub available_ms: i64,
    /// The bodies to send, most significant first. One body for a whole
    /// vector; one per plane for a record a budget will dole out.
    pub bodies: Vec<Vec<u8>>,
}

impl Pending {
    /// A record that goes out in one piece.
    pub fn whole(space_id: u8, record_id: u16, start_ms: i64, end_ms: i64, body: Vec<u8>) -> Self {
        Self {
            space_id,
            record_id,
            start_ms,
            end_ms,
            available_ms: end_ms,
            bodies: vec![body],
        }
    }

    /// A layered record, one body per plane, so a budget can send the
    /// sign plane now and the rest as room turns up.
    pub fn layered(
        space_id: u8,
        record_id: u16,
        start_ms: i64,
        end_ms: i64,
        planes: &Planes,
    ) -> Self {
        Self {
            space_id,
            record_id,
            start_ms,
            end_ms,
            available_ms: end_ms,
            bodies: planes.split().iter().map(Planes::encode).collect(),
        }
    }

    /// When this record may first be written: not before its span has
    /// ended, and not before the writer had it.
    fn ready_ms(&self) -> i64 {
        self.end_ms.max(self.available_ms)
    }
}

/// A record the planner is still working through.
#[derive(Clone, Debug)]
struct Job {
    record: Pending,
    bodies: VecDeque<Vec<u8>>,
    /// Slices of a message too big for one carrier, already cut, with
    /// their offsets fixed against the carrier the first slice rode.
    slices: VecDeque<Message>,
}

/// A space the planner is declaring, and when it started.
#[derive(Clone, Debug)]
struct Declared {
    space: Space,
    /// Declared before any carrier went by, so section 3 puts it on
    /// the first keyframe of the stream whether or not a record rides
    /// there. A space that turned up later waits for the first carrier
    /// this writer writes for it.
    from_the_start: bool,
    /// Whether it has gone out at least once.
    started: bool,
}

/// Records and access units in, messages out.
#[derive(Clone, Debug)]
pub struct Planner {
    policy: Placement,
    spaces: BTreeMap<u8, Declared>,
    jobs: VecDeque<Job>,
    /// Whether a carrier has gone by, which is what tells a space
    /// declared at the start from one that turned up live.
    running: bool,
    skipped: usize,
}

impl Planner {
    /// A planner for one policy.
    pub fn new(policy: Placement) -> Self {
        Self {
            policy,
            spaces: BTreeMap::new(),
            jobs: VecDeque::new(),
            running: false,
            skipped: 0,
        }
    }

    /// Declares a space.
    ///
    /// Section 3: a space the writer has before the stream begins goes
    /// on the FIRST KEYFRAME of the stream and on every keyframe after
    /// it, whether or not a record rides there, so that the first
    /// packet of a file says what the file carries. A space the writer
    /// learns of after the stream has begun goes on the first carrier
    /// it writes for that space, and on every keyframe after that.
    ///
    /// A definition that replaces one already declared starts again, so
    /// that the change is announced rather than waiting for a keyframe
    /// the old one already rode.
    pub fn declare(&mut self, space: Space) {
        let id = space.space_id;
        let from_the_start = !self.running;
        match self.spaces.get_mut(&id) {
            Some(held) if held.space == space => {}
            Some(held) => {
                held.space = space;
                held.started = false;
            }
            None => {
                self.spaces.insert(
                    id,
                    Declared {
                        space,
                        from_the_start,
                        started: false,
                    },
                );
            }
        }
    }

    /// Hands the planner a record to place.
    pub fn submit(&mut self, record: Pending) {
        self.jobs.push_back(Job {
            bodies: record.bodies.iter().cloned().collect(),
            record,
            slices: VecDeque::new(),
        });
    }

    /// The messages for one access unit's unit, which may be empty, and
    /// whether the writer must hold that access unit open.
    pub fn carrier(&mut self, carrier: Carrier) -> Placed {
        let open = matches!(self.policy, Placement::Keyframe) && carrier.keyframe;
        Placed {
            messages: self.messages_for(carrier),
            open,
        }
    }

    fn messages_for(&mut self, carrier: Carrier) -> Vec<Message> {
        let mut out = Vec::new();
        let mut room = match self.policy {
            Placement::Spread { budget_bytes } => budget_bytes,
            _ => usize::MAX,
        };

        // Which spaces write a record here. That is what a space the
        // writer learned of live is waiting for: its own first carrier.
        let writing: BTreeSet<u8> = self
            .jobs
            .iter()
            .filter(|job| self.eligible(job, carrier))
            .map(|job| job.record.space_id)
            .collect();
        self.running = true;

        for id in self.spaces_due(carrier, &writing) {
            let declared = self.spaces.get_mut(&id).expect("a declared space");
            declared.started = true;
            let message = Message::Space(declared.space.clone());
            room = room.saturating_sub(message.encoded_len());
            out.push(message);
        }
        if writing.is_empty() {
            return out;
        }

        let mut jobs = std::mem::take(&mut self.jobs);
        for job in jobs.iter_mut() {
            if !self.eligible(job, carrier) {
                continue;
            }
            room = self.fill(job, carrier, room, &mut out);
        }
        jobs.retain(|job| !job.bodies.is_empty() || !job.slices.is_empty());
        self.jobs = jobs;
        out
    }

    /// Everything still waiting, against the access unit the writer
    /// held open.
    ///
    /// Under [`Placement::Keyframe`] that is the LAST KEYFRAME, which
    /// is the one [`Placed::open`] last said to hold: a record whose
    /// span ends after it has no keyframe of its own, and section 7 has
    /// it ride that one with a positive `end_off` rather than ride the
    /// last access unit, which a reader of sync samples alone would
    /// never look at. Under `next` and `spread` nothing is held and the
    /// caller passes the last access unit it has.
    ///
    /// A caller with no open access unit to give - a `keyframe` stream
    /// with no keyframe in it, or one whose writer could not hold the
    /// last one - has nowhere to put these records, and should report
    /// them rather than call this with an access unit a keyframe reader
    /// will not visit.
    pub fn finish(&mut self, carrier: Carrier) -> Vec<Message> {
        let mut vectors = Vec::new();
        let mut jobs = std::mem::take(&mut self.jobs);
        for job in jobs.iter_mut() {
            self.fill(job, carrier, usize::MAX, &mut vectors);
        }
        jobs.clear();
        self.jobs = jobs;
        if vectors.is_empty() {
            return vectors;
        }
        // A space that never met a keyframe - a stream with none in it,
        // or one whose writer could hold none open - still has to be
        // said, or these records name nothing.
        let mut out = Vec::new();
        let late: Vec<u8> = self
            .spaces
            .iter()
            .filter(|(_, declared)| !declared.started)
            .map(|(id, _)| *id)
            .collect();
        for id in late {
            let declared = self.spaces.get_mut(&id).expect("a declared space");
            declared.started = true;
            out.push(Message::Space(declared.space.clone()));
        }
        out.extend(vectors);
        out
    }

    /// How many records are still waiting for a carrier.
    pub fn pending(&self) -> usize {
        self.jobs.len()
    }

    /// How many bodies were dropped because their span was too far from
    /// any carrier to express as an offset.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    fn eligible(&self, job: &Job, carrier: Carrier) -> bool {
        if job.bodies.is_empty() && job.slices.is_empty() {
            return false;
        }
        match self.policy {
            Placement::Keyframe => carrier.keyframe && carrier.pts_ms >= job.record.ready_ms(),
            // A record already cut into slices keeps going out whatever
            // the times say: the rest of it is owed to the reader.
            _ => !job.slices.is_empty() || carrier.pts_ms >= job.record.ready_ms(),
        }
    }

    /// Which SPACE messages go on this carrier, in id order.
    ///
    /// Section 3: every keyframe carries every space the writer has,
    /// from the first keyframe of the stream, whether or not a record
    /// rides there. A cut or a segment begins at a keyframe, so
    /// declaring on all of them is what makes whatever begins there
    /// readable, and putting them on the first one is what lets a
    /// reader that wants only the shape of a file name a packet in
    /// advance. It costs one message per keyframe per space.
    ///
    /// A space the writer learned of after the stream began has no
    /// keyframe behind it to have ridden, so it goes on the first
    /// carrier written for it, and joins the keyframes after that.
    fn spaces_due(&self, carrier: Carrier, writing: &BTreeSet<u8>) -> Vec<u8> {
        self.spaces
            .iter()
            .filter(|(id, declared)| {
                if declared.started || declared.from_the_start {
                    carrier.keyframe
                } else {
                    writing.contains(id)
                }
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// Puts as much of one job on this carrier as the room allows, and
    /// returns what is left of the room.
    fn fill(
        &mut self,
        job: &mut Job,
        carrier: Carrier,
        mut room: usize,
        out: &mut Vec<Message>,
    ) -> usize {
        while let Some(slice) = job.slices.front() {
            let size = slice.encoded_len();
            if size > room {
                return room;
            }
            room -= size;
            out.push(job.slices.pop_front().expect("a slice"));
        }
        while let Some(body) = job.bodies.front() {
            let Ok(record) = Self::record_for(job, carrier, body.clone()) else {
                job.bodies.pop_front();
                self.skipped += 1;
                continue;
            };
            let message = Message::Vector(record.clone());
            let size = message.encoded_len();
            if size <= room {
                room -= size;
                out.push(message);
                job.bodies.pop_front();
                continue;
            }
            // Too big for what is left. Whole messages are preferred to
            // slices, so it only gets cut when a whole carrier would
            // not hold it either.
            let budget = match self.policy {
                Placement::Spread { budget_bytes } => budget_bytes,
                _ => return room,
            };
            if size <= budget {
                return room;
            }
            let value = record.encode();
            if slice_room(record.record_id, value.len() as u32, 0, room).is_none() {
                // Not even a first slice fits; the next carrier starts
                // it, so the offsets are that carrier's.
                return room;
            }
            let Ok(slices) =
                fragment_with_first(record.space_id, record.record_id, &value, room, budget)
            else {
                job.bodies.pop_front();
                self.skipped += 1;
                continue;
            };
            job.bodies.pop_front();
            job.slices = slices.into_iter().map(Message::Fragment).collect();
            while let Some(slice) = job.slices.front() {
                let size = slice.encoded_len();
                if size > room {
                    return room;
                }
                room -= size;
                out.push(job.slices.pop_front().expect("a slice"));
            }
        }
        room
    }

    /// One VECTOR message for this carrier, its span turned into
    /// offsets from the carrier's own presentation time.
    fn record_for(job: &Job, carrier: Carrier, body: Vec<u8>) -> Result<VectorRecord> {
        let start = offset(job.record.start_ms, carrier.pts_ms)?;
        let end = offset(job.record.end_ms, carrier.pts_ms)?;
        Ok(VectorRecord {
            space_id: job.record.space_id,
            record_id: job.record.record_id,
            start_off: start,
            end_off: end,
            body,
        })
    }
}

/// A span endpoint as an offset from a carrier, refused when the two
/// are further apart than an offset can say.
fn offset(at_ms: i64, carrier_ms: i64) -> Result<i32> {
    i32::try_from(at_ms - carrier_ms).map_err(|_| Error::TooLarge)
}

/// The whole placement of a finished stream in one call: the messages
/// for each carrier, in order.
///
/// What [`Planner`] does, for a caller that has all the carriers and
/// all the records already, which is every test and the file tool.
pub fn plan(
    policy: Placement,
    spaces: &[Space],
    records: &[Pending],
    carriers: &[Carrier],
) -> Vec<Vec<Message>> {
    let mut planner = Planner::new(policy);
    for space in spaces {
        planner.declare(space.clone());
    }
    let mut records = records.to_vec();
    records.sort_by_key(|record| (record.available_ms, record.space_id, record.record_id));
    let mut queued = records.into_iter().peekable();

    let mut out = Vec::with_capacity(carriers.len());
    // Which access unit the leftovers go on: the last one the planner
    // said to hold open, which under `keyframe` is the last keyframe
    // and under the other policies is nothing, so the last carrier of
    // all stands in. A caller with every carrier in hand knows which is
    // which without holding anything, which is why a file writer is
    // two passes and a stream writer holds packets.
    let mut open: Option<usize> = None;
    for (index, carrier) in carriers.iter().enumerate() {
        while queued
            .peek()
            .is_some_and(|record| record.available_ms <= carrier.pts_ms)
        {
            planner.submit(queued.next().expect("a record"));
        }
        let placed = planner.carrier(*carrier);
        if placed.open {
            open = Some(index);
        }
        out.push(placed.messages);
    }
    for record in queued {
        planner.submit(record);
    }
    let last = match planner.policy {
        Placement::Keyframe => open,
        _ => carriers.len().checked_sub(1),
    };
    if let Some(index) = last {
        let left = planner.finish(carriers[index]);
        out[index].extend(left);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Encoding, Unit, VectorBody};

    fn space(id: u8) -> Space {
        let mut space = Space::new(id, 16, Encoding::I8);
        space.model = "test:model".into();
        space
    }

    fn carriers(count: usize, gop: usize) -> Vec<Carrier> {
        (0..count)
            .map(|index| Carrier {
                pts_ms: index as i64 * 100,
                keyframe: index % gop == 0,
            })
            .collect()
    }

    fn planes(dims: usize) -> Planes {
        let vector: Vec<f32> = (0..dims).map(|i| (i as f32 * 0.3).sin()).collect();
        Planes::quantize(&vector, 0).expect("quantized")
    }

    fn records(count: usize) -> Vec<Pending> {
        (0..count)
            .map(|index| {
                Pending::layered(
                    1,
                    index as u16,
                    index as i64 * 1000,
                    index as i64 * 1000 + 1000,
                    &planes(16),
                )
            })
            .collect()
    }

    /// Every VECTOR message in a plan, with the carrier it rode on.
    fn placed(plan: &[Vec<Message>], carriers: &[Carrier]) -> Vec<(i64, VectorRecord)> {
        plan.iter()
            .zip(carriers)
            .flat_map(|(messages, carrier)| {
                messages.iter().filter_map(move |message| match message {
                    Message::Vector(record) => Some((carrier.pts_ms, record.clone())),
                    _ => None,
                })
            })
            .collect()
    }

    #[test]
    fn keyframe_placement_waits_for_the_first_keyframe_past_the_span() {
        let carriers = carriers(60, 15);
        let plan = plan(Placement::Keyframe, &[space(1)], &records(4), &carriers);
        for (pts, record) in placed(&plan, &carriers) {
            let end = pts + i64::from(record.end_off);
            assert!(pts >= end, "a record rode a carrier before its span ended");
            let carrier = carriers
                .iter()
                .find(|carrier| carrier.pts_ms == pts)
                .expect("the carrier");
            assert!(
                carrier.keyframe,
                "a record rode a carrier that is no keyframe"
            );
            // No earlier keyframe would have done.
            let earlier = carriers
                .iter()
                .filter(|other| other.keyframe && other.pts_ms >= end && other.pts_ms < pts)
                .count();
            assert_eq!(earlier, 0, "a keyframe at {pts} was not the first one");
        }
    }

    #[test]
    fn next_placement_takes_the_first_carrier_at_all() {
        let carriers = carriers(60, 15);
        let plan = plan(Placement::Next, &[space(1)], &records(4), &carriers);
        for (pts, record) in placed(&plan, &carriers) {
            let end = pts + i64::from(record.end_off);
            assert!(
                record.start_off <= 0 && record.end_off <= 0,
                "a live span is behind"
            );
            let earlier = carriers
                .iter()
                .filter(|other| other.pts_ms >= end && other.pts_ms < pts)
                .count();
            assert_eq!(earlier, 0, "a carrier at {pts} was not the first one");
        }
    }

    #[test]
    fn the_offsets_put_the_span_back_where_it_was() {
        let carriers = carriers(60, 15);
        let wanted = records(4);
        for policy in [
            Placement::Keyframe,
            Placement::Next,
            Placement::Spread { budget_bytes: 64 },
        ] {
            let plan = plan(policy, &[space(1)], &wanted, &carriers);
            for (pts, record) in placed(&plan, &carriers) {
                let want = &wanted[usize::from(record.record_id)];
                assert_eq!(pts + i64::from(record.start_off), want.start_ms);
                assert_eq!(pts + i64::from(record.end_off), want.end_ms);
            }
        }
    }

    #[test]
    fn every_plane_of_every_record_goes_out_once() {
        let carriers = carriers(80, 15);
        let wanted = records(4);
        for policy in [
            Placement::Keyframe,
            Placement::Next,
            Placement::Spread { budget_bytes: 40 },
            Placement::Spread { budget_bytes: 24 },
        ] {
            let plan = plan(policy, &[space(1)], &wanted, &carriers);
            let mut assembler = crate::assemble::Assembler::default();
            for (messages, carrier) in plan.iter().zip(&carriers) {
                assembler.push_unit(carrier.pts_ms, &Unit::new(messages.clone()));
            }
            let read = assembler.records();
            assert_eq!(read.len(), wanted.len(), "{policy:?} lost a record");
            for record in read {
                assert_eq!(record.planes, Some(0xff), "{policy:?} lost a plane");
                let VectorBody::I8(planes) = &record.body else {
                    panic!("the layered encoding came back as something else");
                };
                assert_eq!(planes, &self::planes(16), "{policy:?} changed a vector");
                let want = &wanted[usize::from(record.record_id)];
                assert_eq!(
                    (record.start_ms, record.end_ms),
                    (want.start_ms, want.end_ms)
                );
            }
        }
    }

    #[test]
    fn a_budget_is_not_overrun() {
        let carriers = carriers(80, 15);
        let budget = 40usize;
        let plan = plan(
            Placement::Spread {
                budget_bytes: budget,
            },
            &[space(1)],
            &records(6),
            &carriers,
        );
        for messages in &plan {
            let vectors: usize = messages
                .iter()
                .filter(|message| !matches!(message, Message::Space(_)))
                .map(Message::encoded_len)
                .sum();
            assert!(vectors <= budget, "a carrier took {vectors} bytes");
        }
    }

    #[test]
    fn a_budget_counts_the_escapes_that_ride_with_plane_zero() {
        // Plane 0's message is bigger than the others by the escapes it
        // carries, and a budget has to know it: the planner asks the
        // message what it costs rather than assuming the planes are all
        // the same size.
        let mut peaked: Vec<f32> = (0..16).map(|i| (i as f32 * 0.7).sin() * 0.2).collect();
        peaked[2] = 9.0;
        peaked[9] = -6.0;
        let escaped = Planes::quantize(&peaked, 2).expect("quantized");
        assert_eq!(escaped.escapes().len(), 2);
        let budget = 24usize;
        let carriers = carriers(60, 15);
        let plan = plan(
            Placement::Spread {
                budget_bytes: budget,
            },
            &[space(1)],
            &[Pending::layered(1, 0, 0, 500, &escaped)],
            &carriers,
        );
        for messages in &plan {
            let vectors: usize = messages
                .iter()
                .filter(|message| !matches!(message, Message::Space(_)))
                .map(Message::encoded_len)
                .sum();
            assert!(vectors <= budget, "a carrier took {vectors} bytes");
        }

        let mut assembler = crate::assemble::Assembler::default();
        for (messages, carrier) in plan.iter().zip(&carriers) {
            assembler.push_unit(carrier.pts_ms, &Unit::new(messages.clone()));
        }
        let read = assembler.records();
        assert_eq!(read.len(), 1);
        let VectorBody::I8(merged) = &read[0].body else {
            panic!("the layered encoding came back as something else");
        };
        assert_eq!(merged, &escaped, "the escapes did not survive the budget");
    }

    #[test]
    fn a_message_too_big_for_a_carrier_is_cut_into_slices() {
        let carriers = carriers(40, 10);
        let mut big = Space::new(2, 64, Encoding::F32);
        big.model = "test:big".into();
        let body: Vec<u8> = (0..64u32).flat_map(|i| (i as f32).to_le_bytes()).collect();
        let record = Pending::whole(2, 1, 0, 500, body.clone());
        let plan = plan(
            Placement::Spread { budget_bytes: 64 },
            &[big.clone()],
            &[record],
            &carriers,
        );
        let slices = plan
            .iter()
            .flatten()
            .filter(|message| matches!(message, Message::Fragment(_)))
            .count();
        assert!(slices > 1, "a 256 byte body fitted a 64 byte carrier");

        let mut assembler = crate::assemble::Assembler::default();
        for (messages, carrier) in plan.iter().zip(&carriers) {
            assembler.push_unit(carrier.pts_ms, &Unit::new(messages.clone()));
        }
        let read = assembler.records();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].body.encode(), body);
        assert_eq!((read[0].start_ms, read[0].end_ms), (0, 500));
    }

    #[test]
    fn spaces_go_on_every_keyframe_from_the_first_one() {
        // Section 3: every keyframe carries every space, from the first
        // keyframe of the stream, whether or not a record rides there.
        // The first record here is five seconds in, and the first
        // packet still says what the file carries.
        let carriers = carriers(400, 30);
        let plan = plan(
            Placement::Keyframe,
            &[space(1), space(2)],
            &[Pending::layered(1, 0, 5000, 6000, &planes(16))],
            &carriers,
        );
        let has_spaces =
            |messages: &Vec<Message>| messages.iter().any(|m| matches!(m, Message::Space(_)));
        let space_carriers: Vec<i64> = plan
            .iter()
            .zip(&carriers)
            .filter(|(messages, _)| has_spaces(messages))
            .map(|(_, carrier)| carrier.pts_ms)
            .collect();
        // Every keyframe and no carrier that is not one, so a cut
        // beginning at any keyframe can read what it keeps and the
        // first packet of the file needs no second read.
        let wanted: Vec<i64> = carriers
            .iter()
            .filter(|carrier| carrier.keyframe)
            .map(|carrier| carrier.pts_ms)
            .collect();
        assert_eq!(space_carriers, wanted);
        assert_eq!(space_carriers[0], 0, "the first keyframe said nothing");
        // Both spaces, every time.
        for (messages, _) in plan.iter().zip(&carriers) {
            let spaces = messages
                .iter()
                .filter(|m| matches!(m, Message::Space(_)))
                .count();
            assert!(spaces == 0 || spaces == 2, "{spaces} spaces on one carrier");
        }
    }

    #[test]
    fn a_space_the_writer_learns_of_live_rides_its_own_first_carrier() {
        // Section 3's second sentence. A space that turned up after the
        // stream began has no keyframe behind it to have ridden, so it
        // waits for the first carrier written for it and joins the
        // keyframes after that.
        let carriers = carriers(120, 25);
        let mut planner = Planner::new(Placement::Next);
        planner.declare(space(1));

        let mut said: Vec<(i64, Vec<u8>)> = Vec::new();
        for (index, carrier) in carriers.iter().enumerate() {
            if index == 40 {
                // Two seconds in, a second space and a record in it.
                planner.declare(space(2));
                planner.submit(Pending::layered(2, 0, 1000, 1600, &planes(16)));
            }
            let ids: Vec<u8> = planner
                .carrier(*carrier)
                .messages
                .iter()
                .filter_map(|m| match m {
                    Message::Space(space) => Some(space.space_id),
                    _ => None,
                })
                .collect();
            if !ids.is_empty() {
                said.push((carrier.pts_ms, ids));
            }
        }

        // Space 1 was there from the start, so it rode every keyframe
        // including the first.
        assert_eq!(said[0], (0, vec![1]), "{said:?}");
        assert_eq!(said[1], (2500, vec![1]), "{said:?}");
        // Space 2 turned up mid-stream and rode the carrier its record
        // did, which is no keyframe.
        let (at, ids) = said
            .iter()
            .find(|(_, ids)| ids.contains(&2))
            .expect("space 2 was never said");
        assert_eq!(*ids, vec![2], "space 1 went out again off a keyframe");
        assert_eq!(*at, 4000, "space 2 did not ride its own first carrier");
        assert!(!carriers
            .iter()
            .any(|carrier| carrier.pts_ms == *at && carrier.keyframe));
        // And from the next keyframe on, both of them.
        let after: Vec<&(i64, Vec<u8>)> = said.iter().filter(|(pts, _)| *pts > *at).collect();
        assert!(!after.is_empty());
        for (pts, ids) in after {
            assert_eq!(*ids, vec![1, 2], "at {pts} ms");
        }
    }

    #[test]
    fn a_reader_joining_at_any_keyframe_has_the_spaces_for_what_follows() {
        // Every keyframe of the stream, as the place a cut or a segment
        // would begin: from there on, every record that goes out can be
        // read, because its space was declared at that keyframe.
        let carriers = carriers(120, 10);
        let wanted = records(4);
        let plan = plan(Placement::Next, &[space(1)], &wanted, &carriers);
        for (start, carrier) in carriers.iter().enumerate() {
            if !carrier.keyframe {
                continue;
            }
            let mut assembler = crate::assemble::Assembler::default();
            for (messages, carrier) in plan.iter().zip(&carriers).skip(start) {
                assembler.push_unit(carrier.pts_ms, &Unit::new(messages.clone()));
            }
            let read = assembler.records();
            let sent = plan
                .iter()
                .skip(start)
                .flatten()
                .filter(|m| matches!(m, Message::Vector(_)))
                .count();
            assert_eq!(
                read.len() * 8,
                sent,
                "a reader joining at {} ms could not read what followed",
                carrier.pts_ms
            );
        }
    }

    #[test]
    fn a_record_that_never_meets_a_keyframe_rides_the_last_one() {
        // Two keyframes and nothing after them that either record's
        // span reaches: section 7 puts both on the second keyframe,
        // with the end of the span ahead of the carrier, so a reader of
        // sync samples alone still has them.
        let carriers = carriers(20, 12); // keyframes at 0 and 1200 ms
        let plan = plan(Placement::Keyframe, &[space(1)], &records(2), &carriers);
        let vectors = plan
            .iter()
            .flatten()
            .filter(|m| matches!(m, Message::Vector(_)))
            .count();
        assert_eq!(vectors, 16, "eight planes of each of two records");
        for (index, messages) in plan.iter().enumerate() {
            let carried = messages
                .iter()
                .filter(|m| matches!(m, Message::Vector(_)))
                .count();
            if carried > 0 {
                assert!(
                    carriers[index].keyframe,
                    "a record rode carrier {index}, which is no keyframe"
                );
            }
        }
        let last_keyframe = carriers
            .iter()
            .rposition(|carrier| carrier.keyframe)
            .expect("a keyframe");
        assert!(
            last_keyframe < carriers.len() - 1,
            "the fixture ends on a keyframe, so nothing is being tested"
        );
        // The second record's span ends at 2000 ms, past every carrier,
        // so it rode the last keyframe looking forward.
        let forward = placed(&plan, &carriers)
            .into_iter()
            .filter(|(_, record)| record.end_off > 0)
            .count();
        assert_eq!(forward, 8, "the later record did not look forward");
        assert!(
            plan[last_keyframe]
                .iter()
                .any(|m| matches!(m, Message::Vector(_))),
            "the leftovers did not go on the last keyframe"
        );
    }

    #[test]
    fn a_keyframe_stays_open_until_the_next_one() {
        let mut planner = Planner::new(Placement::Keyframe);
        planner.declare(space(1));
        let mut opened = Vec::new();
        for (index, carrier) in carriers(40, 10).iter().enumerate() {
            if planner.carrier(*carrier).open {
                opened.push(index);
            }
        }
        assert_eq!(opened, vec![0, 10, 20, 30], "every keyframe and no other");

        // And a live policy holds nothing at all.
        let mut planner = Planner::new(Placement::Next);
        planner.declare(space(1));
        assert!(carriers(40, 10)
            .iter()
            .all(|carrier| !planner.carrier(*carrier).open));
    }

    #[test]
    fn a_span_too_far_from_its_carrier_is_dropped_not_wrapped() {
        let carriers = [Carrier {
            pts_ms: 0,
            keyframe: true,
        }];
        let mut planner = Planner::new(Placement::Next);
        planner.declare(space(1));
        // A span so far behind the carrier that no svarint of the
        // offsets could say where it was.
        planner.submit(Pending {
            space_id: 1,
            record_id: 1,
            start_ms: i64::from(i32::MIN) - 2000,
            end_ms: i64::from(i32::MIN) - 1000,
            available_ms: -1,
            bodies: vec![vec![1, 2, 3]],
        });
        let placed = planner.carrier(carriers[0]);
        assert!(
            !placed
                .messages
                .iter()
                .any(|m| matches!(m, Message::Vector(_))),
            "an offset that does not fit was written anyway"
        );
        assert!(!placed.open, "the live policies never look back");
        assert_eq!(planner.pending(), 0);
        assert_eq!(planner.skipped(), 1);
    }

    #[test]
    fn a_file_with_no_records_still_says_what_it_carries() {
        // A space and nothing in it: the keyframes still declare it, so
        // a reader of the first packet learns the file has a space and
        // no vectors rather than learning nothing.
        let carriers = carriers(10, 5);
        let plan = plan(Placement::Keyframe, &[space(1)], &[], &carriers);
        for (messages, carrier) in plan.iter().zip(&carriers) {
            assert!(
                !messages.iter().any(|m| matches!(m, Message::Vector(_))),
                "a record was written where there are none"
            );
            let spaces = messages
                .iter()
                .filter(|m| matches!(m, Message::Space(_)))
                .count();
            assert_eq!(
                spaces,
                usize::from(carrier.keyframe),
                "at {}",
                carrier.pts_ms
            );
        }
    }
}
