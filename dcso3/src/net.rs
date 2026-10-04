/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `net` multiplayer API, mainly used from the hooks
//! (server) environment.
//!
//! [`Net`] wraps the global `net` table: listing players and getting
//! their info ([`PlayerInfo`]), chat, kicking, reading player stats,
//! querying and forcing player slots, JSON conversion, and running code in
//! another Lua environment ([`Net::dostring_in`]).
//!
//! This module also defines the player identity types: [`PlayerId`] (the
//! per session id DCS assigns a connected player), [`Ucid`] (the player's
//! persistent unique client id), and [`SlotId`] (a slot a player can
//! occupy, including multicrew and combined arms / observer roles).

use super::{as_tbl, coalition::Side, cvt_err, String};
use crate::{
    env::miz::UnitId, err, lua_err, simple_enum, wrapped_prim, wrapped_table, LuaEnv, Sequence,
};
use anyhow::{bail, Result};
use compact_str::format_compact;
use core::fmt;
use fixedstr::str48;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::{ops::Deref, str::FromStr};

// The player statistics readable with `Net::get_stat`, numbered as DCS
// expects them.
simple_enum!(PlayerStat, u8, [
    Car => 2,
    Crash => 1,
    Eject => 7,
    ExtraAllyAAA => 17,
    ExtraAllyFighters => 14,
    ExtraAllySAM => 16,
    ExtraAllyTransports => 15,
    ExtraAllyTroops => 18,
    ExtraAllyCoalition => 19,
    ExtraEnemyAAA => 12,
    ExtraEnemyFighters => 9,
    ExtraEnemySAM => 11,
    ExtraEnemyTransports => 10,
    ExtraEnemyTroops => 13,
    Land => 6,
    Num => 20,
    OldScore => 8,
    Ping => 0,
    Plane => 3,
    Score => 5,
    Ship => 4
]);

// The id DCS assigns a connected player. It identifies a player only for the
// duration of their connection; use `Ucid` for a persistent identity.
wrapped_prim!(PlayerId, i64, Copy, Hash);

impl FromStr for PlayerId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self(s.parse::<i64>()?))
    }
}

impl fmt::Display for PlayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A slot a player can occupy.
///
/// DCS represents slots as either a number (a unit id, 0 meaning
/// spectators) or a string: `""` for spectators, `"<unit id>_<seat>"` for a
/// multicrew seat, or `"<role>_<side>_<n>"` for the combined arms and
/// observer roles (e.g. `"forward_observer_blue_1"`). Serialized (serde
/// and [`Display`](fmt::Display)) in the same string form, with spectator
/// as `"0"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "crate::String")]
#[serde(try_from = "crate::String")]
pub enum SlotId {
    /// The slot of the unit with this unit id
    Unit(i64),
    /// Seat number `.1` of the multicrew unit with unit id `.0`
    MultiCrew(i64, u8),
    Spectator,
    ArtilleryCommander(Side, u8),
    ForwardObserver(Side, u8),
    Observer(Side, u8),
    Instructor(Side, u8),
}

impl<'lua> FromLua<'lua> for SlotId {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> LuaResult<Self> {
        match value {
            Value::Integer(i) => {
                if i == 0 {
                    Ok(Self::Spectator)
                } else {
                    Ok(Self::Unit(i))
                }
            }
            Value::Number(i) => {
                let i = i as i64;
                if i == 0 {
                    Ok(Self::Spectator)
                } else {
                    Ok(Self::Unit(i))
                }
            }
            Value::String(s) => Self::parse_string_slot(s.to_str().map_err(lua_err)?.trim()),
            v => Err(lua_err(format!("invalid slot type {:?}", v))),
        }
    }
}

impl fmt::Display for SlotId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", Into::<String>::into(*self))
    }
}

