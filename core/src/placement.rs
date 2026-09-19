//! Section 7's placement policies, as a state machine with no I/O.
//!
//! A writer sees access units go by and records turn up, and has to
//! decide which carrier each record rides on, repeat the SPACE messages
//! often enough for a reader that joined late, and turn absolute spans
//! into offsets from whichever carrier it picked. That decision is the
//! whole of this module, so the packet filter that will do the weaving
//! inside a pipeline is left with nothing but moving bytes.
//!
//! Feed it [`Planner::carrier`] for every access unit in presentation
//! order and it hands back the messages for that access unit's unit.

use std::collections::{BTreeMap, VecDeque};

use crate::fragment::{fragment_with_first, slice_room};
use crate::message::{Message, Space, VectorRecord};
use crate::quant::Planes;
use crate::{Error, Result};

/// How often a writer repeats its SPACE messages once it has started.
pub const SPACE_REPEAT_MS: i64 = 10_000;

/// Where records go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Whole records ride on keyframes, each on the first keyframe at
    /// or after the end of its span. The policy for files.
    Keyframe,
    /// A record rides on the first carrier after it exists. The policy
    /// for live streams.
    Next,
    /// As `Next`, with a byte budget per carrier, filled with planes
    /// and fragments, most significant first.
    Spread { budget_bytes: usize },
}

/// Whether the stream being written is a file or a live one.
///
/// The difference is only how often SPACE messages repeat: a live or
/// segmented stream puts them on every keyframe, because a watcher can
/// join at any of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    File,
    Live,
}

/// One access unit, as much of it as placement cares about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carrier {
    pub pts_ms: i64,
    pub keyframe: bool,
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

/// Records and access units in, messages out.
#[derive(Clone, Debug)]
pub struct Planner {
    policy: Placement,
    mode: Mode,
    spaces: BTreeMap<u8, Space>,
    jobs: VecDeque<Job>,
    started: bool,
    last_space_ms: i64,
    /// The last keyframe seen, and the widest gap between two of them.
    /// Section 3 asks for a SPACE at least every ten seconds, and a
    /// writer can only put one on a keyframe, so it has to know how
    /// long the next one is likely to be away before it skips this one.
    last_keyframe_ms: Option<i64>,
    keyframe_gap_ms: i64,
    skipped: usize,
}

impl Planner {
    /// A planner for one policy.
    pub fn new(policy: Placement, mode: Mode) -> Self {
        Self {
            policy,
            mode,
            spaces: BTreeMap::new(),
            jobs: VecDeque::new(),
            started: false,
            last_space_ms: i64::MIN,
            last_keyframe_ms: None,
            keyframe_gap_ms: 0,
            skipped: 0,
        }
    }

    /// Declares a space. Its SPACE message goes out on the first
    /// carrier the planner writes to, and repeats from there.
    pub fn declare(&mut self, space: Space) {
        self.spaces.insert(space.space_id, space);
    }

    /// Hands the planner a record to place.
    pub fn submit(&mut self, record: Pending) {
        self.jobs.push_back(Job {
            bodies: record.bodies.iter().cloned().collect(),
            record,
            slices: VecDeque::new(),
        });
    }

