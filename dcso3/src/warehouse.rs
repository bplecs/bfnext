/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `Warehouse` scripting API.
//!
//! Every airbase, FARP, and carrier has a warehouse holding aircraft,
//! weapons, and liquids (fuels). [`Warehouse`] wraps a DCS warehouse object
//! and exposes its methods for querying and changing stock. Items are
//! identified either by their type name (e.g. `"F-16C_50"`) or by their
//! [`WSType`], see [`WarehouseItem`].
//!
//! The global resource map ([`Warehouse::get_resource_map`]) lists every
//! item type DCS knows about, along with its [`WSType`], which
//! [`WSType::category`] decodes into a [`WSCategory`].

use super::as_tbl;
use crate::{
    airbase::Airbase, cvt_err, lua_err, simple_enum, wrapped_table, LuaEnv, MizLua, String,
};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::ops::Deref;

// The liquid types a warehouse stores, numbered as DCS expects them in the
// liquid methods of `Warehouse`.
simple_enum!(LiquidType, u8, [
    JetFuel => 0,
    Avgas => 1,
    MW50 => 2,
    Diesel => 3
]);

impl LiquidType {
    /// Every liquid type
    pub const ALL: [LiquidType; 4] = [Self::Avgas, Self::Diesel, Self::JetFuel, Self::MW50];
}

// The `weapon` or `aircraft` part of an `Inventory`: a table mapping item
// type name to count.
wrapped_table!(ItemInventory, None);

impl<'lua> ItemInventory<'lua> {
    /// The count of the item called `name`
    pub fn item(&self, name: &str) -> Result<u32> {
        Ok(self.t.raw_get(name)?)
    }

    /// Call `f` with the name and count of every item. Iteration stops at,
    /// and returns, the first error.
    pub fn for_each<F: FnMut(String, u32) -> Result<()>>(&self, mut f: F) -> Result<()> {
        Ok(self.t.for_each(|k, v| f(k, v).map_err(lua_err))?)
    }
}

// The `liquids` part of an `Inventory`: a table mapping liquid type to amount.
wrapped_table!(LiquidInventory, None);

impl<'lua> LiquidInventory<'lua> {
    /// The amount of liquid `name`
    pub fn item(&self, name: LiquidType) -> Result<u32> {
        Ok(self.t.raw_get(name)?)
    }

    /// Call `f` with every liquid type and its amount. Iteration stops at,
    /// and returns, the first error.
    pub fn for_each<F: FnMut(LiquidType, u32) -> Result<()>>(&self, mut f: F) -> Result<()> {
        Ok(self.t.for_each(|k, v| f(k, v).map_err(lua_err))?)
    }
}

// A snapshot of a warehouse's contents, returned by
// `Warehouse::get_inventory`.
wrapped_table!(Inventory, None);

impl<'lua> Inventory<'lua> {
    pub fn weapons(&self) -> Result<ItemInventory<'lua>> {
        Ok(self.t.raw_get("weapon")?)
    }

    pub fn aircraft(&self) -> Result<ItemInventory<'lua>> {
        Ok(self.t.raw_get("aircraft")?)
    }

    pub fn liquids(&self) -> Result<LiquidInventory<'lua>> {
        Ok(self.t.raw_get("liquids")?)
    }

    /// True if the inventory is entirely empty, which is how DCS reports a
    /// warehouse set to unlimited stock.
    pub fn is_unlimited(&self) -> Result<bool> {
        Ok(self.weapons()?.is_empty() && self.aircraft()?.is_empty() && self.liquids()?.is_empty())
    }
}

/// The third level of a [`WSType`] for fixed wing aircraft
#[derive(Debug, Clone, Copy)]
pub enum WSFixedWingCategory {
    Fighters,
    FastBombers,
    Interceptors,
    Bombers,
    MiscSupport,
    Attack,
    /// A value not covered by the other variants
    Other(i32),
    None,
}

/// The second level of a [`WSType`] for aircraft
#[derive(Debug, Clone, Copy)]
pub enum WSAircraftCategory {
    FixedWing(WSFixedWingCategory),
    Helicopters,
    Droptank,
    /// A value not covered by the other variants
    Other(i32),
    None,
}

/// The decoded category of a [`WSType`], see [`WSType::category`]
#[derive(Debug, Clone, Copy)]
pub enum WSCategory {
    Aircraft(WSAircraftCategory),
    Vehicles,
    Ships,
    Weapons,
    /// A value not covered by the other variants
    Other(i32),
    None,
}

