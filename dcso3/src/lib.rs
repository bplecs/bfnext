/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! A minimal, safe Rust binding to the DCS World Lua scripting API, built on
//! [`mlua`]. The goal is a direct translation of the DCS API, with Rust types
//! and error handling layered on top.
//!
//! # Lua environments
//!
//! DCS has two separate Lua states a script DLL can be loaded into:
//!
//! - the mission scripting environment, where the simulation APIs
//!   (`coalition`, `world`, `land`, `timer`, `trigger`, ...) live, and
//! - the hooks (GUI/server) environment, where the `DCS` and `net` APIs and
//!   the user callback hooks live.
//!
//! Functions that only work in one environment take a [`MizLua`] or a
//! [`HooksLua`] token, thin `Copy` wrappers around `&Lua` that record which
//! state they came from. Both implement [`LuaEnv`], which recovers the
//! `&Lua`. [`create_root_module`] builds the table a DLL returns from its
//! Lua module entry point; it exports `initHooks` and `initMiz`, which hand
//! the matching token to the caller's init function. [`is_hooks_env`] tells
//! the two states apart at runtime.
//!
//! # Wrapped tables
//!
//! Most DCS objects and singletons are Lua tables. [`wrapped_table!`]
//! generates a struct holding an `mlua::Table` and the `&Lua` it belongs
//! to. The struct derefs to the table and converts to and from Lua. If a
//! class name is given, converting from Lua checks the table's metatable
//! (its `className_` / `parentClass_` chain, see [`as_tbl`]) to make sure it
//! is a DCS object of that class or a subclass. Methods on these types
//! usually just call the corresponding DCS Lua function. [`simple_enum!`],
//! [`bitflags_enum!`], [`string_enum!`], [`wrapped_prim!`], and
//! [`atomic_id!`] generate Lua-convertible enums, newtypes, and ids.
//!
//! # Errors
//!
//! API functions return [`anyhow::Result`], and mlua errors convert into it
//! with `?`. [`lua_err`] converts any `Debug` error back into an mlua
//! [`LuaError`]. Where Lua calls into Rust (callbacks, init functions),
//! [`wrap_f`] and [`wrap`] log errors and panics and return `R::default()`
//! instead of raising a Lua error, so a failure in Rust does not propagate
//! into DCS.
//!
//! # Shared types
//!
//! This module also defines types used throughout the crate: [`String`] (a
//! compact string that converts from any Lua value), [`LuaVec2`],
//! [`LuaVec3`], [`Position3`], [`Box3`], [`Quad2`], [`Color`], [`Time`],
//! [`Sequence`] (a typed view of a Lua array), and [`Path`] (for looking up
//! nested table fields), plus 2d/3d geometry helpers.
//!
//! # Modules
//!
//! Each module binds one DCS API: e.g. [`coalition`], [`world`], [`land`],
//! [`timer`], [`trigger`], [`group`], [`unit`], [`airbase`], [`warehouse`],
//! and [`mission_commands`] (F10 menus) in the mission environment; [`dcs`],
//! [`net`], and [`hooks`] in the hooks environment; [`env`] for the mission
//! (miz) data; [`event`] for DCS events; and [`perf`] for timing the
//! bindings themselves.

extern crate nalgebra as na;
use anyhow::{anyhow, bail, Result};
use compact_str::CompactString;
use fxhash::FxHashMap;
use log::error;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::{
    backtrace::Backtrace,
    borrow::Borrow,
    cell::RefCell,
    collections::hash_map::Entry,
    f64,
    fmt::Debug,
    marker::PhantomData,
    ops::{Add, AddAssign, Deref, DerefMut, Sub},
    panic::{self, AssertUnwindSafe},
};

pub mod airbase;
pub mod attribute;
pub mod coalition;
pub mod controller;
pub mod coord;
pub mod country;
pub mod dcs;
pub mod env;
pub mod event;
pub mod group;
pub mod hooks;
pub mod land;
pub mod lfs;
pub mod mission_commands;
pub mod net;
pub mod object;
pub mod perf;
pub mod spot;
pub mod static_object;
pub mod timer;
pub mod trigger;
pub mod unit;
pub mod warehouse;
pub mod weapon;
pub mod world;

/// Define `$name`, a process-unique `i64` id newtype.
///
/// `new` allocates ids from a global atomic counter (`MAX_<NAME>_ID`)
/// starting at 0. Deserializing an id bumps the counter past it, so ids
/// loaded from saved state are never handed out again by `new`. Converting
/// from Lua does not touch the counter. Also generated: `From<i64>`,
/// `FromStr`, `Display`, `Default` (which calls `new`, so it allocates),
/// serde and Lua conversions, plus `inner`, `seq` (the next id), `setseq`
/// (overwrite the counter), and `zero`.
///
/// Requires the `paste` and `serde` crates in the calling crate.
#[macro_export]
macro_rules! atomic_id {
    ($name:ident) => {
        paste::paste! {
            #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
            pub struct $name(i64);

            impl From<i64> for $name {
                fn from(x: i64) -> Self {
                    Self(x)
                }
            }

            impl std::str::FromStr for $name {
                type Err = anyhow::Error;

                fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                    Ok(Self(s.parse::<i64>()?))
                }
            }

            impl serde::Serialize for $name {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error> where S: serde::Serializer {
                    serializer.serialize_i64(self.0)
                }
            }

            pub struct [<$name Visitor>];

            impl<'de> serde::de::Visitor<'de> for [<$name Visitor>] {
                type Value = $name;

                fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                    write!(formatter, "a i64")
                }

                fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
                where
                    E: serde::de::Error,
                {
                    $name::update_max(v);
                    Ok($name(v))
                }

                fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
                where
                    E: serde::de::Error,
                {
                    let v = v as i64;
                    $name::update_max(v);
                    Ok($name(v))
                }
            }

            impl<'de> serde::Deserialize<'de> for $name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error> where D: serde::Deserializer<'de> {
                    deserializer.deserialize_i64([<$name Visitor>])
                }
            }

            static [<MAX_ $name:upper _ID>]: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

            impl std::fmt::Display for $name {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    write!(f, "{}", self.0)
                }
            }

            impl std::default::Default for $name {
                fn default() -> Self {
                    Self::new()
                }
            }

            impl<'lua> mlua::FromLua<'lua> for $name {
                fn from_lua(value: mlua::Value<'lua>, lua: &'lua mlua::Lua) -> mlua::prelude::LuaResult<Self> {
                    let i = i64::from_lua(value, lua)?;
                    Ok($name(i))
                }
            }

            impl<'lua> mlua::IntoLua<'lua> for $name {
                fn into_lua(self, lua: &'lua mlua::Lua) -> mlua::prelude::LuaResult<mlua::Value<'lua>> {
                    self.0.into_lua(lua)
                }
            }

            impl $name {
                pub fn new() -> Self {
                    Self([<MAX_ $name:upper _ID>].fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                }

                fn update_max(n: i64) {
                    const O: std::sync::atomic::Ordering = std::sync::atomic::Ordering::Relaxed;
                    let _: Result<_, _> = [<MAX_ $name:upper _ID>].fetch_update(O, O, |cur| {
                        if n >= cur {
                            Some(n.wrapping_add(1))
                        } else {
                            None
                        }
                    });
                }

                pub fn inner(&self) -> i64 {
                    self.0
                }

                pub fn setseq(i: i64) {
                    [<MAX_ $name:upper _ID>].store(i, std::sync::atomic::Ordering::Relaxed)
                }

                pub fn seq() -> i64 {
                    [<MAX_ $name:upper _ID>].load(std::sync::atomic::Ordering::Relaxed)
                }

                pub fn zero() -> Self {
                    Self(0)
                }
            }
        }
    }
}

