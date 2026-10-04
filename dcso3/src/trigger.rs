/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Bindings to the DCS `trigger` mission scripting API.
//!
//! [`Trigger`] wraps the global `trigger` table. Its `action` sub table,
//! wrapped by [`Action`], holds the functions that make things happen:
//! user flags, on screen text and sounds, smoke, flares, illumination
//! bombs, explosions, radio transmissions, and F10 map marks and drawn
//! shapes (lines, circles, rectangles, quads, text, arrows) along with
//! the `setMarkup*` functions that modify existing shapes. The `misc`
//! sub table is used only for [`Trigger::get_zone`].
//!
//! Marks and shapes are identified by a caller chosen [`MarkId`].

use crate::{
    as_tbl, atomic_id,
    coalition::Side,
    cvt_err,
    env::miz::{Country, GroupId, UnitId},
    simple_enum, wrapped_table, Color, LuaEnv, LuaVec3, MizLua, String,
};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::ops::Deref;

// The id of an F10 map mark or drawn shape. Allocate new ids with
// `MarkId::new()`.
atomic_id!(MarkId);

/// A circular trigger zone as returned by `trigger.misc.getZone`
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zone {
    pub point: LuaVec3,
    pub radius: f64,
}

impl<'lua> FromLua<'lua> for Zone {
    fn from_lua(value: Value<'lua>, _lua: &'lua Lua) -> LuaResult<Self> {
        match value {
            Value::Table(tbl) => Ok(Self {
                point: tbl.raw_get("point")?,
                radius: tbl.raw_get("radius")?,
            }),
            _ => Err(cvt_err("trigger::Zone")),
        }
    }
}

impl<'lua> IntoLua<'lua> for Zone {
    fn into_lua(self, lua: &'lua Lua) -> LuaResult<Value<'lua>> {
        let table = lua.create_table()?;
        table.raw_set("point", self.point)?;
        table.raw_set("radius", self.radius)?;
        Ok(Value::Table(table))
    }
}

// Smoke marker colors for `Action::smoke` (`trigger.smokeColor`).
simple_enum!(SmokeColor, u8, [
    Green => 0,
    Red => 1,
    White => 2,
    Orange => 3,
    Blue => 4
]);

// The effect presets for `Action::effect_smoke_big`.
simple_enum!(SmokePreset, u8, [
    SmallSmokeAndFire => 1,
    MediumSmokeAndFire => 2,
    LargeSmokeAndFire => 3,
    HugeSmokeAndFire => 4,
    SmallSmoke => 5,
    MediumSmoke => 6,
    LargeSmoke => 7,
    HugeSmoke => 8
]);

// Signal flare colors for `Action::signal_flare` (`trigger.flareColor`).
simple_enum!(FlareColor, u8, [
    Green => 0,
    Red => 1,
    White => 2,
    Yellow => 3
]);

// Radio modulation for `Action::radio_transmission`.
simple_enum!(Modulation, u8, [
    AM => 0,
    FM => 1
]);

// Who can see a drawn shape: one coalition, or everyone (-1). The first
// argument of the `*ToAll` shape functions.
simple_enum!(SideFilter, i8, [
    All => -1,
    Neutral => 0,
    Red => 1,
    Blue => 2
]);

impl SideFilter {
    /// True if a shape drawn with this filter is meant for `side`
    pub fn is_match(&self, side: &Side) -> bool {
        match (self, side) {
            (Self::All, _) => true,
            (Self::Neutral, Side::Neutral) => true,
            (Self::Blue, Side::Blue) => true,
            (Self::Red, Side::Red) => true,
            (_, _) => false
        }
    }
}

impl From<Side> for SideFilter {
    fn from(value: Side) -> Self {
        match value {
            Side::Blue => SideFilter::Blue,
            Side::Red => SideFilter::Red,
            Side::Neutral => SideFilter::Neutral,
        }
    }
}

// The outline style of a drawn shape.
simple_enum!(LineType, u8, [
    NoLine => 0,
    Solid => 1,
    Dashed => 2,
    Dotted => 3,
    DotDash => 4,
    LongDash => 5,
    TwoDash => 6
]);

/// The arguments of [`Action::line_to_all`]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct LineSpec {
    pub start: LuaVec3,
    pub end: LuaVec3,
    pub color: Color,
    pub line_type: LineType,
    /// If true players can't remove the shape
    pub read_only: bool,
}

