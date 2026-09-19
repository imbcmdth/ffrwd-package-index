//! The rows both callers of the codec speak, and what a writer does
//! with them.
//!
//! `core` is the format and stays that way: no dependencies, no I/O, no
//! opinion about how a caller spells a space or a vector. But the tool
//! in `tool/` and the wasm module in `weave/` do have to agree on that
//! spelling, because a row a writer hands one of them means the same
//! thing to the other, and a field renamed in one and not the other is
//! a bug nobody would see until a file came back empty. So the shapes
//! live here, in one crate over `core` that both compile in, rather
//! than in `core` (which the format does not need) or twice over.
//!
//! What is shared is the SPACE fields and the vector body, which are
//! the format's own. What is not is how a caller names a space: the
//! tool's rows carry the wire's `id` directly, and the module's carry a
//! `name` it assigns ids to, because a query writing rows has no reason
//! to know what a `space_id` is. So [`space::read_fields`] reads
//! everything but the id, and each caller puts its own naming around
//! it.
//!
//! - [`json`]: a small JSON reader and writer, one value per line.
//! - [`space`]: a SPACE message as a JSON object, both ways.
//! - [`read`]: packets in, rows out, which is what the two reading
//!   sinks both do.
//! - [`stream`]: which framing a pad's packets are in, and getting
//!   units into and out of one.
//! - [`vector`]: a vector as JSON numbers or as base64 f32, and the
//!   body that goes on the wire.
//! - [`weave`]: rows in, messages on carriers out - the module's whole
//!   decision, kept here so it is tested on the native target.
//!
//! Nothing here opens a file or allocates unboundedly on bad input, so
//! the wasm module compiles it in as happily as the tool does.

#![forbid(unsafe_code)]

pub mod json;
pub mod read;
pub mod space;
pub mod stream;
pub mod vector;
pub mod weave;
