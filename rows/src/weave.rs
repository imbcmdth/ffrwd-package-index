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

use ffrwd_index_core::message::{Message, Space, VectorRecord};
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

/// The most a VECTOR message costs on top of its body: the type byte,
/// the message length, the space id, the record id and the two offsets,
/// each varint at its widest.
const VECTOR_OVERHEAD: usize = 1 + 3 + 1 + 3 + 5 + 5;

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
    overran: usize,
}

/// What one carrier took.
#[derive(Clone, Debug, Default)]
pub struct Carried {
    /// The messages for this access unit's unit, empty when it carries
    /// nothing.
    pub messages: Vec<Message>,
    /// One row per record with something in those messages.
    pub rows: Vec<String>,
    /// Whether this access unit may still be asked to take more, which
    /// under the `keyframe` policy every keyframe is until the next one
    /// goes by: see `core::placement::Placed::open`. A writer that streams holds it,
    /// and every packet behind it, until it closes.
    pub open: bool,
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
            overran: 0,
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
        self.row_from(None, text)
    }

    /// One row of JSON that arrived on the input named `port`, which
    /// names the space of a row that leaves its own out.
    pub fn row_on(&mut self, port: &str, text: &str) -> Option<String> {
        self.row_from(Some(port), text)
    }

    fn row_from(&mut self, port: Option<&str>, text: &str) -> Option<String> {
        match self.submit(port, text) {
            Ok(()) => None,
            Err(reason) => {
                self.dropped += 1;
                Some(dropped_row(&reason, text))
            }
        }
    }

    fn submit(&mut self, port: Option<&str>, text: &str) -> Result<(), String> {
        let row = Json::parse(text.trim()).map_err(|err| format!("not one JSON object: {err}"))?;
        let index = self.space_of(&row, port)?;
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
        let mut bodies = bodies(
            space,
            &values,
            self.config.escapes,
            self.config.plane_cap,
            matches!(self.config.placement, Placement::Spread { .. }),
        )?;
        if let Placement::Spread { budget_bytes } = self.config.placement {
            // A budget too small for even one plane's message gets the
            // record as ONE value, which the planner then cuts into
            // slices. Splitting it into planes first would buy nothing
            // (every plane would be cut anyway) and would cost the
            // reader dearly: a FRAGMENT names its record and not which
            // of the record's values it slices, so planes 1 to 7, whose
            // messages are all the same length, would be
            // indistinguishable from one another. That only shows up
            // where slices reach a reader out of presentation order,
            // which is every elementary stream with B-frames in it.
            let largest = bodies.iter().map(Vec::len).max().unwrap_or(0);
            if largest + VECTOR_OVERHEAD > budget_bytes {
                bodies = self::bodies(
                    space,
                    &values,
                    self.config.escapes,
                    self.config.plane_cap,
                    false,
                )?;
            }
        }
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

    /// Which space a row names, in the order the answers are looked for.
    ///
    /// A row's own `space` field settles it, and a name nothing declares
    /// is an error rather than a fallback: a writer that named a space
    /// meant that one.
    ///
    /// Failing that, the input it arrived on. The module has an input
    /// per declared space, named for it, so a producer whose rows know
    /// nothing of this format is in the space the query bound it to.
    ///
    /// Failing both, a run declaring one space lets a row leave the name
    /// out. A run declaring several does not, because guessing would put
    /// a vector in the wrong space silently.
    fn space_of(&self, row: &Json, port: Option<&str>) -> Result<usize, String> {
        if let Some(name) = row.get("space").and_then(Json::as_str) {
            return self
                .index_of(name)
                .ok_or_else(|| format!("no space is declared as '{name}'"));
        }
        if let Some(index) = port.and_then(|name| self.index_of(name)) {
            return Ok(index);
        }
        if self.config.spaces.len() == 1 {
            return Ok(0);
        }
        Err(match port {
            Some(name) => format!(
                "a row with no space, arriving on '{name}', which names none of the declared \
                 spaces: {}",
                self.declared()
            ),
            None => format!(
                "a row with no space, where several are declared: {}",
                self.declared()
            ),
        })
    }

    /// Where a name sits among the declared spaces.
    fn index_of(&self, name: &str) -> Option<usize> {
        self.config
            .spaces
            .iter()
            .position(|(declared, _)| declared == name)
    }

    /// The declared space names, for a message that has to say what the
    /// row could have been.
    fn declared(&self) -> String {
        self.config
            .spaces
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The messages one access unit carries, and the rows saying so.
    pub fn carrier(&mut self, pts: i64, pts_ms: i64, keyframe: bool) -> Carried {
        let placed = self.planner.carrier(Carrier { pts_ms, keyframe });
        let mut carried = self.report(pts, pts_ms, placed.messages);
        carried.open = placed.open;
        carried
    }

    /// Everything still waiting, against the access unit the caller
    /// held open.
    ///
    /// Under `keyframe` that is the last KEYFRAME, and the records
    /// riding it have a positive `end_off` where their span ends after
    /// it: section 7 keeps every record on a sync sample so that a
    /// reader of sync samples alone has the file. Under `next` and
    /// `spread` nothing is held and this is the last access unit.
    ///
    /// A caller with nothing open does not call this: there is no
    /// access unit a keyframe reader would visit to put them on, and
    /// [`Weaver::trailing`] reports them late instead.
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
        Carried {
            messages,
            rows,
            open: false,
        }
    }

    /// Adds what the codec's own framing cost on top of the messages:
    /// the NAL or OBU header, and whatever escaping it needed.
    pub fn note_bytes(&mut self, added: usize) {
        self.bytes_added += added;
    }

    /// Notes one access unit that went out before it could be asked
    /// for more, because the writer's hold reached its bound. Under
    /// `keyframe` that is a keyframe a record might still have ridden,
    /// and the record falls to the next keyframe or is reported late.
    pub fn note_overrun(&mut self) {
        self.overran += 1;
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
                ("overran", number(self.overran as f64)),
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
        // A budget too small for one message cuts it into slices, and
        // the offsets are inside the value being cut rather than on the
        // slices. They are at the front of it, so the slice that starts
        // at zero still has them, and that is the record's first row.
        Message::Fragment(slice)
            if (slice.space_id, slice.record_id) == key && slice.offset == 0 =>
        {
            VectorRecord::decode(&slice.bytes)
                .ok()
                .map(|record| (record.start_off, record.end_off))
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
///
/// A [`Reorder::barrier`] holds the release back beyond what the
/// reordering needs: a writer whose placement may still add to an
/// access unit cannot let that access unit, or anything behind it,
/// leave. That is what section 7's last keyframe asks of a writer.
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
    /// Nothing from this arrival onwards leaves, whatever it was
    /// decided.
    barrier: Option<u64>,
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

/// One packet whose place in presentation order has just been settled.
pub struct Settled<'a, T> {
    /// Its presentation timestamp, in whatever unit the caller pushed.
    pub pts: i64,
    /// Where it arrived in decode order, which is what a barrier is
    /// set against.
    pub seq: u64,
    pub item: &'a mut T,
}

impl<T> Reorder<T> {
    pub fn new(max_held: usize, decode_delay: u32) -> Self {
        Self {
            slots: VecDeque::new(),
            settled: i64::MIN,
            decode_delay: u64::from(decode_delay),
            pushed: 0,
            barrier: None,
            closed: false,
            max_held: max_held.max(1),
            forced: 0,
        }
    }

    /// Holds the packet most recently settled, and everything behind
    /// it, out of the release until the barrier moves or lifts.
    pub fn hold_from(&mut self, seq: u64) {
        self.barrier = Some(seq);
    }

    /// Lets everything go again.
    pub fn lift(&mut self) {
        self.barrier = None;
    }

    /// Where the barrier stands, if anywhere.
    pub fn barrier(&self) -> Option<u64> {
        self.barrier
    }

    /// How many packets are held behind the barrier, and how many bytes
    /// they measure by the caller's own reckoning.
    pub fn behind_barrier(&self, size: impl Fn(&T) -> usize) -> (usize, usize) {
        let Some(barrier) = self.barrier else {
            return (0, 0);
        };
        self.slots
            .iter()
            .filter(|slot| slot.seq >= barrier)
            .fold((0, 0), |(count, bytes), slot| {
                (count + 1, bytes + size(&slot.item))
            })
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
    pub fn settle(&mut self) -> Option<Settled<'_, T>> {
        let pick = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| !slot.decided)
            .min_by_key(|(index, slot)| (slot.pts, *index))
            .map(|(index, _)| index)?;
        let undecided = self.slots.iter().filter(|slot| !slot.decided).count();
        let slot = &self.slots[pick];
        // How many packets arrived after this one, which is what the
        // decode delay is counted against.
        let behind = self.pushed.saturating_sub(slot.seq + 1);
        let ready = self.closed || slot.pts <= self.settled || behind >= self.decode_delay;
        // At the cap, one packet at a time is forced on: the count is
        // of what is UNDECIDED, since a barrier may be holding any
        // number of decided packets and the reordering is not what put
        // them there.
        let forced = !self.closed && undecided > self.max_held;
        if !ready && !forced {
            return None;
        }
        if !ready {
            self.forced += 1;
        }
        let slot = &mut self.slots[pick];
        slot.decided = true;
        Some(Settled {
            pts: slot.pts,
            seq: slot.seq,
            item: &mut slot.item,
        })
    }

    /// Everything at the front that has been decided and is not behind
    /// the barrier, in the order it arrived, which is the order it has
    /// to leave in.
    ///
    /// Nothing is held past its turn otherwise: the final call of an
    /// instance's life carries the last packets, so a record nobody
    /// carried has a real last carrier to ride and there is no reason
    /// to keep one out of the stream for the whole run.
    pub fn release(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while self
            .slots
            .front()
            .is_some_and(|slot| slot.decided && !self.barrier.is_some_and(|at| slot.seq >= at))
        {
            out.push(self.slots.pop_front().expect("a decided slot").item);
        }
        out
    }

    /// One held packet by the arrival it was settled at.
    pub fn at(&mut self, seq: u64) -> Option<&mut T> {
        self.slots
            .iter_mut()
            .find(|slot| slot.seq == seq)
            .map(|slot| &mut slot.item)
    }

    /// The held packet latest in presentation order, for a caller with
    /// a record nothing else will carry. None where nothing is held.
    pub fn last_held(&mut self) -> Option<(i64, u64, &mut T)> {
        let pick = self
            .slots
            .iter()
            .enumerate()
            .max_by_key(|(index, slot)| (slot.pts, *index))
            .map(|(index, _)| index)?;
        let slot = &mut self.slots[pick];
        Some((slot.pts, slot.seq, &mut slot.item))
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
    use ffrwd_index_core::message::{Encoding, Unit};

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

    /// The rows of one carrier, for a weaver that has taken its rows.
    fn drained(weaver: &mut Weaver) -> Vec<Json> {
        let mut rows = Vec::new();
        for frame in 0..150i64 {
            weaver.seen(frame * 40);
            rows.extend(weaver.carrier(frame, frame * 40, frame % 100 == 0).rows);
        }
        parsed(&rows)
    }

    #[test]
    fn the_input_a_row_arrived_on_names_the_space_it_left_out() {
        // The producer's rows are spans and vectors and nothing else:
        // which space they are in is the input the query bound them to.
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 4), space("text", 1, 4)]));
        assert_eq!(
            weaver.row_on("text", r#"{"start_t":0,"end_t":1,"vector":[1,2,3,4]}"#),
            None
        );
        let woven = drained(&mut weaver);
        assert_eq!(woven.len(), 1, "{woven:?}");
        assert_eq!(woven[0].get("space").and_then(Json::as_str), Some("text"));
    }

    #[test]
    fn a_rows_space_outranks_the_input_it_arrived_on() {
        // An input a query bound for one space carrying rows that name
        // another is the writer's business, not the host's: the row wins,
        // because it is the one that knows.
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 4), space("text", 1, 4)]));
        assert_eq!(
            weaver.row_on(
                "text",
                r#"{"space":"clip","start_t":0,"end_t":1,"vector":[1,2,3,4]}"#
            ),
            None
        );
        let woven = drained(&mut weaver);
        assert_eq!(woven.len(), 1, "{woven:?}");
        assert_eq!(woven[0].get("space").and_then(Json::as_str), Some("clip"));
    }

    #[test]
    fn an_input_naming_no_space_falls_to_the_only_one_declared() {
        // One space is no guess: a run with one space has nowhere else to
        // put the row.
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 4)]));
        assert_eq!(
            weaver.row_on("shots", r#"{"start_t":0,"end_t":1,"vector":[1,2,3,4]}"#),
            None
        );
        let woven = drained(&mut weaver);
        assert_eq!(woven.len(), 1, "{woven:?}");
        assert_eq!(woven[0].get("space").and_then(Json::as_str), Some("clip"));
    }

    #[test]
    fn an_input_naming_no_space_among_several_is_dropped_saying_both() {
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 4), space("text", 1, 4)]));
        let reported = weaver
            .row_on("shots", r#"{"start_t":0,"end_t":1,"vector":[1,2,3,4]}"#)
            .expect("a dropped row");
        let row = Json::parse(&reported).expect("a row of JSON");
        assert_eq!(row.get("event").and_then(Json::as_str), Some("dropped"));
        let reason = row
            .get("reason")
            .and_then(Json::as_str)
            .expect("a reason")
            .to_string();
        assert!(reason.contains("'shots'"), "{reason}");
        assert!(
            reason.contains("clip") && reason.contains("text"),
            "{reason}"
        );
        assert!(drained(&mut weaver).is_empty(), "the row was woven anyway");
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
    fn a_record_past_the_last_keyframe_rides_that_keyframe_looking_forward() {
        // Section 7: the keyframes of a file hold all of its records,
        // so a record whose span ends after the last keyframe rides
        // that keyframe with a positive end_off rather than riding the
        // last access unit, which a reader of sync samples alone would
        // never open.
        let mut weaver = Weaver::new(Config::new(vec![space("clip", 0, 8)]));
        // A span ending at five seconds, in a stream of four.
        assert_eq!(
            weaver.row(r#"{"space":"clip","start_t":4,"end_t":5,"vector":[1,2,3,4,5,6,7,8]}"#),
            None
        );
        let mut open: Option<(i64, i64)> = None;
        let mut rows = Vec::new();
        for frame in 0..100i64 {
            let pts_ms = frame * 40;
            weaver.seen(pts_ms);
            let carried = weaver.carrier(frame, pts_ms, frame % 25 == 0);
            if carried.open {
                open = Some((frame, pts_ms));
            }
            rows.extend(carried.rows);
        }
        assert!(rows.is_empty(), "the record rode something already");
        let (pts, pts_ms) = open.expect("a keyframe was held open");
        assert_eq!(pts_ms, 75 * 40, "the last keyframe was not the one held");
        rows.extend(weaver.flush(pts, pts_ms, true).rows);

        let woven = parsed(&rows);
        assert_eq!(woven.len(), 1, "{rows:?}");
        assert_eq!(
            woven[0].get("carrier_t").and_then(Json::as_f64),
            Some(3.0),
            "the record did not ride the last keyframe"
        );
        assert_eq!(
            woven[0].get("start_off_ms").and_then(Json::as_i64),
            Some(1000)
        );
        assert_eq!(
            woven[0].get("end_off_ms").and_then(Json::as_i64),
            Some(2000),
            "the end of the span is ahead of its carrier and must say so"
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
        // The keyframe held open is the first and only one.
        weaver.flush(0, 0, true);
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
        // Room for one plane's message and not for eight, which is the
        // policy doing what it is for.
        config.placement = Placement::Spread { budget_bytes: 64 };
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
    fn a_budget_below_one_plane_sends_the_record_as_one_value() {
        // A FRAGMENT names the record it slices and not which of the
        // record's values, so a record cut into slices must have only
        // one value to cut. Here is why it matters: the slices are
        // handed to a reader in a DIFFERENT order from the one they
        // were written in, which is what an elementary stream with
        // B-frames does to a reader walking it in decode order, and the
        // record still comes back whole.
        let mut config = Config::new(vec![space("clip", 0, 64)]);
        config.placement = Placement::Spread { budget_bytes: 32 };
        let mut weaver = Weaver::new(config);
        assert_eq!(weaver.row(&row("clip", 0.0, 1.0, 64)), None);
        let mut written: Vec<(i64, Vec<Message>)> = Vec::new();
        for frame in 0..120i64 {
            weaver.seen(frame * 40);
            let carried = weaver.carrier(frame, frame * 40, frame % 25 == 0);
            if !carried.messages.is_empty() {
                written.push((frame * 40, carried.messages));
            }
        }
        let slices = written
            .iter()
            .flat_map(|(_, messages)| messages)
            .filter(|m| matches!(m, Message::Fragment(_)))
            .count();
        assert!(slices > 1, "a 64-dim record fitted one 32-byte carrier");
        assert!(
            written
                .iter()
                .flat_map(|(_, messages)| messages)
                .all(|m| !matches!(m, Message::Vector(_))),
            "a value went whole where the budget could not hold one"
        );

        // Reversed, so no slice arrives after the one it follows.
        let mut assembler = ffrwd_index_core::assemble::Assembler::default();
        for (carrier_ms, messages) in written.iter().rev() {
            assembler.push_unit(*carrier_ms, &Unit::new(messages.clone()));
        }
        assert_eq!(assembler.dropped(), 0, "a slice was refused");
        let read = assembler.records();
        assert_eq!(read.len(), 1, "the record did not come back");
        assert_eq!(read[0].planes, Some(0xff), "a plane was lost");
        assert_eq!((read[0].start_ms, read[0].end_ms), (0, 1000));
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
            while let Some(settle) = reorder.settle() {
                settled.push(settle.pts);
            }
            left.extend(reorder.release().into_iter().map(|index| index as i64));
            most = most.max(reorder.held());
        }
        reorder.close();
        while let Some(settle) = reorder.settle() {
            settled.push(settle.pts);
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