/// The arguments of [`Action::circle_to_all`]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct CircleSpec {
    pub center: LuaVec3,
    pub radius: f64,
    pub color: Color,
    pub fill_color: Color,
    pub line_type: LineType,
    pub read_only: bool,
}

/// The arguments of [`Action::rect_to_all`]; `start` and `end` are opposite
/// corners
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RectSpec {
    pub start: LuaVec3,
    pub end: LuaVec3,
    pub color: Color,
    pub fill_color: Color,
    pub line_type: LineType,
    pub read_only: bool,
}

/// The arguments of [`Action::quad_to_all`], a four sided polygon
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct QuadSpec {
    pub p0: LuaVec3,
    pub p1: LuaVec3,
    pub p2: LuaVec3,
    pub p3: LuaVec3,
    pub color: Color,
    pub fill_color: Color,
    pub line_type: LineType,
    pub read_only: bool,
}

/// The arguments of [`Action::text_to_all`]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextSpec {
    pub pos: LuaVec3,
    pub color: Color,
    pub fill_color: Color,
    pub font_size: u8,
    pub read_only: bool,
    pub text: String,
}

/// The arguments of [`Action::arrow_to_all`]
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ArrowSpec {
    pub start: LuaVec3,
    pub end: LuaVec3,
    pub color: Color,
    pub fill_color: Color,
    pub line_type: LineType,
    pub read_only: bool,
}

// The `trigger.action` table. Each method calls the DCS function of the same
// (camel case) name.
wrapped_table!(Action, None);

impl<'lua> Action<'lua> {
    /// Set user flag `key` to `value` (`trigger.action.setUserFlag`)
    pub fn set_user_flag<K: IntoLua<'lua>, V: IntoLua<'lua>>(
        &self,
        key: K,
        value: V,
    ) -> Result<()> {
        Ok(self.call_function("setUserFlag", (key, value))?)
    }

    /// Run the triggered action number `num` of a group (`setAITask`)
    pub fn set_ai_task(&self, group: GroupId, num: i64) -> Result<()> {
        Ok(self.call_function("setAITask", (group, num))?)
    }

    /// Create an explosion of strength `power` at `position`
    pub fn explosion(&self, position: LuaVec3, power: f32) -> Result<()> {
        Ok(self.call_function("explosion", (position, power))?)
    }

    /// Create a colored smoke marker at `position`
    pub fn smoke(&self, position: LuaVec3, color: SmokeColor) -> Result<()> {
        Ok(self.call_function("smoke", (position, color))?)
    }

    /// Start a large smoke (and optionally fire) effect at `position`
    /// (`effectSmokeBig`). `name` identifies it for
    /// [`Action::effect_smoke_stop`].
    pub fn effect_smoke_big(
        &self,
        position: LuaVec3,
        preset: SmokePreset,
        density: f32,
        name: String,
    ) -> Result<()> {
        Ok(self.call_function("effectSmokeBig", (position, preset, density, name))?)
    }

    /// Stop the smoke effect started with `name`
    pub fn effect_smoke_stop(&self, name: String) -> Result<()> {
        Ok(self.call_function("effectSmokeStop", name)?)
    }

    /// Fire an illumination bomb at `position` (`illuminationBomb`)
    pub fn illumination_bomb(&self, position: LuaVec3, power: f32) -> Result<()> {
        Ok(self.call_function("illuminationBomb", (position, power))?)
    }

    /// Fire a signal flare from `position` toward `azimuth` (`signalFlare`)
    pub fn signal_flare(&self, position: LuaVec3, color: FlareColor, azimuth: u16) -> Result<()> {
        Ok(self.call_function("signalFlare", (position, color, azimuth))?)
    }

    /// Transmit the sound `file` from `origin` on `frequency`
    /// (`radioTransmission`). If `repeat` is true it loops. `name`
    /// identifies the transmission for [`Action::stop_transmission`].
    pub fn radio_transmission(
        &self,
        file: String,
        origin: LuaVec3,
        modulation: Modulation,
        repeat: bool,
        frequency: u64,
        power: u64,
        name: String,
    ) -> Result<()> {
        Ok(self.call_function(
            "radioTransmission",
            (file, origin, modulation, repeat, frequency, power, name),
        )?)
    }

    /// Stop the radio transmission started with `name`
    /// (`stopRadioTransmission`)
    pub fn stop_transmission(&self, name: String) -> Result<()> {
        Ok(self.call_function("stopRadioTransmission", name)?)
    }