/// A quadrilateral in the 2d map plane, read from a Lua array of four
/// `Vec2` tables (e.g. a quad shaped trigger zone).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Quad2 {
    pub p0: LuaVec2,
    pub p1: LuaVec2,
    pub p2: LuaVec2,
    pub p3: LuaVec2,
}

impl<'lua> FromLua<'lua> for Quad2 {
    fn from_lua(value: Value<'lua>, _lua: &'lua Lua) -> LuaResult<Self> {
        let verts = as_tbl("Quad", None, value).map_err(lua_err)?;
        Ok(Self {
            p0: verts.raw_get(1)?,
            p1: verts.raw_get(2)?,
            p2: verts.raw_get(3)?,
            p3: verts.raw_get(4)?,
        })
    }
}

impl Quad2 {
    /// True if `p` is inside the quad or is one of its vertices. Uses a
    /// horizontal ray casting (even-odd) test, so it works for concave quads.
    pub fn contains(&self, p: LuaVec2) -> bool {
        fn horizontal_ray_intersects_edge(
            p: &LuaVec2,
            v0: &LuaVec2,
            v1: &LuaVec2,
        ) -> bool {
            if (v0.y > p.y) == (v1.y > p.y) {
                // we're casting horizontally so we don't need to consider the case where
                // there couldn't be a horizontal intersection
                false
            } else {
                let int_x = v0.x + (p.y - v0.y) * (v1.x - v0.x) / (v1.y - v0.y);
                int_x > p.x
            }
        }
        if p == self.p0 || p == self.p1 || p == self.p2 || p == self.p3 {
            return true;
        }
        let mut intersections = 0;
        macro_rules! check_edge {
            ($v0:expr, $v1:expr) => {
                if $v0.y != $v1.y && horizontal_ray_intersects_edge(&p, &$v0, &$v1) {
                    // if the ray passes through a vertex only count it if
                    // it passes through the upper vertex (to avoid counting it twice)
                    if $v0.y == p.y || $v1.y == p.y {
                        let to_check = if $v0.y == p.y { $v1 } else { $v0 };
                        if to_check.y < p.y {
                            intersections += 1;
                        }
                    } else {
                        intersections += 1
                    }
                }
            };
        }
        check_edge!(self.p0, self.p1);
        check_edge!(self.p1, self.p2);
        check_edge!(self.p2, self.p3);
        check_edge!(self.p3, self.p0);
        intersections % 2 == 1
    }

    /// The endpoints of the longest edge and its *squared* length.
    pub fn longest_edge(&self) -> (Vector2, Vector2, f64) {
        [
            (self.p0.0, self.p1.0),
            (self.p1.0, self.p2.0),
            (self.p2.0, self.p3.0),
            (self.p3.0, self.p0.0),
        ]
        .into_iter()
        .fold(
            (Vector2::default(), Vector2::default(), 0.),
            |x @ (_, _, d), (v0, v1)| {
                let d2 = na::distance_squared(&v0.into(), &v1.into());
                if d2 > d {
                    (v0, v1, d2)
                } else {
                    x
                }
            },
        )
    }

    /// The average of the four vertices.
    pub fn center(&self) -> Vector2 {
        centroid2d([self.p0.0, self.p1.0, self.p2.0, self.p3.0])
    }

    /// Scale this quad by the specified factor. The factor must be
    /// positive. Numbers less than 1 will make the quad smaller,
    /// numbers greater than 1 will make it bigger. If a negative
    /// number is provided then 0 will be used.
    pub fn scale(&self, factor: f64) -> Self {
        let factor = factor.clamp(0., f64::INFINITY);
        let center = self.center();
        let p0 = (self.p0.0 - center).normalize();
        let p1 = (self.p1.0 - center).normalize();
        let p2 = (self.p2.0 - center).normalize();
        let p3 = (self.p3.0 - center).normalize();
        let pd0 = na::distance(&center.into(), &self.p0.0.into()) * factor;
        let pd1 = na::distance(&center.into(), &self.p1.0.into()) * factor;
        let pd2 = na::distance(&center.into(), &self.p2.0.into()) * factor;
        let pd3 = na::distance(&center.into(), &self.p3.0.into()) * factor;
        Self {
            p0: LuaVec2(center + p0 * pd0),
            p1: LuaVec2(center + p1 * pd1),
            p2: LuaVec2(center + p2 * pd2),
            p3: LuaVec2(center + p3 * pd3),
        }
    }

    /// return true if the specified circle is fully contained within the quad.
    pub fn contains_circle(&self, center: Vector2, radius: f64) -> bool {
        // distance from p to the closest point on segment ab
        fn distance_to_segment(p: Vector2, a: Vector2, b: Vector2) -> f64 {
            let ap = p - a;
            let ab = b - a;
            let t = (ap.dot(&ab) / ab.dot(&ab)).clamp(0., 1.);
            na::distance(&p.into(), &(a + t * ab).into())
        }
        self.contains(LuaVec2(center))
            && distance_to_segment(center, self.p0.0, self.p1.0) >= radius
            && distance_to_segment(center, self.p1.0, self.p2.0) >= radius
            && distance_to_segment(center, self.p2.0, self.p3.0) >= radius
            && distance_to_segment(center, self.p3.0, self.p0.0) >= radius
    }
}

