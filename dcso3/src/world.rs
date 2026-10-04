/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `world` singleton.
//!
//! [`World`] registers event handlers, searches for objects in a volume,
//! removes junk, and lists airbases and map mark panels.

use super::{as_tbl, event::Event, unit::Unit, wrap_f, String};
use crate::{
    airbase::Airbase,
    atomic_id, cvt_err,
    env::miz::GroupId,
    object::{Object, ObjectCategory},
    trigger::{MarkId, SideFilter},
    wrapped_table, LuaEnv, LuaVec3, MizLua, Position3, Sequence, Time,
};
use anyhow::Result;
use compact_str::format_compact;
use log::warn;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::ops::Deref;

/// A volume of space for [`World::search_objects`] and
/// [`World::remove_junk`]. Converted to the DCS volume table
/// `{id = world.VolumeType value, params = {...}}`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum SearchVolume {
    /// A line segment between two points
    Segment {
        from: LuaVec3,
        to: LuaVec3,
    },
    /// An axis aligned box with opposite corners `min` and `max`
    Box {
        min: LuaVec3,
        max: LuaVec3,
    },
    /// A sphere around `point`
    Sphere {
        point: LuaVec3,
        radius: f64,
    },
    /// A pyramid with its apex at `pos`, pointing along the orientation of
    /// `pos`, extending `length` with the given horizontal and vertical
    /// half angles
    Pyramid {
        pos: Position3,
        length: f32,
        half_angle_hor: f32,
        half_angle_ver: f32,
    },
}

impl<'lua> IntoLua<'lua> for SearchVolume {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        let tbl = lua.create_table()?;
        let params = lua.create_table()?;
        match self {
            Self::Segment { from, to } => {
                tbl.raw_set("id", 0)?;
                params.raw_set("from", from)?;
                params.raw_set("to", to)?;
            }
            Self::Box { min, max } => {
                tbl.raw_set("id", 1)?;
                params.raw_set("min", min)?;
                params.raw_set("max", max)?;
            }
            Self::Sphere { point, radius } => {
                tbl.raw_set("id", 2)?;
                params.raw_set("point", point)?;
                params.raw_set("radius", radius)?;
            }
            Self::Pyramid {
                pos,
                length,
                half_angle_hor,
                half_angle_ver,
            } => {
                tbl.raw_set("id", 3)?;
                params.raw_set("pos", pos)?;
                params.raw_set("length", length)?;
                params.raw_set("halfAngleHor", half_angle_hor)?;
                params.raw_set("halfAngleVer", half_angle_ver)?;
            }
        }
        tbl.raw_set("params", params)?;
        Ok(Value::Table(tbl))
    }
}

/// A map mark (F10 map marker), as returned by [`World::get_mark_panels`]
#[derive(Debug, Clone, Serialize)]
pub struct MarkPanel<'lua> {
    pub id: MarkId,
    pub time: Time,
    pub initiator: Option<Unit<'lua>>,
    /// Who the mark is visible to
    pub side: SideFilter,
    /// The group the mark is visible to, `None` if not restricted to a group
    pub group_id: Option<GroupId>,
    pub text: String,
    pub pos: LuaVec3,
}

impl<'lua> FromLua<'lua> for MarkPanel<'lua> {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        let tbl = LuaTable::from_lua(value, lua)?;
        Ok(Self {
            id: tbl.raw_get("idx")?,
            time: tbl.raw_get("time")?,
            initiator: tbl.raw_get("initiator")?,
            // DCS reports a mark visible to all as -1 or 255
            side: match tbl.raw_get::<_, i64>("coalition")? {
                -1 | 255 => SideFilter::All,
                0 => SideFilter::Neutral,
                1 => SideFilter::Red,
                2 => SideFilter::Blue,
                _ => return Err(cvt_err("side filter")),
            },
            group_id: match tbl.raw_get::<_, i64>("groupID")? {
                -1 => None,
                n => Some(GroupId::from(n)),
            },
            text: tbl.raw_get("text")?,
            pos: tbl.raw_get("pos")?,
        })
    }
}

// Identifies an event handler registered with `World::add_event_handler`.
atomic_id!(HandlerId);