    /// Set the internal cargo mass carried by the unit called `unit_name`
    /// (`setUnitInternalCargo`)
    pub fn set_unit_internal_cargo(&self, unit_name: String, mass: i64) -> Result<()> {
        Ok(self.call_function("setUnitInternalCargo", (unit_name, mass))?)
    }

    // The out_sound* functions play the sound `file` to everyone, or to the
    // given coalition, country, group, or unit.

    pub fn out_sound(&self, file: String) -> Result<()> {
        Ok(self.call_function("outSound", file)?)
    }

    pub fn out_sound_for_coalition(&self, side: Side, file: String) -> Result<()> {
        Ok(self.call_function("outSoundForCoalition", (side, file))?)
    }

    pub fn out_sound_for_country(&self, country: Country, file: String) -> Result<()> {
        Ok(self.call_function("outSoundForCountry", (country, file))?)
    }

    pub fn out_sound_for_group(&self, group: GroupId, file: String) -> Result<()> {
        Ok(self.call_function("outSoundForGroup", (group, file))?)
    }

    pub fn out_sound_for_unit(&self, unit: UnitId, file: String) -> Result<()> {
        Ok(self.call_function("outSoundForUnit", (unit, file))?)
    }

    // The out_text* functions show `text` on screen for `display_time`
    // seconds to everyone, or to the given coalition, country, group, or
    // unit. `clear_view` is passed through as DCS's `clearview` argument.

    pub fn out_text(&self, text: String, display_time: i64, clear_view: bool) -> Result<()> {
        Ok(self.call_function("outText", (text, display_time, clear_view))?)
    }

    pub fn out_text_for_coalition(
        &self,
        side: Side,
        text: String,
        display_time: i64,
        clear_view: bool,
    ) -> Result<()> {
        Ok(self.call_function(
            "outTextForCoalition",
            (side, text, display_time, clear_view),
        )?)
    }

    pub fn out_text_for_country(
        &self,
        country: Country,
        text: String,
        display_time: i64,
        clear_view: bool,
    ) -> Result<()> {
        Ok(self.call_function(
            "outTextForCountry",
            (country, text, display_time, clear_view),
        )?)
    }

    pub fn out_text_for_group(
        &self,
        group: GroupId,
        text: String,
        display_time: i64,
        clear_view: bool,
    ) -> Result<()> {
        Ok(self.call_function("outTextForGroup", (group, text, display_time, clear_view))?)
    }

    pub fn out_text_for_unit(
        &self,
        unit: UnitId,
        text: String,
        display_time: i64,
        clear_view: bool,
    ) -> Result<()> {
        Ok(self.call_function("outTextForUnit", (unit, text, display_time, clear_view))?)
    }

    // The mark_to_* functions place an F10 map mark with label `text` at
    // `position`, visible to everyone, one coalition, or one group.
    // `message`, if given, is shown when the mark is added.

