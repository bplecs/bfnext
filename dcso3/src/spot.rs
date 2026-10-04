/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `Spot` scripting class: laser and infrared pointers.
//!
//! A [`Spot`] is created from a source object and points at a target
//! position until it is destroyed. It is used, for example, by JTACs to
//! designate targets.

use super::{as_tbl, object::Object};
use crate::{
    object::{DcsObject, DcsOid},
    wrapped_table, LuaEnv, LuaVec3, MizLua,
};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::Serialize;
use std::{marker::PhantomData, ops::Deref};

// A DCS laser or infrared spot.
wrapped_table!(Spot, Some("Spot"));

impl<'lua> Spot<'lua> {
    /// Create a laser spot from `source` pointing at `target` with laser
    /// code `code`. `local_ref`, if given, is the beam origin relative to
    /// the source object. Calls `Spot.createLaser`.
    pub fn create_laser(
        lua: MizLua<'lua>,
        source: Object<'lua>,
        local_ref: Option<LuaVec3>,
        target: LuaVec3,
        code: u16,
    ) -> Result<Self> {
        let globals = lua.inner().globals();
        let spot: LuaTable = globals.raw_get("Spot")?;
        Ok(spot.call_function("createLaser", (source, local_ref, target, code))?)
    }

    /// Create an infrared pointer from `source` pointing at `target`.
    /// `local_ref` is as in [`Spot::create_laser`]. Calls
    /// `Spot.createInfraRed`.
    pub fn create_infra_red(
        lua: MizLua<'lua>,
        source: Object<'lua>,
        local_ref: Option<LuaVec3>,
        target: LuaVec3,
    ) -> Result<Self> {
        let globals = lua.inner().globals();
        let spot: LuaTable = globals.raw_get("Spot")?;
        Ok(spot.call_function("createInfraRed", (source, local_ref, target))?)
    }

    /// Turn the spot off. Calls `Spot:destroy`.
    pub fn destroy(self) -> Result<()> {
        Ok(self.t.call_method("destroy", ())?)
    }

    /// The position the spot is pointing at
    pub fn get_point(&self) -> Result<LuaVec3> {
        Ok(self.t.call_method("getPoint", ())?)
    }

    /// Move the spot to point at `target`
    pub fn set_point(&self, target: LuaVec3) -> Result<()> {
        Ok(self.t.call_method("setPoint", target)?)
    }

    /// The laser code of a laser spot
    pub fn get_code(&self) -> Result<u16> {
        Ok(self.t.call_method("getCode", ())?)
    }

    /// Change the laser code of a laser spot
    pub fn set_code(&self, code: u16) -> Result<()> {
        Ok(self.t.call_method("setCode", code)?)
    }
}

/// Class marker for [`DcsOid`]s of [`Spot`]s
#[derive(Debug, Clone)]
pub struct ClassSpot;

// Unlike most other DcsObject impls, these don't check that the spot still
// exists.

impl<'lua> DcsObject<'lua> for Spot<'lua> {
    type Class = ClassSpot;

    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self> {
        let t = lua.inner().create_table()?;
        t.set_metatable(Some(lua.inner().globals().raw_get(&**id.class)?));
        t.raw_set("id_", id.id)?;
        let t = Self {
            t,
            lua: lua.inner(),
        };
        Ok(t)
    }

    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(lua, "Spot")?;
        let id = DcsOid {
            id: id.id,
            class: id.class.clone(),
            t: PhantomData,
        };
        Self::get_instance(lua, &id)
    }

    fn change_instance(self, id: &DcsOid<Self::Class>) -> Result<Self> {
        self.raw_set("id_", id.id)?;
        Ok(self)
    }

    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(MizLua(self.lua), "Spot")?;
        self.t.raw_set("id_", id.id)?;
        Ok(self)
    }
}
