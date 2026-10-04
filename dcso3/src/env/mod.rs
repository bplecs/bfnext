/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `env` global, the mission scripting environment.
//!
//! The [`miz`] submodule binds the mission file contents. The [`warehouse`]
//! submodule is currently empty.

use crate::{as_tbl, wrapped_table, LuaEnv, String};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::Serialize;
use std::ops::Deref;

pub mod miz;
pub mod warehouse;

// The global `env` table, obtained with `Env::singleton`.
wrapped_table!(Env, None);

impl<'lua> Env<'lua> {
    /// Get the global `env` table. Fails if it is not present in this Lua
    /// environment.
    pub fn singleton<L: LuaEnv<'lua>>(lua: L) -> Result<Self> {
        Ok(lua.inner().globals().raw_get("env")?)
    }

    /// Look up `key` in the mission's dictionary, the table of localized
    /// strings that mission text refers to by key. Calls
    /// `env.getValueDictByKey`.
    pub fn get_value_dict_by_key(&self, key: String) -> Result<String> {
        Ok(self.t.call_function("getValueDictByKey", key)?)
    }
}