// The matches below list every variant explicitly, rather than using `_`, so
// adding a variant forces each predicate to be reconsidered.
impl WSCategory {
    /// True for fixed wing aircraft and helicopters. Drop tanks are in the
    /// aircraft category but are not aircraft.
    pub fn is_aircraft(&self) -> bool {
        match self {
            Self::Aircraft(WSAircraftCategory::FixedWing(_))
            | Self::Aircraft(WSAircraftCategory::Helicopters) => true,
            Self::Aircraft(WSAircraftCategory::Droptank)
            | Self::Aircraft(WSAircraftCategory::Other(_))
            | Self::Aircraft(WSAircraftCategory::None)
            | Self::None
            | Self::Ships
            | Self::Vehicles
            | Self::Weapons
            | Self::Other(_) => false,
        }
    }

    pub fn is_fixedwing(&self) -> bool {
        match self {
            Self::Aircraft(WSAircraftCategory::FixedWing(_)) => true,
            Self::Aircraft(WSAircraftCategory::Helicopters)
            | Self::Aircraft(WSAircraftCategory::None)
            | Self::Aircraft(WSAircraftCategory::Droptank)
            | Self::Aircraft(WSAircraftCategory::Other(_))
            | Self::None
            | Self::Ships
            | Self::Vehicles
            | Self::Weapons
            | Self::Other(_) => false,
        }
    }

    pub fn is_helicopter(&self) -> bool {
        match self {
            Self::Aircraft(WSAircraftCategory::Helicopters) => true,
            Self::Aircraft(WSAircraftCategory::FixedWing(_))
            | Self::Aircraft(WSAircraftCategory::None)
            | Self::Aircraft(WSAircraftCategory::Droptank)
            | Self::Aircraft(WSAircraftCategory::Other(_))
            | Self::None
            | Self::Ships
            | Self::Vehicles
            | Self::Weapons
            | Self::Other(_) => false,
        }
    }

    pub fn is_weapon(&self) -> bool {
        match self {
            Self::Weapons => true,
            Self::Aircraft(_) | Self::None | Self::Ships | Self::Vehicles | Self::Other(_) => false,
        }
    }

    pub fn is_vehicle(&self) -> bool {
        match self {
            Self::Vehicles => true,
            Self::Aircraft(_) | Self::None | Self::Ships | Self::Weapons | Self::Other(_) => false,
        }
    }

    pub fn is_ship(&self) -> bool {
        match self {
            Self::Ships => true,
            Self::Aircraft(_) | Self::None | Self::Weapons | Self::Vehicles | Self::Other(_) => {
                false
            }
        }
    }
}

// A DCS wsType: an array of numbers classifying an object, from the broadest
// category (index 1) to progressively narrower ones.
wrapped_table!(WSType, None);

impl<'lua> WSType<'lua> {
    /// Decode the type's category. Level 1 is the broad category, for
    /// aircraft level 2 distinguishes fixed wing, helicopters, and drop
    /// tanks, and for fixed wing level 3 is the role. Unrecognized values
    /// become `Other`; only the levels needed are read.
    pub fn category(&self) -> Result<WSCategory> {
        match self.t.raw_get(1)? {
            0 => Ok(WSCategory::None),
            1 => match self.t.raw_get(2)? {
                0 => Ok(WSCategory::Aircraft(WSAircraftCategory::None)),
                1 => Ok(WSCategory::Aircraft(WSAircraftCategory::FixedWing(
                    match self.t.raw_get(3)? {
                        0 => WSFixedWingCategory::None,
                        1 => WSFixedWingCategory::Fighters,
                        2 => WSFixedWingCategory::FastBombers,
                        3 => WSFixedWingCategory::Interceptors,
                        4 => WSFixedWingCategory::Bombers,
                        5 => WSFixedWingCategory::MiscSupport,
                        6 => WSFixedWingCategory::Attack,
                        n => WSFixedWingCategory::Other(n),
                    },
                ))),
                2 => Ok(WSCategory::Aircraft(WSAircraftCategory::Helicopters)),
                3 => Ok(WSCategory::Aircraft(WSAircraftCategory::Droptank)),
                n => Ok(WSCategory::Aircraft(WSAircraftCategory::Other(n))),
            },
            2 => Ok(WSCategory::Vehicles),
            3 => Ok(WSCategory::Ships),
            4 => Ok(WSCategory::Weapons),
            n => Ok(WSCategory::Other(n)),
        }
    }
}

// Every item type DCS knows about, mapping type name to `WSType`. Returned by
// `Warehouse::get_resource_map`.
wrapped_table!(ResourceMap, None);

impl<'lua> ResourceMap<'lua> {
    /// Call `f` with the name and [`WSType`] of every known item. Iteration
    /// stops at, and returns, the first error.
    pub fn for_each<F: FnMut(String, WSType) -> Result<()>>(&self, mut f: F) -> Result<()> {
        Ok(self.t.for_each(|k, v| f(k, v).map_err(lua_err))?)
    }
}

