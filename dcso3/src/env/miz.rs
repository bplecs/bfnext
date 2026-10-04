/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! Typed access to the mission (`.miz`) table.
//!
//! A `.miz` file's `mission` file is a big Lua table describing the
//! mission as authored in the editor: coalitions, their countries, the
//! groups and units of each country (by category), trigger zones,
//! weather, and so on. In the mission scripting environment it is
//! `env.mission`, in the hooks environment `_current_mission.mission`;
//! [`Miz::singleton`] fetches the right one.
//!
//! The types here are thin wrappers over the raw tables, mostly
//! getters (and a few setters) for named fields. [`Miz::index`] walks
//! the whole mission once and builds a [`MizIndex`], which records the
//! table path of every group, unit, and trigger zone so they can later
//! be looked up by id or name without searching.

use crate::{
    as_tbl, coalition::Side, controller::MissionPoint, country, is_hooks_env, net::SlotId,
    string_enum, wrapped_prim, wrapped_table, Color, DcsTableExt, LuaEnv, LuaVec2, Path, Quad2,
    Sequence, String,
};
use anyhow::{bail, Result};
use fxhash::FxHashMap;
use mlua::{prelude::*, Value};
use serde_derive::{Deserialize, Serialize};
use std::{cmp::max, collections::hash_map::Entry, ops::Deref};

// The mission's `weather` table. No accessors yet, use the raw table.
wrapped_table!(Weather, None);

// A unit id as stored in the mission (`unitId`).
wrapped_prim!(UnitId, i64, Hash, Copy);

impl Default for UnitId {
    fn default() -> Self {
        UnitId(0)
    }
}

impl UnitId {
    /// Increment the id in place, e.g. to allocate a new id after
    /// [`MizIndex::max_uid`]
    pub fn next(&mut self) {
        self.0 += 1
    }
}

// A group id as stored in the mission (`groupId`).
wrapped_prim!(GroupId, i64, Hash, Copy);

impl Default for GroupId {
    fn default() -> Self {
        GroupId(0)
    }
}

impl GroupId {
    /// Increment the id in place, e.g. to allocate a new id after
    /// [`MizIndex::max_gid`]
    pub fn next(&mut self) {
        self.0 += 1
    }
}

// The `skill` field of a unit. `Client` and `Player` mark slots flown by
// humans. Unrecognized strings become `Custom`.
string_enum!(Skill, u8, [
    Client => "Client",
    Excellent => "Excellent",
    Player => "Player",
    Average => "Average",
    Good => "Good",
    High => "High"
]);

/// A key/value property attached to a trigger zone in the editor
#[derive(Debug, Clone)]
pub struct Property {
    pub key: String,
    pub value: String,
}

impl<'lua> FromLua<'lua> for Property {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        let tbl = LuaTable::from_lua(value, lua)?;
        Ok(Self {
            key: tbl.raw_get("key")?,
            value: tbl.raw_get("value")?,
        })
    }
}

/// The shape of a trigger zone
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum TriggerZoneTyp {
    /// A circle centered on the zone position (mission `type` 0)
    Circle { radius: f64 },
    /// A four sided polygon given by its vertices (mission `type` 2)
    Quad(Quad2),
}

// A trigger zone id as stored in the mission (`zoneId`).
wrapped_prim!(TriggerZoneId, i64, Copy, Hash);

// A trigger zone, an element of the mission's `triggers.zones` list.
wrapped_table!(TriggerZone, None);

impl<'lua> TriggerZone<'lua> {
    pub fn name(&self) -> Result<String> {
        Ok(self.raw_get("name")?)
    }

    /// The zone position from its `x` and `y` fields
    pub fn pos(&self) -> Result<na::base::Vector2<f64>> {
        Ok(na::base::Vector2::new(
            self.raw_get("x")?,
            self.raw_get("y")?,
        ))
    }

