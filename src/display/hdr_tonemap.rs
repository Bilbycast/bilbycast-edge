// HDR-to-SDR tonemapping for the local-display confidence-monitor
// output. Real-world UHD broadcast contribution feeds use either
// SMPTE 2084 (PQ / HDR10) or ARIB STD-B67 (HLG); when PQ content is
// piped at a stock Rec.709 confidence panel without tonemapping,
// libswscale's YUV→RGB matrix produces a frame whose RGB values
// are still in PQ-encoded space — PQ packs ~50 % of the curve into
// the bottom 1 % of the signal range, so the picture comes out
// noticeably dim and low-contrast even though the colours are
// approximately correct.
//
// We can't ask libswscale to do the EOTF math for us — its
// `sws_setColorspaceDetails` only swaps the YUV→RGB matrix
// coefficients, not the transfer function. Instead, after
// libswscale produces 8-bit BGRA, we walk a per-channel 256-byte
// LUT that for PQ:
//
//   1. Inverts the PQ EOTF to get scene-linear light.
//   2. Rescales so the BT.2100 SDR diffuse-white anchor (100 cd/m²)
//      lands at linear 1.0.
//   3. Clips to `[0, 1]` (HDR highlights paste-clip on the SDR
//      panel — for confidence monitoring, recognisable midtones
//      matter more than highlight detail).
//   4. Applies the sRGB OETF for the panel.
//
// HLG is handled differently. ARIB STD-B67 was deliberately
// designed with backward-compatibility in mind: the HLG OETF
// approximately inverts the sRGB EOTF in the lower half of the
// signal range, so showing an HLG signal directly on an sRGB
// confidence panel produces the intended SDR look without any
// tonemapping at all. Inserting an EOTF / OOTF / OETF chain here
// would only **darken** the midtones (`HLG 0.5 → 1/12 linear → 89
// SDR` instead of the BT.2100-intended `HLG 0.5 → ≈ 188 SDR` from
// the panel's own sRGB decode). For HLG sources we therefore
// **build no LUT** — the resolver returns `None` and
// `apply_bgra` is never called. This matches what every modern
// SDR HLG-aware TV does internally.
//
// 256 entries × 3 channels = 768 bytes — fits in L1, < 1 cycle
// per lookup. At 4K50 the per-frame cost is ~25 M lookups ≈ 5 ms
// on a modern CPU — acceptable inside the 20 ms-per-frame budget.
//
// Limitations (deliberate scope cuts for the confidence-monitor
// use case — full HDR → SDR for production master output should
// use libzimg / libplacebo):
// - No BT.2020 → BT.709 gamut compression. UHD HDR sources use
//   the wider BT.2020 primaries; we apply the PQ transfer
//   inversion per-channel and let the panel show the result in
//   Rec.709 primaries. Saturated reds and greens will look
//   slightly desaturated, but the picture is faithful enough for
//   "is this feed live?" confidence checks.
// - PQ highlights >100 cd/m² hard-clip to SDR white. Acceptable
//   on a Rec.709 panel (the panel can't display brighter than
//   that anyway). Master-output tonemapping with a soft shoulder
//   needs operator-tunable knobs that don't belong on the
//   confidence display.

#![cfg(all(feature = "display", target_os = "linux"))]

/// Per-channel 256-entry LUT mapping an 8-bit PQ-encoded channel
/// value (after libswscale produced BGRA) into an 8-bit SDR sRGB
/// channel value. Apply identically to R, G, and B (alpha is left
/// untouched).
///
/// **Only built for PQ**. HLG sources resolve to `None` at the
/// call site — see the file-level rationale.
#[derive(Clone)]
pub struct HdrTonemap {
    lut: [u8; 256],
}