/// How an item is identified when calling the item methods of
/// [`Warehouse`]: DCS accepts either the type name or the wsType table.
#[derive(Debug, Clone)]
pub enum WarehouseItem<'lua> {
    Name(String),
    Typ(WSType<'lua>),
}

impl<'lua> IntoLua<'lua> for WarehouseItem<'lua> {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        match self {
            Self::Name(s) => s.into_lua(lua),
            Self::Typ(t) => Ok(Value::Table(t.t)),
        }
    }
}

impl<'lua> From<String> for WarehouseItem<'lua> {
    fn from(value: String) -> Self {
        Self::Name(value)
    }
}

impl<'lua> From<WSType<'lua>> for WarehouseItem<'lua> {
    fn from(value: WSType<'lua>) -> Self {
        Self::Typ(value)
    }
}

// A DCS warehouse object. Usually obtained from an airbase
// (`Airbase::get_warehouse`) or by name with `Warehouse::get_by_name`.
wrapped_table!(Warehouse, Some("Warehouse"));

impl<'lua> Warehouse<'lua> {
    /// Look up a warehouse by the name of its airbase. Calls the static
    /// `Warehouse.getByName`.
    pub fn get_by_name(lua: MizLua<'lua>, name: String) -> Result<Self> {
        let wh: LuaTable = lua.inner().globals().raw_get("Warehouse")?;
        Ok(wh.call_function("getByName", name)?)
    }

    /// The map of every item type DCS knows about. Calls the static
    /// `Warehouse.getResourceMap`.
    pub fn get_resource_map(lua: MizLua<'lua>) -> Result<ResourceMap<'lua>> {
        let wh: LuaTable = lua.inner().globals().raw_get("Warehouse")?;
        Ok(wh.call_function("getResourceMap", ())?)
    }

    /// Add `count` of `item` to the stock
    pub fn add_item<T: Into<WarehouseItem<'lua>>>(&self, item: T, count: u32) -> Result<()> {
        Ok(self
            .t
            .call_method("addItem", (Into::<WarehouseItem>::into(item), count))?)
    }

    /// Remove `count` of `item` from the stock
    pub fn remove_item<T: Into<WarehouseItem<'lua>>>(&self, item: T, count: u32) -> Result<()> {
        Ok(self
            .t
            .call_method("removeItem", (Into::<WarehouseItem>::into(item), count))?)
    }

    /// Set the stock of `item` to exactly `count`
    pub fn set_item<T: Into<WarehouseItem<'lua>>>(&self, item: T, count: u32) -> Result<()> {
        Ok(self
            .t
            .call_method("setItem", (Into::<WarehouseItem>::into(item), count))?)
    }

    /// The current stock of `item`
    pub fn get_item_count<T: Into<WarehouseItem<'lua>>>(&self, item: T) -> Result<u32> {
        Ok(self
            .t
            .call_method("getItemCount", Into::<WarehouseItem>::into(item))?)
    }

    /// Add `count` of liquid `typ`
    pub fn add_liquid(&self, typ: LiquidType, count: u32) -> Result<()> {
        Ok(self.t.call_method("addLiquid", (typ, count))?)
    }

    /// Remove `count` of liquid `typ`
    pub fn remove_liquid(&self, typ: LiquidType, count: u32) -> Result<()> {
        Ok(self.t.call_method("removeLiquid", (typ, count))?)
    }

    /// The current amount of liquid `typ`
    pub fn get_liquid_amount(&self, typ: LiquidType) -> Result<u32> {
        Ok(self.t.call_method("getLiquidAmount", typ)?)
    }

    /// Set the amount of liquid `typ` to exactly `count`
    pub fn set_liquid_amount(&self, typ: LiquidType, count: u32) -> Result<()> {
        Ok(self.t.call_method("setLiquidAmount", (typ, count))?)
    }

    /// A snapshot of the warehouse's contents. `filter`, if given, is passed
    /// to DCS to restrict the inventory to matching items.
    pub fn get_inventory(&self, filter: Option<String>) -> Result<Inventory<'lua>> {
        Ok(self.t.call_method("getInventory", filter)?)
    }

    /// The airbase this warehouse belongs to
    pub fn get_owner(&self) -> Result<Airbase<'_>> {
        Ok(self.t.call_method("getOwner", ())?)
    }

    /// The warehouse's internal DCS id, read from its `whid_` field
    pub fn whid(&self) -> Result<String> {
        Ok(self.t.raw_get("whid_")?)
    }
}