    /// The zone shape. Returns an error for zone types other than 0
    /// (circle) and 2 (quad).
    pub fn typ(&self) -> Result<TriggerZoneTyp> {
        Ok(match self.raw_get("type")? {
            0 => TriggerZoneTyp::Circle {
                radius: self.raw_get("radius")?,
            },
            // the mission file spells the key "verticies"
            2 => TriggerZoneTyp::Quad(self.raw_get("verticies")?),
            n => bail!("unknown trigger zone type {}", n),
        })
    }

    pub fn color(&self) -> Result<Color> {
        Ok(self.raw_get("color")?)
    }

    pub fn id(&self) -> Result<TriggerZoneId> {
        Ok(self.raw_get("zoneId")?)
    }

    /// The custom properties set on the zone in the editor
    pub fn properties(&self) -> Result<Sequence<'lua, Property>> {
        Ok(self.raw_get("properties")?)
    }
}

// An element of a coalition's `nav_points` list. No accessors yet.
wrapped_table!(NavPoint, None);

// An element of a group's `tasks` list. No accessors yet.
wrapped_table!(Task, None);

// A group's `route` table, which holds its waypoints.
wrapped_table!(Route, None);

impl<'lua> Route<'lua> {
    pub fn points(&self) -> Result<Sequence<'lua, MissionPoint<'_>>> {
        Ok(self.t.raw_get("points")?)
    }

    /// Replace the route's waypoint list
    pub fn set_points(&self, points: Vec<MissionPoint>) -> Result<()> {
        Ok(self.t.raw_set("points", points)?)
    }
}

// A unit as defined in the mission, an element of a group's `units` list.
// Setters modify the mission table in place.
wrapped_table!(Unit, None);

impl<'lua> Unit<'lua> {
    pub fn name(&self) -> Result<String> {
        Ok(self.raw_get("name")?)
    }

    pub fn id(&self) -> Result<UnitId> {
        Ok(self.raw_get("unitId")?)
    }

    pub fn set_id(&self, id: UnitId) -> Result<()> {
        Ok(self.raw_set("unitId", id)?)
    }

    /// The multiplayer slot id of this unit, derived from its unit id
    pub fn slot(&self) -> Result<SlotId> {
        Ok(SlotId::from(self.id()?))
    }

    pub fn set_name(&self, name: String) -> Result<()> {
        Ok(self.raw_set("name", name)?)
    }

    /// The unit position from its `x` and `y` fields
    pub fn pos(&self) -> Result<na::base::Vector2<f64>> {
        Ok(na::base::Vector2::new(
            self.raw_get("x")?,
            self.raw_get("y")?,
        ))
    }

    pub fn set_pos(&self, pos: na::base::Vector2<f64>) -> Result<()> {
        self.raw_set("x", pos.x)?;
        self.raw_set("y", pos.y)?;
        Ok(())
    }

    pub fn heading(&self) -> Result<f64> {
        Ok(self.raw_get("heading")?)
    }

    /// Set the unit heading. Also sets `psi`, which the mission stores
    /// as the negated heading, so the two fields stay consistent.
    pub fn set_heading(&self, h: f64) -> Result<()> {
        self.raw_set("psi", -h)?;
        Ok(self.raw_set("heading", h)?)
    }

    /// The `alt` field, `None` if the unit has none
    pub fn alt(&self) -> Result<Option<f64>> {
        Ok(self.raw_get("alt")?)
    }

    pub fn set_alt(&self, a: f64) -> Result<()> {
        Ok(self.raw_set("alt", a)?)
    }

    /// The unit type name, e.g. `"F-16C_50"`
    pub fn typ(&self) -> Result<String> {
        Ok(self.raw_get("type")?)
    }

    pub fn skill(&self) -> Result<Skill> {
        Ok(self.raw_get("skill")?)
    }
}

// A group as defined in the mission, an element of a country's per category
// `group` list. Setters modify the mission table in place.
wrapped_table!(Group, None);

impl<'lua> Group<'lua> {
    pub fn name(&self) -> Result<String> {
        Ok(self.raw_get("name")?)
    }

