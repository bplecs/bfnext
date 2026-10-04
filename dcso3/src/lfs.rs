/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the `lfs` (Lua File System) global that DCS provides.
//!
//! Only the DCS specific directory queries are bound: [`Lfs::writedir`] and
//! [`Lfs::tempdir`].

use super::{as_tbl, String};
use crate::{wrapped_table, LuaEnv};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::Serialize;
use std::ops::Deref;

// The global `lfs` table, obtained with `Lfs::singleton`.
wrapped_table!(Lfs, None);

impl<'lua> Lfs<'lua> {
    /// Get the global `lfs` table. Fails if it is not present in this Lua
    /// environment.
    pub fn singleton<L: LuaEnv<'lua>>(lua: L) -> Result<Self> {
        Ok(lua.inner().globals().raw_get("lfs")?)
    }

    /// The DCS write directory (the user's Saved Games DCS folder). Calls
    /// `lfs.writedir`.
    pub fn writedir(&self) -> Result<String> {
        Ok(self.t.call_function("writedir", ())?)
    }

    /// The temporary directory DCS uses. Calls `lfs.tempdir`.
    pub fn tempdir(&self) -> Result<String> {
        Ok(self.t.call_function("tempdir", ())?)
    }
}
