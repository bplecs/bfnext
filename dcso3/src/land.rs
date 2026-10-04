/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `land` API: terrain height, surface type, line of
//! sight, terrain intersection and profiles, and road network queries.
//!
//! [`Land`] wraps the global `land` table of the mission scripting
//! environment.

use super::{as_tbl, cvt_err, LuaVec3};
use crate::{record_perf, simple_enum, wrapped_table, LuaEnv, LuaVec2, MizLua, Sequence};
use anyhow::Result;
use mlua::{prelude::*, Value};
use na::Vector2;
use serde_derive::{Deserialize, Serialize};
use std::ops::Deref;

// The type of surface at a point (`land.SurfaceType`).
simple_enum!(SurfaceType, u8, [
    Land => 1,
    ShallowWater => 2,
    Water => 3,
    Road => 4,
    Runway => 5
]);

/// Which network to use in the road queries of [`Land`]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoadType {
    Road,
    Rail,
}

// The global `land` table.
wrapped_table!(Land, None);

impl<'lua> Land<'lua> {
    /// Get the global `land` table
    pub fn singleton(lua: MizLua<'lua>) -> Result<Self> {
        Ok(lua.inner().globals().raw_get("land")?)
    }

    /// The terrain height at `p` (`land.getHeight`). Timed by
    /// [`record_perf!`](crate::record_perf) as `land_get_height`.
    pub fn get_height(&self, p: LuaVec2) -> Result<f64> {
        Ok(record_perf!(land_get_height, self.t.call_function("getHeight", p)?))
    }

    /// The surface height and the seabed depth at `p`
    /// (`land.getSurfaceHeightWithSeabed`)
    pub fn get_surface_height_with_seabed(&self, p: LuaVec2) -> Result<(f64, f64)> {
        Ok(self.t.call_function("getSurfaceHeightWithSeabed", p)?)
    }

    /// The type of surface at `p` (`land.getSurfaceType`)
    pub fn get_surface_type(&self, p: LuaVec2) -> Result<SurfaceType> {
        Ok(self.t.call_function("getSurfaceType", p)?)
    }

    /// True if the terrain does not block the line of sight from `origin` to
    /// `destination` (`land.isVisible`). Timed as `land_is_visible`.
    pub fn is_visible(&self, origin: LuaVec3, destination: LuaVec3) -> Result<bool> {
        Ok(record_perf!(land_is_visible, self.t.call_function("isVisible", (origin, destination))?))
    }

    /// The point where a ray from `origin` in `direction` hits the terrain,
    /// searching up to `distance` (`land.getIP`)
    pub fn get_ip(&self, origin: LuaVec3, direction: LuaVec3, distance: f64) -> Result<LuaVec3> {
        Ok(self
            .t
            .call_function("getIP", (origin, direction, distance))?)
    }

    /// Points along the terrain between `origin` and `destination`
    /// (`land.profile`)
    pub fn get_profile(&self, origin: LuaVec3, destination: LuaVec3) -> Result<Sequence<'lua, LuaVec3>> {
        Ok(self.t.call_function("profile", (origin, destination))?)
    }

    /// The point on the road or rail network closest to `from`
    /// (`land.getClosestPointOnRoads`)
    pub fn get_closest_point_on_roads(&self, typ: RoadType, from: LuaVec2) -> Result<LuaVec2> {
        // rail is passed as "railroads" here, but as "rails" to findPathOnRoads
        let typ = match typ {
            RoadType::Road => "roads",
            RoadType::Rail => "railroads",
        };
        let (x, y) = self
            .t
            .call_function("getClosestPointOnRoads", (typ, from.x, from.y))?;
        Ok(LuaVec2(Vector2::new(x, y)))
    }

    /// A path of points along the road or rail network from `origin` to
    /// `destination` (`land.findPathOnRoads`)
    pub fn find_path_on_roads(
        &self,
        typ: RoadType,
        origin: LuaVec2,
        destination: LuaVec2,
    ) -> Result<Sequence<'lua, LuaVec2>> {
        // unlike getClosestPointOnRoads, this one is passed "rails"
        let typ = match typ {
            RoadType::Road => "roads",
            RoadType::Rail => "rails",
        };
        Ok(self.t.call_function(
            "findPathOnRoads",
            (typ, origin.x, origin.y, destination.x, destination.y),
        )?)
    }
}