/// Converts to the form DCS uses: unit slots as integers, spectator as `""`,
/// everything else as a string. Fails if a unit id or slot number is less
/// than 1.
impl<'lua> IntoLua<'lua> for SlotId {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        match self {
            Self::Unit(i) => {
                if i < 1 {
                    return Err(lua_err("invalid unit number"));
                }
                Ok(Value::Integer(i))
            }
            Self::Spectator => Ok(Value::String(lua.create_string("")?)),
            Self::ArtilleryCommander(s, n) => {
                if n < 1 {
                    return Err(lua_err("invalid ca slot number"));
                }
                String(format_compact!("artillery_commander_{s}_{n}")).into_lua(lua)
            }
            Self::ForwardObserver(s, n) => {
                if n < 1 {
                    return Err(lua_err("invalid ca slot number"));
                }
                String(format_compact!("forward_observer_{s}_{n}")).into_lua(lua)
            }
            Self::Instructor(s, n) => {
                if n < 1 {
                    return Err(lua_err("invalid ca slot number"));
                }
                String(format_compact!("instructor_{s}_{n}")).into_lua(lua)
            }
            Self::Observer(s, n) => {
                if n < 1 {
                    return Err(lua_err("invalid ca slot number"));
                }
                String(format_compact!("observer_{s}_{n}")).into_lua(lua)
            }
            Self::MultiCrew(i, n) => {
                if n < 1 {
                    return Err(lua_err("invalid multi crew slot number"));
                }
                String(format_compact!("{i}_{n}")).into_lua(lua)
            }
        }
    }
}

impl TryFrom<String> for SlotId {
    type Error = anyhow::Error;

    fn try_from(s: String) -> Result<Self> {
        match s.parse::<i64>() {
            Ok(i) => {
                if i == 0 {
                    Ok(Self::Spectator)
                } else {
                    Ok(Self::Unit(i))
                }
            }
            Err(_) => Ok(Self::parse_string_slot(s.as_str())?),
        }
    }
}

impl Into<String> for SlotId {
    fn into(self) -> String {
        String::from(match self {
            Self::Unit(i) => format_compact!("{i}"),
            Self::Spectator => format_compact!("0"),
            Self::ArtilleryCommander(s, n) => format_compact!("artillery_commander_{s}_{n}"),
            Self::ForwardObserver(s, n) => format_compact!("forward_observer_{s}_{n}"),
            Self::Instructor(s, n) => format_compact!("instructor_{s}_{n}"),
            Self::Observer(s, n) => format_compact!("observer_{s}_{n}"),
            Self::MultiCrew(i, n) => format_compact!("{i}_{n}"),
        })
    }
}

impl From<UnitId> for SlotId {
    fn from(value: UnitId) -> Self {
        Self::Unit(value.inner())
    }
}

impl SlotId {
    /// Parse the string form of a slot (see [`SlotId`])
    fn parse_string_slot(s: &str) -> LuaResult<SlotId> {
        // parse the "<side>_<n>" suffix of a role slot
        fn side_and_num(s: &str) -> LuaResult<(Side, u8)> {
            match s.split_once("_") {
                None => Err(lua_err(format_compact!("side number {s}"))),
                Some((s, n)) => {
                    let side = match s {
                        "red" => Ok(Side::Red),
                        "blue" => Ok(Side::Blue),
                        "neutrals" => Ok(Side::Neutral),
                        s => Err(lua_err(format_compact!("slot side {s}"))),
                    }?;
                    let n = n.parse::<u8>().map_err(lua_err)?;
                    Ok((side, n))
                }
            }
        }
        if s == "" {
            Ok(Self::Spectator)
        } else if let Ok(i) = s.parse::<i64>() {
            if i == 0 {
                Ok(Self::Spectator)
            } else {
                Ok(Self::Unit(i))
            }
        } else if let Some(s) = s.strip_prefix("artillery_commander_") {
            let (side, n) = side_and_num(s)?;
            Ok(Self::ArtilleryCommander(side, n))
        } else if let Some(s) = s.strip_prefix("observer_") {
            let (side, n) = side_and_num(s)?;
            Ok(Self::Observer(side, n))
        } else if let Some(s) = s.strip_prefix("forward_observer_") {
            let (side, n) = side_and_num(s)?;
            Ok(Self::ForwardObserver(side, n))
        } else if let Some(s) = s.strip_prefix("instructor_") {
            let (side, n) = side_and_num(s)?;
            Ok(Self::Instructor(side, n))
        } else {
            // anything else must be a multicrew seat, "<unit id>_<seat>"
            match s.split_once("_") {
                None => Err(lua_err(format!("invalid string slot {s}"))),
                Some((i, n)) => {
                    let i = i.parse::<i64>().map_err(lua_err)?;
                    let n = n.parse::<u8>().map_err(lua_err)?;
                    Ok(Self::MultiCrew(i, n))
                }
            }
        }
    }

