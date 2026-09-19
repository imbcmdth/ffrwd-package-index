//! Rows in, messages on carriers out.
//!
//! Between a row arriving and a unit going into an access unit there is
//! one decision after another that has nothing to do with wasm, with
//! ffmpeg or with which codec is underneath: which space a row names,
//! what record id it takes, when the writer had it, which carrier the
//! policy puts it on, and what to say about the ones that never got
//! one. [`Weaver`] is all of that, over plain Rust types, so it is
//! tested natively and the module in `weave/` is left with moving
//! bytes.
//!
//! [`Reorder`] is the other half, and it is here for the same reason:
//! packets arrive in DECODE order and [`Planner`] wants carriers in
//! PRESENTATION order, so something has to hold a packet until its
//! place among its neighbours is settled. It holds anything, so a test
//! can drive it with a number where the module drives it with a packet.

use std::collections::{BTreeMap, VecDeque};

use ffrwd_index_core::message::{Message, Space};
use ffrwd_index_core::placement::{Carrier, Pending, Placement, Planner};
use ffrwd_index_core::RECORD_ID_WRAP;

use crate::json::{number, object, string, Json};
use crate::vector::{bodies, plane_numbers, read_values};

/// How many records may be waiting for a carrier at once.
///
/// A file hands over every row it has before the first packet moves, so
/// this is the ceiling on what a run holds, and it is what makes the
/// module's memory a function of the cap rather than of the input. A
/// row arriving past it is dropped and said so.
pub const MAX_PENDING_RECORDS: usize = 4096;

/// How many packets may be held while their presentation order settles.
///
/// A reorder depth is a handful of frames in anything an encoder
/// writes, so this is slack rather than a limit; a stream whose
/// timestamps never settle hits it and is forced on rather than
/// growing.
pub const MAX_HELD_PACKETS: usize = 256;

/// What a caller asks the weaver for.
#[derive(Clone, Debug)]
pub struct Config {
    /// The spaces, in declaration order: a name, and the SPACE message
    /// it stands for. A space's `space_id` is its index here.
    pub spaces: Vec<(String, Space)>,
    pub placement: Placement,
    /// How many of a vector's largest components an `i8` space sends
    /// exactly rather than quantized.
    pub escapes: usize,
    /// How many of the eight bit-planes an `i8` space sends at all.
    pub plane_cap: Option<u8>,
    pub max_pending_records: usize,
}

impl Config {
    /// The defaults, for a caller that names spaces and nothing else.
    pub fn new(spaces: Vec<(String, Space)>) -> Self {
        Self {
            spaces,
            placement: Placement::Keyframe,
            escapes: 2,
            plane_cap: None,
            max_pending_records: MAX_PENDING_RECORDS,
        }
    }
}

/// A record submitted and not yet started, kept so that one nobody
/// carried can be named rather than counted.
#[derive(Clone, Debug)]
struct Waiting {
    space: usize,
    start_ms: i64,
    end_ms: i64,
}

/// Rows in, messages out.
pub struct Weaver {
    config: Config,
    planner: Planner,
    /// The next record id of each space, counting up and wrapping.
    next_id: Vec<u32>,
    /// Every record submitted whose first message has not gone out.
    /// Bounded by `max_pending_records`, since a record leaves it as
    /// soon as anything of it is written.
    waiting: BTreeMap<(u8, u16), Waiting>,
    /// The newest presentation time any packet has carried, which is
    /// what "now" means to a record arriving live.
    now_ms: Option<i64>,
    records: usize,
    dropped: usize,
    bytes_added: usize,
}

/// What one carrier took.
#[derive(Clone, Debug, Default)]
pub struct Carried {
    /// The messages for this access unit's unit, empty when it carries
    /// nothing.
    pub messages: Vec<Message>,
    /// One row per record with something in those messages.
    pub rows: Vec<String>,
}

impl Weaver {
    pub fn new(config: Config) -> Self {
        let mut planner = Planner::new(config.placement);
        for (_, space) in &config.spaces {
            planner.declare(space.clone());
        }
        Self {
            next_id: vec![0; config.spaces.len()],
            planner,
            config,
            waiting: BTreeMap::new(),
            now_ms: None,
            records: 0,
            dropped: 0,
            bytes_added: 0,
        }
    }

    /// The spaces this weaver was opened with, in id order.
    pub fn spaces(&self) -> &[(String, Space)] {
        &self.config.spaces
    }

    /// What future records cost, changed between calls. The records
    /// already submitted keep the encoding they were built with: a
    /// record half written cannot change its mind about how many planes
    /// it has.
    pub fn set_escapes(&mut self, escapes: usize, plane_cap: Option<u8>) {
        self.config.escapes = escapes;
        self.config.plane_cap = plane_cap;
    }

    /// Notes that a packet at this presentation time has arrived, which
    /// is what tells a record whether it is behind the stream.
    pub fn seen(&mut self, pts_ms: i64) {
        self.now_ms = Some(match self.now_ms {
            Some(now) => now.max(pts_ms),
            None => pts_ms,
        });
    }

    /// One row of JSON. A row this weaver cannot use is reported and
    /// dropped: a writer upstream that sent one bad row among a
    /// thousand good ones should lose the one.
    pub fn row(&mut self, text: &str) -> Option<String> {
        match self.submit(text) {
            Ok(()) => None,
            Err(reason) => {
                self.dropped += 1;
                Some(dropped_row(&reason, text))
            }
        }
    }

