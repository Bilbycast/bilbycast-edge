//! SMPTE 2022-7 hitless redundancy: merge (RX) and duplication (TX).
//!
//! - [`merger::HitlessMerger`] -- De-duplicates RTP packets arriving from two
//!   independent SRT input legs, forwarding only sequence-advancing packets
//!   downstream. Uses wrapping u16 arithmetic for correct handling of RTP
//!   sequence number wraparound at 65535.
//!
//! - [`duplicator::SrtDuplicator`] -- Sends each outgoing packet to both SRT
//!   output legs using non-blocking `try_send`. If one leg is congested, the
//!   packet is dropped on that leg without blocking the other.

pub mod duplicator;
pub mod merger;