    pub fn set_name(&self, name: String) -> Result<()> {
        Ok(self.raw_set("name", name)?)
    }

    /// The group position from its `x` and `y` fields
    pub fn pos(&self) -> Result<na::base::Vector2<f64>> {
        Ok(na::base::Vector2::new(
            self.t.raw_get("x")?,
            self.t.raw_get("y")?,
        ))
    }

    pub fn set_pos(&self, pos: na::base::Vector2<f64>) -> Result<()> {
        self.t.raw_set("x", pos.x)?;
        self.t.raw_set("y", pos.y)?;
        Ok(())
    }

    pub fn frequency(&self) -> Result<f64> {
        Ok(self.raw_get("frequency")?)
    }

    /// The raw `modulation` field of the group radio
    pub fn modulation(&self) -> Result<i64> {
        Ok(self.raw_get("modulation")?)
    }

    /// The `lateActivation` flag, false if missing or unreadable
    pub fn late_activation(&self) -> bool {
        self.raw_get("lateActivation").unwrap_or(false)
    }

    pub fn id(&self) -> Result<GroupId> {
        Ok(self.raw_get("groupId")?)
    }

    pub fn set_id(&self, id: GroupId) -> Result<()> {
        Ok(self.raw_set("groupId", id)?)
    }

    pub fn tasks(&self) -> Result<Sequence<'lua, Task<'_>>> {
        Ok(self.raw_get("tasks")?)
    }

    pub fn route(&self) -> Result<Route<'_>> {
        Ok(self.raw_get("route")?)
    }

    pub fn set_route(&self, r: Route) -> Result<()> {
        Ok(self.raw_set("route", r)?)
    }

    /// The `hidden` flag, false if missing or unreadable
    pub fn hidden(&self) -> bool {
        self.raw_get("hidden").unwrap_or(false)
    }

    pub fn units(&self) -> Result<Sequence<'lua, Unit<'lua>>> {
        Ok(self.raw_get("units")?)
    }

    /// The `uncontrolled` flag. Note this is true if the field can't be
    /// read; a missing (nil) field reads as false.
    pub fn uncontrolled(&self) -> bool {
        self.raw_get("uncontrolled").unwrap_or(true)
    }
}

// A country within a coalition. Its groups are stored per category under
// `plane`, `helicopter`, `ship`, `vehicle` and `static`, each holding a
// `group` list. The accessors return an empty sequence if a category is
// absent.
wrapped_table!(Country, None);

impl<'lua> Country<'lua> {
    pub fn id(&self) -> Result<country::Country> {
        Ok(self.raw_get("id")?)
    }

    pub fn name(&self) -> Result<String> {
        Ok(self.raw_get("name")?)
    }

    pub fn planes(&self) -> Result<Sequence<'lua, Group<'lua>>> {
        let g: Option<mlua::Table> = self.raw_get("plane")?;
        g.map(|g| Ok(g.raw_get("group")?))
            .unwrap_or_else(|| Sequence::empty(self.lua))
    }

    pub fn helicopters(&self) -> Result<Sequence<'lua, Group<'lua>>> {
        let g: Option<mlua::Table> = self.raw_get("helicopter")?;
        g.map(|g| Ok(g.raw_get("group")?))
            .unwrap_or_else(|| Sequence::empty(self.lua))
    }

    pub fn ships(&self) -> Result<Sequence<'lua, Group<'lua>>> {
        let g: Option<mlua::Table> = self.raw_get("ship")?;
        g.map(|g| Ok(g.raw_get("group")?))
            .unwrap_or_else(|| Sequence::empty(self.lua))
    }

    pub fn vehicles(&self) -> Result<Sequence<'lua, Group<'lua>>> {
        let g: Option<mlua::Table> = self.raw_get("vehicle")?;
        g.map(|g| Ok(g.raw_get("group")?))
            .unwrap_or_else(|| Sequence::empty(self.lua))
    }