/// An RGBA color with components in 0..=1, converted to and from a Lua
/// array `{r, g, b, a}` as used by the DCS drawing functions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Color {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

// named colors, each taking the alpha component
impl Color {
    pub fn black(a: f32) -> Color {
        Color { r: 0., g: 0., b: 0., a }
    }

    pub fn red(a: f32) -> Color {
        Color { r: 1., g: 0., b: 0., a }
    }

    pub fn white(a: f32) -> Color {
        Color { r: 1., g: 1., b: 1., a }
    }

    pub fn blue(a: f32) -> Color {
        Color { r: 0., g: 0., b: 1., a }
    }

    pub fn gray(a: f32) -> Color {
        Color { r: 0.25, g: 0.25, b: 0.25, a }
    }

    pub fn green(a: f32) -> Color {
        Color { r: 0., g: 1., b: 0., a }
    }

    pub fn yellow(a: f32) -> Color {
        Color { r: 0.75, g: 1., b: 0., a }
    }
}

impl<'lua> FromLua<'lua> for Color {
    fn from_lua(value: Value<'lua>, _lua: &'lua Lua) -> LuaResult<Self> {
        let tbl = as_tbl("Color", None, value).map_err(lua_err)?;
        Ok(Self {
            r: tbl.raw_get(1)?,
            g: tbl.raw_get(2)?,
            b: tbl.raw_get(3)?,
            a: tbl.raw_get(4)?,
        })
    }
}

impl<'lua> IntoLua<'lua> for Color {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        let tbl = lua.create_table()?;
        tbl.set(1, self.r)?;
        tbl.set(2, self.g)?;
        tbl.set(3, self.b)?;
        tbl.set(4, self.a)?;
        Ok(Value::Table(tbl))
    }
}

/// Run `f` for a function called from Lua, named `name` in log messages.
///
/// Errors (see [`wrap`]) and panics are logged and turn into
/// `Ok(R::default())`, so neither unwinds into, nor raises an error in, DCS.
pub fn wrap_f<'lua, L: LuaEnv<'lua>, R: Default, F: FnOnce(L) -> Result<R>>(
    name: &str,
    lua: L,
    f: F,
) -> LuaResult<R> {
    match panic::catch_unwind(AssertUnwindSafe(|| wrap(name, f(lua)))) {
        Ok(r) => r,
        Err(e) => {
            match e.downcast_ref::<anyhow::Error>() {
                Some(e) => error!("{name} panicked {e:?} {}", Backtrace::capture()),
                None => error!("{name} panicked {e:?} {}", Backtrace::capture()),
            }
            Ok(R::default())
        }
    }
}

/// Convert `res` for returning to Lua. An error is logged with `name` and
/// replaced by `R::default()`; this never returns `Err`.
pub fn wrap<'lua, R: Default>(name: &str, res: Result<R>) -> LuaResult<R> {
    match res {
        Ok(r) => Ok(r),
        Err(e) => {
            error!("{}: {:?}", name, e);
            Ok(R::default())
        }
    }
}

/// Convert any `Debug` error (typically an `anyhow::Error`) into a Lua
/// runtime error carrying its debug formatting.
pub fn lua_err<E: Debug>(err: E) -> LuaError {
    LuaError::RuntimeError(format!("{:?}", err))
}

/// A handle to a Lua state. Implemented by plain `&Lua` and by the
/// environment tokens [`MizLua`] and [`HooksLua`].
pub trait LuaEnv<'a> {
    /// The underlying Lua state
    fn inner(self) -> &'a Lua;
}

impl<'lua> LuaEnv<'lua> for &'lua Lua {
    fn inner(self) -> &'lua Lua {
        self
    }
}

/// The hooks (GUI/server) Lua environment, where `DCS`, `net`, and user
/// callbacks live. Obtained from the `initHooks` export of
/// [`create_root_module`].
#[derive(Debug, Clone, Copy)]
pub struct HooksLua<'lua>(&'lua Lua);

impl<'lua> LuaEnv<'lua> for HooksLua<'lua> {
    fn inner(self) -> &'lua Lua {
        self.0
    }
}

/// The mission scripting Lua environment, where the simulation APIs
/// (`coalition`, `world`, `land`, ...) live. Obtained from the `initMiz`
/// export of [`create_root_module`].
#[derive(Debug, Clone, Copy)]
pub struct MizLua<'lua>(&'lua Lua);

impl<'lua> LuaEnv<'lua> for MizLua<'lua> {
    fn inner(self) -> &'lua Lua {
        self.0
    }
}

/// Build the module table a DLL returns to Lua's `require`.
///
/// The table has two functions: `initHooks`, which calls `init_hooks` with
/// a [`HooksLua`], and `initMiz`, which calls `init_miz` with a [`MizLua`].
/// Lua code in each environment calls the matching one. Errors and panics
/// from the init functions are logged, not raised (see [`wrap_f`]).
pub fn create_root_module<H, M>(
    lua: &Lua,
    init_hooks: H,
    init_miz: M,
) -> LuaResult<LuaTable<'_>>
where
    H: Fn(HooksLua) -> Result<()> + 'static,
    M: Fn(MizLua) -> Result<()> + 'static,
{
    let exports = lua.create_table()?;
    exports.set(
        "initHooks",
        lua.create_function(move |lua, _: ()| {
            wrap_f("init_hooks", HooksLua(lua), &init_hooks)
        })?,
    )?;
    exports.set(
        "initMiz",
        lua.create_function(move |lua, _: ()| {
            wrap_f("init_miz", MizLua(lua), &init_miz)
        })?,
    )?;
    Ok(exports)
}