    fn submit(&mut self, text: &str) -> Result<(), String> {
        let row = Json::parse(text.trim()).map_err(|err| format!("not one JSON object: {err}"))?;
        let index = self.space_of(&row)?;
        let (_, space) = &self.config.spaces[index];
        let values = read_values(row.get("vector"), space.dims)?;
        let start_ms = seconds_to_ms(&row, "start_t")?;
        let end_ms = seconds_to_ms(&row, "end_t")?;
        if end_ms < start_ms {
            return Err("a span that ends before it starts".into());
        }
        if self.planner.pending() >= self.config.max_pending_records {
            return Err(format!(
                "{} records are already waiting for a carrier",
                self.config.max_pending_records
            ));
        }
        let record_id = match row.get("record_id").and_then(Json::as_i64) {
            Some(id) => u16::try_from(id).map_err(|_| "a record_id outside 0 to 65535")?,
            None => {
                let id = self.next_id[index] % RECORD_ID_WRAP;
                self.next_id[index] = (self.next_id[index] + 1) % RECORD_ID_WRAP;
                id as u16
            }
        };
        // Only a budget splits a record across carriers, and section 5
        // would rather a writer with room sent every plane in one
        // message than pay for a header eight times.
        let bodies = bodies(
            space,
            &values,
            self.config.escapes,
            self.config.plane_cap,
            matches!(self.config.placement, Placement::Spread { .. }),
        )?;
        // A record exists when the writer hands it over, which for a
        // live feed is after its span ended and for a file is before
        // the stream even started. A row may say so itself, which is
        // what lets a file of rows describe a live run exactly.
        let available_ms = match row.get("available_t") {
            Some(_) => seconds_to_ms(&row, "available_t")?,
            None => self.now_ms.unwrap_or(i64::MIN),
        };
        let space_id = space.space_id;
        self.planner.submit(Pending {
            space_id,
            record_id,
            start_ms,
            end_ms,
            available_ms,
            bodies,
        });
        self.waiting.insert(
            (space_id, record_id),
            Waiting {
                space: index,
                start_ms,
                end_ms,
            },
        );
        Ok(())
    }

    /// Which space a row names. A run declaring one space lets a row
    /// leave the name out; a run declaring several does not, because
    /// guessing would put a vector in the wrong space silently.
    fn space_of(&self, row: &Json) -> Result<usize, String> {
        match row.get("space").and_then(Json::as_str) {
            Some(name) => self
                .config
                .spaces
                .iter()
                .position(|(declared, _)| declared == name)
                .ok_or_else(|| format!("no space is declared as '{name}'")),
            None if self.config.spaces.len() == 1 => Ok(0),
            None => Err("a row with no space, where several are declared".into()),
        }
    }

    /// The messages one access unit carries, and the rows saying so.
    pub fn carrier(&mut self, pts: i64, pts_ms: i64, keyframe: bool) -> Carried {
        let messages = self.planner.carrier(Carrier { pts_ms, keyframe });
        self.report(pts, pts_ms, messages)
    }

    /// Everything still waiting, against the last access unit of the
    /// stream. With the `keyframe` policy a record whose span ends
    /// after the last keyframe has no carrier of its own, and the
    /// alternative is losing it, so it rides the last access unit with
    /// the offsets that says.
    pub fn flush(&mut self, pts: i64, pts_ms: i64, keyframe: bool) -> Carried {
        let messages = self.planner.finish(Carrier { pts_ms, keyframe });
        self.report(pts, pts_ms, messages)
    }

    /// Turns one carrier's messages into the rows that describe them.
    fn report(&mut self, pts: i64, pts_ms: i64, messages: Vec<Message>) -> Carried {
        // One row per record with something on this carrier, and the
        // bytes of every message it put there.
        let mut order: Vec<(u8, u16)> = Vec::new();
        let mut bytes: BTreeMap<(u8, u16), usize> = BTreeMap::new();
        let mut planes: BTreeMap<(u8, u16), u8> = BTreeMap::new();
        for message in &messages {
            let key = match message {
                Message::Vector(record) => (record.space_id, record.record_id),
                Message::Fragment(slice) => (slice.space_id, slice.record_id),
                _ => continue,
            };
            if !bytes.contains_key(&key) {
                order.push(key);
            }
            *bytes.entry(key).or_default() += message.encoded_len();
            if let Message::Vector(record) = message {
                let space = self.space_at(key.0);
                if let Some(present) = present_planes(space, &record.body) {
                    *planes.entry(key).or_default() |= present;
                }
            }
        }

        let mut rows = Vec::with_capacity(order.len());
        for key in order {
            if self.waiting.remove(&key).is_some() {
                self.records += 1;
            }
            rows.push(woven_row(
                self.name_of(key.0),
                key,
                pts,
                pts_ms,
                &messages,
                bytes[&key],
                planes.get(&key).copied(),
            ));
        }
        Carried { messages, rows }
    }

    /// Adds what the codec's own framing cost on top of the messages:
    /// the NAL or OBU header, and whatever escaping it needed.
    pub fn note_bytes(&mut self, added: usize) {
        self.bytes_added += added;
    }

