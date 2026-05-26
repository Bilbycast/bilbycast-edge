//! MXL video helpers — V210 packed-pixel format math and conversion.
//!
//! V210 packs 6 pixels of 4:2:2 10-bit YCbCr into 16 bytes (4 LE u32 words).
//! Each word holds three 10-bit components in bits [9:0], [19:10], [29:20].
//!
//! Word layout per 6-pixel group:
//!
//! | Word | [9:0]  | [19:10] | [29:20] |
//! |------|--------|---------|---------|
//! | 0    | Cb[0]  | Y[0]    | Cr[0]   |
//! | 1    | Y[1]   | Cb[1]   | Y[2]    |
//! | 2    | Cr[1]  | Y[3]    | Cb[2]   |
//! | 3    | Y[4]   | Cr[2]   | Y[5]    |
//!
//! Row stride is `((width + 47) / 48) * 128` bytes — V210 requires the
//! source width to round up to 48-pixel groups (each group = 128 bytes).
//! Most broadcast resolutions (1920, 1280, 720) are 48-divisible already.

/// Bytes per row for V210 video at the given pixel width.
pub fn v210_row_stride(width: u32) -> usize {
    let groups = (width as usize + 47) / 48;
    groups * 128
}

/// Total grain payload bytes for V210 at the given resolution.
pub fn v210_frame_bytes(width: u32, height: u32) -> usize {
    v210_row_stride(width) * (height as usize)
}

/// Pack planar YUV 4:2:2 10-bit into V210 bytes.
///
/// - `y`: luma plane, `width × height` samples (u16, low 10 bits valid)
/// - `cb`, `cr`: chroma planes, `(width/2) × height` samples each
///
/// Returns V210 frame bytes at `v210_row_stride(width)` pitch per row.
pub fn pack_v210(
    y: &[u16],
    cb: &[u16],
    cr: &[u16],
    width: u32,
    height: u32,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let stride = v210_row_stride(width);
    let mut out = vec![0u8; stride * h];

    for row in 0..h {
        let y_row = &y[row * w..];
        let cb_row = &cb[row * cw..];
        let cr_row = &cr[row * cw..];
        let dst_row = &mut out[row * stride..row * stride + stride];

        // Process 6 luma pixels (3 chroma pairs) per iteration = 16 bytes
        let full_groups = w / 6;
        for g in 0..full_groups {
            let yi = g * 6;
            let ci = g * 3;
            let di = g * 16;

            let w0 = (cb_row[ci] as u32 & 0x3FF)
                | ((y_row[yi] as u32 & 0x3FF) << 10)
                | ((cr_row[ci] as u32 & 0x3FF) << 20);
            let w1 = (y_row[yi + 1] as u32 & 0x3FF)
                | ((cb_row[ci + 1] as u32 & 0x3FF) << 10)
                | ((y_row[yi + 2] as u32 & 0x3FF) << 20);
            let w2 = (cr_row[ci + 1] as u32 & 0x3FF)
                | ((y_row[yi + 3] as u32 & 0x3FF) << 10)
                | ((cb_row[ci + 2] as u32 & 0x3FF) << 20);
            let w3 = (y_row[yi + 4] as u32 & 0x3FF)
                | ((cr_row[ci + 2] as u32 & 0x3FF) << 10)
                | ((y_row[yi + 5] as u32 & 0x3FF) << 20);

            dst_row[di..di + 4].copy_from_slice(&w0.to_le_bytes());
            dst_row[di + 4..di + 8].copy_from_slice(&w1.to_le_bytes());
            dst_row[di + 8..di + 12].copy_from_slice(&w2.to_le_bytes());
            dst_row[di + 12..di + 16].copy_from_slice(&w3.to_le_bytes());
        }

        // Handle remaining pixels (width not divisible by 6) by padding
        // with black (Y=64, Cb=Cr=512 for limited-range 10-bit).
        let rem = w % 6;
        if rem > 0 {
            let yi = full_groups * 6;
            let ci = full_groups * 3;
            let di = full_groups * 16;
            let mut yp = [64u16; 6];
            let mut cbp = [512u16; 3];
            let mut crp = [512u16; 3];
            for i in 0..rem {
                yp[i] = y_row[yi + i];
            }
            for i in 0..(rem + 1) / 2 {
                cbp[i] = cb_row[ci + i];
                crp[i] = cr_row[ci + i];
            }
            let w0 = (cbp[0] as u32 & 0x3FF)
                | ((yp[0] as u32 & 0x3FF) << 10)
                | ((crp[0] as u32 & 0x3FF) << 20);
            let w1 = (yp[1] as u32 & 0x3FF)
                | ((cbp[1] as u32 & 0x3FF) << 10)
                | ((yp[2] as u32 & 0x3FF) << 20);
            let w2 = (crp[1] as u32 & 0x3FF)
                | ((yp[3] as u32 & 0x3FF) << 10)
                | ((cbp[2] as u32 & 0x3FF) << 20);
            let w3 = (yp[4] as u32 & 0x3FF)
                | ((crp[2] as u32 & 0x3FF) << 10)
                | ((yp[5] as u32 & 0x3FF) << 20);

            dst_row[di..di + 4].copy_from_slice(&w0.to_le_bytes());
            dst_row[di + 4..di + 8].copy_from_slice(&w1.to_le_bytes());
            dst_row[di + 8..di + 12].copy_from_slice(&w2.to_le_bytes());
            dst_row[di + 12..di + 16].copy_from_slice(&w3.to_le_bytes());
        }
    }
    out
}