/// Define `$name<'lua>`, a typed wrapper around a Lua table.
///
/// `$class` is an `Option<&'static str>`. If it is `Some(class)`,
/// converting from Lua fails unless the table's metatable says it is a DCS
/// object of `class` or a subclass (see [`as_tbl`]); with `None` any table
/// is accepted. The struct has fields `t` (the table) and `lua`, derefs to
/// `mlua::Table`, implements `FromLua`/`IntoLua`, `Clone`, and `Serialize`.
/// Its `Debug` impl prints DCS objects (tables with an `id_` field) as
/// their class name and id, and other tables as JSON (see
/// [`value_to_json`]).
///
/// The caller must have `Lua`, `Value`, `FromLua`, `IntoLua`, `LuaResult`,
/// `Deref`, `Serialize`, and `as_tbl` in scope.
#[macro_export]
macro_rules! wrapped_table {
    ($name:ident, $class:expr) => {
        #[derive(Clone, Serialize)]
        pub struct $name<'lua> {
            t: mlua::Table<'lua>,
            #[allow(dead_code)]
            #[serde(skip)]
            lua: &'lua Lua,
        }

        impl<'lua> std::fmt::Debug for $name<'lua> {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                match self.t.raw_get::<&str, Value>("id_") {
                    Ok(Value::Nil) => {
                        let v = crate::value_to_json(&Value::Table(self.t.clone()));
                        write!(f, "{v}")
                    }
                    Ok(v) => {
                        let class: String = self
                            .t
                            .get_metatable()
                            .and_then(|mt| mt.raw_get("className_").ok())
                            .unwrap_or(String::from("unknown"));
                        write!(f, "{{ class: {}, id: {:?} }}", class, v)
                    }
                    Err(_) => write!(f, "{:?}", self.t),
                }
            }
        }

        impl<'lua> Deref for $name<'lua> {
            type Target = mlua::Table<'lua>;

            fn deref(&self) -> &Self::Target {
                &self.t
            }
        }

        impl<'lua> FromLua<'lua> for $name<'lua> {
            fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
                Ok(Self {
                    t: as_tbl(stringify!($name), $class, value)
                        .map_err(crate::lua_err)?,
                    lua,
                })
            }
        }

        impl<'lua> IntoLua<'lua> for $name<'lua> {
            fn into_lua(self, _lua: &'lua Lua) -> LuaResult<Value<'lua>> {
                Ok(Value::Table(self.t))
            }
        }
    };
}

/// Define a fieldless enum `$name` with representation `$repr`, where each
/// `$case => $num` gives a variant and its numeric value.
///
/// It converts to a Lua integer, and from a Lua number; an unknown number
/// is a conversion error. The caller must have `Serialize`, `Deserialize`,
/// the mlua prelude, `Value`, and `cvt_err` in scope.
#[macro_export]
macro_rules! simple_enum {
    ($name:ident, $repr:ident, [$($case:ident => $num:literal),+]) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[allow(non_camel_case_types)]
        #[repr($repr)]
        pub enum $name {
            $($case = $num),+
        }

        impl<'lua> FromLua<'lua> for $name {
            fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
                Ok(match $repr::from_lua(value, lua)? {
                    $($num => Self::$case),+,
                    _ => return Err(cvt_err(stringify!($name)))
                })
            }
        }

        impl<'lua> IntoLua<'lua> for $name {
            fn into_lua(self, _lua: &'lua Lua) -> LuaResult<Value<'lua>> {
                Ok(Value::Integer(self as i64))
            }
        }
    };
}

/// Like [`simple_enum!`], but the enum is also annotated with
/// `#[bitflags]` (from `enumflags2`, which must be in scope), so each
/// `$num` should be a distinct power of two. Lua conversion handles single
/// flags only, not combinations.
#[macro_export]
macro_rules! bitflags_enum {
    ($name:ident, $repr:ident, [$($case:ident => $num:literal),+]) => {
        #[bitflags]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[allow(non_camel_case_types)]
        #[repr($repr)]
        pub enum $name {
            $($case = $num),+
        }

        impl<'lua> FromLua<'lua> for $name {
            fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
                Ok(match $repr::from_lua(value, lua)? {
                    $($num => Self::$case),+,
                    _ => return Err(cvt_err(stringify!($name)))
                })
            }
        }

        impl<'lua> IntoLua<'lua> for $name {
            fn into_lua(self, _lua: &'lua Lua) -> LuaResult<Value<'lua>> {
                Ok(Value::Integer(self as i64))
            }
        }
    };
}

/// Define an enum `$name` whose variants map to Lua strings, each
/// `$case => $str` giving a variant and its string.
///
/// The optional second list, `$altcase => $altstr`, gives extra strings that
/// also convert to an existing variant (aliases); converting back to Lua
/// always uses the primary string. Any other string converts to
/// `Custom(String)`, so conversion from a string never fails. The caller
/// must have `Serialize`, `Deserialize`, the mlua prelude, `Value`, and
/// this crate's `String` in scope.
#[macro_export]
macro_rules! string_enum {
    ($name:ident, $repr:ident, [$($case:ident => $str:literal),+]) => {
        string_enum!($name, $repr, [$($case => $str),+], []);
    };
    ($name:ident,
     $repr:ident,
     [$($case:ident => $str:literal),+],
     [$($altcase:ident => $altstr:literal),*]) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[allow(non_camel_case_types)]
        #[repr($repr)]
        pub enum $name {
            $($case),+,
            Custom(String)
        }

        impl<'lua> FromLua<'lua> for $name {
            fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
                let s = String::from_lua(value, lua)?;
                Ok(match s.as_str() {
                    $($str => Self::$case,)+
                    $($altstr => Self::$altcase,)*
                    _ => Self::Custom(s)
                })
            }
        }

        impl<'lua> IntoLua<'lua> for $name {
            fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
                Ok(Value::String(match self {
                    $(Self::$case => lua.create_string($str)?),+,
                    Self::Custom(s) => lua.create_string(s.as_str())?
                }))
            }
        }
    };
}

/// Define `$name`, a newtype around the primitive `$type` that converts to
/// and from Lua as `$type` does. Any extra idents are added to the derive
/// list (e.g. `Copy`, `Hash`). Also generates `inner` and `From<$type>`.
#[macro_export]
macro_rules! wrapped_prim {
    ($name:ident, $type:ty) => {
        wrapped_prim!($name, $type, );
    };
    ($name:ident, $type:ty, $($extra_derives:ident),*) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, $($extra_derives),*)]
        pub struct $name($type);

        impl $name {
            pub fn inner(self) -> $type {
                self.0
            }
        }

        impl From<$type> for $name {
            fn from(t: $type) -> Self {
                Self(t)
            }
        }

        impl<'lua> FromLua<'lua> for $name {
            fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
                Ok(Self(FromLua::from_lua(value, lua)?))
            }
        }

        impl<'lua> IntoLua<'lua> for $name {
            fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
                Ok(self.0.into_lua(lua)?)
            }
        }
    };
}

/// A `FromLuaConversionError` for a failed conversion to the type `to`.
pub fn cvt_err(to: &'static str) -> LuaError {
    LuaError::FromLuaConversionError { from: "value", to, message: None }
}