impl HdrTonemap {
    /// Build a LUT for SMPTE 2084 (PQ / HDR10) → Rec.709 SDR. The
    /// PQ inverse EOTF maps `[0, 1]` → `[0, 10000]` cd/m²; we
    /// rescale so the BT.2100 SDR diffuse-white anchor (100 cd/m²)
    /// lands at linear 1.0 before clip + sRGB OETF.
    pub fn for_pq() -> Self {
        let mut lut = [0u8; 256];
        for (i, slot) in lut.iter_mut().enumerate() {
            let normalized = i as f32 / 255.0;
            // PQ inverse EOTF. Output normalised to 1.0 == 10 000
            // cd/m². Scale so SDR diffuse-white (100 cd/m²) lands at
            // linear 1.0 — anything above that is a true HDR
            // highlight and gets clipped on the confidence panel.
            let linear = pq_eotf_normalized(normalized)
                * (10000.0 / SDR_REFERENCE_WHITE_NITS);
            let clipped = linear.clamp(0.0, 1.0);
            let srgb = srgb_oetf(clipped);
            *slot = (srgb.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        Self { lut }
    }

    /// Apply this LUT to every pixel of the BGRA frame. Alpha is
    /// preserved; B / G / R get remapped through the same LUT (the
    /// source after libswscale is in HDR-encoded primaries — same
    /// transfer applies to each channel).
    ///
    /// `pitch` is bytes-per-row (matches the KMS dumb buffer pitch);
    /// the function only walks the first `width * 4` bytes of each
    /// row so any trailing pad is preserved.
    pub fn apply_bgra(&self, dst: &mut [u8], pitch: usize, width: usize, height: usize) {
        for row in 0..height {
            let row_start = row * pitch;
            let row_end = row_start + width * 4;
            let row_slice = &mut dst[row_start..row_end];
            for px in row_slice.chunks_exact_mut(4) {
                px[0] = self.lut[px[0] as usize]; // B
                px[1] = self.lut[px[1] as usize]; // G
                px[2] = self.lut[px[2] as usize]; // R
                // px[3] (alpha) untouched.
            }
        }
    }

    /// Borrow the raw LUT — exposed for tests so we can verify the
    /// curve shape without going through `apply_bgra`.
    #[cfg(test)]
    pub fn lut(&self) -> &[u8; 256] {
        &self.lut
    }
}

// ── Reference whites ───────────────────────────────────────────────

/// BT.2100 / BT.2408 SDR diffuse-white anchor in HDR signal terms.
/// PQ encodes brightness on an absolute scale up to 10 000 cd/m²,
/// while a calibrated SDR panel reproduces "white" at 100 cd/m².
/// Mapping 100 nits → linear 1.0 means PQ content sitting at SDR
/// diffuse white renders at full SDR brightness on the confidence
/// panel; anything brighter than that in the HDR signal is a true
/// HDR highlight and clips.
const SDR_REFERENCE_WHITE_NITS: f32 = 100.0;

// ── PQ (SMPTE 2084) inverse EOTF ───────────────────────────────────

// Constants from BT.2100 / SMPTE 2084.
const PQ_M1: f32 = 2610.0 / 16384.0;
const PQ_M2: f32 = 2523.0 / 4096.0 * 128.0;
const PQ_C1: f32 = 3424.0 / 4096.0;
const PQ_C2: f32 = 2413.0 / 4096.0 * 32.0;
const PQ_C3: f32 = 2392.0 / 4096.0 * 32.0;

/// PQ inverse EOTF — maps PQ-coded value `[0, 1]` → linear nits
/// in `[0, 10000]`, normalised to `[0, 1]` (i.e. divided by 10000).
fn pq_eotf_normalized(e_prime: f32) -> f32 {
    if e_prime <= 0.0 {
        return 0.0;
    }
    let p = e_prime.powf(1.0 / PQ_M2);
    let num = (p - PQ_C1).max(0.0);
    let den = PQ_C2 - PQ_C3 * p;
    if den <= 0.0 {
        return 0.0;
    }
    (num / den).powf(1.0 / PQ_M1)
}

// ── sRGB OETF ──────────────────────────────────────────────────────

/// sRGB OETF (forward) — encodes linear in `[0, 1]` for display.
fn srgb_oetf(linear: f32) -> f32 {
    if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pq_eotf_at_anchor_points() {
        // BT.2100 anchor points: PQ=0 → 0 nits, PQ≈0.508 → 100 nits,
        // PQ=1 → 10000 nits. Tolerances reflect the 32-bit float
        // round-trip.
        assert!(pq_eotf_normalized(0.0).abs() < 1e-6);
        let mid = pq_eotf_normalized(0.5081) * 10000.0;
        assert!(
            (mid - 100.0).abs() < 5.0,
            "PQ 0.5081 should be ≈ 100 nits, got {mid}"
        );
        let peak = pq_eotf_normalized(1.0) * 10000.0;
        assert!(
            (peak - 10000.0).abs() < 1.0,
            "PQ 1.0 should be ≈ 10000 nits, got {peak}"
        );
    }

    #[test]
    fn pq_lut_is_monotonic_non_decreasing() {
        // The full pipeline (PQ EOTF → clip → sRGB OETF) is monotonic
        // by construction. A regression that introduces a sign error
        // anywhere shows up as a non-monotonic LUT.
        let map = HdrTonemap::for_pq();
        let lut = map.lut();
        for w in lut.windows(2) {
            assert!(
                w[1] >= w[0],
                "LUT not monotonic at boundary: {} → {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn pq_lut_lifts_sdr_white_to_near_full_brightness() {
        // PQ 0.508 == 100 cd/m², the BT.2100 SDR diffuse-white anchor.
        // Confidence-monitor expectation: content sitting at SDR
        // white in the HDR signal must arrive at full SDR brightness
        // on the panel — otherwise a UHD HDR contribution feed still
        // looks dim even with the tonemap engaged.
        let map = HdrTonemap::for_pq();
        let lut = map.lut();
        // 0.508 * 255 ≈ 129.5 → look up nearest 8-bit slot.
        let sdr_white = lut[130];
        assert!(
            sdr_white >= 250,
            "PQ-coded SDR-white (≈ 130/255) must lift to SDR full-bright, got {sdr_white}"
        );
    }

    #[test]
    fn pq_lut_renders_dark_content_visible() {
        // PQ 0.4 ≈ 10 cd/m² — a lit dark-room scene. After tonemap
        // it should be a visible mid-low-grey (not a black blob).
        let map = HdrTonemap::for_pq();
        let lut = map.lut();
        let dark = lut[(0.4 * 255.0) as usize];
        assert!(
            (40..=160).contains(&dark),
            "PQ 0.4 should render as visible mid-low-grey, got {dark}"
        );
    }

    #[test]
    fn pq_lut_clips_hdr_highlights_at_sdr_white() {
        // Above SDR diffuse white (100 cd/m², PQ ≈ 0.508), HDR
        // highlights legitimately clip on a 100-nit SDR confidence
        // panel — the panel can't display brighter than its own
        // peak. The LUT must hold at SDR white for the entire
        // upper signal range.
        let map = HdrTonemap::for_pq();
        let lut = map.lut();
        for input in 200..=255 {
            assert_eq!(
                lut[input], 255,
                "PQ above SDR-white anchor must clip at 255, lut[{input}] = {}",
                lut[input]
            );
        }
    }

    #[test]
    fn pq_lut_endpoints_are_zero_and_full() {
        let map = HdrTonemap::for_pq();
        let lut = map.lut();
        assert_eq!(lut[0], 0, "PQ 0 must map to SDR 0");
        // White point lands at 255 — clip + sRGB OETF maps 1.0 → 255.
        assert_eq!(lut[255], 255, "PQ 255 must clip to SDR full-bright");
    }

    // HLG isn't tonemapped by this module — see file-level rationale.
    // The display-output call site resolves HLG sources to `None` and
    // skips `apply_bgra` entirely, relying on the panel's sRGB EOTF
    // approximately inverting the HLG OETF (BT.2100 backward-compat
    // design intent). No HLG LUT to test here.

    #[test]
    fn apply_bgra_preserves_alpha() {
        let map = HdrTonemap::for_pq();
        // 1 px wide × 1 row, BGRA. Set RGB to mid-grey-ish, alpha
        // to a sentinel. After apply_bgra, alpha must be unchanged.
        let mut buf: [u8; 4] = [128, 128, 128, 0xAB];
        map.apply_bgra(&mut buf, 4, 1, 1);
        assert_eq!(buf[3], 0xAB);
    }

    #[test]
    fn apply_bgra_does_not_touch_padding_columns() {
        // 1 px × 1 row visible in a 2-px-wide buffer (pitch = 8).
        // The second pixel slot must be untouched (typical KMS
        // dumb-buffer pitch padding).
        let map = HdrTonemap::for_pq();
        let mut buf: [u8; 8] = [128, 128, 128, 0xAB, 0xDE, 0xAD, 0xBE, 0xEF];
        map.apply_bgra(&mut buf, 8, 1, 1);
        assert_eq!(&buf[4..], &[0xDE, 0xAD, 0xBE, 0xEF]);
    }
}