    pub fn is_artillery_commander(&self) -> bool {
        match self {
            Self::ArtilleryCommander(_, _) => true,
            _ => false,
        }
    }

    pub fn is_observer(&self) -> bool {
        match self {
            Self::Observer(_, _) => true,
            _ => false,
        }
    }

    pub fn is_forward_observer(&self) -> bool {
        match self {
            Self::ForwardObserver(_, _) => true,
            _ => false,
        }
    }

    pub fn is_instructor(&self) -> bool {
        match self {
            Self::Instructor(_, _) => true,
            _ => false,
        }
    }

    pub fn is_spectator(&self) -> bool {
        match self {
            Self::Spectator => true,
            _ => false,
        }
    }

    pub fn is_multicrew(&self) -> bool {
        match self {
            Self::MultiCrew(_, _) => true,
            _ => false,
        }
    }

    /// The unit id of a unit or multicrew slot, `None` for every other slot
    pub fn as_unit_id(&self) -> Option<UnitId> {
        match self {
            Self::Unit(i) => Some(UnitId::from(*i)),
            Self::MultiCrew(i, _) => Some(UnitId::from(*i)),
            Self::ArtilleryCommander(_, _)
            | Self::ForwardObserver(_, _)
            | Self::Instructor(_, _)
            | Self::Observer(_, _)
            | Self::Spectator => None,
        }
    }
}

/// A player's unique client id, a persistent identity across sessions.
///
/// DCS gives it as a 32 character hex string; it is stored as the 16 bytes
/// it encodes. Displays and serializes as lowercase hex.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "str48", into = "str48")]
pub struct Ucid([u8; 16]);

impl Default for Ucid {
    fn default() -> Self {
        Ucid([0; 16])
    }
}

impl fmt::Display for Ucid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", Into::<str48>::into(*self))
    }
}

impl fmt::Debug for Ucid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl TryFrom<str48> for Ucid {
    type Error = anyhow::Error;

    fn try_from(s: str48) -> Result<Self> {
        s.parse()
    }
}

/// Parse a 32 character hex string. Fails on any other length or non hex
/// characters.
impl FromStr for Ucid {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        if s.len() != 32 {
            bail!("expected a 32 character string got \"{s}\"")
        }
        let mut a = [0; 16];
        for i in 0..16 {
            // each byte is two hex digits
            let j = i << 1;
            a[i] = u8::from_str_radix(&s[j..j + 2], 16)?;
        }
        Ok(Self(a))
    }
}

impl Into<str48> for Ucid {
    fn into(self) -> str48 {
        use std::fmt::Write;
        let mut s = str48::new();
        for i in 0..16 {
            write!(s, "{:02x}", self.0[i]).unwrap()
        }
        s
    }
}

impl<'lua> FromLua<'lua> for Ucid {
    fn from_lua(value: Value<'lua>, _lua: &'lua Lua) -> LuaResult<Self> {
        match value {
            Value::String(s) => s
                .to_str()?
                .parse()
                .map_err(|e| err(&format_compact!("decoding ucid {}", e))),
            _ => Err(err("expected ucid to be a string")),
        }
    }
}

impl<'lua> IntoLua<'lua> for Ucid {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        let s: str48 = self.into();
        Ok(Value::String(lua.create_string(s.as_str())?))
    }
}

// The table returned by `Net::get_player_info`.
wrapped_table!(PlayerInfo, None);

impl<'lua> PlayerInfo<'lua> {
    pub fn id(&self) -> Result<PlayerId> {
        Ok(self.t.raw_get("id")?)
    }

    pub fn name(&self) -> Result<String> {
        Ok(self.t.raw_get("name")?)
    }

    pub fn side(&self) -> Result<Side> {
        Ok(self.t.raw_get("side")?)
    }

    pub fn slot(&self) -> Result<SlotId> {
        Ok(self.t.raw_get("slot")?)
    }

    pub fn ping(&self) -> Result<f32> {
        Ok(self.t.raw_get("ping")?)
    }

    /// The player's IP address (`ipaddr`), `None` if DCS didn't provide it
    pub fn ip(&self) -> Result<Option<String>> {
        Ok(self.t.raw_get("ipaddr")?)
    }

