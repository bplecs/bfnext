/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `Weapon` scripting class.
//!
//! [`Weapon`] wraps an in-flight weapon (missile, bomb, rocket, shell),
//! typically obtained from a shot event. [`WeaponFlag`] lists the DCS
//! `Weapon.flag` values that classify weapon types.

use super::{as_tbl, object::Object, unit::Unit};
use crate::{cvt_err, object::{DcsObject, DcsOid}, simple_enum, wrapped_table, LuaEnv, MizLua};
use anyhow::{bail, Result};
use mlua::{prelude::*, Value};
use serde::Deserialize;
use serde_derive::Serialize;
use std::{marker::PhantomData, ops::Deref};

// the documentation is unfortunately not sufficient for this to be a
// proper bitflags
//
// Single-bit values are individual weapon types; the `Any*`, `Guided*`, and
// similar entries are DCS's predefined combinations. Because this is a
// plain enum, a value from Lua only converts if it exactly matches one of
// these entries.
simple_enum!(WeaponFlag, u64, [
    NoWeapon => 0,
    LGB => 2,
    TvGB => 4,
    SNSGB => 8,
    HEBomb => 16,
    Penetrator => 32,
    NapalmBomb => 64,
    FAEBomb => 128,
    ClusterBomb => 256,
    Dispenser => 512,
    CandleBomb => 1024,
    ParachuteBomb => 2147483648,
    GuidedBomb => 14,
    AnyUnguidedBomb => 2147485680,
    AnyBomb => 2147485694,
    LightRocket => 2048,
    MarkerRocket => 4096,
    CandleRocket => 8192,
    HeavyRocket => 16384,
    AnyRocket => 30720,
    AntiRadarMissile => 32768,
    AntiShipMissile => 65536,
    AntiTankMissile => 131072,
    FireAndForgetASM => 262144,
    LaserASM => 524288,
    TeleASM => 1048576,
    CruiseMissile => 2097152,
    GuidedASM => 1572864,
    TacticalASM => 1835008,
    AnyASM => 4161536,
    SRAAM => 4194304,
    MRAAM => 8388608,
    LRAAM => 16777216,
    IRAAM => 33554432,
    SARAAM => 67108864,
    ARAAM => 134217728,
    AnyAAM => 264241152,
    AnyMissile => 268402688,
    AnyAutonomousMissile => 36012032,
    GunPod => 268435456,
    BuiltInCannon => 536870912,
    Cannons => 805306368,
    AntiRadarMissile2 => 1073741824,
    SmokeShell => 17179869184,
    IlluminationShell => 34359738368,
    MarkerShell => 51539607552,
    SubmunitionDispenserShell => 68719476736,
    GuidedShell => 137438953472,
    ConventionalShell => 206963736576,
    AnyShell => 258503344128,
    Decoys => 8589934592,
    Torpedo => 4294967296,
    AnyAGWeapon => 2956984318,
    AnyAAWeapon => 1069547520,
    UnguidedWeapon => 2952822768,
    GuidedWeapon => 268402702,
    AnyWeapon => 3221225470,
    MarkerWeapon => 13312,
    ArmWeapon => 209379642366
]);

// A DCS weapon object.
wrapped_table!(Weapon, Some("Weapon"));

impl<'lua> Weapon<'lua> {
    /// View this weapon as a generic [`Object`]
    pub fn as_object(&self) -> Result<Object<'lua>> {
        Ok(Object::from_lua(Value::Table(self.t.clone()), self.lua)?)
    }

    /// True if the weapon still exists (e.g. hasn't impacted or been
    /// destroyed). Calls `Weapon:isExist`.
    pub fn is_exist(&self) -> Result<bool> {
        Ok(self.t.call_method("isExist", ())?)
    }

    pub fn get_name(&self) -> Result<String> {
        Ok(self.t.call_method("getName", ())?)
    }

    /// The weapon's DCS type name. Calls `Weapon:getTypeName`.
    pub fn get_type(&self) -> Result<String> {
        Ok(self.t.call_method("getTypeName", ())?)
    }

    /// The unit that fired the weapon. Calls `Weapon:getLauncher`; returns
    /// an error if DCS returns something that isn't a unit (e.g. nil).
    pub fn get_launcher(&self) -> Result<Unit<'lua>> {
        Ok(self.t.call_method("getLauncher", ())?)
    }

    /// The object the weapon is guiding to, or `None` if it has no target.
    /// Calls `Weapon:getTarget`.
    pub fn get_target(&self) -> Result<Option<Object<'lua>>> {
        match self.t.call_method("getTarget", ())? {
            Value::Nil => Ok(None),
            v => Ok(Some(Object::from_lua(v, self.lua)?)),
        }
    }

    /// The weapon's description table, returned raw. Calls
    /// `Weapon:getDesc`.
    pub fn get_desc(&self) -> Result<mlua::Table<'lua>> {
        Ok(self.t.call_method("getDesc", ())?)
    }
}

/// Class marker for [`DcsOid`]s of [`Weapon`]s
#[derive(Debug, Clone)]
pub struct ClassWeapon;

// get_instance and change_instance return an error if the weapon no longer
// exists.

impl<'lua> DcsObject<'lua> for Weapon<'lua> {
    type Class = ClassWeapon;

    fn get_instance(lua: MizLua<'lua>, id: &DcsOid<Self::Class>) -> Result<Self> {
        let t = lua.inner().create_table()?;
        t.set_metatable(Some(lua.inner().globals().raw_get(&**id.class)?));
        t.raw_set("id_", id.id)?;
        let t = Weapon {
            t,
            lua: lua.inner(),
        };
        if !t.is_exist()? {
            bail!("{} is an invalid weapon", id.id)
        }
        Ok(t)
    }

    fn get_instance_dyn<T>(lua: MizLua<'lua>, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(lua, "Weapon")?;
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
            bail!("{} is an invalid weapon", id.id)
        }
        Ok(self)
    }

    fn change_instance_dyn<T>(self, id: &DcsOid<T>) -> Result<Self> {
        id.check_implements(MizLua(self.lua), "Weapon")?;
        self.t.raw_set("id_", id.id)?;
        if !self.is_exist()? {
            bail!("{} is an invalid weapon", id.id)
        }
        Ok(self)
    }
}