    pub fn statics(&self) -> Result<Sequence<'lua, Group<'lua>>> {
        let g: Option<mlua::Table> = self.raw_get("static")?;
        g.map(|g| Ok(g.raw_get("group")?))
            .unwrap_or_else(|| Sequence::empty(self.lua))
    }
}

// One side's entry in the mission's `coalition` table, holding its
// bullseye, nav points and countries.
wrapped_table!(Coalition, None);

impl<'lua> Coalition<'lua> {
    pub fn bullseye(&self) -> Result<LuaVec2> {
        Ok(self.t.raw_get("bullseye")?)
    }

    pub fn nav_points(&self) -> Result<Sequence<'lua, NavPoint<'lua>>> {
        Ok(self.t.raw_get("nav_points")?)
    }

    pub fn name(&self) -> Result<String> {
        Ok(self.t.raw_get("name")?)
    }

    pub fn countries(&self) -> Result<Sequence<'lua, Country<'lua>>> {
        Ok(self.t.raw_get("country")?)
    }

    /// Find the country with id `country` in this coalition by linear search
    pub fn country(&self, country: country::Country) -> Result<Option<Country<'lua>>> {
        for c in self.countries()? {
            let c = c?;
            if c.id()? == country {
                return Ok(Some(c));
            }
        }
        return Ok(None);
    }

    /// Index every group and unit in this coalition. `base` is the path of
    /// this coalition's table within the mission; recorded paths are
    /// relative to the mission root. Fails on any duplicate group id,
    /// group name, unit id or unit name within the coalition.
    fn index(&self, side: Side, base: Path) -> Result<CoalitionIndex> {
        let base = base.append(["country"]);
        let mut idx = CoalitionIndex::default();
        for (i, country) in self.countries()?.into_iter().enumerate() {
            let country = country?;
            let cid = country.id()?;
            // lua sequences are 1 based
            let base = base.append([i + 1]);
            // index the groups (and their units) of one category, `$name` is
            // the category key in the country table, `$tbl` both the
            // `Country` accessor and the per category `CoalitionIndex` map
            macro_rules! index_group {
                ($name:literal, $cat:expr, $tbl:ident) => {
                    for (i, group) in country.$tbl()?.into_iter().enumerate() {
                        let group = group?;
                        let name = group.name()?;
                        let gid = group.id()?;
                        idx.max_gid = GroupId(max(idx.max_gid.0, gid.0));
                        let base = base.append([$name, "group"]).append([i + 1]);
                        match idx.groups.entry(gid) {
                            Entry::Occupied(_) => bail!("duplicate group id {:?}", gid),
                            Entry::Vacant(e) => {
                                e.insert(IndexedGroup {
                                    side,
                                    country: cid,
                                    category: $cat,
                                    path: base.clone(),
                                });
                            }
                        }
                        match idx.groups_by_name.entry(name.clone()) {
                            Entry::Occupied(_) => bail!("duplicate group name {name}"),
                            Entry::Vacant(e) => e.insert(gid),
                        };
                        match idx.$tbl.entry(name.clone()) {
                            Entry::Occupied(_) => bail!("duplicate group name {name}"),
                            Entry::Vacant(e) => e.insert(gid),
                        };
                        for (i, unit) in group.units()?.into_iter().enumerate() {
                            let unit = unit?;
                            let base = base.append(["units"]).append([i + 1]);
                            let name = unit.name()?;
                            let uid = unit.id()?;
                            idx.max_uid = UnitId(max(idx.max_uid.0, uid.0));
                            match idx.units.entry(uid) {
                                Entry::Occupied(_) => bail!("duplicate unit id {:?}", uid),
                                Entry::Vacant(e) => e.insert(IndexedUnit {
                                    side,
                                    country: cid,
                                    path: base.clone(),
                                }),
                            };
                            match idx.units_by_name.entry(name.clone()) {
                                Entry::Occupied(_) => bail!("duplicate unit name {name}"),
                                Entry::Vacant(e) => e.insert(uid),
                            };
                            match idx.groups_by_unit.entry(uid) {
                                Entry::Occupied(_) => bail!("guplicate unit id {:?}", uid),
                                Entry::Vacant(e) => e.insert(gid),
                            };
                        }
                    }
                };
            }
            index_group!("plane", GroupKind::Plane, planes);
            index_group!("helicopter", GroupKind::Helicopter, helicopters);
            index_group!("ship", GroupKind::Ship, ships);
            index_group!("vehicle", GroupKind::Vehicle, vehicles);
            index_group!("static", GroupKind::Static, statics);
        }
        Ok(idx)
    }
}