/// A Lua runtime error with message `msg`.
pub fn err(msg: &str) -> LuaError {
    LuaError::runtime(msg)
}

/// Borrow `value` as a table, or fail naming the target type `to`.
pub fn as_tbl_ref<'a: 'lua, 'lua>(
    to: &'static str,
    value: &'a Value<'lua>,
) -> Result<&'a mlua::Table<'lua>> {
    value.as_table().ok_or_else(|| anyhow!("can't convert {:?} to {}", value, to))
}

// DCS classes are metatables with a `className_` field and a `parentClass_`
// link to the superclass. walk that chain looking for `class`.
fn check_implements(tbl: &mlua::Table, class: &str) -> bool {
    let mut parent = None;
    loop {
        let tbl = match parent.as_ref() {
            None => tbl,
            Some(tbl) => tbl,
        };
        match tbl.raw_get::<_, String>("className_") {
            Err(_) => break false,
            Ok(s) if s.as_str() == class => break true,
            Ok(_) => match tbl.raw_get::<_, mlua::Table>("parentClass_") {
                Err(_) => break false,
                Ok(t) => {
                    parent = Some(t);
                }
            },
        }
    }
}

/// Convert `value` to a table, failing if it isn't one. `to` names the
/// target type in error messages.
///
/// If `objtyp` is `Some(class)`, the table must also be a DCS object of
/// that class or a subclass: it must have a metatable whose
/// `className_` / `parentClass_` chain includes `class`.
pub fn as_tbl<'lua>(
    to: &'static str,
    objtyp: Option<&'static str>,
    value: Value<'lua>,
) -> Result<mlua::Table<'lua>> {
    match value {
        Value::Table(tbl) => match objtyp {
            None => Ok(tbl),
            Some(typ) => match tbl.get_metatable() {
                None => bail!(
                    "to: {to}. not an object, expected object of type {} got {:?}",
                    typ,
                    tbl
                ),
                Some(meta) => {
                    if check_implements(&meta, typ) {
                        Ok(tbl)
                    } else {
                        bail!("to: {to}. expected object of type {}, got {:?}", typ, tbl)
                    }
                }
            },
        },
        _ => bail!("expected a table, got {:?}", value),
    }
}

/// Copy a Lua value so that changes to the copy don't affect the original.
pub trait DeepClone<'lua>: IntoLua<'lua> + FromLua<'lua> + Clone {
    /// Tables are copied recursively (keys and values), and the copy shares
    /// the original's metatable. Other values (functions, userdata, threads)
    /// are shared, not copied. Cyclic tables are not handled.
    fn deep_clone(&self, lua: &'lua Lua) -> Result<Self>;
}

impl<'lua, T> DeepClone<'lua> for T
where
    T: IntoLua<'lua> + FromLua<'lua> + Clone,
{
    fn deep_clone(&self, lua: &'lua Lua) -> Result<Self> {
        let v = match self.clone().into_lua(lua)? {
            Value::Boolean(b) => Value::Boolean(b),
            Value::Error(e) => Value::Error(e),
            Value::Function(f) => Value::Function(f),
            Value::Integer(i) => Value::Integer(i),
            Value::LightUserData(d) => Value::LightUserData(d),
            Value::Nil => Value::Nil,
            Value::Number(n) => Value::Number(n),
            Value::String(s) => Value::String(lua.create_string(s)?),
            Value::Table(t) => {
                let new = lua.create_table()?;
                new.set_metatable(t.get_metatable());
                for r in t.pairs::<Value, Value>() {
                    let (k, v) = r?;
                    new.set(k.deep_clone(lua)?, v.deep_clone(lua)?)?
                }
                Value::Table(new)
            }
            Value::Thread(t) => Value::Thread(t),
            Value::UserData(d) => Value::UserData(d),
        };
        Ok(T::from_lua(v, lua)?)
    }
}

/// True if `lua` is the hooks environment, detected by the presence of the
/// `DCS` global.
pub fn is_hooks_env(lua: &Lua) -> bool {
    lua.globals().contains_key("DCS").unwrap_or(false)
}

/// One key in a [`Path`]: an integer (array index) or a string.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum PathElt {
    Integer(i64),
    String(String),
}

impl From<&str> for PathElt {
    fn from(value: &str) -> Self {
        PathElt::String(String::from(value))
    }
}

impl From<String> for PathElt {
    fn from(value: String) -> Self {
        PathElt::String(value)
    }
}

impl From<std::string::String> for PathElt {
    fn from(value: std::string::String) -> Self {
        PathElt::String(String::from(value))
    }
}

impl From<usize> for PathElt {
    fn from(value: usize) -> Self {
        PathElt::Integer(value as i64)
    }
}

impl From<u64> for PathElt {
    fn from(value: u64) -> Self {
        PathElt::Integer(value as i64)
    }
}

impl From<u32> for PathElt {
    fn from(value: u32) -> Self {
        PathElt::Integer(value as i64)
    }
}

impl From<i64> for PathElt {
    fn from(value: i64) -> Self {
        PathElt::Integer(value)
    }
}

impl From<i32> for PathElt {
    fn from(value: i32) -> Self {
        PathElt::Integer(value as i64)
    }
}

impl From<u8> for PathElt {
    fn from(value: u8) -> Self {
        PathElt::Integer(value as i64)
    }
}

impl<'lua> IntoLua<'lua> for &PathElt {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        Ok(match self {
            PathElt::Integer(i) => Value::Integer(*i),
            PathElt::String(s) => Value::String(lua.create_string(s.as_bytes())?),
        })
    }
}

impl<'lua> FromLua<'lua> for PathElt {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        Ok(match value {
            Value::Integer(n) => PathElt::Integer(n),
            Value::String(_) => PathElt::String(String::from_lua(value, lua)?),
            _ => return Err(cvt_err("String")),
        })
    }
}

/// A sequence of keys locating a value nested inside Lua tables, e.g.
/// `coalition.blue.country[1]`. Build one with [`path!`] and look it up
/// with [`DcsTableExt`].
#[derive(Debug, Clone, Serialize, Default)]
pub struct Path(Vec<PathElt>);

impl Deref for Path {
    type Target = [PathElt];

    fn deref(&self) -> &Self::Target {
        &self.0[..]
    }
}

impl<'a> IntoIterator for &'a Path {
    type IntoIter = std::slice::Iter<'a, PathElt>;
    type Item = &'a PathElt;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Path {
    pub fn new() -> Self {
        Self(vec![])
    }