    pub fn mark_to_all(
        &self,
        id: MarkId,
        text: String,
        position: LuaVec3,
        read_only: bool,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function("markToAll", (id, text, position, read_only, message))?)
    }

    pub fn mark_to_coalition(
        &self,
        id: MarkId,
        text: String,
        position: LuaVec3,
        side: Side,
        read_only: bool,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function(
            "markToCoalition",
            (id, text, position, side, read_only, message),
        )?)
    }

    pub fn mark_to_group(
        &self,
        id: MarkId,
        text: String,
        position: LuaVec3,
        group: GroupId,
        read_only: bool,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function(
            "markToGroup",
            (id, text, position, group, read_only, message),
        )?)
    }

    /// Remove the mark or drawn shape `id` (`removeMark`)
    pub fn remove_mark(&self, id: MarkId) -> Result<()> {
        Ok(self.call_function("removeMark", id)?)
    }

    // The *_to_all shape functions draw on the F10 map for the coalitions
    // selected by `side`. The spec struct fields are passed in the
    // positional order DCS expects. `message`, if given, is shown when the
    // shape is added.

    pub fn line_to_all(
        &self,
        side: SideFilter,
        id: MarkId,
        spec: LineSpec,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function(
            "lineToAll",
            (
                side,
                id,
                spec.start,
                spec.end,
                spec.color,
                spec.line_type,
                spec.read_only,
                message,
            ),
        )?)
    }

    pub fn circle_to_all(
        &self,
        side: SideFilter,
        id: MarkId,
        spec: CircleSpec,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function(
            "circleToAll",
            (
                side,
                id,
                spec.center,
                spec.radius,
                spec.color,
                spec.fill_color,
                spec.line_type,
                spec.read_only,
                message,
            ),
        )?)
    }

    pub fn rect_to_all(
        &self,
        side: SideFilter,
        id: MarkId,
        spec: RectSpec,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function(
            "rectToAll",
            (
                side,
                id,
                spec.start,
                spec.end,
                spec.color,
                spec.fill_color,
                spec.line_type,
                spec.read_only,
                message,
            ),
        )?)
    }

    pub fn quad_to_all(
        &self,
        side: SideFilter,
        id: MarkId,
        spec: QuadSpec,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function(
            "quadToAll",
            (
                side,
                id,
                spec.p0,
                spec.p1,
                spec.p2,
                spec.p3,
                spec.color,
                spec.fill_color,
                spec.line_type,
                spec.read_only,
                message,
            ),
        )?)
    }

    /// Draw text on the F10 map. Unlike the other shapes it takes no
    /// `message`; DCS takes the text in that position.
    pub fn text_to_all(&self, side: SideFilter, id: MarkId, spec: TextSpec) -> Result<()> {
        Ok(self.call_function(
            "textToAll",
            (
                side,
                id,
                spec.pos,
                spec.color,
                spec.fill_color,
                spec.font_size,
                spec.read_only,
                spec.text,
            ),
        )?)
    }

    pub fn arrow_to_all(
        &self,
        side: SideFilter,
        id: MarkId,
        spec: ArrowSpec,
        message: Option<String>,
    ) -> Result<()> {
        Ok(self.call_function(
            "arrowToAll",
            (
                side,
                id,
                spec.start,
                spec.end,
                spec.color,
                spec.fill_color,
                spec.line_type,
                spec.read_only,
                message,
            ),
        )?)
    }

    // The set_markup_* functions modify an existing drawn shape `id` in
    // place.

    pub fn set_markup_radius(&self, id: MarkId, radius: f64) -> Result<()> {
        Ok(self.call_function("setMarkupRadius", (id, radius))?)
    }

    pub fn set_markup_text(&self, id: MarkId, text: String) -> Result<()> {
        Ok(self.call_function("setMarkupText", (id, text))?)
    }

    pub fn set_markup_font_size(&self, id: MarkId, font_size: u8) -> Result<()> {
        Ok(self.call_function("setMarkupFontSize", (id, font_size))?)
    }

    pub fn set_markup_color(&self, id: MarkId, color: Color) -> Result<()> {
        Ok(self.call_function("setMarkupColor", (id, color))?)
    }

    pub fn set_markup_fill_color(&self, id: MarkId, fill_color: Color) -> Result<()> {
        Ok(self.call_function("setMarkupColorFill", (id, fill_color))?)
    }

    pub fn set_markup_line_type(&self, id: MarkId, line_type: LineType) -> Result<()> {
        Ok(self.call_function("setMarkupTypeLine", (id, line_type))?)
    }

    pub fn set_markup_position_end(&self, id: MarkId, pos: LuaVec3) -> Result<()> {
        Ok(self.call_function("setMarkupPositionEnd", (id, pos))?)
    }

    pub fn set_markup_position_start(&self, id: MarkId, pos: LuaVec3) -> Result<()> {
        Ok(self.call_function("setMarkupPositionStart", (id, pos))?)
    }
}

// The global `trigger` table.
wrapped_table!(Trigger, None);

impl<'lua> Trigger<'lua> {
    /// Get the global `trigger` table. Mission environment only.
    pub fn singleton(lua: MizLua<'lua>) -> Result<Self> {
        Ok(lua.inner().globals().raw_get("trigger")?)
    }

    /// The `trigger.action` table
    pub fn action(&self) -> Result<Action<'lua>> {
        Ok(self.raw_get("action")?)
    }

    /// Look up the trigger zone called `name` (`trigger.misc.getZone`).
    /// Returns an error if DCS doesn't return a zone table (e.g. no such
    /// zone).
    pub fn get_zone(&self, name: String) -> Result<Zone> {
        let misc: LuaTable = self.t.raw_get("misc")?;
        Ok(misc.call_function("getZone", name)?)
    }
}