/// Unpack V210 bytes into planar YUV 4:2:2 10-bit.
///
/// Returns `(y, cb, cr)` with tight strides (`width`, `width/2`, `width/2`),
/// all u16 with low 10 bits valid.
pub fn unpack_v210(
    v210: &[u8],
    width: u32,
    height: u32,
) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let stride = v210_row_stride(width);
    let mut y = vec![0u16; w * h];
    let mut cb = vec![0u16; cw * h];
    let mut cr = vec![0u16; cw * h];

    for row in 0..h {
        let src_row = &v210[row * stride..];
        let y_row = &mut y[row * w..];
        let cb_row = &mut cb[row * cw..];
        let cr_row = &mut cr[row * cw..];

        let full_groups = w / 6;
        for g in 0..full_groups {
            let si = g * 16;
            let yi = g * 6;
            let ci = g * 3;

            let w0 = u32::from_le_bytes([src_row[si], src_row[si + 1], src_row[si + 2], src_row[si + 3]]);
            let w1 = u32::from_le_bytes([src_row[si + 4], src_row[si + 5], src_row[si + 6], src_row[si + 7]]);
            let w2 = u32::from_le_bytes([src_row[si + 8], src_row[si + 9], src_row[si + 10], src_row[si + 11]]);
            let w3 = u32::from_le_bytes([src_row[si + 12], src_row[si + 13], src_row[si + 14], src_row[si + 15]]);

            cb_row[ci] = (w0 & 0x3FF) as u16;
            y_row[yi] = ((w0 >> 10) & 0x3FF) as u16;
            cr_row[ci] = ((w0 >> 20) & 0x3FF) as u16;

            y_row[yi + 1] = (w1 & 0x3FF) as u16;
            cb_row[ci + 1] = ((w1 >> 10) & 0x3FF) as u16;
            y_row[yi + 2] = ((w1 >> 20) & 0x3FF) as u16;

            cr_row[ci + 1] = (w2 & 0x3FF) as u16;
            y_row[yi + 3] = ((w2 >> 10) & 0x3FF) as u16;
            cb_row[ci + 2] = ((w2 >> 20) & 0x3FF) as u16;

            y_row[yi + 4] = (w3 & 0x3FF) as u16;
            cr_row[ci + 2] = ((w3 >> 10) & 0x3FF) as u16;
            y_row[yi + 5] = ((w3 >> 20) & 0x3FF) as u16;
        }

        let rem = w % 6;
        if rem > 0 {
            let si = full_groups * 16;
            let yi = full_groups * 6;
            let ci = full_groups * 3;

            let w0 = u32::from_le_bytes([src_row[si], src_row[si + 1], src_row[si + 2], src_row[si + 3]]);
            let w1 = u32::from_le_bytes([src_row[si + 4], src_row[si + 5], src_row[si + 6], src_row[si + 7]]);
            let w2 = u32::from_le_bytes([src_row[si + 8], src_row[si + 9], src_row[si + 10], src_row[si + 11]]);
            let w3 = u32::from_le_bytes([src_row[si + 12], src_row[si + 13], src_row[si + 14], src_row[si + 15]]);

            let yp = [
                ((w0 >> 10) & 0x3FF) as u16,
                (w1 & 0x3FF) as u16,
                ((w1 >> 20) & 0x3FF) as u16,
                ((w2 >> 10) & 0x3FF) as u16,
                (w3 & 0x3FF) as u16,
                ((w3 >> 20) & 0x3FF) as u16,
            ];
            let cbp = [
                (w0 & 0x3FF) as u16,
                ((w1 >> 10) & 0x3FF) as u16,
                ((w2 >> 20) & 0x3FF) as u16,
            ];
            let crp = [
                ((w0 >> 20) & 0x3FF) as u16,
                (w2 & 0x3FF) as u16,
                ((w3 >> 10) & 0x3FF) as u16,
            ];
            for i in 0..rem {
                y_row[yi + i] = yp[i];
            }
            for i in 0..(rem + 1) / 2 {
                cb_row[ci + i] = cbp[i];
                cr_row[ci + i] = crp[i];
            }
        }
    }
    (y, cb, cr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_1920x1080() {
        let w: u32 = 1920;
        let h: u32 = 1080;
        let cw = (w / 2) as usize;
        let size_y = w as usize * h as usize;
        let size_c = cw * h as usize;

        let y: Vec<u16> = (0..size_y).map(|i| (i % 940 + 64) as u16).collect();
        let cb: Vec<u16> = (0..size_c).map(|i| (i % 896 + 64) as u16).collect();
        let cr: Vec<u16> = (0..size_c).map(|i| ((i * 3) % 896 + 64) as u16).collect();

        let packed = pack_v210(&y, &cb, &cr, w, h);
        assert_eq!(packed.len(), v210_frame_bytes(w, h));

        let (y2, cb2, cr2) = unpack_v210(&packed, w, h);
        assert_eq!(y, y2);
        assert_eq!(cb, cb2);
        assert_eq!(cr, cr2);
    }

    #[test]
    fn round_trip_width_not_divisible_by_6() {
        let w: u32 = 1918;
        let h: u32 = 2;
        let cw = (w / 2) as usize;
        let size_y = w as usize * h as usize;
        let size_c = cw * h as usize;

        let y: Vec<u16> = (0..size_y).map(|i| (i % 1023) as u16).collect();
        let cb: Vec<u16> = (0..size_c).map(|i| (i % 512) as u16).collect();
        let cr: Vec<u16> = (0..size_c).map(|i| (i % 768) as u16).collect();

        let packed = pack_v210(&y, &cb, &cr, w, h);
        let (y2, cb2, cr2) = unpack_v210(&packed, w, h);

        // Only the first `w` pixels per row round-trip exactly; remainder
        // pixels are padded on pack and read back from those pads on unpack.
        for row in 0..h as usize {
            assert_eq!(
                &y[row * w as usize..row * w as usize + w as usize],
                &y2[row * w as usize..row * w as usize + w as usize],
            );
            assert_eq!(
                &cb[row * cw..row * cw + cw],
                &cb2[row * cw..row * cw + cw],
            );
            assert_eq!(
                &cr[row * cw..row * cw + cw],
                &cr2[row * cw..row * cw + cw],
            );
        }
    }

    #[test]
    fn known_value_single_group() {
        // 6 pixels, 1 row
        let y = [100u16, 200, 300, 400, 500, 600];
        let cb = [64u16, 128, 192];
        let cr = [256u16, 384, 512];

        let packed = pack_v210(&y, &cb, &cr, 6, 1);
        assert_eq!(packed.len(), v210_row_stride(6));

        // Verify word 0: Cb[0]=64, Y[0]=100, Cr[0]=256
        let w0 = u32::from_le_bytes([packed[0], packed[1], packed[2], packed[3]]);
        assert_eq!(w0 & 0x3FF, 64);
        assert_eq!((w0 >> 10) & 0x3FF, 100);
        assert_eq!((w0 >> 20) & 0x3FF, 256);

        let (y2, cb2, cr2) = unpack_v210(&packed, 6, 1);
        assert_eq!(&y[..], &y2[..]);
        assert_eq!(&cb[..], &cb2[..]);
        assert_eq!(&cr[..], &cr2[..]);
    }

    #[test]
    fn stride_standard_resolutions() {
        assert_eq!(v210_row_stride(1920), 5120);  // 1920/48 = 40 groups
        assert_eq!(v210_row_stride(1280), 3456);  // ceil(1280/48) = 27 groups
        assert_eq!(v210_row_stride(3840), 10240); // 3840/48 = 80 groups
    }
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
            "tags": {{ "urn:x-nmos:tag:grouphint/v1.0": ["{flow_name}:Video"] }},
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
