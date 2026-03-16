use bytes::Bytes;

/// Maximum RTP packet size for SMPTE 2022-2 (7 x 188 byte TS packets + 12 byte RTP header)
pub const MAX_RTP_PACKET_SIZE: usize = 1500;

/// Broadcast channel capacity for fan-out
pub const BROADCAST_CHANNEL_CAPACITY: usize = 2048;

/// The packet that flows through broadcast channels
#[derive(Clone, Debug)]
pub struct RtpPacket {
    /// Raw packet data including RTP header
    pub data: Bytes,
    /// RTP sequence number (parsed from header for 2022-7 merge and stats)
    pub sequence_number: u16,
    /// RTP timestamp (parsed from header)
    pub rtp_timestamp: u32,
    /// Monotonic receive time in microseconds (for stats/throughput)
    pub recv_time_us: u64,
}
