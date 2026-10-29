//! The data built into the binary by build.rs.

use engine_rs::replay::{MapFiles, Resources};
use serde_json::Value;

mod maps {
    include!(concat!(env!("OUT_DIR"), "/maps.rs"));
}

static MAP_BLOB: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/maps.bin"));
static TRIBE_NAMES: &str = include_str!(concat!(env!("OUT_DIR"), "/tribeNameThemes.json"));
pub static THEME: &str = include_str!(concat!(env!("OUT_DIR"), "/default-theme.json"));
pub static RENDER_SETTINGS: &str = include_str!(concat!(env!("OUT_DIR"), "/render-settings.json"));
pub static ICON_ATLAS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon-atlas.png"));
pub static UNIT_ATLAS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/unit-atlas.png"));
pub static FONT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/overpass-bold.ttf"));
/// The OpenFrontIO commit the assets came from (empty if unknown).
pub static ENGINE_COMMIT: &str = env!("OPENFRONT_ENGINE_COMMIT");

pub struct Embedded;

impl Resources for Embedded {
    fn map(&self, name: &str) -> MapFiles {
        let Some(&(_, at, len)) = maps::MAPS.iter().find(|m| m.0 == name) else {
            panic!("the map {name} is not built in (maps from OpenFrontIO {})", &ENGINE_COMMIT[..ENGINE_COMMIT.len().min(9)]);
        };
        let raw = zstd::stream::decode_all(&MAP_BLOB[at..at + len]).expect("unpack map");
        let mut parts = Vec::new();
        let mut i = 0;
        while i < raw.len() {
            let n = u32::from_le_bytes(raw[i..i + 4].try_into().unwrap()) as usize;
            parts.push(raw[i + 4..i + 4 + n].to_vec());
            i += 4 + n;
        }
        let [manifest, map, map4x]: [Vec<u8>; 3] = parts.try_into().unwrap();
        MapFiles { manifest: serde_json::from_slice(&manifest).unwrap(), map, map4x }
    }

    fn tribe_name_themes(&self) -> Value {
        serde_json::from_str(TRIBE_NAMES).unwrap()
    }
}
