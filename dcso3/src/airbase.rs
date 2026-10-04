/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `Airbase` scripting class.
//!
//! Airbases include airfields, FARPs, and ships with flight decks. Each
//! has a coalition, runways, parking, and a [`Warehouse`].

use super::{as_tbl, coalition::Side, object::Object, warehouse::Warehouse, LuaVec3, String};
use crate::{
    object::{DcsObject, DcsOid},
    wrapped_table, LuaEnv, MizLua, Sequence, wrapped_prim,
};
use anyhow::{bail, Result};
use mlua::{prelude::*, Value};
use serde_derive::{Serialize, Deserialize};
use std::{marker::PhantomData, ops::Deref};

// The identifier of a runway, read from the runway table's `Name` field.
wrapped_prim!(RunwayId, i64, Hash, Copy);
// The DCS id of an airbase.
wrapped_prim!(AirbaseId, i64, Hash, Copy);

// One entry of the table returned by `Airbase::get_runways`.
wrapped_table!(Runway, None);

impl<'lua> Runway<'lua> {
    /// The runway's `Name` field
    pub fn id(&self) -> Result<RunwayId> {
        Ok(self.t.raw_get("Name")?)
    }

    pub fn course(&self) -> Result<f64> {
        Ok(self.t.raw_get("course")?)
    }

    /// The position of the runway
    pub fn position(&self) -> Result<LuaVec3> {
        Ok(self.t.raw_get("position")?)
    }

    pub fn length(&self) -> Result<f64> {
        Ok(self.t.raw_get("length")?)
    }

    pub fn width(&self) -> Result<f64> {
        Ok(self.t.raw_get("width")?)
    }
}

// The parking table returned by `Airbase::get_parking`. No accessors are
// provided; use the underlying table directly.
wrapped_table!(Parking, None);

// A DCS airbase object.
wrapped_table!(Airbase, Some("Airbase"));

impl<'lua> Airbase<'lua> {
    /// Look up an airbase by name. Calls `Airbase.getByName`; returns an
    /// error if no airbase is found.
    pub fn get_by_name(lua: MizLua<'lua>, name: String) -> Result<Self> {
        let globals = lua.inner().globals();
        let airbase: LuaTable = globals.raw_get("Airbase")?;
        Ok(airbase.call_function("getByName", name)?)
    }

    pub fn is_exist(&self) -> Result<bool> {
        Ok(self.t.call_method("isExist", ())?)
    }

    pub fn destroy(&self) -> Result<()> {
        Ok(self.t.call_method("destroy", ())?)
    }

    /// The airbase's description table, returned raw. Calls
    /// `Airbase:getDesc`.
    pub fn get_desc(&self) -> Result<mlua::Table<'lua>> {
        Ok(self.t.call_method("getDesc", ())?)
    }
    
    /// The airbase's position in world coordinates
    pub fn get_point(&self) -> Result<LuaVec3> {
        Ok(self.t.call_method("getPoint", ())?)
    }

    /// The airbase's callsign. Calls `Airbase:getCallsign`.
    pub fn get_callsign(&self) -> Result<String> {
        Ok(self.t.call_method("getCallsign", ())?)
    }

    /// The `i`th unit associated with the airbase (e.g. the ship of a
    /// carrier). Calls `Airbase:getUnit`.
    pub fn get_unit(&self, i: i64) -> Result<Object<'lua>> {
        Ok(self.t.call_method("getUnit", i)?)
    }

    /// The airbase's DCS id. Calls the method `getId`.
    pub fn get_id(&self) -> Result<AirbaseId> {
        Ok(self.t.call_method("getId", ())?)
    }

    /// The airbase's parking spots. `available` is passed to
    /// `Airbase:getParking` to restrict the result to available spots.
    pub fn get_parking(&self, available: bool) -> Result<Parking<'lua>> {
        Ok(self.t.call_method("getParking", available)?)
    }

    /// The airbase's runways. Calls `Airbase:getRunways`.
    pub fn get_runways(&self) -> Result<Sequence<'lua, Runway<'lua>>> {
        Ok(self.t.call_method("getRunways", ())?)
    }

    /// The position of the airbase's technical object called `obj`. Calls
    /// `Airbase:getTechObjectPos`.
    pub fn get_tech_object_pos(&self, obj: String) -> Result<LuaVec3> {
        Ok(self.t.call_method("getTechObjectPos", obj)?)
    }

    /// True if the airbase's ATC radio is silenced. Calls
    /// `Airbase:getRadioSilentMode`.
    pub fn get_radio_silent_mode(&self) -> Result<bool> {
        Ok(self.t.call_method("getRadioSilentMode", ())?)
    }

    /// Silence or unsilence the airbase's ATC radio. Calls
    /// `Airbase:setRadioSilentMode`.
    pub fn set_radio_silent_mode(&self, on: bool) -> Result<()> {
        Ok(self.t.call_method("setRadioSilentMode", on)?)
    }

    /// Enable or disable DCS's automatic capture of the airbase by nearby
    /// ground units. Calls `Airbase:autoCapture`.
    pub fn auto_capture(&self, on: bool) -> Result<()> {
        Ok(self.t.call_method("autoCapture", on)?)
    }

    /// True if automatic capture is enabled. Calls `Airbase:autoCaptureIsOn`.
    pub fn auto_capture_is_on(&self) -> Result<bool> {
        Ok(self.t.call_method("autoCaptureIsOn", ())?)
    }

    /// Change the airbase's coalition. Calls `Airbase:setCoalition`.
    pub fn set_coalition(&self, coa: Side) -> Result<()> {
        Ok(self.t.call_method("setCoalition", coa)?)
    }

    /// The airbase's warehouse. Calls `Airbase:getWarehouse`.
    pub fn get_warehouse(&self) -> Result<Warehouse<'lua>> {
        Ok(self.t.call_method("getWarehouse", ())?)
    }

    /// View this airbase as a generic [`Object`]
    pub fn as_object(&self) -> Result<Object<'lua>> {
        Ok(Object::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }
}

/// Class marker for [`DcsOid`]s of [`Airbase`]s
#[derive(Debug, Clone)]
pub struct ClassAirbase;

// get_instance and change_instance return an error if the airbase no
// longer exists.

impl<'lua> DcsObject<'lua> for Airbase<'lua> {
    type Class = ClassAirbase;

    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self> {
        let t = lua.inner().create_table()?;
        t.set_metatable(Some(lua.inner().globals().raw_get(&**id.class)?));
        t.raw_set("id_", id.id)?;
        let t = Airbase {
            t,
            lua: lua.inner(),
        };
        if !t.is_exist()? {
            bail!("{} is an invalid airbase", id.id)
        }
        Ok(t)
    }

    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(lua, "Airbase")?;
        let id = DcsOid {
            id: id.id,
            class: id.class.clone(),
            t: PhantomData,
        };
        Self::get_instance(lua, &id)
    }

    fn change_instance(self, id: &DcsOid<Self::Class>) -> Result<Self> {
        self.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid airbase", id.id)
        }
        Ok(self)
    }

    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(MizLua(self.lua), "Airbase")?;
        self.t.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid airbase", id.id)
        }
        Ok(self)
    }
}
