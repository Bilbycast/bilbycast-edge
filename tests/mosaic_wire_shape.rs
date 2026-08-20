// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: LicenseRef-Bilbycast-EULA

//! The edge half of the wall-deploy wire contract.
//!
//! `bilbycast-manager`'s `device_edge::mosaic::compile_wall` turns an authored
//! wall into the JSON body of a `create_input` command. This asserts the edge
//! accepts exactly that shape — every field, by value, not merely "it parses".
//!
//! **Why it is worth a whole test target.** The push path is raw JSON end to
//! end: the manager's `device-edge` crate has no typed `mosaic` variant, and
//! `InputConfig` deserialises with `#[serde(tag = "type")]`, so an unknown or
//! misspelled field is not a hard error on either side — it is a default. A
//! renamed `source_input_id` does not fail anything; it silently produces a
//! wall whose every tile reads UNASSIGNED, on a node with no error to show for
//! it. The two repos are separate git repositories, so no compiler sees both
//! ends; this file and its manager-side twin are the only thing that does.
//!
//! Keep `GOLDEN` byte-identical to
//! `bilbycast-manager/crates/device-edge/src/mosaic.rs`'s `GOLDEN_WALL_INPUT`.
//! If they drift, this side fails and the manager side passes — the right
//! direction, but only if this target actually runs. It does: CI names it
//! explicitly (`cargo test --features multiviewer --lib --test
//! mosaic_wire_shape`), because a bare `--lib` skips integration targets and a
//! contract test nothing runs is worse than no contract test at all.
#![cfg(feature = "multiviewer")]

use bilbycast_edge::config::models::{InputConfig, InputDefinition};

/// The canonical wall input, exactly as the manager emits it.
const GOLDEN: &str = r#"{
  "id": "mvwall-w1",
  "name": "Gallery Wall",
  "active": true,
  "type": "mosaic",
  "width": 1920,
  "height": 1080,
  "fps": 25,
  "video_bitrate_kbps": 8000,
  "codec": "h264_auto",
  "tiles": [
    { "id": "tile-a", "source_input_id": "cam4", "x": 0,   "y": 0, "width": 960, "height": 540, "z": 0, "label": "CAM 4" },
    { "id": "tile-b", "source_input_id": null,   "x": 960, "y": 0, "width": 960, "height": 540, "z": 1, "label": "" }
  ]
}"#;

#[test]
fn the_managers_compiled_wall_deserialises_field_for_field() {
    let def: InputDefinition = serde_json::from_str(GOLDEN).expect("the golden body must parse");

    assert_eq!(def.id, "mvwall-w1");
    assert_eq!(def.name, "Gallery Wall");
    assert!(def.active, "a deployed wall arrives enabled");

    let InputConfig::Mosaic(m) = &def.config else {
        panic!("`type: mosaic` must select InputConfig::Mosaic, got {:?}", def.config);
    };
    assert_eq!(m.width, 1920);
    assert_eq!(m.height, 1080);
    assert_eq!(m.fps, 25);
    assert_eq!(m.video_bitrate_kbps, 8000);
    assert_eq!(m.codec, "h264_auto");
    assert_eq!(m.tiles.len(), 2);

    let a = &m.tiles[0];
    assert_eq!(a.id, "tile-a");
    assert_eq!(a.source_input_id.as_deref(), Some("cam4"));
    assert_eq!((a.x, a.y, a.width, a.height), (0, 0, 960, 540));
    assert_eq!(a.z, 0);
    assert_eq!(a.label, "CAM 4");

    // An unrouted tile carries an explicit null rather than being omitted, so
    // the wire says "deliberately unassigned" instead of "field forgotten".
    // Both deserialise to None here; only one is honest in a log.
    let b = &m.tiles[1];
    assert_eq!(b.id, "tile-b");
    assert_eq!(b.source_input_id, None);
    assert_eq!((b.x, b.y, b.width, b.height), (960, 0, 960, 540));
    assert_eq!(b.z, 1);
    assert_eq!(b.label, "");
}

/// The golden body is one the node would actually accept.
///
/// Parsing is not acceptance: `validate_mosaic_input` enforces the even-canvas
/// rule, the fps and bitrate ranges, the id lengths and the 64-tile cap, and
/// `MosaicLayout::validate` enforces that every tile fits the canvas. A
/// contract test that stopped at serde would let the manager ship a body the
/// node refuses at flow start.
#[test]
fn the_golden_body_passes_the_nodes_own_validation() {
    let def: InputDefinition = serde_json::from_str(GOLDEN).expect("parse");
    bilbycast_edge::config::validation::validate_input_definition(&def)
        .expect("the manager's compiled wall must pass the edge's validator");
}
