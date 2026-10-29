//! A Rust port of the OpenFront.io simulation (engine commit 9f4a28f),
//! checked tick by tick against the TypeScript engine. Used by the
//! `engine-rs` harness binary and by `openfront-video`.

pub mod detmath;
pub mod fxhash;
pub mod game;
pub mod heap;
pub mod js;
pub mod map;
pub mod nation_names;
pub mod prof;
pub mod replay;
pub mod rng;
pub mod tileset;
pub mod water;
