/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the `DCS` table of the hooks (GUI/server) environment.
//!
//! [`Dcs`] wraps the global `DCS` table, which controls the running
//! mission (pause, stop, exit) and reports information about it (name,
//! file, options, slots, model and real time). It is only available in the
//! hooks environment, hence [`Dcs::singleton`] takes a [`HooksLua`].

use super::{as_tbl, coalition::Side, String};
use crate::{wrapped_table, HooksLua, LuaEnv, Time};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::Serialize;
use std::ops::Deref;

// The global `DCS` table of the hooks environment.
wrapped_table!(Dcs, None);

impl<'lua> Dcs<'lua> {
    /// Get the global `DCS` table. Fails if it isn't a table.
    pub fn singleton(lua: HooksLua<'lua>) -> Result<Self> {
        let globals = lua.inner().globals();
        Ok(globals.raw_get("DCS")?)
    }

    /// The name of the current mission (`DCS.getMissionName`)
    pub fn get_mission_name(&self) -> Result<String> {
        Ok(self.t.call_function("getMissionName", ())?)
    }

    /// The file name of the current mission (`DCS.getMissionFilename`)
    pub fn get_mission_filename(&self) -> Result<String> {
        Ok(self.t.call_function("getMissionFilename", ())?)
    }

    /// The mission result for `side` (`DCS.getMissionResult`)
    pub fn get_mission_result(&self, side: Side) -> Result<i64> {
        Ok(self.t.call_function("getMissionResult", side)?)
    }

    /// Call `DCS.getUnitProperty` with `name`, returning the raw Lua result
    pub fn get_unit_property(&self, name: String) -> Result<Value<'lua>> {
        Ok(self.t.call_function("getUnitProperty", name)?)
    }

    /// Pause or unpause the simulation (`DCS.setPause`)
    pub fn set_pause(&self, pause: bool) -> Result<()> {
        Ok(self.t.call_function("setPause", pause)?)
    }

    /// True if the simulation is paused (`DCS.getPause`)
    pub fn get_pause(&self) -> Result<bool> {
        Ok(self.t.call_function("getPause", ())?)
    }

    /// Stop the running mission (`DCS.stopMission`)
    pub fn stop_mission(&self) -> Result<()> {
        Ok(self.t.call_function("stopMission", ())?)
    }

    /// Exit the DCS process (`DCS.exitProcess`)
    pub fn exit_process(&self) -> Result<()> {
        Ok(self.t.call_function("exitProcess", ())?)
    }

    /// `DCS.isMultiplayer`
    pub fn is_multiplayer(&self) -> Result<bool> {
        Ok(self.t.call_function("isMultiplayer", ())?)
    }

    /// `DCS.isServer`
    pub fn is_server(&self) -> Result<bool> {
        Ok(self.t.call_function("isServer", ())?)
    }

    /// The simulation (model) time (`DCS.getModelTime`)
    pub fn get_model_time(&self) -> Result<Time> {
        Ok(self.t.call_function("getModelTime", ())?)
    }

    /// The real (wall clock) time (`DCS.getRealTime`)
    pub fn get_real_time(&self) -> Result<Time> {
        Ok(self.t.call_function("getRealTime", ())?)
    }

    /// The mission options table (`DCS.getMissionOptions`)
    pub fn get_mission_options(&self) -> Result<LuaTable<'lua>> {
        Ok(self.t.call_function("getMissionOptions", ())?)
    }

    /// The coalitions players can join (`DCS.getAvailableCoalitions`)
    pub fn get_available_coalitions(&self) -> Result<LuaTable<'lua>> {
        Ok(self.t.call_function("getAvailableCoalitions", ())?)
    }

    /// The player slots table (`DCS.getAvailableSlots`), called with no
    /// arguments
    pub fn get_available_slots(&self) -> Result<LuaTable<'lua>> {
        Ok(self.t.call_function("getAvailableSlots", ())?)
    }

    /// The current mission's data table (`DCS.getCurrentMission`)
    pub fn get_current_mission(&self) -> Result<LuaTable<'lua>> {
        Ok(self.t.call_function("getCurrentMission", ())?)
    }
}