/// The number of slots for a ground control role on each side
#[derive(Debug, Clone, Copy)]
pub struct Role {
    pub neutrals: u8,
    pub red: u8,
    pub blue: u8,
}

impl<'lua> FromLua<'lua> for Role {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        let tbl: LuaTable = FromLua::from_lua(value, lua)?;
        Ok(Self {
            neutrals: tbl.raw_get("neutrals")?,
            red: tbl.raw_get("red")?,
            blue: tbl.raw_get("blue")?,
        })
    }
}

// The `roles` table of `GroundControl`, giving per side slot counts for the
// combined arms and observer roles.
wrapped_table!(Roles, None);

impl<'lua> Roles<'lua> {
    pub fn artillery_commander(&self) -> Result<Role> {
        Ok(self.raw_get("artillery_commander")?)
    }

    pub fn instructor(&self) -> Result<Role> {
        Ok(self.raw_get("instructor")?)
    }

    pub fn observer(&self) -> Result<Role> {
        Ok(self.raw_get("observer")?)
    }

    pub fn forward_observer(&self) -> Result<Role> {
        Ok(self.raw_get("forward_observer")?)
    }
}

// The mission's `groundControl` table (combined arms / game master settings).
wrapped_table!(GroundControl, None);

impl<'lua> GroundControl<'lua> {
    pub fn roles(&self) -> Result<Roles<'lua>> {
        Ok(self.raw_get("roles")?)
    }
}

/// Where an indexed group lives in the mission table, and its owner
#[derive(Debug, Clone, Serialize)]
struct IndexedGroup {
    side: Side,
    country: country::Country,
    category: GroupKind,
    /// The path of the group table from the mission root
    path: Path,
}

/// Where an indexed unit lives in the mission table, and its owner
#[derive(Debug, Clone, Serialize)]
struct IndexedUnit {
    side: Side,
    country: country::Country,
    /// The path of the unit table from the mission root
    path: Path,
}

/// The index of one coalition's groups and units, built by
/// [`Miz::index`]
#[derive(Debug, Clone, Serialize, Default)]
pub struct CoalitionIndex {
    /// The largest unit id seen
    max_uid: UnitId,
    /// The largest group id seen
    max_gid: GroupId,
    units: FxHashMap<UnitId, IndexedUnit>,
    units_by_name: FxHashMap<String, UnitId>,
    groups: FxHashMap<GroupId, IndexedGroup>,
    /// Group names of every category
    groups_by_name: FxHashMap<String, GroupId>,
    /// The group each unit belongs to
    groups_by_unit: FxHashMap<UnitId, GroupId>,
    // group names by category
    planes: FxHashMap<String, GroupId>,
    helicopters: FxHashMap<String, GroupId>,
    ships: FxHashMap<String, GroupId>,
    vehicles: FxHashMap<String, GroupId>,
    statics: FxHashMap<String, GroupId>,
}

/// An index of the mission, built by [`Miz::index`], mapping group, unit and
/// trigger zone ids and names to their path in the mission table. Pass it
/// to the `Miz` lookup methods. It only reflects the mission as it was when
/// indexed.
#[derive(Debug, Clone, Serialize, Default)]
pub struct MizIndex {
    by_side: FxHashMap<Side, CoalitionIndex>,
    /// Trigger zone name to its path from the mission root
    triggers: FxHashMap<String, Path>,
}