impl HandlerId {
    /// The global variable name the handler table is stored under
    fn key(&self) -> String {
        String(format_compact!("rustHandler{}", self.0))
    }
}

// The global `world` table.
wrapped_table!(World, None);

impl<'lua> World<'lua> {
    /// Get the global `world` table
    pub fn singleton(lua: MizLua<'lua>) -> Result<Self> {
        Ok(lua.inner().globals().raw_get("world")?)
    }

    /// Register `f` to be called with every DCS event. Calls
    /// `world.addEventHandler` with a table whose `onEvent` calls `f`.
    /// Events that fail to convert to [`Event`] are logged and skipped, and
    /// errors and panics in `f` are logged. The returned id can be passed
    /// to [`World::remove_event_handler`].
    pub fn add_event_handler<F>(&self, f: F) -> Result<HandlerId>
    where
        F: Fn(MizLua<'lua>, Event) -> Result<()> + 'static,
    {
        let globals = self.lua.globals();
        let id = HandlerId::new();
        let tbl = self.lua.create_table()?;
        tbl.set(
            "onEvent",
            self.lua
                .create_function(move |lua, (_, ev): (Value, Value)| {
                    match Event::from_lua(ev, lua) {
                        Ok(ev) => wrap_f("event handler", MizLua(lua), |lua| f(lua, ev)),
                        Err(e) => {
                            warn!("error translating event: {:?}", e);
                            Ok(())
                        }
                    }
                })?,
        )?;
        self.t.call_function::<_, ()>("addEventHandler", tbl.clone())?;
        // keep the handler table in a global so remove_event_handler can
        // pass the same table back to DCS
        globals.raw_set(id.key(), tbl)?;
        Ok(id)
    }

    /// Unregister a handler added by [`World::add_event_handler`]. Calls
    /// `world.removeEventHandler`. Returns an error if `id` is not
    /// registered.
    pub fn remove_event_handler(&self, id: HandlerId) -> Result<()> {
        let globals = self.lua.globals();
        let key = id.key();
        let handler = globals.raw_get(key.clone())?;
        let handler = as_tbl("EventHandler", None, handler)?;
        self.t.call_function::<_, ()>("removeEventHandler", handler)?;
        globals.raw_remove(key)?;
        Ok(())
    }

    /// Calls `world.getPlayer`
    pub fn get_player(&self) -> Result<Sequence<'lua, Unit<'lua>>> {
        Ok(self.t.call_function("getPlayer", ())?)
    }

    /// Every airbase in the mission. Calls `world.getAirbases`.
    pub fn get_airbases(&self) -> Result<Sequence<'lua, Airbase<'lua>>> {
        Ok(self.t.call_function("getAirbases", ())?)
    }

    /// Find objects of `category` inside `volume`. Calls
    /// `world.searchObjects`, which calls `f` with each object found and
    /// `arg`. `f` returns true to continue the search. Errors and panics
    /// in `f` are logged and treated as returning false.
    pub fn search_objects<F, T>(
        &self,
        category: ObjectCategory,
        volume: SearchVolume,
        arg: T,
        f: F,
    ) -> Result<()>
    where
        T: IntoLua<'lua> + FromLua<'lua>,
        F: Fn(MizLua, Object<'lua>, T) -> Result<bool> + 'static,
    {
        let f = self
            .lua
            .create_function(move |lua, (o, arg): (Object, T)| {
                wrap_f("searchObjects", MizLua(lua), |lua| f(lua, o, arg))
            })?;
        Ok(self
            .t
            .call_function("searchObjects", (category, volume, f, arg))?)
    }

    /// Remove debris (e.g. wreckage) inside `volume`. Calls
    /// `world.removeJunk` and returns the number of objects it reports
    /// removing.
    pub fn remove_junk(&self, volume: SearchVolume) -> Result<i64> {
        Ok(self.t.call_function("removeJunk", volume)?)
    }

    /// Every map mark currently placed. Calls `world.getMarkPanels`.
    pub fn get_mark_panels(&self) -> Result<Sequence<'lua, MarkPanel<'lua>>> {
        Ok(self.t.call_function("getMarkPanels", ())?)
    }
}
