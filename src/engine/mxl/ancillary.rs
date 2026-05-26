//! MXL ancillary (RFC 8331) helpers.
//!
//! MXL carries RFC 8331 ancillary data as discrete grains — each grain
//! holds one ANC packet (or a small batch). Wire format is the same as
//! ST 2110-40 over the network, so downstream consumers can treat the
//! grain payload as-is.

/// Build the libmxl data flow definition JSON (data_flow.json shape, per
/// `vendor/mxl/lib/tests/data/data_flow.json`). Used by the writer side of
/// MXL ANC outputs to declare the flow before opening a GrainWriter.
pub fn build_anc_flow_def(
    flow_name: &str,
    grain_rate_num: u32,
    grain_rate_den: u32,
) -> (String, uuid::Uuid) {
    let flow_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, flow_name.as_bytes());
    let source_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, flow_name.as_bytes());
    let device_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, flow_name.as_bytes());
    let json = format!(
        r#"{{
            "description": "bilbycast-edge MXL ANC flow {flow_name}",
            "tags": {{ "urn:x-nmos:tag:grouphint/v1.0": ["{flow_name}:Ancillary Data"] }},
            "format": "urn:x-nmos:format:data",
            "label": "{flow_name}",
            "version": "0:0",
            "parents": [],
            "source_id": "{source_id}",
            "device_id": "{device_id}",
            "id": "{flow_id}",
            "media_type": "video/smpte291",
            "grain_rate": {{
                "numerator": {grain_rate_num},
                "denominator": {grain_rate_den}
            }}
        }}"#
    );
    (json, flow_id)
}
