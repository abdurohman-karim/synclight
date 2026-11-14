//! App icon, drawn procedurally: a small monitor with a glow around it
//! (rainbow when syncing, grey otherwise).

/// RGBA pixels (straight alpha), `size` x `size`.
pub fn rgba(size: u32, active: bool) -> Vec<u8> {
    let n = size as usize;
    let sz = size as f32;
    let mut px = vec![0u8; n * n * 4];
    for y in 0..n {
        for x in 0..n {
            let u = (x as f32 + 0.5) / sz - 0.5;
            let v = (y as f32 + 0.5) / sz - 0.5;
            // Signed distance to a rounded 16:10 rectangle.
            let (hw, hh, r) = (0.27f32, 0.18f32, 0.05f32);
            let qx = u.abs() - (hw - r);
            let qy = v.abs() - (hh - r);
            let d = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r;

            let (gr, gg, gb) = if active {
                let h = (v.atan2(u) / std::f32::consts::TAU + 0.5) * 6.0;
                let f = |k: f32| ((((k + h) % 6.0) - 3.0).abs() - 1.0).clamp(0.0, 1.0);
                (f(0.0), f(4.0), f(2.0))
            } else {
                (0.62, 0.62, 0.66)
            };
            let glow_a = (1.0 - d.max(0.0) / 0.23).clamp(0.0, 1.0).powf(1.6);
            let (mr, mg, mb) = if d > -0.06 { (0.93, 0.93, 0.95) } else { (0.11, 0.12, 0.16) };
            let cover = (0.5 - d * sz).clamp(0.0, 1.0);

            let a = cover + glow_a * (1.0 - cover);
            let mix = |m: f32, g: f32| if a > 0.0 { (m * cover + g * glow_a * (1.0 - cover)) / a } else { 0.0 };
            let i = (y * n + x) * 4;
            px[i] = (mix(mr, gr) * 255.0) as u8;
            px[i + 1] = (mix(mg, gg) * 255.0) as u8;
            px[i + 2] = (mix(mb, gb) * 255.0) as u8;
            px[i + 3] = (a * 255.0) as u8;
        }
    }
    px
}
