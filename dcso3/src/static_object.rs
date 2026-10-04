/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `StaticObject` scripting class.
//!
//! Static objects are non-moving mission objects such as buildings, cargo,
//! and FARP pads. Some static objects (e.g. FARPs) are reported by DCS as
//! airbases, so lookups by name return a [`Static`], which can be either.

use super::{as_tbl, coalition::Side, country::Country, object::Object};
use crate::{airbase::Airbase, coalition::Static, object::{DcsObject, DcsOid}, wrapped_prim, wrapped_table, LuaEnv, MizLua};
use anyhow::{anyhow, bail, Result};
use mlua::{prelude::*, Value};
use serde_derive::{Serialize, Deserialize};
use std::{marker::PhantomData, ops::Deref};

// The id DCS assigns a static object, returned by `StaticObject:getID`.
wrapped_prim!(StaticObjectId, i64, Hash, Copy);

// A DCS static object.
wrapped_table!(StaticObject, Some("StaticObject"));

impl<'lua> StaticObject<'lua> {
    /// Look up a static object by name. Calls `StaticObject.getByName`.
    /// If the returned object's class is `Airbase` it is returned as
    /// [`Static::Airbase`], otherwise as [`Static::Static`]. Returns an error
    /// if nothing is found or the result has no metatable.
    pub fn get_by_name(lua: MizLua<'lua>, name: &str) -> Result<Static<'lua>> {
        let globals = lua.inner().globals();
        let sobj = as_tbl("StaticObject", None, globals.raw_get("StaticObject")?)?;
        let tbl: LuaTable = sobj.call_function("getByName", name)?;
        let mt = tbl
            .get_metatable()
            .ok_or_else(|| anyhow!("returned static object has no meta table"))?;
        if mt.raw_get::<_, String>("className_")?.as_str() == "Airbase" {
            Ok(Static::Airbase(Airbase::from_lua(
                Value::Table(tbl),
                lua.inner(),
            )?))
        } else {
            Ok(Static::Static(StaticObject::from_lua(
                Value::Table(tbl),
                lua.inner(),
            )?))
        }
    }

    /// Remove the static object from the mission. Calls
    /// `StaticObject:destroy`.
    pub fn destroy(self) -> Result<()> {
        Ok(self.t.call_method("destroy", ())?)
    }

    /// The object's DCS id. Calls `StaticObject:getID`.
    pub fn id(&self) -> Result<StaticObjectId> {
        Ok(self.t.call_method("getID", ())?)
    }

    pub fn get_name(&self) -> Result<String> {
        Ok(self.t.call_method("getName", ())?)
    }

    pub fn get_coalition(&self) -> Result<Side> {
        Ok(self.t.call_method("getCoalition", ())?)
    }

    pub fn get_country(&self) -> Result<Country> {
        Ok(self.t.call_method("getCountry", ())?)
    }
 
    /// The object's current hit points. Calls `StaticObject:getLife`.
    pub fn get_life(&self) -> Result<i64> {
        Ok(self.t.call_method("getLife", ())?)
    }

    pub fn is_exist(&self) -> Result<bool> {
        Ok(self.t.call_method("isExist", ())?)
    }

    /// View this static object as a generic [`Object`]
    pub fn as_object(&self) -> Result<Object<'lua>> {
        Ok(Object::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }

    /// The object's description table, returned raw. Calls
    /// `StaticObject:getDesc`.
    pub fn get_desc(&self) -> Result<mlua::Table<'lua>> {
        Ok(self.t.call_method("getDesc", ())?)
    }
}


/// Class marker for [`DcsOid`]s of [`StaticObject`]s
#[derive(Debug, Clone)]
pub struct ClassStatic;

// get_instance and change_instance return an error if the object no longer
// exists.

impl<'lua> DcsObject<'lua> for StaticObject<'lua> {
    type Class = ClassStatic;

    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self> {
        let t = lua.inner().create_table()?;
        t.set_metatable(Some(lua.inner().globals().raw_get(&**id.class)?));
        t.raw_set("id_", id.id)?;
        let t = StaticObject {
            t,
            lua: lua.inner(),
        };
        if !t.is_exist()? {
            bail!("{} is an invalid object", id.id)
        }
        Ok(t)
    }

    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(lua, "StaticObject")?;
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
            bail!("{} is an invalid object", id.id)
        }
        Ok(self)
    }

    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(MizLua(self.lua), "StaticObject")?;
        self.t.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid object", id.id)
        }
        Ok(self)
    }
}
