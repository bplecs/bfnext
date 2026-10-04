/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `Group` scripting class.
//!
//! A [`Group`] is a DCS group of units (aircraft, ground vehicles, or
//! ships). Groups own a [`Controller`] used to give them AI tasks and
//! options.

use super::{as_tbl, coalition::Side, controller::Controller, cvt_err, unit::Unit, String};
use crate::{
    env::miz::{GroupId, GroupKind},
    object::{DcsObject, DcsOid, Object},
    simple_enum, wrapped_table, LuaEnv, MizLua, Sequence,
};
use anyhow::{Result, bail};
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::{marker::PhantomData, ops::Deref};

// The DCS `Group.Category` values returned by `Group:getCategory`.
simple_enum!(GroupCategory, u8, [
    Airplane => 0,
    Helicopter => 1,
    Ground => 2,
    Ship => 3,
    Train => 4
]);

impl GroupCategory {
    /// The category corresponding to a mission file group kind. Returns
    /// `None` for kinds with no group category (`Any` and `Static`).
    pub fn from_kind(k: GroupKind) -> Option<Self> {
        match k {
            GroupKind::Any | GroupKind::Static => None,
            GroupKind::Plane => Some(Self::Airplane),
            GroupKind::Helicopter => Some(Self::Helicopter),
            GroupKind::Vehicle => Some(Self::Ground),
            GroupKind::Ship => Some(Self::Ship),
        }
    }
}

/// The coalition reported for a group. A value of 3 converts to
/// `Contested`; anything else must be a valid [`Side`].
#[derive(Debug, Clone, Serialize)]
pub enum Owner {
    Contested,
    Side(Side),
}

impl<'lua> FromLua<'lua> for Owner {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        match i64::from_lua(value.clone(), lua)? {
            3 => Ok(Self::Contested),
            _ => Ok(Owner::Side(Side::from_lua(value, lua)?)),
        }
    }
}

// A DCS group object.
wrapped_table!(Group, Some("Group"));

impl<'lua> Group<'lua> {
    /// Look up a group by name. Calls `Group.getByName`. Returns an error if
    /// no group is found, or if the group has no units left.
    pub fn get_by_name(lua: MizLua<'lua>, name: &str) -> Result<Group<'lua>> {
        let globals = lua.inner().globals();
        let class = as_tbl("Group", None, globals.raw_get("Group")?)?;
        let g: Group = class.call_function("getByName", name)?;
        // work around for dcs bug that can cause getByName to return
        // a group even though the group is dead
        if g.get_size()? == 0 {
            bail!("{} is dead", name)
        }
        Ok(g)
    }

    pub fn is_exist(&self) -> Result<bool> {
        Ok(self.t.call_method("isExist", ())?)
    }

    /// Remove the group and its units from the mission. Calls
    /// `Group:destroy`.
    pub fn destroy(self) -> Result<()> {
        Ok(self.t.call_method("destroy", ())?)
    }

    /// Activate a late activation group. Calls `Group:activate`.
    pub fn activate(&self) -> Result<()> {
        Ok(self.t.call_method("activate", ())?)
    }

    pub fn get_category(&self) -> Result<GroupCategory> {
        Ok(self.t.call_method("getCategory", ())?)
    }

    pub fn get_coalition(&self) -> Result<Owner> {
        Ok(self.t.call_method("getCoalition", ())?)
    }

    pub fn get_name(&self) -> Result<String> {
        Ok(self.t.call_method("getName", ())?)
    }

    /// The group's DCS id. Calls `Group:getID`.
    pub fn id(&self) -> Result<GroupId> {
        Ok(self.t.call_method("getID", ())?)
    }

    /// The number of units currently in the group. Calls `Group:getSize`.
    pub fn get_size(&self) -> Result<i64> {
        Ok(self.t.call_method("getSize", ())?)
    }

    /// The number of units the group was spawned with. Calls
    /// `Group:getInitialSize`.
    pub fn get_initial_size(&self) -> Result<i64> {
        Ok(self.t.call_method("getInitialSize", ())?)
    }

    /// The unit at `index` (Lua indexes start at 1). Calls `Group:getUnit`.
    pub fn get_unit(&self, index: usize) -> Result<Unit<'lua>> {
        Ok(self.t.call_method("getUnit", index)?)
    }

    /// The group's units. Calls `Group:getUnits`.
    pub fn get_units(&self) -> Result<Sequence<'lua, Unit<'lua>>> {
        Ok(self.t.call_method("getUnits", ())?)
    }

    /// The group's AI controller. Calls `Group:getController`.
    pub fn get_controller(&self) -> Result<Controller<'lua>> {
        Ok(self.t.call_method("getController", ())?)
    }

    /// Turn the group's radar emissions on or off. Calls
    /// `Group:enableEmission`.
    pub fn enable_emission(&self, on: bool) -> Result<()> {
        Ok(self.t.call_method("enableEmission", on)?)
    }

    /// View this group as a generic [`Object`]. This fails if the group's
    /// class does not implement `Object`.
    pub fn as_object(&self) -> Result<Object<'lua>> {
        Ok(Object::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }
}

/// Class marker for [`DcsOid`]s of [`Group`]s
#[derive(Debug, Clone)]
pub struct ClassGroup;

// get_instance and change_instance return an error if the group no longer
// exists or has no units left.

impl<'lua> DcsObject<'lua> for Group<'lua> {
    type Class = ClassGroup;

    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self> {
        let t = lua.inner().create_table()?;
        t.set_metatable(Some(lua.inner().globals().raw_get(&**id.class)?));
        t.raw_set("id_", id.id)?;
        let t = Group {
            t,
            lua: lua.inner(),
        };
        if !t.is_exist()? {
            bail!("{} is an invalid group", id.id)
        }
        // work around for dcs bug that can cause isExist to return
        // true even though the group is dead
        if t.get_size()? == 0 {
            bail!("{} is dead", id.id)
        }
        Ok(t)
    }

    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(lua, "Group")?;
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
            bail!("{} is an invalid group", id.id)
        }
        // work around for dcs bug that can cause isExist to return
        // true even though the group is dead
        if self.get_size()? == 0 {
            bail!("{} is dead", id.id)
        }
        Ok(self)
    }

    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(MizLua(self.lua), "Group")?;
        self.t.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid group", id.id)
        }
        // work around for dcs bug that can cause isExist to return
        // true even though the group is dead
        if self.get_size()? == 0 {
            bail!("{} is dead", id.id)
        }
        Ok(self)
    }
}
