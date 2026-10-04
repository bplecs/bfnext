/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `Object` scripting class, the base class of units,
//! weapons, static objects, airbases, and scenery.
//!
//! [`Object`] wraps a generic DCS object and exposes the methods every
//! object has. A DCS object is a Lua table carrying an `id_` field whose
//! metatable is the object's class (`Unit`, `Weapon`, ...). [`DcsOid`]
//! captures that id and class name so an object can be stored on the Rust
//! side and turned back into a Lua object later via the [`DcsObject`] trait.

use super::{as_tbl, cvt_err, unit::Unit, weapon::Weapon, LuaVec3, Position3, String};
use crate::{
    check_implements, record_perf, simple_enum, static_object::StaticObject, wrapped_table, LuaEnv,
    MizLua,
};
use anyhow::{anyhow, bail, Result};
use core::fmt;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::{hash::Hash, marker::PhantomData, ops::Deref};

/// A stored reference to a DCS object: its `id_` and the name of its class
/// (`className_` of its metatable). `T` is a marker type naming the Rust
/// wrapper class (e.g. [`ClassObject`]) and only exists at compile time.
/// Equality, ordering, and hashing use only the id.
#[derive(Clone, Serialize, Deserialize)]
pub struct DcsOid<T> {
    pub(crate) id: u64,
    pub(crate) class: String,
    #[serde(skip)]
    pub(crate) t: PhantomData<T>,
}

impl<T> DcsOid<T> {
    /// The same id with its class marker replaced by [`ClassObject`]
    pub fn erased(&self) -> DcsOid<ClassObject> {
        DcsOid {
            id: self.id,
            class: self.class.clone(),
            t: PhantomData,
        }
    }

    /// Return an error unless this object's class, looked up as a global
    /// table by name, is `class` or inherits from it (via `parentClass_`).
    pub fn check_implements(&self, lua: MizLua, class: &str) -> Result<()> {
        let m = lua.inner().globals().raw_get(&**self.class)?;
        if !check_implements(&m, class) {
            bail!("{:?} is does not implement {class}", self)
        }
        Ok(())
    }
}

impl<T> fmt::Debug for DcsOid<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{ id: {}, class: {} }}", self.id, self.class)
    }
}

impl<T> Hash for DcsOid<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}

impl<T> PartialEq for DcsOid<T> {
    fn eq(&self, other: &Self) -> bool {
        self.id.eq(&other.id)
    }
}

impl<T> Eq for DcsOid<T> {}

impl<T> PartialOrd for DcsOid<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.id.partial_cmp(&other.id)
    }
}

impl<T> Ord for DcsOid<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.id.cmp(&other.id)
    }
}

/// Class marker for [`DcsOid`]s of generic [`Object`]s
#[derive(Debug, Clone)]
pub struct ClassObject;

/// A wrapper around a DCS object that can be converted to and from a
/// [`DcsOid`].
///
/// The `_dyn` variants accept an id of any class marker and first check
/// (via [`DcsOid::check_implements`]) that the object's DCS class implements
/// the wrapper's class, returning an error if it does not.
pub trait DcsObject<'lua>: Sized + Deref<Target = mlua::Table<'lua>> {
    /// The marker type used in this wrapper's [`DcsOid`]s
    type Class: fmt::Debug + Clone;

    /// Build a [`DcsOid`] from the object's `id_` field and the
    /// `className_` of its metatable
    fn object_id(&self) -> Result<DcsOid<Self::Class>> {
        let id = self.raw_get("id_")?;
        let m = self
            .get_metatable()
            .ok_or_else(|| anyhow!("object with no metatable"))?;
        let class = m.raw_get("className_")?;
        Ok(DcsOid {
            id,
            class,
            t: PhantomData,
        })
    }

    /// Reuse this wrapper's table for the object `id` by overwriting its
    /// `id_` field in place, avoiding a new table allocation. The metatable
    /// is not changed.
    fn change_instance(self, id: &DcsOid<Self::Class>) -> Result<Self>;
    /// Like [`DcsObject::change_instance`] for an id of any class marker
    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self>;
    /// Create a new wrapper for `id`: a fresh table with `id_` set and the
    /// metatable set to the global class table named by the id
    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self>;
    /// Like [`DcsObject::get_instance`] for an id of any class marker
    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self>;
}

