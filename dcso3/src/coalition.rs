/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `coalition` singleton.
//!
//! [`Side`] is the DCS coalition (`coalition.side`). [`Coalition`] wraps the
//! global `coalition` table, which spawns groups and static objects and
//! lists the groups, statics, airbases, and players belonging to a side.

use super::{
    airbase::Airbase,
    as_tbl,
    country::Country,
    cvt_err, env,
    group::{Group, GroupCategory},
    static_object::StaticObject,
    unit::Unit,
};
use crate::{record_perf, simple_enum, wrapped_table, LuaEnv, MizLua, Sequence};
use anyhow::{anyhow, bail, Result};
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::{fmt, ops::Deref, str::FromStr};

// A DCS coalition, numbered as in `coalition.side`.
simple_enum!(Side, u8, [Neutral => 0, Red => 1, Blue => 2]);

impl Default for Side {
    fn default() -> Self {
        Side::Red
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_str())
    }
}

/// Parses the names produced by [`Side::to_str`]
impl FromStr for Side {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "blue" => Side::Blue,
            "red" => Side::Red,
            "neutrals" => Side::Neutral,
            s => bail!("unknown side {s}"),
        })
    }
}

impl Side {
    /// Every side
    pub const ALL: [Side; 3] = [Side::Red, Side::Blue, Side::Neutral];

    /// The lowercase side name, `"blue"`, `"red"`, or `"neutrals"`, the
    /// same names used as coalition keys in the mission file
    pub fn to_str(&self) -> &'static str {
        match self {
            Side::Blue => "blue",
            Side::Red => "red",
            Side::Neutral => "neutrals",
        }
    }

    /// The enemy side: red for blue and vice versa. Neutral is its own
    /// opposite.
    pub fn opposite(&self) -> Side {
        match self {
            Self::Blue => Self::Red,
            Self::Red => Self::Blue,
            Self::Neutral => Self::Neutral,
        }
    }
}

/// A static object as returned by DCS, which may be an airbase (e.g. a
/// FARP) or an ordinary static object, distinguished by the object's class
#[derive(Debug, Clone)]
pub enum Static<'lua> {
    Airbase(Airbase<'lua>),
    Static(StaticObject<'lua>),
}

// The services a unit can provide to its coalition (`coalition.service`),
// used with `Coalition::get_service_providers`.
simple_enum!(Service, u8, [Atc => 0, Awacs => 1, Fac => 3, Tanker => 2]);
// The global `coalition` table.
wrapped_table!(Coalition, None);

impl<'lua> Coalition<'lua> {
    /// Get the global `coalition` table
    pub fn singleton(lua: MizLua<'lua>) -> Result<Self> {
        Ok(Self {
            t: lua.inner().globals().raw_get("coalition")?,
            lua: lua.inner(),
        })
    }

    /// Spawn a group for `country` from the mission-format group table
    /// `data`. Calls `coalition.addGroup`.
    pub fn add_group(
        &self,
        country: Country,
        category: GroupCategory,
        data: env::miz::Group<'lua>,
    ) -> Result<Group<'lua>> {
        Ok(record_perf!(
            add_group,
            self.t
                .call_function("addGroup", (country, category, data))?
        ))
    }

    /// Spawn a static object for `country` from the mission-format unit
    /// table `data`. Calls `coalition.addStaticObject`. The result is
    /// [`Static::Airbase`] if DCS returns an object of class `Airbase`,
    /// otherwise [`Static::Static`].
    pub fn add_static_object(
        &self,
        country: Country,
        data: env::miz::Unit<'lua>,
    ) -> Result<Static<'lua>> {
        let tbl: LuaTable = record_perf!(
            add_static_object,
            self.t.call_function("addStaticObject", (country, data))?
        );
        let mt = tbl
            .get_metatable()
            .ok_or_else(|| anyhow!("returned static object has no meta table"))?;
        if mt.raw_get::<_, String>("className_")?.as_str() == "Airbase" {
            Ok(Static::Airbase(Airbase::from_lua(
                Value::Table(tbl),
                self.lua,
            )?))
        } else {
            Ok(Static::Static(StaticObject::from_lua(
                Value::Table(tbl),
                self.lua,
            )?))
        }
    }

    /// All groups of `side`. Calls `coalition.getGroups`.
    pub fn get_groups(&self, side: Side) -> Result<Sequence<'lua, Group<'lua>>> {
        Ok(self.t.call_function("getGroups", side)?)
    }

    /// All static objects of `side`. Calls `coalition.getStaticObjects`.
    pub fn get_static_objects(&self, side: Side) -> Result<Sequence<'lua, StaticObject<'lua>>> {
        Ok(self.t.call_function("getStaticObjects", side)?)
    }

    /// All airbases of `side`. Calls `coalition.getAirbases`.
    pub fn get_airbases(&self, side: Side) -> Result<Sequence<'lua, Airbase<'lua>>> {
        Ok(self.t.call_function("getAirbases", side)?)
    }

    /// The units of `side` occupied by players. Calls `coalition.getPlayers`.
    pub fn get_players(&self, side: Side) -> Result<Sequence<'lua, Unit<'lua>>> {
        Ok(self.t.call_function("getPlayers", side)?)
    }

    /// The units of `side` providing `service`. Calls
    /// `coalition.getServiceProviders`.
    pub fn get_service_providers(
        &self,
        side: Side,
        service: Service,
    ) -> Result<Sequence<'lua, Unit<'lua>>> {
        Ok(self
            .t
            .call_function("getServiceProviders", (side, service))?)
    }

    /// The side `country` belongs to. Calls `coalition.getCountrySide`.
    pub fn get_country_coalition(&self, country: Country) -> Result<Side> {
        Ok(self.t.call_function("getCountrySide", country)?)
    }
}