    /// The player's [`Ucid`], `None` if DCS didn't provide it
    pub fn ucid(&self) -> Result<Option<Ucid>> {
        Ok(self.t.raw_get("ucid")?)
    }
}

/// The DCS Lua states that [`Net::dostring_in`] can run code in
#[derive(Debug, Clone, Copy)]
pub enum DcsLuaEnvironment {
    /// aka hooks
    Server,
    Mission,
    Config,
    Export,
}

impl<'lua> IntoLua<'lua> for DcsLuaEnvironment {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        Ok(Value::String(match self {
            Self::Server => lua.create_string("server"),
            Self::Mission => lua.create_string("mission"),
            Self::Config => lua.create_string("config"),
            Self::Export => lua.create_string("export"),
        }?))
    }
}

// The global `net` table. Each method calls the `net` function of the same
// name.
wrapped_table!(Net, None);

impl<'lua> Net<'lua> {
    /// Get the global `net` table
    pub fn singleton<L: LuaEnv<'lua>>(lua: L) -> Result<Self> {
        Ok(lua.inner().globals().raw_get("net")?)
    }

    /// Send a chat message. If `all` is false it goes only to the sender's
    /// coalition.
    pub fn send_chat(&self, message: String, all: bool) -> Result<()> {
        Ok(self.t.call_function("send_chat", (message, all))?)
    }

    /// Send a chat message to one player, optionally appearing to come
    /// from player `from_id`
    pub fn send_chat_to(
        &self,
        message: String,
        player: PlayerId,
        from_id: Option<PlayerId>,
    ) -> Result<()> {
        Ok(self
            .t
            .call_function("send_chat_to", (message, player, from_id))?)
    }

    /// The ids of all connected players
    pub fn get_player_list(&self) -> Result<Sequence<'lua, PlayerId>> {
        Ok(self.t.call_function("get_player_list", ())?)
    }

    /// The player id of this machine
    pub fn get_my_player_id(&self) -> Result<PlayerId> {
        Ok(self.t.call_function("get_my_player_id", ())?)
    }

    /// The player id of the server
    pub fn get_server_id(&self) -> Result<PlayerId> {
        Ok(self.t.call_function("get_server_id", ())?)
    }

    /// Get information about player `id`. Fails if DCS returns no table
    /// (e.g. the player is not connected).
    pub fn get_player_info(&self, id: PlayerId) -> Result<PlayerInfo<'_>> {
        Ok(self.t.call_function("get_player_info", id)?)
    }

    /// Kick player `id` from the server, showing them `message`
    pub fn kick(&self, id: PlayerId, message: String) -> Result<()> {
        Ok(self.t.call_function("kick", (id, message))?)
    }

    pub fn get_stat(&self, id: PlayerId, stat: PlayerStat) -> Result<i64> {
        Ok(self.t.call_function("get_stat", (id, stat))?)
    }

    pub fn get_name(&self, id: PlayerId) -> Result<String> {
        Ok(self.t.call_function("get_name", id)?)
    }

    /// The side and slot player `id` currently occupies
    pub fn get_slot(&self, id: PlayerId) -> Result<(Side, SlotId)> {
        Ok(self.t.call_function("get_slot", id)?)
    }

    /// Move player `id` into `slot` on `side`
    pub fn force_player_slot(&self, id: PlayerId, side: Side, slot: SlotId) -> Result<()> {
        Ok(self
            .t
            .call_function("force_player_slot", (id, side, slot))?)
    }

    /// Encode a Lua value as JSON using DCS's `net.lua2json`
    pub fn lua2json<T: IntoLua<'lua>>(&self, v: T) -> Result<String> {
        Ok(self.t.call_function("lua2json", v)?)
    }

    /// Decode JSON into a Lua value using DCS's `net.json2lua`, then
    /// convert it to `T`
    pub fn json2lua<T: FromLua<'lua>>(&self, v: String) -> Result<T> {
        Ok(self.t.call_function("json2lua", v)?)
    }

    /// Run the Lua code `dostring` in the Lua state `state` and return its
    /// result as a string
    pub fn dostring_in(&self, state: DcsLuaEnvironment, dostring: String) -> Result<String> {
        Ok(self.t.call_function("dostring_in", (state, dostring))?)
    }

    /// Write `message` to the DCS log
    pub fn log(&self, message: String) -> Result<()> {
        Ok(self.t.call_function("log", message)?)
    }
}