    pub fn push<T: Into<PathElt>>(&mut self, t: T) {
        self.0.push(t.into())
    }

    pub fn pop(&mut self) -> Option<PathElt> {
        self.0.pop()
    }

    /// A copy of this path with `elts` added to the end.
    pub fn append<T: Into<PathElt>, I: IntoIterator<Item = T>>(&self, elts: I) -> Self {
        let mut new_t = self.clone();
        for elt in elts {
            new_t.push(elt)
        }
        new_t
    }

    pub fn get(&self, i: usize) -> Option<&PathElt> {
        self.0.get(i)
    }
}

/// Build a [`Path`] from a list of keys, each anything that converts into a
/// [`PathElt`], e.g. `path!["coalition", "blue", "country", 1]`. Expands to
/// `dcso3::Path`, so it only works where the crate is named `dcso3`.
#[macro_export]
macro_rules! path {
    ($($v:expr),*) => {{
        let mut path = dcso3::Path::new();
        $(path.push($v));*;
        path
    }}
}

/// Look up a [`Path`] in nested tables. Fails if the path is empty, if an
/// intermediate value isn't a table, or if the final value doesn't convert
/// to `T`.
pub trait DcsTableExt<'lua> {
    /// Look up `path` using raw gets (ignoring metatables)
    fn raw_get_path<T>(&self, path: &Path) -> Result<T>
    where
        T: FromLua<'lua>;

    /// Look up `path` using normal gets (respecting `__index` metamethods)
    fn get_path<T>(&self, path: &Path) -> Result<T>
    where
        T: FromLua<'lua>;
}

fn table_raw_get_path<'lua, T>(tbl: &mlua::Table<'lua>, path: &[PathElt]) -> Result<T>
where
    T: FromLua<'lua>,
{
    match path {
        [] => bail!("path not found"),
        [elt] => Ok(tbl.raw_get(elt)?),
        [elt, path @ ..] => {
            let tbl: mlua::Table = tbl.raw_get(elt)?;
            table_raw_get_path(&tbl, path)
        }
    }
}

fn table_get_path<'lua, T>(tbl: &mlua::Table<'lua>, path: &[PathElt]) -> Result<T>
where
    T: FromLua<'lua>,
{
    match path {
        [] => bail!("path not found"),
        [elt] => Ok(tbl.get(elt)?),
        [elt, path @ ..] => {
            let tbl: mlua::Table = tbl.get(elt)?;
            table_get_path(&tbl, path)
        }
    }
}

impl<'lua> DcsTableExt<'lua> for mlua::Table<'lua> {
    fn raw_get_path<T>(&self, path: &Path) -> Result<T>
    where
        T: FromLua<'lua>,
    {
        table_raw_get_path(self, &**path)
    }

    fn get_path<T>(&self, path: &Path) -> Result<T>
    where
        T: FromLua<'lua>,
    {
        table_get_path(self, &**path)
    }
}

/// A 2d vector. In DCS map coordinates `x` is north/south and `y` is
/// east/west (see [`pointing_towards2`]).
pub type Vector2 = na::base::Vector2<f64>;

/// A [`Vector2`] that converts to and from a DCS `Vec2` table `{x, y}`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
pub struct LuaVec2(pub na::base::Vector2<f64>);

impl Deref for LuaVec2 {
    type Target = na::base::Vector2<f64>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for LuaVec2 {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'lua> IntoLua<'lua> for LuaVec2 {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        let tbl = lua.create_table()?;
        tbl.set("x", self.0.x)?;
        tbl.set("y", self.0.y)?;
        Ok(Value::Table(tbl))
    }
}

impl<'lua> FromLua<'lua> for LuaVec2 {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> LuaResult<Self> {
        let tbl = as_tbl("Vec2", None, value).map_err(lua_err)?;
        Ok(Self(na::base::Vector2::new(tbl.raw_get("x")?, tbl.raw_get("y")?)))
    }
}

impl LuaVec2 {
    pub fn new(x: f64, y: f64) -> Self {
        LuaVec2(na::base::Vector2::new(x, y))
    }
}

/// A 3d vector. In DCS world coordinates `y` is altitude, and `x` and `z`
/// are the horizontal axes.
pub type Vector3 = na::base::Vector3<f64>;

/// A [`Vector3`] that converts to and from a DCS `Vec3` table `{x, y, z}`.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize, Deserialize)]
pub struct LuaVec3(pub na::base::Vector3<f64>);

impl Deref for LuaVec3 {
    type Target = na::base::Vector3<f64>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for LuaVec3 {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<'lua> FromLua<'lua> for LuaVec3 {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> LuaResult<Self> {
        let tbl = as_tbl("Vec3", None, value).map_err(lua_err)?;
        Ok(Self(na::base::Vector3::new(
            tbl.raw_get("x")?,
            tbl.raw_get("y")?,
            tbl.raw_get("z")?,
        )))
    }
}

impl<'lua> IntoLua<'lua> for LuaVec3 {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        let tbl = lua.create_table()?;
        tbl.raw_set("x", self.0.x)?;
        tbl.raw_set("y", self.0.y)?;
        tbl.raw_set("z", self.0.z)?;
        Ok(Value::Table(tbl))
    }
}

impl LuaVec3 {
    pub fn new(x: f64, y: f64, z: f64) -> Self {
        Self(na::base::Vector3::new(x, y, z))
    }
}

/// A DCS `Position3`: a point and an orientation.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Position3 {
    /// The position
    pub p: LuaVec3,
    /// Unit vector pointing forward
    pub x: LuaVec3,
    /// Unit vector pointing up
    pub y: LuaVec3,
    /// Unit vector pointing right
    pub z: LuaVec3,
}

impl<'lua> FromLua<'lua> for Position3 {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> LuaResult<Self> {
        let tbl = as_tbl("Position3", None, value).map_err(lua_err)?;
        Ok(Self {
            p: tbl.raw_get("p")?,
            x: tbl.raw_get("x")?,
            y: tbl.raw_get("y")?,
            z: tbl.raw_get("z")?,
        })
    }
}

impl<'lua> IntoLua<'lua> for Position3 {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        let tbl = lua.create_table()?;
        tbl.raw_set("p", self.p)?;
        tbl.raw_set("x", self.x)?;
        tbl.raw_set("y", self.y)?;
        tbl.raw_set("z", self.z)?;
        Ok(Value::Table(tbl))
    }
}

/// A DCS `Box3`: an axis aligned box given by its `min` and `max` corners.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Box3 {
    pub min: LuaVec3,
    pub max: LuaVec3,
}