// The DCS `Object.Category` values returned by `Object:getCategory`.
simple_enum!(ObjectCategory, u8, [
    Void => 0,
    Unit => 1,
    Weapon => 2,
    Static => 3,
    Base => 4,
    Scenery => 5,
    Cargo => 6
]);

// A generic DCS object. Accepts any table whose class implements `Object`.
wrapped_table!(Object, Some("Object"));

impl<'lua> Object<'lua> {
    /// Remove the object from the mission. Calls `Object:destroy`.
    pub fn destroy(self) -> Result<()> {
        Ok(self.t.call_method("destroy", ())?)
    }

    pub fn get_category(&self) -> Result<ObjectCategory> {
        Ok(self.t.call_method("getCategory", ())?)
    }

    /// The object's description table, returned raw. Calls `Object:getDesc`.
    pub fn get_desc(&self) -> Result<mlua::Table<'lua>> {
        Ok(self.t.call_method("getDesc", ())?)
    }

    /// True if the object has the DCS attribute `attr`. Calls
    /// `Object:hasAttribute`.
    pub fn has_attribute(&self, attr: String) -> Result<bool> {
        Ok(self.t.call_method("hasAttribute", attr)?)
    }

    pub fn get_name(&self) -> Result<String> {
        Ok(self.t.call_method("getName", ())?)
    }

    /// The object's DCS type name (e.g. `"F-16C_50"`)
    pub fn get_type_name(&self) -> Result<String> {
        Ok(self.t.call_method("getTypeName", ())?)
    }

    /// The object's position in world coordinates. Calls `Object:getPoint`.
    pub fn get_point(&self) -> Result<LuaVec3> {
        Ok(record_perf!(get_point, self.t.call_method("getPoint", ())?))
    }

    /// The object's position and orientation. Calls `Object:getPosition`.
    pub fn get_position(&self) -> Result<Position3> {
        Ok(record_perf!(
            get_position,
            self.t.call_method("getPosition", ())?
        ))
    }

    /// The object's velocity vector in m/s. Calls `Object:getVelocity`.
    pub fn get_velocity(&self) -> Result<LuaVec3> {
        Ok(record_perf!(
            get_velocity,
            self.t.call_method("getVelocity", ())?
        ))
    }

    pub fn in_air(&self) -> Result<bool> {
        Ok(self.t.call_method("inAir", ())?)
    }

    /// True if the object still exists in the mission. Calls
    /// `Object:isExist`.
    pub fn is_exist(&self) -> Result<bool> {
        Ok(self.t.call_method("isExist", ())?)
    }

    // The as_* conversions fail if the object's class does not implement
    // the target class.

    pub fn as_unit(&self) -> Result<Unit<'lua>> {
        Ok(Unit::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }

    pub fn as_weapon(&self) -> Result<Weapon<'lua>> {
        Ok(Weapon::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }

    pub fn as_static(&self) -> Result<StaticObject<'lua>> {
        Ok(StaticObject::from_lua(
            Value::Table(self.t.clone()),
            self.lua,
        )?)
    }
}

impl<'lua> DcsObject<'lua> for Object<'lua> {
    type Class = ClassObject;

    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self> {
        let t = lua.inner().create_table()?;
        t.set_metatable(Some(lua.inner().globals().raw_get(&**id.class)?));
        t.raw_set("id_", id.id)?;
        let t = Object {
            t,
            lua: lua.inner(),
        };
        // the id may refer to an object that has since been destroyed
        if !t.is_exist()? {
            bail!("{} is an invalid object", id.id)
        }
        Ok(t)
    }

    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(lua, "Object")?;
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
        id.check_implements(MizLua(self.lua), "Object")?;
        self.t.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid object", id.id)
        }
        Ok(self)
    }
}
