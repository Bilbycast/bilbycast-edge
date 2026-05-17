//! MXL audio helpers.
//!
//! v1.0 of upstream libmxl carries audio as **Float32 PCM @ 48 kHz**
//! exclusively. Channel count and packet_time_us are operator-configurable.
//! libmxl exposes audio via `SamplesReader` / `SamplesWriter` — a planar
//! ring-buffer model where each channel is a separate `&[u8]` slice (so
//! 2-channel Float32 = 8 bytes per frame split as 4 + 4 across channels).

/// MXL v1.0 audio sample rate (Hz). Fixed at 48000.
pub const MXL_AUDIO_SAMPLE_RATE: u32 = 48_000;

/// MXL v1.0 audio sample format is Float32 (IEEE-754 single, little-endian).
pub const MXL_AUDIO_BIT_DEPTH: u8 = 32;
/// Bytes per sample (Float32 = 4 bytes). Used by the future PCM
/// conversion path when wiring `audio_encode`.
#[allow(dead_code)]
pub const MXL_AUDIO_BYTES_PER_SAMPLE: usize = 4;

/// Samples per channel for a given packet_time_us at the v1.0 sample rate.
/// e.g. 1000 µs → 48 samples; 4000 µs → 192 samples.
pub fn samples_per_channel(packet_time_us: u32) -> usize {
    ((MXL_AUDIO_SAMPLE_RATE as u64) * (packet_time_us as u64) / 1_000_000) as usize
}

/// Build the libmxl audio flow definition JSON (audio_flow.json shape, per
/// `vendor/mxl/lib/tests/data/audio_flow.json`). Used by the writer side of
/// MXL audio outputs to declare the flow before opening a SamplesWriter.
///
/// Returns the JSON string and the flow's UUID (deterministically derived
/// from `flow_name` so producer and consumer agree).
pub fn build_audio_flow_def(flow_name: &str, channels: u8) -> (String, uuid::Uuid) {
    let flow_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, flow_name.as_bytes());
    let source_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, flow_name.as_bytes());
    let device_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, flow_name.as_bytes());
    let json = format!(
        r#"{{
            "description": "bilbycast-edge MXL audio flow {flow_name}",
            "format": "urn:x-nmos:format:audio",
            "tags": {{}},
            "label": "{flow_name}",
            "version": "0:0",
            "id": "{flow_id}",
            "media_type": "audio/float32",
            "sample_rate": {{ "numerator": {} }},
            "channel_count": {channels},
            "bit_depth": {},
            "parents": [],
            "source_id": "{source_id}",
            "device_id": "{device_id}"
        }}"#,
        MXL_AUDIO_SAMPLE_RATE, MXL_AUDIO_BIT_DEPTH
    );
    (json, flow_id)
}
