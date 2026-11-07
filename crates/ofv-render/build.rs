//! Builds the data openfront-video carries inside itself from an OpenFrontIO
//! checkout ($OPENFRONT_ENGINE, default ../OpenFrontIO; it should be at the
//! engine commit the port follows, 9f4a28f):
//!
//! - every map's manifest, map.bin, map4x.bin and map16x.bin (compact maps),
//!   each map zstd-compressed
//!   (maps.bin, indexed by maps.rs)
//! - tribe names, the client's theme and render settings, the structure and
//!   unit sprite atlases
//! - the name font, converted from WOFF to TrueType
//!
//! Only openfront-video includes these files, so the engine-rs harness binary
//! stays small.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let engine = std::env::var("OPENFRONT_ENGINE")
        .ok()
        .map(PathBuf::from)
        .into_iter()
        .chain([
            manifest_dir.join("../../OpenFrontIO"),
            manifest_dir.join("../../../OpenFrontIO"),
            PathBuf::from("/home/dr_floral_funk/Documents/antigravity/modest-rutherford/data/legacy_quarantine/repos/OpenFrontIO"),
        ])
        .find(|p| p.join("resources/maps").is_dir())
        .unwrap_or_else(|| manifest_dir.join("../../OpenFrontIO"));
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    println!("cargo:rerun-if-env-changed=OPENFRONT_ENGINE");
    println!("cargo:rerun-if-changed=build.rs");

    let res = engine.join("resources");
    if !res.join("maps").is_dir() {
        panic!("no OpenFrontIO checkout at {} (set OPENFRONT_ENGINE)", engine.display());
    }

    let commit = std::process::Command::new("git")
        .args(["-C", &engine.to_string_lossy(), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=OPENFRONT_ENGINE_COMMIT={commit}");

    let copy = |from: PathBuf, name: &str| {
        println!("cargo:rerun-if-changed={}", from.display());
        fs::copy(&from, out.join(name)).unwrap_or_else(|e| panic!("{}: {e}", from.display()));
    };
    copy(res.join("tribeNameThemes.json"), "tribeNameThemes.json");
    copy(res.join("atlases/icon-atlas.png"), "icon-atlas.png");
    copy(res.join("atlases/unit-atlas.png"), "unit-atlas.png");
    copy(res.join("atlases/status-atlas.png"), "status-atlas.png");
    let gl = engine.join("src/client/render/gl");
    copy(gl.join("default-theme.json"), "default-theme.json");
    copy(gl.join("render-settings.json"), "render-settings.json");

    let woff = res.join("fonts/overpass-bold.woff");
    println!("cargo:rerun-if-changed={}", woff.display());
    fs::write(out.join("overpass-bold.ttf"), woff_to_sfnt(&fs::read(&woff).unwrap())).unwrap();

    build_maps(&res.join("maps"), &out);
}

/// Every map, each compressed on its own so only the one a game uses is
/// unpacked: [u32 length + bytes] for the manifest, map.bin, map4x.bin and map16x.bin.
fn build_maps(maps: &Path, out: &Path) {
    println!("cargo:rerun-if-changed={}", maps.display());
    let maps_bin = out.join("maps.bin");
    let maps_rs = out.join("maps.rs");
    if maps_bin.is_file()
        && maps_rs.is_file()
        && fs::metadata(&maps_bin).map(|m| m.len() > 0).unwrap_or(false)
    {
        return;
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(maps)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|d| d.join("manifest.json").is_file() && d.join("map.bin").is_file())
        .collect();
    dirs.sort();

    // Level 19 is slow, so the maps are compressed in parallel.
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Option<(String, Vec<u8>)>>> =
        dirs.iter().map(|_| std::sync::Mutex::new(None)).collect();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(dir) = dirs.get(i) else { break };
                let manifest = fs::read(dir.join("manifest.json")).unwrap();
                let name = serde_name(&manifest);
                let mut raw = Vec::new();
                let bin = |f: &str| fs::read(dir.join(f)).unwrap();
                for part in [manifest, bin("map.bin"), bin("map4x.bin"), bin("map16x.bin")] {
                    raw.extend_from_slice(&(part.len() as u32).to_le_bytes());
                    raw.extend_from_slice(&part);
                }
                let packed = zstd::bulk::compress(&raw, 19).unwrap();
                *results[i].lock().unwrap() = Some((name, packed));
            });
        }
    });

    let mut blob = Vec::new();
    let mut index = String::from("pub static MAPS: &[(&str, usize, usize)] = &[\n");
    for r in results {
        let (name, packed) = r.into_inner().unwrap().unwrap();
        index.push_str(&format!("    ({:?}, {}, {}),\n", name, blob.len(), packed.len()));
        blob.extend_from_slice(&packed);
    }
    index.push_str("];\n");
    fs::write(out.join("maps.bin"), blob).unwrap();
    fs::write(out.join("maps.rs"), index).unwrap();
}

/// The manifest's display name (its top-level "name").
fn serde_name(manifest: &[u8]) -> String {
    let v: serde_json::Value = serde_json::from_slice(manifest).expect("manifest.json");
    v["name"].as_str().expect("manifest without a name").to_string()
}

/// WOFF 1.0 to the TrueType/OpenType file it wraps: the same tables,
/// inflated, behind a plain sfnt header.
fn woff_to_sfnt(woff: &[u8]) -> Vec<u8> {
    let u32_at = |o: usize| u32::from_be_bytes(woff[o..o + 4].try_into().unwrap());
    let u16_at = |o: usize| u16::from_be_bytes(woff[o..o + 2].try_into().unwrap());
    assert_eq!(&woff[0..4], b"wOFF", "not a WOFF 1.0 font");
    let flavor = u32_at(4);
    let num_tables = u16_at(12) as usize;
    let mut tables = Vec::new();
    for i in 0..num_tables {
        let e = 44 + i * 20;
        let (tag, offset, comp_len, orig_len, checksum) = (u32_at(e), u32_at(e + 4) as usize, u32_at(e + 8) as usize, u32_at(e + 12) as usize, u32_at(e + 16));
        let data = &woff[offset..offset + comp_len];
        let table = if comp_len < orig_len {
            let mut v = Vec::with_capacity(orig_len);
            flate2::read::ZlibDecoder::new(data).read_to_end(&mut v).unwrap();
            v
        } else {
            data.to_vec()
        };
        tables.push((tag, checksum, table));
    }
    let mut sfnt = Vec::new();
    let mut entry_selector = 0u16;
    while (2usize << entry_selector) <= num_tables {
        entry_selector += 1;
    }
    let search_range = (1u16 << entry_selector) * 16;
    sfnt.write_all(&flavor.to_be_bytes()).unwrap();
    for v in [num_tables as u16, search_range, entry_selector, num_tables as u16 * 16 - search_range] {
        sfnt.write_all(&v.to_be_bytes()).unwrap();
    }
    let mut offset = 12 + 16 * num_tables;
    for (tag, checksum, table) in &tables {
        for v in [*tag, *checksum, offset as u32, table.len() as u32] {
            sfnt.write_all(&v.to_be_bytes()).unwrap();
        }
        offset += (table.len() + 3) & !3;
    }
    for (_, _, table) in &tables {
        sfnt.extend_from_slice(table);
        while sfnt.len() % 4 != 0 {
            sfnt.push(0);
        }
    }
    sfnt
}