    /// One row per record nothing carried, and the run's own tally.
    ///
    /// A record is late when the stream ended before any carrier the
    /// policy would choose came along, or when its span turned out to
    /// be further from its carrier than an offset can say.
    pub fn trailing(&mut self) -> Vec<String> {
        let mut rows = Vec::with_capacity(self.waiting.len() + 1);
        let late = std::mem::take(&mut self.waiting);
        for ((_, record_id), waiting) in &late {
            rows.push(late_row(
                &self.config.spaces[waiting.space].0,
                *record_id,
                waiting.start_ms,
                waiting.end_ms,
            ));
        }
        rows.push(
            object(vec![
                ("event", string("summary")),
                ("records", number(self.records as f64)),
                ("late", number(late.len() as f64)),
                ("dropped", number(self.dropped as f64)),
                ("bytes_added", number(self.bytes_added as f64)),
                ("spaces", number(self.config.spaces.len() as f64)),
            ])
            .write(),
        );
        rows
    }

    fn space_at(&self, space_id: u8) -> Option<&Space> {
        self.config
            .spaces
            .iter()
            .find(|(_, space)| space.space_id == space_id)
            .map(|(_, space)| space)
    }

    fn name_of(&self, space_id: u8) -> &str {
        self.config
            .spaces
            .iter()
            .find(|(_, space)| space.space_id == space_id)
            .map(|(name, _)| name.as_str())
            .unwrap_or_default()
    }
}

/// Which planes a body holds, for an `i8` space and no other.
fn present_planes(space: Option<&Space>, body: &[u8]) -> Option<u8> {
    let space = space?;
    if space.encoding != ffrwd_index_core::message::Encoding::I8 {
        return None;
    }
    ffrwd_index_core::quant::Planes::decode(space.dims as usize, body)
        .ok()
        .map(|planes| planes.present())
}

fn woven_row(
    space: &str,
    key: (u8, u16),
    pts: i64,
    pts_ms: i64,
    messages: &[Message],
    bytes: usize,
    planes: Option<u8>,
) -> String {
    // The span is the carrier's time plus the offsets that went out,
    // which is what a reader will compute, so a row saying anything
    // else would be saying what was meant rather than what was written.
    //
    // A record is its SPACE and its id together: ids count per space, so
    // two spaces both writing their first record are both record 0.
    let record_id = key.1;
    let offsets = messages.iter().find_map(|message| match message {
        Message::Vector(record) if (record.space_id, record.record_id) == key => {
            Some((record.start_off, record.end_off))
        }
        _ => None,
    });
    let mut members = vec![
        ("event", string("woven")),
        ("space", string(space)),
        ("record_id", number(record_id)),
        ("carrier_pts", number(pts as f64)),
        ("carrier_t", seconds(pts_ms)),
    ];
    if let Some((start_off, end_off)) = offsets {
        members.push(("start_t", seconds(pts_ms + i64::from(start_off))));
        members.push(("end_t", seconds(pts_ms + i64::from(end_off))));
        members.push(("start_off_ms", number(start_off)));
        members.push(("end_off_ms", number(end_off)));
    }
    members.push(("bytes", number(bytes as f64)));
    if let Some(present) = planes {
        members.push((
            "planes",
            Json::Array(plane_numbers(present).into_iter().map(number).collect()),
        ));
    }
    object(members).write()
}

fn late_row(space: &str, record_id: u16, start_ms: i64, end_ms: i64) -> String {
    object(vec![
        ("event", string("late")),
        ("space", string(space)),
        ("record_id", number(record_id)),
        ("start_t", seconds(start_ms)),
        ("end_t", seconds(end_ms)),
        (
            "reason",
            string("no carrier this placement would choose was left"),
        ),
    ])
    .write()
}

/// A row nothing could be made of, with enough of it to find the writer
/// that wrote it.
fn dropped_row(reason: &str, text: &str) -> String {
    const SHOWN: usize = 200;
    let trimmed = text.trim();
    let shown: String = trimmed.chars().take(SHOWN).collect();
    object(vec![
        ("event", string("dropped")),
        ("reason", string(reason)),
        ("row", string(shown)),
    ])
    .write()
}

/// Milliseconds as the seconds a row spells times in.
fn seconds(ms: i64) -> Json {
    number(ms as f64 / 1000.0)
}

fn seconds_to_ms(row: &Json, field: &str) -> Result<i64, String> {
    let value = row
        .get(field)
        .and_then(Json::as_f64)
        .ok_or_else(|| format!("a row with no {field}"))?;
    if !value.is_finite() {
        return Err(format!("a {field} that is not a finite number"));
    }
    let ms = (value * 1000.0).round();
    if ms.abs() > i64::MAX as f64 {
        return Err(format!("a {field} outside any stream's clock"));
    }
    Ok(ms as i64)
}

