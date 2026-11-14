//! Per-segment colour processing, matching SyncLight's defaults.

use anyhow::{bail, Result};

pub struct Pipeline {
    /// Output channel i takes input channel wire[i] (strip colour order).
    pub wire: [usize; 3],
    /// All channels <= this value -> off (original `thresholdTint` = 20).
    pub black: u8,
    /// Original "convertToBlack" tweak: one channel >= 200 and the others <= 50
    /// -> make it a pure, full-intensity colour.
    pub vivid: bool,
    /// Brightness multiplier applied after the black threshold (1.0 = as captured).
    pub gain: f32,
}

impl Pipeline {
    pub fn wire_map(s: &str) -> Result<[usize; 3]> {
        let s = s.to_ascii_uppercase();
        if s.len() != 3 {
            bail!("порядок цветов должен быть из 3 букв, например RGB");
        }
        let mut out = [0; 3];
        for (i, ch) in s.chars().enumerate() {
            out[i] = match ch {
                'R' => 0,
                'G' => 1,
                'B' => 2,
                _ => bail!("неверный порядок цветов: {s}"),
            };
        }
        Ok(out)
    }

    /// Colour as it should look (black threshold, gain, vivid), in screen RGB order.
    /// Used for the live preview in the settings window.
    pub fn look(&self, c: [f32; 3]) -> [u8; 3] {
        let mut o = c.map(|x| x.round().clamp(0.0, 255.0) as u8);
        if o.iter().all(|&x| x <= self.black) {
            return [0, 0, 0];
        }
        if self.gain != 1.0 {
            o = o.map(|x| (x as f32 * self.gain).round().clamp(0.0, 255.0) as u8);
        }
        if self.vivid {
            let hi = (0..3).filter(|&i| o[i] >= 200).count();
            let lo = (0..3).filter(|&i| o[i] <= 50).count();
            if hi == 1 && lo == 2 {
                o = o.map(|x| if x >= 200 { 255 } else { 0 });
            }
        }
        o
    }

    /// Bytes for the strip: `look` reordered for the strip wiring, within the USB power budget.
    pub fn to_strip(&self, look: [u8; 3]) -> [u8; 3] {
        limit_power([look[self.wire[0]], look[self.wire[1]], look[self.wire[2]]])
    }

    pub fn apply(&self, c: [f32; 3]) -> [u8; 3] {
        self.to_strip(self.look(c))
    }
}

/// The strip is powered from USB: SyncLight never lets r+g+b exceed 255 for this model
/// ("isLightCompression"). Kept unconditionally to stay within the same current budget.
pub fn limit_power(mut o: [u8; 3]) -> [u8; 3] {
    let sum = o[0] as u32 + o[1] as u32 + o[2] as u32;
    if sum > 255 {
        let f = sum as f32 / 255.0;
        for x in &mut o {
            *x = (*x as f32 / f) as u8;
        }
    }
    o
}
