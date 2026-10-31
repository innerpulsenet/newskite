//! NZB manifest parsing and yEnc encoding/decoding for Newskite.
//!
//! * [`nzb`] parses NZB XML documents (namespaced or not) into [`nzb::Nzb`], with segments
//!   sorted and de-duplicated, plus filename extraction from Usenet subjects and a strict,
//!   cross-platform [`nzb::sanitize_filename`].
//! * [`yenc`] decodes yEnc article bodies (single- and multi-part, CRC-checked) and provides an
//!   encoder used by tests and the NNTP mock server.
//!
//! This crate performs no network I/O; [`nzb::parse_file`] is the only function that touches
//! the filesystem.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod nzb;
pub mod yenc;

pub use nzb::{Nzb, NzbError, NzbFile, Segment};
pub use yenc::{YencError, YencPart};