/// Packets in decode order, carriers out in presentation order.
///
/// Two rules settle a packet's place, and either is enough.
///
/// The stream's DECODE DELAY is how many packets a decoder holds back
/// before the first picture leaves it, which is the same thing as how
/// far decode order and presentation order can differ: once that many
/// more packets have arrived after a held one, nothing still to come
/// can be shown before it. That rule works from the first packet, which
/// is what the head of a reordering stream needs, because the wire
/// settles no `dts` for exactly those packets.
///
/// After them, `dts` says it more tightly: it never decreases and no
/// packet is shown before it is decoded, so once a packet with
/// `dts = T` has arrived, nothing still to come can be presented before
/// `T`. Whichever rule fires first settles the packet.
///
/// The cap is the backstop under both, for a stream whose timestamps
/// say something neither rule can use.
pub struct Reorder<T> {
    slots: VecDeque<Slot<T>>,
    /// The floor every future packet's presentation time sits on.
    settled: i64,
    /// How far decode order and presentation order may differ, from the
    /// stream's own header.
    decode_delay: u64,
    /// How many packets have arrived, for the decode delay to count
    /// against.
    pushed: u64,
    /// No more packets are coming, so every held packet is settled.
    closed: bool,
    max_held: usize,
    forced: usize,
}

struct Slot<T> {
    item: T,
    pts: i64,
    /// Where this packet arrived in decode order.
    seq: u64,
    decided: bool,
}

impl<T> Reorder<T> {
    pub fn new(max_held: usize, decode_delay: u32) -> Self {
        Self {
            slots: VecDeque::new(),
            settled: i64::MIN,
            decode_delay: u64::from(decode_delay),
            pushed: 0,
            closed: false,
            max_held: max_held.max(1),
            forced: 0,
        }
    }

    /// One packet, in decode order.
    pub fn push(&mut self, item: T, pts: i64, dts: Option<i64>) {
        if let Some(dts) = dts {
            self.settled = self.settled.max(dts);
        }
        self.slots.push_back(Slot {
            item,
            pts,
            seq: self.pushed,
            decided: false,
        });
        self.pushed += 1;
    }

