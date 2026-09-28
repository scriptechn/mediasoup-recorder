//! Layout policy as data (`docs/layout.md`): `policies/<name>.json`, shipped in the image, its
//! version stamped into the manifest. Every number the layout uses lives here, not in code.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub name: String,
    pub version: u32,
    pub canvas: Canvas,
    pub gap_px: u32,
    pub aspect_ratio: f64,
    pub max_grid_tiles: usize,
    pub name_strip_height_px: u32,
    pub share_stage_width_ratio: f64,
    pub filmstrip_tiles: usize,
    pub label_height_px: u32,
    pub label_font_px: f32,
    pub slate_initials_font_px: f32,
    pub speaker_border_px: u32,
    pub tile_corner_radius_px: u32,
    pub transition_ms: u64,
    pub speaker_hold_ms: u64,
    pub freeze_max_ms: u64,
    pub thumbnail_at_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Canvas {
    pub width: u32,
    pub height: u32,
}

impl Policy {
    pub fn load(dir: &Path, name: &str) -> Result<Self> {
        let file = dir.join(format!("{name}.json"));
        let text =
            std::fs::read_to_string(&file).with_context(|| format!("policy {}", file.display()))?;
        let policy: Policy = serde_json::from_str(&text)
            .with_context(|| format!("policy {} is not valid", file.display()))?;
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_auto_policy_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../policies");
        let p = Policy::load(&dir, "auto").expect("auto.json");
        assert_eq!(p.name, "auto");
        assert_eq!(p.max_grid_tiles, 9);
        assert_eq!(p.canvas.width, 1280);
    }
}