impl<'lua> FromLua<'lua> for Box3 {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> LuaResult<Self> {
        let tbl = as_tbl("Box3", None, value).map_err(lua_err)?;
        Ok(Self { min: tbl.raw_get("min")?, max: tbl.raw_get("max")? })
    }
}

/// The crate's string type, a [`CompactString`] (short strings are stored
/// inline). Converting from Lua accepts more than strings: booleans and
/// numbers are formatted as text, and other values use mlua's
/// `Value::to_string`.
#[derive(
    Debug, Clone, Default, Hash, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct String(CompactString);

impl std::fmt::Display for String {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Deref for String {
    type Target = compact_str::CompactString;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for String {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AsRef<str> for String {
    fn as_ref(&self) -> &str {
        self.0.as_ref()
    }
}

impl Borrow<str> for String {
    fn borrow(&self) -> &str {
        self.0.borrow()
    }
}

impl<'lua> IntoLua<'lua> for String {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        Ok(Value::String(lua.create_string(self.0)?))
    }
}

impl<'lua> FromLua<'lua> for String {
    fn from_lua(value: Value<'lua>, _: &'lua Lua) -> LuaResult<Self> {
        use compact_str::format_compact;
        match value {
            Value::String(s) => Ok(Self(CompactString::from(s.to_str()?))),
            Value::Boolean(b) => Ok(Self(format_compact!("{b}"))),
            Value::Integer(n) => Ok(Self(format_compact!("{n}"))),
            Value::Number(n) => Ok(Self(format_compact!("{n}"))),
            v => Ok(Self(CompactString::from(v.to_string()?))),
        }
    }
}

impl From<&str> for String {
    fn from(value: &str) -> Self {
        Self(CompactString::from(value))
    }
}

impl From<std::string::String> for String {
    fn from(value: std::string::String) -> Self {
        Self(CompactString::from(value))
    }
}

impl From<CompactString> for String {
    fn from(value: CompactString) -> Self {
        Self(value)
    }
}

/// A DCS time value in seconds, as returned by e.g. `timer.getTime`.
/// Subtracting two times gives the difference in seconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Time(pub f32);

impl<'lua> IntoLua<'lua> for Time {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        self.0.into_lua(lua)
    }
}

impl<'lua> FromLua<'lua> for Time {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        Ok(Self(f32::from_lua(value, lua)?))
    }
}

impl Add<f32> for Time {
    type Output = Self;

    fn add(self, rhs: f32) -> Self::Output {
        Time(self.0 + rhs)
    }
}

impl AddAssign<f32> for Time {
    fn add_assign(&mut self, rhs: f32) {
        self.0 += rhs
    }
}

impl Sub for Time {
    type Output = f32;

    fn sub(self, rhs: Self) -> f32 {
        self.0 - rhs.0
    }
}

/// The kinds of volume DCS can search in (`world.VolumeType`).
#[derive(Debug, Clone, Serialize)]
pub enum VolumeType {
    Segment,
    Box,
    Sphere,
    Pyramid,
}

/// A typed view of a Lua array table whose elements are `T`. Elements are
/// converted when accessed, so a bad element only fails when it is read.
/// Converting `nil` from Lua gives an empty sequence. Indexes are 1-based,
/// as in Lua.
#[derive(Debug, Clone, Serialize)]
pub struct Sequence<'lua, T> {
    t: mlua::Table<'lua>,
    #[serde(skip)]
    lua: &'lua Lua,
    ph: PhantomData<T>,
}

impl<'lua, T: FromLua<'lua> + 'lua> FromLua<'lua> for Sequence<'lua, T> {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        match value {
            Value::Table(t) => Ok(Self { t, lua, ph: PhantomData }),
            Value::Nil => Ok(Self { t: lua.create_table()?, lua, ph: PhantomData }),
            _ => Err(cvt_err("Sequence")),
        }
    }
}

impl<'lua, T: IntoLua<'lua> + 'lua> IntoLua<'lua> for Sequence<'lua, T> {
    fn into_lua(self, _lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        Ok(Value::Table(self.t))
    }
}

impl<'lua, T: FromLua<'lua> + 'lua> IntoIterator for Sequence<'lua, T> {
    type IntoIter = mlua::TableSequence<'lua, T>;
    type Item = LuaResult<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.t.sequence_values()
    }
}

impl<'lua, T: FromLua<'lua> + 'lua> Sequence<'lua, T> {
    pub fn get(&self, i: i64) -> Result<T> {
        Ok(self.t.raw_get(i)?)
    }
}

impl<'lua, T: IntoLua<'lua> + 'lua> Sequence<'lua, T> {
    pub fn set(&self, i: i64, t: T) -> Result<()> {
        Ok(self.t.raw_set(i, t)?)
    }
}

impl<'lua, T: FromLua<'lua> + 'lua> Sequence<'lua, T> {
    pub fn empty(lua: &'lua Lua) -> Result<Self> {
        Ok(Self { t: lua.create_table()?, lua: lua, ph: PhantomData })
    }

    pub fn len(&self) -> usize {
        self.t.raw_len()
    }

    pub fn into_inner(self) -> mlua::Table<'lua> {
        self.t
    }

    pub fn remove(&self, i: i64) -> Result<()> {
        Ok(self.t.raw_remove(i)?)
    }

    pub fn first(&self) -> Result<T> {
        Ok(self.t.raw_get(1)?)
    }

    /// Call `f` with every value in the table. This iterates all pairs, not
    /// just the array part, and not necessarily in index order. Iteration
    /// stops at, and returns, the first error from `f`.
    pub fn for_each<F: FnMut(Result<T>) -> Result<()>>(&self, mut f: F) -> Result<()> {
        Ok(self.t.for_each(|_: Value, v: Value| {
            f(T::from_lua(v, &self.lua).map_err(anyhow::Error::from)).map_err(lua_err)
        })?)
    }
}

impl<'lua, T: FromLua<'lua> + IntoLua<'lua> + 'lua> Sequence<'lua, T> {
    pub fn push(&self, t: T) -> Result<()> {
        Ok(self.t.push(t)?)
    }

    pub fn pop(&self) -> Result<T> {
        Ok(self.t.pop()?)
    }
}

