//! MXL video helpers — V210 packed-pixel format math.
//!
//! V210 packs 6 pixels of 4:2:2 10-bit YCbCr into 16 bytes:
//!   - 24 components per macropixel-group (8 Y + 8 Cb + 8 Cr, but only 4 Y
//!     of those carry distinct samples; the 4:2:2 sampling shares chroma).
//!   - 4 little-endian u32 words, each holding three 10-bit components in
//!     bits [0..10], [10..20], [20..30] (top 2 bits zero).
//!
//! Row stride is `((width + 47) / 48) * 128` bytes — V210 requires the
//! source width to round up to 48-pixel groups (each group = 128 bytes).
//! Most broadcast resolutions (1920, 1280, 720) are 48-divisible already.

/// Bytes per row for V210 video at the given pixel width. Used by the
/// future-wired V210 → planar YUV conversion path; M3 spawn loop today
/// uses libmxl's grain `payload.len()` directly.
#[allow(dead_code)]
pub fn v210_row_stride(width: u32) -> usize {
    let groups = (width as usize + 47) / 48;
    groups * 128
}

/// Total grain payload bytes for V210 at the given resolution.
#[allow(dead_code)]
pub fn v210_frame_bytes(width: u32, height: u32) -> usize {
    v210_row_stride(width) * (height as usize)
}

/// Build the libmxl video flow definition JSON (v210_flow.json shape, per
/// `vendor/mxl/lib/tests/data/v210_flow.json`). Used by the writer side of
/// MXL video outputs to declare the flow before opening a GrainWriter.
pub fn build_video_flow_def(
    flow_name: &str,
    width: u32,
    height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
) -> (String, uuid::Uuid) {
    let flow_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, flow_name.as_bytes());
    let json = format!(
        r#"{{
            "description": "bilbycast-edge MXL video flow {flow_name}",
            "id": "{flow_id}",
            "tags": {{}},
            "format": "urn:x-nmos:format:video",
            "label": "{flow_name}",
            "parents": [],
            "media_type": "video/v210",
            "grain_rate": {{
                "numerator": {frame_rate_num},
                "denominator": {frame_rate_den}
            }},
            "frame_width": {width},
            "frame_height": {height},
            "interlace_mode": "progressive",
            "colorspace": "BT709",
            "components": [
                {{ "name": "Y",  "width": {width},     "height": {height}, "bit_depth": 10 }},
                {{ "name": "Cb", "width": {}, "height": {height}, "bit_depth": 10 }},
                {{ "name": "Cr", "width": {}, "height": {height}, "bit_depth": 10 }}
            ]
        }}"#,
        width / 2,
        width / 2
    );
    (json, flow_id)
}
