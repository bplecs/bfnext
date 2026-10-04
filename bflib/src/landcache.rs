//! Cache for DCS terrain line of sight checks.
//!
//! `land.isVisible` is called very frequently (jtac target detection, EWR
//! coverage, objective threat checks) and is expensive. [`LandCache`]
//! quantizes both endpoints of a query into tiles whose size scales with the
//! distance between them, and caches the result per pair of tiles. Visible
//! results are sticky; not visible results are periodically rechecked so a
//! unit that moves into view within the same tile is eventually seen.

use anyhow::Result;
use core::fmt;
use dcso3::{LuaVec3, Vector3, land::Land};
use fxhash::FxBuildHasher;
use indexmap::{IndexMap, map::Entry};
use std::{cmp::max, hash::Hash};

/// A cube of space used as a cache key. `x`, `y`, `z` are the tile indexes
/// (position divided by the tile size) and `d` is the tile edge length in
/// meters. Including `d` keeps tiles of different scales distinct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct Tile {
    x: i32,
    y: i32,
    z: i32,
    d: u32,
}

impl Tile {
    /// Compute the tile containing `v` for a line of sight query spanning
    /// `d` meters. Longer queries use coarser tiles (minimum 1 meter).
    fn new(d: f64, v: Vector3) -> Self {
        // tile size is 1 / 32th of the distance between the two
        // points being checked rounded to the nearest power of 2
        let d = max(1, ((d.trunc() as i64) >> 5) as u32).next_power_of_two();
        let df = d as f64;
        let x = v.x.div_euclid(df) as i32;
        let y = v.y.div_euclid(df) as i32;
        let z = v.z.div_euclid(df) as i32;
        Self { x, y, z, d }
    }
}

#[derive(Debug, Clone, Copy)]
struct CacheEntry {
    /// Cached result. Once true it stays true for the life of the entry.
    visible: bool,
    /// Cache hits since the entry was created or last rechecked. Also used
    /// to rank entries for eviction.
    hits: u32,
}

/// Cache performance counters, logged periodically.
#[derive(Debug, Clone, Copy)]
pub struct Stats {
    /// Total calls to [`LandCache::is_visible`].
    pub calls: usize,
    /// Calls answered from the cache without querying DCS.
    pub hits: usize,
    /// Intended to count cached answers that differed from DCS.
    pub diffs: usize,
}

impl std::fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hitrate = (self.hits as f32 / self.calls as f32) * 100.;
        let diffrate = (self.diffs as f32 / self.hits as f32) * 100.;
        write!(
            f,
            "calls: {}, hits: {}({:.02}%), diffs: {}({:.02}%)",
            self.calls, self.hits, hitrate, self.diffs, diffrate
        )
    }
}

/// Tile pair keyed cache of terrain line of sight results.
#[derive(Debug, Clone)]
pub struct LandCache {
    h: IndexMap<(Tile, Tile), CacheEntry, FxBuildHasher>,
    /// Number of entries kept after an eviction pass.
    max_size: usize,
    /// Entries inserted since the last eviction pass.
    added: usize,
    stats: Stats,
}

impl Default for LandCache {
    fn default() -> Self {
        Self::new(10 * 1024 * 1024)
    }
}

impl LandCache {
    /// Create a cache holding up to `max_size` entries. Capacity for
    /// `max_size` entries is allocated up front.
    pub fn new(max_size: usize) -> LandCache {
        Self {
            h: IndexMap::with_capacity_and_hasher(max_size, FxBuildHasher::default()),
            added: 0,
            max_size,
            stats: Stats {
                calls: 0,
                hits: 0,
                diffs: 0,
            },
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Return whether `p1` is visible from `p0` over the terrain, using the
    /// cache when possible. `d` is the distance between the points in
    /// meters, used to pick the tile size. Errors only if the underlying DCS
    /// `land.isVisible` call fails.
    ///
    /// The answer is approximate: any two points in the same tile pair share
    /// a result, and a visible result is never revoked.
    pub fn is_visible(&mut self, land: &Land, d: f64, p0: Vector3, p1: Vector3) -> Result<bool> {
        self.stats.calls += 1;
        let t0 = Tile::new(d, p0);
        let t1 = Tile::new(d, p1);
        let ans = match self.h.entry((t0, t1)) {
            Entry::Occupied(mut e) => {
                let ent = e.get_mut();
                // not visible results are rechecked with DCS every 10 hits
                if ent.visible || ent.hits < 10 {
                    self.stats.hits += 1;
                    ent.hits += 1;
                    Ok(ent.visible)
                } else {
                    let visible = land.is_visible(LuaVec3(p0), LuaVec3(p1))?;
                    ent.visible |= visible;
                    ent.hits = 0;
                    Ok(visible)
                }
            }
            Entry::Vacant(e) => {
                let visible = land.is_visible(LuaVec3(p0), LuaVec3(p1))?;
                e.insert(CacheEntry { visible, hits: 1 });
                self.added += 1;
                Ok(visible)
            }
        };
        // evict the least hit entries once max_size new entries have been
        // added since the last pass, so the map can hold up to ~2x max_size
        if self.added > self.max_size {
            self.added = 0;
            self.h.sort_by(|_, e0, _, e1| e1.hits.cmp(&e0.hits));
            while self.h.len() > self.max_size {
                self.h.pop();
            }
        }
        ans
    }
}