    /// Nothing more is coming: what is held is all there is.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// The next packet in presentation order whose place is settled,
    /// marked as decided. The caller writes into it and then releases
    /// it in decode order with [`Reorder::release`].
    pub fn settle(&mut self) -> Option<(i64, &mut T)> {
        let pick = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !slot.decided)
            .min_by_key(|(index, slot)| (slot.pts, *index))
            .map(|(index, _)| index)?;
        let slot = &self.slots[pick];
        // How many packets arrived after this one, which is what the
        // decode delay is counted against.
        let behind = self.pushed.saturating_sub(slot.seq + 1);
        let ready = self.closed || slot.pts <= self.settled || behind >= self.decode_delay;
        // At the cap, one packet at a time is forced on: the front slot
        // is what blocks the release, so forcing stops the moment it is
        // decided rather than emptying the whole hold and giving up on
        // the reordering that is still working.
        let blocked = !self.slots.front().expect("a slot is held").decided;
        let forced = !self.closed && blocked && self.slots.len() > self.max_held;
        if !ready && !forced {
            return None;
        }
        if !ready {
            self.forced += 1;
        }
        let slot = &mut self.slots[pick];
        slot.decided = true;
        Some((slot.pts, &mut slot.item))
    }

    /// Everything at the front that has been decided, in the order it
    /// arrived, which is the order it has to leave in.
    ///
    /// Nothing is held past its turn: the final call of an instance's
    /// life carries the last packets, so a record nobody carried has a
    /// real last carrier to ride and there is no reason to keep one out
    /// of the stream for the whole run.
    pub fn release(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while self.slots.front().is_some_and(|slot| slot.decided) {
            out.push(self.slots.pop_front().expect("a decided slot").item);
        }
        out
    }

    /// The held packet latest in presentation order, for a caller with
    /// a record nothing else will carry. None where nothing is held.
    pub fn last_held(&mut self) -> Option<(i64, &mut T)> {
        let pick = self
            .slots
            .iter()
            .enumerate()
            .max_by_key(|(index, slot)| (slot.pts, *index))
            .map(|(index, _)| index)?;
        let slot = &mut self.slots[pick];
        Some((slot.pts, &mut slot.item))
    }

    /// How many packets are held.
    pub fn held(&self) -> usize {
        self.slots.len()
    }

    /// How many held packets have yet to be settled, which is how a
    /// caller draining the hold knows which one is the last.
    pub fn undecided(&self) -> usize {
        self.slots.iter().filter(|slot| !slot.decided).count()
    }

    /// How many packets the cap settled before their order was.
    pub fn forced(&self) -> usize {
        self.forced
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ffrwd_index_core::message::Encoding;

    fn space(name: &str, id: u8, dims: u32) -> (String, Space) {
        let mut space = Space::new(id, dims, Encoding::I8);
        space.model = "test:model".into();
        (name.to_string(), space)
    }

    fn row(space: &str, start: f64, end: f64, dims: usize) -> String {
        let values: Vec<String> = (0..dims)
            .map(|i| format!("{}", (i as f32 * 0.3).sin()))
            .collect();
        format!(
            r#"{{"space":"{space}","start_t":{start},"end_t":{end},"vector":[{}]}}"#,
            values.join(",")
        )
    }

    fn parsed(rows: &[String]) -> Vec<Json> {
        rows.iter()
            .map(|row| Json::parse(row).expect("a row of JSON"))
            .collect()
    }

    #[test]
    fn a_file_puts_every_record_on_a_keyframe_and_says_so() {
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 16)]));
        // Every row before the first packet, which is the file case.
        for index in 0..4 {
            assert_eq!(
                weaver.row(&row("clip", index as f64, index as f64 + 1.0, 16)),
                None
            );
        }
        let mut rows = Vec::new();
        for frame in 0..150 {
            let pts_ms = frame * 40;
            weaver.seen(pts_ms);
            let carried = weaver.carrier(frame, pts_ms, frame % 25 == 0);
            if !carried.messages.is_empty() {
                assert!(
                    frame % 25 == 0,
                    "a record rode a carrier that is no keyframe"
                );
            }
            rows.extend(carried.rows);
        }
        let woven = parsed(&rows);
        assert_eq!(woven.len(), 4, "one row per record");
        for row in &woven {
            assert_eq!(row.get("event").and_then(Json::as_str), Some("woven"));
            assert_eq!(row.get("space").and_then(Json::as_str), Some("clip"));
            // Whole records: every plane on the one carrier.
            assert_eq!(
                row.get("planes")
                    .and_then(Json::as_array)
                    .map(<[Json]>::len),
                Some(8)
            );
            let end = row.get("end_t").and_then(Json::as_f64).expect("an end");
            let carrier = row
                .get("carrier_t")
                .and_then(Json::as_f64)
                .expect("a carrier");
            assert!(carrier >= end, "a record rode a carrier before its span");
        }
        let trailing = parsed(&weaver.trailing());
        assert_eq!(trailing.len(), 1, "nothing was late");
        assert_eq!(trailing[0].get("records").and_then(Json::as_i64), Some(4));
        assert_eq!(trailing[0].get("late").and_then(Json::as_i64), Some(0));
        assert_eq!(trailing[0].get("spaces").and_then(Json::as_i64), Some(1));
    }

    #[test]
    fn a_record_that_arrives_behind_the_stream_rides_ahead_with_negative_offsets() {
        let mut config = Config::new(vec![space("clip", 0, 16)]);
        config.placement = Placement::Next;
        let mut weaver = Weaver::new(config);
        let mut rows = Vec::new();
        for frame in 0..60i64 {
            let pts_ms = frame * 40;
            weaver.seen(pts_ms);
            // The vector for the second that just ended, handed over now.
            if frame % 25 == 24 {
                let start = (frame - 24) as f64 * 0.04;
                let end = (frame + 1) as f64 * 0.04;
                assert_eq!(weaver.row(&row("clip", start, end, 16)), None);
            }
            rows.extend(weaver.carrier(frame, pts_ms, frame % 25 == 0).rows);
        }
        let woven = parsed(&rows);
        assert!(!woven.is_empty(), "nothing was woven");
        for row in &woven {
            let start = row
                .get("start_off_ms")
                .and_then(Json::as_i64)
                .expect("a start offset");
            let end = row
                .get("end_off_ms")
                .and_then(Json::as_i64)
                .expect("an end offset");
            assert!(start < 0 && end <= 0, "a live record looked forward");
        }
    }

    #[test]
    fn every_kind_of_broken_row_is_reported_and_none_stops_the_rest() {
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 4), space("text", 1, 4)]));
        let broken = [
            (
                r#"{"space":"clip","start_t":0,"end_t":1,"vector":[1,2]}"#,
                "components",
            ),
            (
                r#"{"space":"nope","start_t":0,"end_t":1,"vector":[1,2,3,4]}"#,
                "no space is declared",
            ),
            (r#"{"start_t":0,"end_t":1,"vector":[1,2,3,4]}"#, "no space"),
            (
                r#"{"space":"clip","start_t":0,"end_t":1,"vector":["a","b","c","d"]}"#,
                "not a number",
            ),
            (r#"{"space":"clip","start_t":0,"end_t":1}"#, "no vector"),
            (
                r#"{"space":"clip","end_t":1,"vector":[1,2,3,4]}"#,
                "no start_t",
            ),
            (
                r#"{"space":"clip","start_t":5,"end_t":1,"vector":[1,2,3,4]}"#,
                "ends before",
            ),
            ("{not json", "not one JSON object"),
        ];
        for (text, wanted) in broken {
            let reported = weaver.row(text).expect("a dropped row");
            let row = Json::parse(&reported).expect("a row of JSON");
            assert_eq!(row.get("event").and_then(Json::as_str), Some("dropped"));
            assert!(
                row.get("reason")
                    .and_then(Json::as_str)
                    .is_some_and(|reason| reason.contains(wanted)),
                "{text} gave {reported}"
            );
        }
        // And a good row after every one of them still goes through.
        assert_eq!(weaver.row(&row("text", 0.0, 1.0, 4)), None);
        let mut rows = Vec::new();
        for frame in 0..60i64 {
            weaver.seen(frame * 40);
            rows.extend(weaver.carrier(frame, frame * 40, frame % 25 == 0).rows);
        }
        assert_eq!(parsed(&rows).len(), 1);
        let trailing = parsed(&weaver.trailing());
        let summary = trailing.last().expect("a summary");
        assert_eq!(summary.get("dropped").and_then(Json::as_i64), Some(8));
        assert_eq!(summary.get("records").and_then(Json::as_i64), Some(1));
    }

    #[test]
    fn a_row_may_say_when_the_writer_had_it() {
        // The live shape from a file: every vector existed three
        // seconds after the span it describes, and none may ride a
        // carrier before then.
        let mut config = Config::new(vec![space("clip", 0, 8)]);
        config.placement = Placement::Next;
        let mut weaver = Weaver::new(config);
        for index in 0..3 {
            let start = index as f64;
            let text = format!(
                r#"{{"space":"clip","start_t":{start},"end_t":{},"available_t":{},"vector":[1,2,3,4,5,6,7,8]}}"#,
                start + 1.0,
                start + 4.0
            );
            assert_eq!(weaver.row(&text), None);
        }
        let mut rows = Vec::new();
        for frame in 0..250i64 {
            weaver.seen(frame * 40);
            rows.extend(weaver.carrier(frame, frame * 40, frame % 25 == 0).rows);
        }
        let woven = parsed(&rows);
        assert_eq!(woven.len(), 3);
        for (index, row) in woven.iter().enumerate() {
            let carrier = row.get("carrier_t").and_then(Json::as_f64).expect("a time");
            assert!(
                carrier >= index as f64 + 4.0,
                "a record rode a carrier before its writer had it"
            );
            assert!(
                row.get("start_off_ms")
                    .and_then(Json::as_i64)
                    .expect("an offset")
                    < 0
            );
        }
    }

    #[test]
    fn two_spaces_on_one_carrier_each_report_their_own_span() {
        // Ids count per space, so two spaces writing their first record
        // are both record 0, and a row that looked a record up by its id
        // alone would hand one of them the other's span.
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 4), space("text", 1, 4)]));
        assert_eq!(
            weaver.row(r#"{"space":"clip","start_t":0,"end_t":1,"vector":[1,2,3,4]}"#),
            None
        );
        assert_eq!(
            weaver.row(r#"{"space":"text","start_t":2,"end_t":3,"vector":[4,3,2,1]}"#),
            None
        );
        let mut rows = Vec::new();
        for frame in 0..150i64 {
            weaver.seen(frame * 40);
            rows.extend(weaver.carrier(frame, frame * 40, frame % 100 == 0).rows);
        }
        let woven = parsed(&rows);
        assert_eq!(woven.len(), 2, "{rows:?}");
        // One keyframe at four seconds takes both of them.
        for row in &woven {
            assert_eq!(row.get("record_id").and_then(Json::as_i64), Some(0));
            assert_eq!(row.get("carrier_t").and_then(Json::as_f64), Some(4.0));
        }
        let clip = woven
            .iter()
            .find(|row| row.get("space").and_then(Json::as_str) == Some("clip"))
            .expect("the clip row");
        let text = woven
            .iter()
            .find(|row| row.get("space").and_then(Json::as_str) == Some("text"))
            .expect("the text row");
        assert_eq!(clip.get("start_t").and_then(Json::as_f64), Some(0.0));
        assert_eq!(clip.get("end_t").and_then(Json::as_f64), Some(1.0));
        assert_eq!(text.get("start_t").and_then(Json::as_f64), Some(2.0));
        assert_eq!(text.get("end_t").and_then(Json::as_f64), Some(3.0));
    }

    #[test]
    fn a_keyframe_record_that_arrives_too_late_rides_the_next_keyframe_anyway() {
        // The keyframe a record's span ends before has already gone by
        // when the record turns up. Section 4 has an answer for that
        // and it is the only one that keeps the record: the next
        // keyframe, with the offsets that says, both of them negative.
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 8)]));
        let mut rows = Vec::new();
        for frame in 0..150i64 {
            let pts_ms = frame * 40;
            weaver.seen(pts_ms);
            // Just past three seconds, a vector for the first half
            // second: two keyframes too late for the one it should have
            // had, and past the one at three seconds as well.
            if frame == 76 {
                assert_eq!(
                    weaver.row(
                        r#"{"space":"clip","start_t":0,"end_t":0.5,"vector":[1,2,3,4,5,6,7,8]}"#
                    ),
                    None
                );
            }
            rows.extend(weaver.carrier(frame, pts_ms, frame % 25 == 0).rows);
        }
        let woven = parsed(&rows);
        assert_eq!(woven.len(), 1, "the record was lost: {rows:?}");
        let carrier = woven[0]
            .get("carrier_t")
            .and_then(Json::as_f64)
            .expect("a time");
        assert_eq!(carrier, 4.0, "the first keyframe after it arrived");
        assert_eq!(
            woven[0].get("start_off_ms").and_then(Json::as_i64),
            Some(-4000)
        );
        assert_eq!(
            woven[0].get("end_off_ms").and_then(Json::as_i64),
            Some(-3500)
        );
        let trailing = parsed(&weaver.trailing());
        assert_eq!(
            trailing.last().unwrap().get("late").and_then(Json::as_i64),
            Some(0)
        );
    }

    #[test]
    fn a_span_no_offset_can_reach_is_late_rather_than_wrong() {
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 4)]));
        // Four hundred days from the stream, which no svarint of
        // milliseconds from a carrier can say.
        assert_eq!(
            weaver
                .row(r#"{"space":"clip","start_t":34560000,"end_t":34560001,"vector":[1,2,3,4]}"#),
            None
        );
        for frame in 0..30i64 {
            weaver.seen(frame * 40);
            weaver.carrier(frame, frame * 40, frame == 0);
        }
        weaver.flush(29, 29 * 40, false);
        let trailing = parsed(&weaver.trailing());
        assert_eq!(trailing.len(), 2, "one late row and the summary");
        assert_eq!(
            trailing[0].get("event").and_then(Json::as_str),
            Some("late")
        );
        assert_eq!(
            trailing[0].get("space").and_then(Json::as_str),
            Some("clip")
        );
        assert_eq!(trailing[1].get("late").and_then(Json::as_i64), Some(1));
        assert_eq!(trailing[1].get("records").and_then(Json::as_i64), Some(0));
    }

    #[test]
    fn the_pending_cap_drops_rows_rather_than_growing() {
        let mut config = Config::new(vec![space("clip", 0, 4)]);
        config.max_pending_records = 3;
        let mut weaver = Weaver::new(config);
        for index in 0..3 {
            assert_eq!(
                weaver.row(&row("clip", index as f64, index as f64 + 1.0, 4)),
                None
            );
        }
        let refused = weaver
            .row(&row("clip", 9.0, 10.0, 4))
            .expect("a dropped row");
        assert!(refused.contains("already waiting"), "{refused}");
    }

    #[test]
    fn a_budget_spreads_one_record_over_several_carriers() {
        let mut config = Config::new(vec![space("clip", 0, 64)]);
        config.placement = Placement::Spread { budget_bytes: 32 };
        let mut weaver = Weaver::new(config);
        assert_eq!(weaver.row(&row("clip", 0.0, 1.0, 64)), None);
        let mut rows = Vec::new();
        let mut carriers = 0;
        for frame in 0..80i64 {
            weaver.seen(frame * 40);
            let carried = weaver.carrier(frame, frame * 40, frame % 25 == 0);
            if carried
                .messages
                .iter()
                .any(|m| !matches!(m, Message::Space(_)))
            {
                carriers += 1;
            }
            rows.extend(carried.rows);
        }
        assert!(carriers > 1, "a 64-dim record fitted one 32-byte carrier");
        let woven = parsed(&rows);
        assert_eq!(woven.len(), carriers, "one row per carrier the record used");
        // The first row carries the sign plane, which is what a reader
        // needs before anything else is worth having.
        let first = woven[0]
            .get("planes")
            .and_then(Json::as_array)
            .expect("planes");
        assert_eq!(first[0].as_i64(), Some(0));
        // And the record counts once however many carriers it took.
        let trailing = parsed(&weaver.trailing());
        assert_eq!(
            trailing
                .last()
                .unwrap()
                .get("records")
                .and_then(Json::as_i64),
            Some(1)
        );
    }

    #[test]
    fn a_plane_cap_shortens_every_record() {
        let mut config = Config::new(vec![space("clip", 0, 16)]);
        config.plane_cap = Some(4);
        let mut weaver = Weaver::new(config);
        assert_eq!(weaver.row(&row("clip", 0.0, 0.5, 16)), None);
        let mut rows = Vec::new();
        for frame in 0..30i64 {
            weaver.seen(frame * 40);
            rows.extend(weaver.carrier(frame, frame * 40, frame % 25 == 0).rows);
        }
        let woven = parsed(&rows);
        assert_eq!(woven.len(), 1);
        let planes: Vec<i64> = woven[0]
            .get("planes")
            .and_then(Json::as_array)
            .expect("planes")
            .iter()
            .filter_map(Json::as_i64)
            .collect();
        assert_eq!(planes, vec![0, 1, 2, 3]);
    }

    #[test]
    fn base64_and_an_array_are_the_same_row() {
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 2)]));
        // 1.0 and -2.0 as little-endian binary32.
        assert_eq!(
            weaver.row(r#"{"space":"clip","start_t":0,"end_t":1,"vector":"AACAPwAAAMA="}"#),
            None
        );
        assert_eq!(
            weaver.row(r#"{"space":"clip","start_t":0,"end_t":1,"vector":[1.0,-2.0]}"#),
            None
        );
        let mut carried = Vec::new();
        for frame in 0..60i64 {
            weaver.seen(frame * 40);
            carried.extend(weaver.carrier(frame, frame * 40, frame % 20 == 0).messages);
        }
        let vectors: Vec<&Message> = carried
            .iter()
            .filter(|m| matches!(m, Message::Vector(_)))
            .collect();
        assert_eq!(vectors.len(), 2);
        let Message::Vector(first) = vectors[0] else {
            unreachable!()
        };
        let Message::Vector(second) = vectors[1] else {
            unreachable!()
        };
        assert_eq!(first.body, second.body, "two spellings, one vector");
    }

    // ------------------------------------------------------------ //
    // Reorder.
    // ------------------------------------------------------------ //

    /// How deep the streams below reorder, and how many of their first
    /// packets the wire therefore settles no `dts` for.
    const DELAY: u32 = 2;

    /// One IBBP stream as a wire hands it over: (pts, dts), in decode
    /// order, with no dts on the first packets.
    fn ibbp(frames: usize) -> Vec<(i64, Option<i64>)> {
        // I0 P3 B1 B2 P6 B4 B5 ..., decoded two frames ahead of display.
        let mut pts = vec![0i64];
        let mut anchor = 3i64;
        while pts.len() < frames {
            pts.push(anchor);
            for offset in 1..3 {
                if pts.len() >= frames {
                    break;
                }
                pts.push(anchor - 3 + offset);
            }
            anchor += 3;
        }
        // dts counts up from zero and lags the picture by the delay, so
        // the first `DELAY` packets have none: the wire has not settled
        // them, which is exactly where the delay rule earns its keep.
        pts.iter()
            .enumerate()
            .map(|(index, pts)| {
                let dts = index as i64 - i64::from(DELAY);
                (*pts, (dts >= 0).then_some(dts))
            })
            .collect()
    }

    /// Drives a whole stream through a hold and answers what settled, in
    /// the order it settled, and what left, in the order it left.
    fn through(
        reorder: &mut Reorder<usize>,
        frames: &[(i64, Option<i64>)],
    ) -> (Vec<i64>, Vec<i64>) {
        let mut settled = Vec::new();
        let mut left = Vec::new();
        let mut most = 0usize;
        for (index, (pts, dts)) in frames.iter().enumerate() {
            reorder.push(index, *pts, *dts);
            while let Some((pts, _)) = reorder.settle() {
                settled.push(pts);
            }
            left.extend(reorder.release().into_iter().map(|index| index as i64));
            most = most.max(reorder.held());
        }
        reorder.close();
        while let Some((pts, _)) = reorder.settle() {
            settled.push(pts);
        }
        left.extend(reorder.release().into_iter().map(|index| index as i64));
        // A packet settles once `DELAY` more have arrived after it, so
        // that many are undecided at worst, plus the one at the front
        // whose turn has not come and the one that just arrived.
        assert!(
            most <= usize::try_from(DELAY).expect("a small delay") + 2,
            "{most} packets held for a reorder of {DELAY}"
        );
        (settled, left)
    }

    #[test]
    fn a_reordered_stream_settles_in_presentation_order_and_leaves_in_decode_order() {
        let frames = ibbp(31);
        let mut reorder = Reorder::new(MAX_HELD_PACKETS, DELAY);
        let (settled, left) = through(&mut reorder, &frames);

        let mut wanted: Vec<i64> = frames.iter().map(|(pts, _)| *pts).collect();
        wanted.sort_unstable();
        assert_eq!(settled, wanted, "carriers did not settle in pts order");
        assert_eq!(
            left,
            (0..frames.len() as i64).collect::<Vec<i64>>(),
            "packets did not leave in the order they arrived"
        );
        assert_eq!(reorder.held(), 0, "a packet was held past the end");
        assert_eq!(reorder.forced(), 0, "the cap was reached on a sane stream");
    }

    #[test]
    fn the_decode_delay_settles_the_head_where_no_dts_does() {
        // The same stream with no dts at all, which is the head of it
        // for the whole run. Nothing but the delay can settle a packet,
        // and it settles every one of them in the right order without
        // the cap ever coming into it.
        let frames: Vec<(i64, Option<i64>)> =
            ibbp(31).into_iter().map(|(pts, _)| (pts, None)).collect();
        let mut reorder = Reorder::new(MAX_HELD_PACKETS, DELAY);
        let (settled, left) = through(&mut reorder, &frames);
        let mut wanted: Vec<i64> = frames.iter().map(|(pts, _)| *pts).collect();
        wanted.sort_unstable();
        assert_eq!(settled, wanted, "the delay did not settle in pts order");
        assert_eq!(left, (0..frames.len() as i64).collect::<Vec<i64>>());
        assert_eq!(reorder.forced(), 0, "the cap settled what the delay should");
    }

    #[test]
    fn a_stream_that_does_not_reorder_settles_every_packet_at_once() {
        let frames: Vec<(i64, Option<i64>)> = (0..12i64).map(|index| (index, None)).collect();
        let mut reorder = Reorder::new(MAX_HELD_PACKETS, 0);
        let mut held = Vec::new();
        for (index, (pts, dts)) in frames.iter().enumerate() {
            reorder.push(index, *pts, *dts);
            while reorder.settle().is_some() {}
            reorder.release();
            held.push(reorder.held());
        }
        assert!(held.iter().all(|count| *count == 0), "{held:?}");
    }

    #[test]
    fn a_stream_that_never_settles_is_forced_on_at_the_cap() {
        // Every packet's pts ahead of every dts, and a reorder depth
        // deeper than the stream is long: nothing would ever settle of
        // itself, and the cap is what stops the hold growing.
        let mut reorder = Reorder::new(4, 1000);
        for index in 0..20i64 {
            reorder.push(index, 1_000_000 + index, Some(index));
            while reorder.settle().is_some() {}
            reorder.release();
            assert!(reorder.held() <= 5, "the cap did not bound the hold");
        }
        assert!(reorder.forced() > 0, "nothing was forced");
    }

    #[test]
    fn a_stream_whose_header_says_nothing_true_still_drains() {
        // No dts on the wire and a declared depth nothing reaches: the
        // cap is all that is left, and every packet still leaves exactly
        // once and in the order it arrived.
        let mut reorder = Reorder::new(4, 1000);
        let mut left = Vec::new();
        for index in 0..10i64 {
            reorder.push(index, index, None);
            while reorder.settle().is_some() {}
            left.extend(reorder.release());
            assert!(reorder.held() <= 5, "the cap did not bound the hold");
        }
        reorder.close();
        while reorder.settle().is_some() {}
        left.extend(reorder.release());
        assert_eq!(reorder.held(), 0);
        assert_eq!(left, (0..10i64).collect::<Vec<i64>>());
    }
}
