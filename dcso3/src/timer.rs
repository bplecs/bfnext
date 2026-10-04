/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `timer` singleton: mission time and scheduled
//! functions.
//!
//! [`Timer::schedule_function`] registers a Rust closure with
//! `timer.scheduleFunction`; it is the main way to run code periodically
//! from the mission scripting environment.

use crate::{as_tbl, cvt_err, record_perf, wrap_f, wrapped_table, LuaEnv, MizLua, Time};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::Serialize;
use std::ops::Deref;

/// The id DCS returns for a scheduled function, used to remove or
/// reschedule it
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct FunId(i64);

impl<'lua> FromLua<'lua> for FunId {
    fn from_lua(value: Value<'lua>, _lua: &'lua Lua) -> LuaResult<Self> {
        match value {
            Value::Integer(i) => Ok(FunId(i)),
            _ => Err(cvt_err("FunId")),
        }
    }
}

impl<'lua> IntoLua<'lua> for FunId {
    fn into_lua(self, _lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        Ok(Value::Integer(self.0))
    }
}

// The global `timer` table.
wrapped_table!(Timer, None);

impl<'lua> Timer<'lua> {
    /// Get the global `timer` table
    pub fn singleton(lua: MizLua<'lua>) -> Result<Self> {
        Ok(lua.inner().globals().raw_get("timer")?)
    }

    /// Mission time in seconds since the mission started. Calls
    /// `timer.getTime`.
    pub fn get_time(&self) -> Result<Time> {
        Ok(record_perf!(
            timer_get_time,
            self.t.call_function("getTime", ())?
        ))
    }

    /// Absolute mission time in seconds (time of day in the mission). Calls
    /// `timer.getAbsTime`.
    pub fn get_abs_time(&self) -> Result<Time> {
        Ok(record_perf!(
            timer_get_abs_time,
            self.t.call_function("getAbsTime", ())?
        ))
    }

    /// The absolute time at which the mission started. Calls
    /// `timer.getTime0`.
    pub fn get_time0(&self) -> Result<Time> {
        Ok(record_perf!(
            timer_get_time0,
            self.t.call_function("getTime0", ())?
        ))
    }

    /// Schedule `f` to run at mission time `when` (see
    /// [`Timer::get_time`]). Calls `timer.scheduleFunction`.
    ///
    /// `f` is called with `arg` and the current time. Its return value is
    /// handed back to DCS: `Some(t)` reschedules it to run again at time
    /// `t`, `None` stops it. An error or panic in `f` is logged and treated
    /// as `None`.
    pub fn schedule_function<T, F>(&self, when: Time, arg: T, f: F) -> Result<FunId>
    where
        F: Fn(MizLua, T, Time) -> Result<Option<Time>> + 'static,
        T: IntoLua<'lua> + FromLua<'lua>,
    {
        let f = self
            .lua
            .create_function(move |lua, (arg, time): (T, Time)| {
                // wrap_f logs errors and returns the default (None) so a
                // failing callback doesn't raise a Lua error inside DCS
                wrap_f("scheduled function", MizLua(lua), |lua| f(lua, arg, time))
            })?;
        Ok(record_perf!(
            timer_schedule_function,
            self.t.call_function("scheduleFunction", (f, arg, when))?
        ))
    }

    /// Cancel a scheduled function. Calls `timer.removeFunction`.
    pub fn remove_function(&self, id: FunId) -> Result<()> {
        Ok(record_perf!(
            timer_remove_function,
            self.t.call_function("removeFunction", id)?
        ))
    }

    /// Change when scheduled function `id` next runs to model time `when`.
    /// Calls `timer.setFunctionTime`. There is no dedicated perf counter, so
    /// the call is recorded under `timer_remove_function`.
    pub fn set_function_time(&self, id: FunId, when: f64) -> Result<()> {
        Ok(record_perf!(
            timer_remove_function,
            self.t.call_function("setFunctionTime", (id, when))?
        ))
    }
}