impl MizIndex {
    /// The largest unit id in the mission, across all coalitions
    pub fn max_uid(&self) -> UnitId {
        self.by_side
            .iter()
            .fold(UnitId::default(), |muid, (_, cidx)| {
                UnitId(max(muid.0, cidx.max_uid.0))
            })
    }

    /// The largest group id in the mission, across all coalitions
    pub fn max_gid(&self) -> GroupId {
        self.by_side
            .iter()
            .fold(GroupId::default(), |mgid, (_, cidx)| {
                GroupId(max(mgid.0, cidx.max_gid.0))
            })
    }
}

/// The category a group is stored under in its country table
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum GroupKind {
    /// Matches every category, for lookups only. Indexed groups always have
    /// a specific kind.
    Any,
    Plane,
    Helicopter,
    Ship,
    Vehicle,
    Static,
}

/// A group found through a [`MizIndex`], with its side, country and category
#[derive(Debug, Clone, Serialize)]
pub struct GroupInfo<'lua> {
    pub side: Side,
    pub country: country::Country,
    pub category: GroupKind,
    pub group: Group<'lua>,
}

/// A unit found through a [`MizIndex`], with its side and country
#[derive(Debug, Clone, Serialize)]
pub struct UnitInfo<'lua> {
    pub side: Side,
    pub country: country::Country,
    pub unit: Unit<'lua>,
}

// The root mission table.
wrapped_table!(Miz, None);