/// Convert a Lua value to JSON, for debugging output.
///
/// Tables become objects keyed by the stringified key. Functions, userdata,
/// and threads become placeholder strings. A table that has already been
/// converted (whether a cycle or just shared) becomes
/// `"<Table(0x.. key)>"`, naming the key it was first seen under.
pub fn value_to_json(v: &Value) -> serde_json::Value {
    // the map of already visited tables, by address, to the key they were
    // first seen under. cleared after each top level call.
    thread_local! {
        static CTX: RefCell<FxHashMap<usize, String>> = RefCell::new(FxHashMap::default());
    }
    fn inner(
        ctx: &mut FxHashMap<usize, String>,
        key: Option<&str>,
        v: &Value,
    ) -> serde_json::Value {
        use serde_json::{json, Map, Value as JVal};
        match v {
            Value::Nil => JVal::Null,
            Value::Boolean(b) => json!(b),
            Value::LightUserData(_) => json!("<LightUserData>"),
            Value::Integer(i) => json!(*i),
            Value::Number(i) => json!(*i),
            Value::UserData(_) => json!("<UserData>"),
            Value::String(s) => json!(s),
            Value::Function(_) => json!("<Function>"),
            Value::Thread(_) => json!("<Thread>"),
            Value::Error(e) => json!(format!("{e}")),
            Value::Table(tbl) => {
                let address = tbl.to_pointer() as usize;
                match ctx.entry(address) {
                    Entry::Occupied(e) => {
                        json!(format!("<Table(0x{:x} {})>", address, e.get()))
                    }
                    Entry::Vacant(e) => {
                        e.insert(String::from(key.unwrap_or("Root")));
                        let mut map = Map::new();
                        for pair in tbl.clone().pairs::<Value, Value>() {
                            let (k, v) = pair.unwrap();
                            let k = match inner(ctx, None, &k) {
                                JVal::String(s) => s,
                                v => v.to_string(),
                            };
                            let v = inner(ctx, Some(k.as_str()), &v);
                            map.insert(k, v);
                        }
                        JVal::Object(map)
                    }
                }
            }
        }
    }
    CTX.with_borrow_mut(|ctx| {
        let r = inner(ctx, None, v);
        ctx.clear();
        r
    })
}

/// The average of `points`, or the zero vector if there are none.
pub fn centroid2d(points: impl IntoIterator<Item = Vector2>) -> Vector2 {
    let (n, sum) =
        points.into_iter().fold((0, Vector2::new(0., 0.)), |(n, c), p| (n + 1, c + p));
    if n == 0 {
        sum
    } else {
        sum / (n as f64)
    }
}

/// The average of `points`, or the zero vector if there are none.
pub fn centroid3d(points: impl IntoIterator<Item = Vector3>) -> Vector3 {
    let (n, sum) = points
        .into_iter()
        .fold((0, Vector3::new(0., 0., 0.)), |(n, c), p| (n + 1, c + p));
    if n == 0 {
        sum
    } else {
        sum / (n as f64)
    }
}

/// Rotate a collection of points in 2d space around their center
/// point keeping the relative orientations of the points
/// constant. The angle is in radians. General about the underlying
/// point container type
pub fn rotate2d_gen<T, F: Fn(&mut T) -> &mut Vector2>(
    angle: f64,
    points: &mut [T],
    f: F,
) {
    let centroid = centroid2d(points.into_iter().map(|t| *f(t)));
    let sin = angle.sin();
    let cos = angle.cos();
    for t in points {
        let p = f(t);
        *p -= centroid;
        let x = p.x;
        let y = p.y;
        p.x = x * cos - y * sin;
        p.y = x * sin + y * cos;
        *p += centroid
    }
}

/// Rotate a collection of points in 2d space around their center point
/// keeping the relative orientations of the points constant. The angle
/// is in radians.
pub fn rotate2d(angle: f64, points: &mut [Vector2]) {
    rotate2d_gen(angle, points, |x| x)
}

/// return a unit vector pointing in the specified direction. angle is in radians
pub fn pointing_towards2(angle: f64) -> Vector2 {
    let sin = angle.sin();
    let cos = angle.cos();
    // dcs coords are reversed x is north/south y is east/west
    Vector2::new(cos, sin).normalize()
}

/// A vector perpendicular to `v` with the same length, `(v.y, -v.x)`.
pub fn normal2(v: Vector2) -> Vector2 {
    Vector2::new(v.y, -v.x)
}

/// Same as rotate2d, but construct and return a vec containing the rotated points
/// in the same order as the they appear in the input slice.
pub fn rotate2d_vec(angle: f64, points: &[Vector2]) -> Vec<Vector2> {
    let mut points = Vec::from_iter(points.into_iter().map(|p| *p));
    rotate2d(angle, &mut points);
    points
}

pub fn radians_to_degrees(radians: f64) -> f64 {
    radians * (180. / std::f64::consts::PI)
}

pub fn degrees_to_radians(degrees: f64) -> f64 {
    degrees * (std::f64::consts::PI / 180.)
}

/// change the heading (in radians) by the specified amount, adjusting if it rotates through 0/2 pi.
/// e.g. `change_heading(3/2 pi, pi) -> pi / 2 not 5 / 2 pi`
pub fn change_heading(heading: f64, change: f64) -> f64 {
    const PI2: f64 = f64::consts::PI * 2.;
    let change = if change.abs() > PI2 { change % PI2 } else { change };
    let res = heading + change;
    if res > PI2 {
        res - PI2
    } else if res < 0. {
        res + PI2
    } else {
        res
    }
}

/// The azimuth (heading) of `v` in radians, in 0..2pi, measured from the
/// `x` (north) axis towards the `y` axis.
pub fn azumith2d(v: Vector2) -> f64 {
    let az = v.y.atan2(v.x);
    if az < 0. {
        az + 2. * std::f64::consts::PI
    } else {
        az
    }
}

/// The azimuth in radians from `from` to `to` (see [`azumith2d`]).
pub fn azumith2d_to(from: Vector2, to: Vector2) -> f64 {
    azumith2d(to - from)
}

/// The azimuth of `v` in radians, in 0..2pi, in the horizontal (`x`, `z`)
/// plane, measured from the `x` axis towards the `z` axis. Altitude (`y`)
/// is ignored.
pub fn azumith3d(v: Vector3) -> f64 {
    let az = v.z.atan2(v.x);
    if az < 0. {
        az + 2. * std::f64::consts::PI
    } else {
        az
    }
}

/// The azimuth in radians from `from` to `to` (see [`azumith3d`]).
pub fn azumith3d_to(from: Vector3, to: Vector3) -> f64 {
    azumith3d(to - from)
}