    /// The messages for one access unit's unit, which may be empty.
    pub fn carrier(&mut self, carrier: Carrier) -> Vec<Message> {
        if carrier.keyframe {
            if let Some(previous) = self.last_keyframe_ms {
                self.keyframe_gap_ms = self.keyframe_gap_ms.max(carrier.pts_ms - previous);
            }
            self.last_keyframe_ms = Some(carrier.pts_ms);
        }
        let mut out = Vec::new();
        let mut room = match self.policy {
            Placement::Spread { budget_bytes } => budget_bytes,
            _ => usize::MAX,
        };

        let writing = self.has_work(carrier);
        if self.spaces_due(carrier, writing) {
            for space in self.spaces.values() {
                let message = Message::Space(space.clone());
                room = room.saturating_sub(message.encoded_len());
                out.push(message);
            }
            self.started = true;
            self.last_space_ms = carrier.pts_ms;
        }
        if !writing {
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
        if !out.is_empty() {
            self.started = true;
        }
        out
    }

    /// Everything still waiting, against one last carrier.
    ///
    /// A file writer calls this after its last access unit and puts
    /// what comes back on that access unit: with the `keyframe` policy
    /// a record whose span ends after the last keyframe has no carrier
    /// of its own, and the alternative is losing it.
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
        // A writer that never had room until now still has to say what
        // space these records are in.
        let mut out = Vec::new();
        if !self.started {
            for space in self.spaces.values() {
                out.push(Message::Space(space.clone()));
            }
            self.started = true;
            self.last_space_ms = carrier.pts_ms;
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

    /// Whether any record could go on this carrier.
    fn has_work(&self, carrier: Carrier) -> bool {
        self.jobs.iter().any(|job| self.eligible(job, carrier))
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

    /// Whether the SPACE messages go on this carrier.
    fn spaces_due(&self, carrier: Carrier, writing: bool) -> bool {
        if self.spaces.is_empty() {
            return false;
        }
        if !self.started {
            // The first carrier the writer writes to, and not before.
            return writing;
        }
        if !carrier.keyframe {
            return false;
        }
        match self.mode {
            Mode::Live => true,
            // Repeat once waiting for the keyframe after this one would
            // take the gap past ten seconds.
            Mode::File => {
                let due = self.last_space_ms.saturating_add(SPACE_REPEAT_MS);
                carrier.pts_ms.saturating_add(self.keyframe_gap_ms) >= due
            }
        }
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
    mode: Mode,
    spaces: &[Space],
    records: &[Pending],
    carriers: &[Carrier],
) -> Vec<Vec<Message>> {
    let mut planner = Planner::new(policy, mode);
    for space in spaces {
        planner.declare(space.clone());
    }
    let mut records = records.to_vec();
    records.sort_by_key(|record| (record.available_ms, record.space_id, record.record_id));
    let mut queued = records.into_iter().peekable();

    let mut out = Vec::with_capacity(carriers.len());
    for carrier in carriers {
        while queued
            .peek()
            .is_some_and(|record| record.available_ms <= carrier.pts_ms)
        {
            planner.submit(queued.next().expect("a record"));
        }
        out.push(planner.carrier(*carrier));
    }
    for record in queued {
        planner.submit(record);
    }
    if let Some(last) = carriers.last() {
        let left = planner.finish(*last);
        if let Some(messages) = out.last_mut() {
            messages.extend(left);
        }
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
        Planes::quantize(&vector).expect("quantized")
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
        let plan = plan(
            Placement::Keyframe,
            Mode::File,
            &[space(1)],
            &records(4),
            &carriers,
        );
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
        let plan = plan(
            Placement::Next,
            Mode::Live,
            &[space(1)],
            &records(4),
            &carriers,
        );
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
            let plan = plan(policy, Mode::File, &[space(1)], &wanted, &carriers);
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
            let plan = plan(policy, Mode::File, &[space(1)], &wanted, &carriers);
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
            Mode::File,
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
    fn a_message_too_big_for_a_carrier_is_cut_into_slices() {
        let carriers = carriers(40, 10);
        let mut big = Space::new(2, 64, Encoding::F32);
        big.model = "test:big".into();
        let body: Vec<u8> = (0..64u32).flat_map(|i| (i as f32).to_le_bytes()).collect();
        let record = Pending::whole(2, 1, 0, 500, body.clone());
        let plan = plan(
            Placement::Spread { budget_bytes: 64 },
            Mode::Live,
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
    fn spaces_go_on_the_first_carrier_written_to_and_repeat() {
        // Nothing to write until the first record's span ends, so the
        // spaces wait with it.
        let carriers = carriers(400, 30);
        let plan = plan(
            Placement::Keyframe,
            Mode::File,
            &[space(1), space(2)],
            &[Pending::layered(1, 0, 5000, 6000, &planes(16))],
            &carriers,
        );
        let space_carriers: Vec<i64> = plan
            .iter()
            .zip(&carriers)
            .filter(|(messages, _)| messages.iter().any(|m| matches!(m, Message::Space(_))))
            .map(|(_, carrier)| carrier.pts_ms)
            .collect();
        assert!(!space_carriers.is_empty());
        assert!(
            space_carriers[0] >= 6000,
            "spaces went out before any record"
        );
        for pair in space_carriers.windows(2) {
            assert!(
                pair[1] - pair[0] <= SPACE_REPEAT_MS,
                "spaces went {} ms without repeating",
                pair[1] - pair[0]
            );
        }
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
    fn a_live_writer_repeats_the_spaces_on_every_keyframe() {
        let carriers = carriers(120, 10);
        let plan = plan(
            Placement::Next,
            Mode::Live,
            &[space(1)],
            &[Pending::layered(1, 0, 0, 100, &planes(16))],
            &carriers,
        );
        let keyframes = carriers.iter().filter(|carrier| carrier.keyframe).count();
        let with_spaces = plan
            .iter()
            .filter(|messages| messages.iter().any(|m| matches!(m, Message::Space(_))))
            .count();
        // Every keyframe from the first written carrier on, which is
        // the second keyframe here.
        assert!(with_spaces >= keyframes - 1, "{with_spaces} of {keyframes}");
    }

    #[test]
    fn a_record_that_never_meets_a_keyframe_still_goes_out_at_the_end() {
        let carriers = carriers(10, 20); // one keyframe, at the start
        let plan = plan(
            Placement::Keyframe,
            Mode::File,
            &[space(1)],
            &records(2),
            &carriers,
        );
        let vectors = plan
            .iter()
            .flatten()
            .filter(|m| matches!(m, Message::Vector(_)))
            .count();
        assert_eq!(vectors, 16, "eight planes of each of two records");
        let last = plan.last().expect("a carrier");
        assert!(
            last.iter().any(|m| matches!(m, Message::Vector(_))),
            "the leftovers went on the last carrier"
        );
    }

    #[test]
    fn a_span_too_far_from_its_carrier_is_dropped_not_wrapped() {
        let carriers = [Carrier {
            pts_ms: 0,
            keyframe: true,
        }];
        let mut planner = Planner::new(Placement::Next, Mode::File);
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
        let messages = planner.carrier(carriers[0]);
        assert!(
            !messages.iter().any(|m| matches!(m, Message::Vector(_))),
            "an offset that does not fit was written anyway"
        );
        assert_eq!(planner.pending(), 0);
        assert_eq!(planner.skipped(), 1);
    }

    #[test]
    fn nothing_is_written_when_there_is_nothing_to_say() {
        let carriers = carriers(10, 5);
        let plan = plan(Placement::Keyframe, Mode::File, &[space(1)], &[], &carriers);
        assert!(plan.iter().all(|messages| messages.is_empty()));
    }
}