impl<'lua> Miz<'lua> {
    /// Get the current mission table: `_current_mission.mission` in the
    /// hooks environment, `env.mission` in the mission scripting environment
    pub fn singleton<L: LuaEnv<'lua> + Copy>(lua: L) -> Result<Self> {
        if is_hooks_env(lua.inner()) {
            let current: mlua::Table = lua.inner().globals().get("_current_mission")?;
            Ok(current.get("mission")?)
        } else {
            let env: mlua::Table = lua.inner().globals().get("env")?;
            Ok(env.get("mission")?)
        }
    }

    pub fn ground_control(&self) -> Result<GroundControl<'_>> {
        Ok(self.raw_get("groundControl")?)
    }

    /// The coalition table for `side`, from `coalition[side.to_str()]`
    pub fn coalition(&self, side: Side) -> Result<Coalition<'lua>> {
        let coa: mlua::Table = self.raw_get("coalition")?;
        Ok(coa.raw_get(side.to_str())?)
    }

    /// The trigger zones, from `triggers.zones`
    pub fn triggers(&self) -> Result<Sequence<'lua, TriggerZone<'lua>>> {
        let triggers: mlua::Table = self.t.raw_get("triggers")?;
        Ok(triggers.raw_get("zones")?)
    }

    pub fn weather(&self) -> Result<Weather<'lua>> {
        Ok(self.t.raw_get("weather")?)
    }

    /// Look up a group by id using `idx`. `Ok(None)` if the id isn't in the
    /// index; an error if the indexed path no longer resolves.
    pub fn get_group(&self, idx: &MizIndex, id: &GroupId) -> Result<Option<GroupInfo<'lua>>> {
        idx.by_side
            .iter()
            .find_map(|(_, idx)| idx.groups.get(id))
            .map(|ifo| {
                self.raw_get_path(&ifo.path).map(|group| GroupInfo {
                    side: ifo.side,
                    country: ifo.country,
                    category: ifo.category,
                    group,
                })
            })
            .transpose()
    }

    /// Look up a group of `side` by name. With [`GroupKind::Any`] every
    /// category is searched, otherwise only the given one.
    pub fn get_group_by_name(
        &self,
        idx: &MizIndex,
        kind: GroupKind,
        side: Side,
        name: &str,
    ) -> Result<Option<GroupInfo<'lua>>> {
        idx.by_side
            .get(&side)
            .and_then(|cidx| match kind {
                GroupKind::Any => cidx.groups_by_name.get(name),
                GroupKind::Plane => cidx.planes.get(name),
                GroupKind::Helicopter => cidx.helicopters.get(name),
                GroupKind::Vehicle => cidx.vehicles.get(name),
                GroupKind::Ship => cidx.ships.get(name),
                GroupKind::Static => cidx.statics.get(name),
            })
            .and_then(|gid| self.get_group(idx, gid).transpose())
            .transpose()
    }

    /// Look up a unit by id using `idx`, `Ok(None)` if it isn't indexed
    pub fn get_unit(&self, idx: &MizIndex, id: &UnitId) -> Result<Option<UnitInfo<'lua>>> {
        idx.by_side
            .iter()
            .find_map(|(_, idx)| idx.units.get(id))
            .map(|ifo| {
                self.raw_get_path(&ifo.path).map(|unit| UnitInfo {
                    side: ifo.side,
                    country: ifo.country,
                    unit,
                })
            })
            .transpose()
    }

    /// Look up a unit by name in any coalition, `Ok(None)` if not indexed
    pub fn get_unit_by_name(&self, idx: &MizIndex, name: &str) -> Result<Option<UnitInfo<'lua>>> {
        idx.by_side
            .iter()
            .find_map(|(_, idx)| idx.units_by_name.get(name).and_then(|id| idx.units.get(id)))
            .map(|ifo| {
                self.raw_get_path(&ifo.path).map(|unit| UnitInfo {
                    side: ifo.side,
                    country: ifo.country,
                    unit,
                })
            })
            .transpose()
    }

    /// The group that the unit with id `id` belongs to
    pub fn get_group_by_unit(
        &self,
        idx: &MizIndex,
        id: &UnitId,
    ) -> Result<Option<GroupInfo<'lua>>> {
        idx.by_side
            .iter()
            .find_map(|(_, idx)| idx.groups_by_unit.get(id))
            .and_then(|gid| self.get_group(idx, gid).transpose())
            .transpose()
    }

    /// The group that the unit called `name` belongs to
    pub fn get_group_by_unit_name(
        &self,
        idx: &MizIndex,
        name: &str,
    ) -> Result<Option<GroupInfo<'lua>>> {
        idx.by_side
            .iter()
            .find_map(|(_, idx)| {
                idx.units_by_name
                    .get(name)
                    .and_then(|uid| idx.groups_by_unit.get(uid))
            })
            .and_then(|gid| self.get_group(idx, gid).transpose())
            .transpose()
    }

    /// Look up a trigger zone by name using `idx`
    pub fn get_trigger_zone(
        &self,
        idx: &MizIndex,
        name: &str,
    ) -> Result<Option<TriggerZone<'lua>>> {
        idx.triggers
            .get(name)
            .map(|path| self.raw_get_path(path))
            .transpose()
    }

    pub fn sortie(&self) -> Result<String> {
        Ok(self.raw_get("sortie")?)
    }

    /// Walk the mission and build a [`MizIndex`] of every trigger zone,
    /// group and unit. Fails if a trigger zone name is duplicated, or if
    /// a coalition contains duplicate group/unit ids or names (see
    /// [`Coalition`]'s indexing). Duplicates across coalitions are not
    /// detected.
    pub fn index(&self) -> Result<MizIndex> {
        let base = Path::default();
        let mut idx = MizIndex::default();
        {
            let base = base.append(["triggers", "zones"]);
            for (i, tz) in self.triggers()?.into_iter().enumerate() {
                let tz = tz?;
                let base = base.append([i + 1]);
                let name = tz.name()?;
                match idx.triggers.entry(name.clone()) {
                    Entry::Vacant(e) => e.insert(base),
                    Entry::Occupied(_) => bail!("duplicate trigger zone {name}"),
                };
            }
        }
        for side in Side::ALL {
            let base = base.append(["coalition", side.to_str()]);
            idx.by_side
                .entry(side)
                .or_insert(self.coalition(side)?.index(side, base)?);
        }
        Ok(idx)
    }
}
