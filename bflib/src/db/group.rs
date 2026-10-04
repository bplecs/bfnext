/*
Copyright 2024 Eric Stokes.

This file is part of bflib.

bflib is free software: you can redistribute it and/or modify it under
the terms of the GNU Affero Public License as published by the Free
Software Foundation, either version 3 of the License, or (at your
option) any later version.

bflib is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero Public License
for more details.
*/

//! Spawned groups and units.
//!
//! Every group the campaign creates (objective defenses, player deployables,
//! troops, crates, and actions) is recorded in the db as a [`SpawnedGroup`]
//! with its [`SpawnedUnit`]s, so it can be persisted and respawned across
//! restarts and culling. Groups are created from miz templates by
//! [`Db::add_group`], which computes unit positions for a [`SpawnLoc`]; the
//! actual DCS spawn is queued separately.
//!
//! This module also handles DCS birth/death events for units and statics
//! (including validating players in dynamic slots), F10 map marks for
//! groups, and incremental tracking of unit positions.

use super::{ephemeral::SlotInfo, objective::ObjGroupClass, player::SlotAuth, Db, SetS};
use crate::{
    group, group_by_name, group_health, group_mut, objective,
    spawnctx::{Despawn, SpawnCtx, SpawnLoc},
    unit, unit_by_name, unit_mut, Connected,
};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::{Action, ActionKind, Crate, Deployable, Troop, UnitTag, UnitTags, Vehicle},
    db::objective::ObjectiveId,
    stats::{self, EnId},
};
use bfprotocols::{
    db::group::{GroupId, UnitId},
    stats::Stat,
};
use chrono::prelude::*;
use compact_str::{format_compact, CompactString};
use dcso3::{
    azumith3d, centroid2d, centroid3d, change_heading,
    coalition::Side,
    coord::Coord,
    env::miz,
    env::miz::{Group, GroupKind, MizIndex},
    group::GroupCategory,
    land::{Land, SurfaceType},
    net::{SlotId, Ucid},
    object::{DcsObject, DcsOid},
    rotate2d_gen,
    static_object::{ClassStatic, StaticObject},
    trigger::MarkId,
    unit::{ClassUnit, Unit},
    LuaVec2, LuaVec3, MizLua, Position3, String, Vector2, Vector3,
};
use enumflags2::BitFlags;
use fxhash::{FxHashMap, FxHashSet};
use log::{error, warn};
use serde_derive::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::{cmp::max, collections::VecDeque};

/// The result of handling a unit birth event in [`Db::unit_born`].
#[derive(Debug, Clone)]
pub enum BirthRes {
    /// Nothing for the caller to do (a db unit was born, or an empty
    /// slot was destroyed)
    None,
    /// A player successfully occupied this slot
    OccupiedSlot(SlotId),
    /// A player spawned in a dynamic slot they aren't allowed to use. The
    /// unit has already been destroyed.
    DynamicSlotDenied(Ucid, SlotAuth),
}

/// serde default for `cost_fraction`, groups saved before it existed paid
/// full price
fn default_cost_fraction() -> f32 {
    1.
}

/// Where a group came from. Determines which db indexes it is in (see
/// [`Db::add_group`] and [`Db::delete_group`]) and how it is marked.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum DeployKind {
    /// An objective group from an old save format that didn't record the
    /// objective. Kept for deserialization compatibility.
    #[serde(rename = "Objective")]
    ObjectiveDeprecated,
    /// A group belonging to the objective `origin`
    #[serde(rename = "ObjectiveV2")]
    Objective {
        origin: ObjectiveId,
    },
    /// A deployable unpacked by `player`
    Deployed {
        player: Ucid,
        /// The last player to move the group, and the points they will
        /// lose if it is destroyed
        #[serde(default)]
        moved_by: Option<(Ucid, u32)>,
        spec: Deployable,
        /// The fraction of the full cost that was charged, used to scale refunds
        #[serde(default = "default_cost_fraction")]
        cost_fraction: f32,
        #[serde(default)]
        origin: Option<ObjectiveId>,
    },
    /// Troops unloaded by `player`
    Troop {
        player: Ucid,
        /// The objective the troops were loaded at, if any
        origin: Option<ObjectiveId>,
        /// The last player to move the group, and the points they will
        /// lose if it is destroyed
        #[serde(default)]
        moved_by: Option<(Ucid, u32)>,
        spec: Troop,
        /// The fraction of the full cost that was charged, used to scale refunds
        #[serde(default = "default_cost_fraction")]
        cost_fraction: f32,
    },
    /// A cargo crate spawned by `player` at objective `origin`
    Crate {
        origin: ObjectiveId,
        player: Ucid,
        spec: Crate,
    },
    /// A group spawned by an action (e.g. drone, awacs, bomber). `player` is
    /// None when the server initiated it.
    Action {
        /// Extra map marks owned by the action (e.g. its destination)
        #[serde(skip)]
        marks: FxHashSet<MarkId>,
        loc: SpawnLoc,
        player: Option<Ucid>,
        name: String,
        spec: Action,
        time: DateTime<Utc>,
        destination: Option<Vector2>,
        rtb: Option<Vector2>,
        #[serde(default)]
        origin: Option<ObjectiveId>,
        #[serde(skip)]
        ammo: i32,
    },
}

/// A unit created by the campaign. `spawn_*` fields record where the unit
/// was originally placed; the others are its last known position. Positions
/// are in meters, headings in radians. `pos` is the 2d (x, z) projection of
/// `position`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpawnedUnit {
    /// The unique name used to spawn the unit in DCS
    pub name: String,
    pub id: UnitId,
    pub group: GroupId,
    pub side: Side,
    pub typ: Vehicle,
    pub tags: UnitTags,
    /// The name of the unit in the miz template it was created from
    pub template_name: String,
    pub spawn_pos: Vector2,
    pub spawn_heading: f64,
    pub spawn_position: Position3,
    pub pos: Vector2,
    pub heading: f64,
    pub position: Position3,
    /// Dead units stay in the db so they can be repaired / respawned
    pub dead: bool,
    /// The last time the unit was observed to move
    #[serde(skip)]
    pub moved: Option<DateTime<Utc>>,
    /// The unit's velocity if it is an aircraft in the air
    #[serde(skip)]
    pub airborne_velocity: Option<Vector3>,
}

/// A group created by the campaign from a miz template
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnedGroup {
    pub id: GroupId,
    /// The unique name used to spawn the group in DCS
    pub name: String,
    pub template_name: String,
    pub side: Side,
    /// The DCS group category, or None if the group is made of statics
    pub kind: Option<GroupCategory>,
    pub class: ObjGroupClass,
    pub origin: DeployKind,
    pub units: SetS<UnitId>,
    /// The union of the tags of all units in the group
    pub tags: UnitTags,
}

impl Db {
    #[allow(dead_code)]
    pub fn groups(&self) -> impl Iterator<Item = (&GroupId, &SpawnedGroup)> {
        self.persisted.groups.into_iter()
    }

    pub fn group(&self, id: &GroupId) -> Result<&SpawnedGroup> {
        group!(self, id)
    }

    /// The 2d centroid of the group's living units
    pub fn group_center(&self, id: &GroupId) -> Result<Vector2> {
        let group = group!(self, id)?;
        Ok(centroid2d(
            group
                .units
                .into_iter()
                .filter_map(|uid| self.persisted.units.get(uid))
                .filter_map(|unit| if unit.dead { None } else { Some(unit.pos) }),
        ))
    }

    /// The 3d centroid of the group's living units
    #[allow(dead_code)]
    pub fn group_center3(&self, id: &GroupId) -> Result<Vector3> {
        let group = group!(self, id)?;
        Ok(centroid3d(
            group
                .units
                .into_iter()
                .filter_map(|uid| self.persisted.units.get(uid))
                .filter_map(
                    |unit| {
                        if unit.dead {
                            None
                        } else {
                            Some(unit.position.p.0)
                        }
                    },
                ),
        ))
    }

    #[allow(dead_code)]
    pub fn group_by_name(&self, name: &str) -> Result<&SpawnedGroup> {
        group_by_name!(self, name)
    }

    pub fn unit(&self, id: &UnitId) -> Result<&SpawnedUnit> {
        unit!(self, id)
    }

    #[allow(dead_code)]
    pub fn unit_by_name(&self, name: &str) -> Result<&SpawnedUnit> {
        unit_by_name!(self, name)
    }

    /// The DCS object id of the first unit in the group that currently
    /// exists in the mission. Fails if none do.
    pub fn first_living_unit(&self, gid: &GroupId) -> Result<&DcsOid<ClassUnit>> {
        group!(self, gid)?
            .units
            .into_iter()
            .find_map(|uid| self.ephemeral.get_object_id_by_uid(uid))
            .ok_or_else(|| anyhow!("all units are dead"))
    }

    /// All db units that currently exist in DCS, with their object ids
    pub fn instanced_units(
        &self,
    ) -> impl Iterator<Item = (&SpawnedUnit, &DcsOid<ClassUnit>)> {
        self.persisted.units.into_iter().filter_map(|(uid, sp)| {
            self.ephemeral.object_id_by_uid.get(uid).map(|id| (sp, id))
        })
    }

    /// All player deployed groups and troops
    pub fn deployed(&self) -> impl Iterator<Item = &SpawnedGroup> {
        self.persisted
            .deployed
            .into_iter()
            .chain(self.persisted.troops.into_iter())
            .filter_map(|gid| self.persisted.groups.get(gid))
    }

    /// All action groups. Note that troops are included as well.
    pub fn actions(&self) -> impl Iterator<Item = &SpawnedGroup> {
        self.persisted
            .actions
            .into_iter()
            .chain(self.persisted.troops.into_iter())
            .filter_map(|gid| self.persisted.groups.get(gid))
    }

    /// (Re)create the F10 map mark for a group, visible to its side, at the
    /// centroid of its units. The text describes the group and who deployed
    /// (and moved) it. Objective groups are only marked when owned by the
    /// objective's owner. Actions with a destination also get a one time
    /// destination mark stored in the action's `marks`.
    pub(super) fn mark_group(&mut self, gid: &GroupId) -> Result<()> {
        if let Some(id) = self.ephemeral.group_marks.remove(gid) {
            self.ephemeral.msgs.delete_mark(id)
        }
        let group = group_mut!(self, gid)?;
        let group_center =
            centroid2d(group.units.into_iter().map(|uid| self.persisted.units[uid].pos));
        let id = match &mut group.origin {
            DeployKind::ObjectiveDeprecated => None,
            DeployKind::Objective { origin: oid } => match objective!(self, oid) {
                Err(_) => None,
                Ok(obj) => {
                    if group.side == obj.owner {
                        let msg = format_compact!(
                            "objective group id {} name {} of class {:?}",
                            group.id,
                            group.name,
                            group.class
                        );
                        Some(self.ephemeral.msgs.mark_to_side(
                            group.side,
                            group_center,
                            true,
                            msg,
                        ))
                    } else {
                        None
                    }
                }
            },
            DeployKind::Action { name, spec: _, destination, player, marks, .. } => {
                let pname = player
                    .as_ref()
                    .map(|p| self.persisted.players[p].name.clone())
                    .unwrap_or(String::from("Server"));
                let pos_msg = format_compact!("{name} {gid} deployed by {pname}");
                let pos_mark = self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    pos_msg,
                );
                match destination {
                    None => Some(pos_mark),
                    Some(dst) => {
                        if !marks.is_empty() {
                            Some(pos_mark)
                        } else {
                            let dst_msg = format_compact!("{name} {gid} destination");
                            marks.insert(
                                self.ephemeral
                                    .msgs
                                    .mark_to_side(group.side, *dst, true, dst_msg),
                            );
                            Some(pos_mark)
                        }
                    }
                }
            }
            DeployKind::Crate { player, spec, .. } => {
                let name = self.persisted.players[player].name.clone();
                let msg = format_compact!("{} {gid} deployed by {name}", spec.name);
                Some(self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    msg,
                ))
            }
            DeployKind::Deployed {
                spec,
                player,
                moved_by,
                cost_fraction: _,
                origin: _,
            } => {
                let name = self.persisted.players[player].name.clone();
                let resp = moved_by
                    .as_ref()
                    .map(|(u, _)| {
                        let name = self.persisted.players[u].name.clone();
                        format_compact!("\nresponsible party: {name}")
                    })
                    .unwrap_or(CompactString::from(""));
                let msg = format_compact!(
                    "{} {gid} deployed by {name}{resp}",
                    spec.path.last().unwrap()
                );
                Some(self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    msg,
                ))
            }
            DeployKind::Troop { player, spec, moved_by, origin: _, cost_fraction: _ } => {
                let name = self.persisted.players[player].name.clone();
                let resp = moved_by
                    .as_ref()
                    .map(|(u, _)| {
                        let name = self.persisted.players[u].name.clone();
                        format_compact!("\nresponsible party: {name}")
                    })
                    .unwrap_or(CompactString::from(""));
                let msg = format_compact!("{} {gid} deployed by {name}{resp}", spec.name);
                Some(self.ephemeral.msgs.mark_to_side(
                    group.side,
                    group_center,
                    true,
                    msg,
                ))
            }
        };
        if let Some(id) = id {
            self.ephemeral.group_marks.insert(*gid, id);
        }
        Ok(())
    }

    /// Remove a group and its units from the db and all indexes, delete its
    /// marks, and queue it to be despawned from DCS. Fails if the group
    /// doesn't exist.
    pub fn delete_group(&mut self, gid: &GroupId) -> Result<()> {
        let group = self
            .persisted
            .groups
            .remove_cow(gid)
            .ok_or_else(|| anyhow!("no such group {:?}", gid))?;
        self.persisted.groups_by_name.remove_cow(&group.name);
        self.persisted.groups_by_side.get_mut_cow(&group.side).map(|m| m.remove_cow(gid));
        match &group.origin {
            DeployKind::ObjectiveDeprecated | DeployKind::Objective { .. } => (),
            DeployKind::Action { marks, .. } => {
                for id in marks {
                    self.ephemeral.msgs().delete_mark(*id);
                }
                self.persisted.actions.remove_cow(gid);
                self.persisted.jtacs.remove_cow(gid);
                self.persisted.ewrs.remove_cow(gid);
            }
            DeployKind::Crate { player, .. } => {
                self.persisted.crates.remove_cow(gid);
                self.persisted.players[player].crates.remove_cow(gid);
            }
            DeployKind::Deployed { spec, .. } => {
                self.persisted.deployed.remove_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.remove_cow(gid);
                }
                if spec.ewr.is_some() {
                    self.persisted.ewrs.remove_cow(gid);
                }
            }
            DeployKind::Troop { spec, .. } => {
                self.persisted.troops.remove_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.remove_cow(gid);
                }
            }
        }
        if let Some(id) = self.ephemeral.group_marks.remove(gid) {
            self.ephemeral.msgs.delete_mark(id);
        }
        let mut units: SmallVec<[String; 16]> = smallvec![];
        for uid in &group.units {
            self.ephemeral.units_potentially_close_to_enemies.remove(uid);
            self.ephemeral.units_able_to_move.swap_remove(uid);
            if let Some(id) = self.ephemeral.object_id_by_uid.remove(uid) {
                self.ephemeral.uid_by_object_id.remove(&id);
            }
            if let Some(unit) = self.persisted.units.remove_cow(uid) {
                self.persisted.units_by_name.remove_cow(&unit.name);
                units.push(unit.name);
            }
        }
        self.ephemeral.dirty();
        match group.kind {
            None => {
                // it's a static, we have to get it's units
                for unit in &units {
                    self.ephemeral.push_despawn(*gid, Despawn::Static(unit.clone()))
                }
            }
            Some(_) => {
                // it's a normal group
                if let Some(oid) = self.ephemeral.object_id_by_gid.get(gid) {
                    self.ephemeral.push_despawn(*gid, Despawn::Group(oid.clone()));
                }
            }
        }
        self.ephemeral.stat(Stat::GroupDeleted { id: *gid });
        Ok(())
    }

    /// add the units to the db, but don't actually spawn them
    ///
    /// The group is built from the miz template `template_name` for `side`,
    /// with unit positions computed from `location`. Ground placements fail
    /// if a boat would be on land or any other unit in water (crates may be
    /// placed anywhere). `extra_tags` are added to every unit. Group and unit
    /// names get a unique id suffix, except naval spawn points, which keep
    /// their template names. Also registers the group in the index matching
    /// its `origin` and marks it on the map.
    pub(super) fn add_group<'lua>(
        &mut self,
        spctx: &'lua SpawnCtx<'lua>,
        idx: &MizIndex,
        side: Side,
        location: SpawnLoc,
        template_name: &str,
        origin: DeployKind,
        extra_tags: BitFlags<UnitTag>,
    ) -> Result<GroupId> {
        /// Fold the distances from `pos` to `positions` with `cmp` (e.g.
        /// `f64::max` for the farthest). Returns 0 if `positions` is empty.
        fn distance<'a, F: Fn(f64, f64) -> f64>(
            pos: Vector2,
            cmp: F,
            positions: impl IntoIterator<Item = &'a Vector2>,
        ) -> f64 {
            positions
                .into_iter()
                .fold(None, |acc, p| {
                    let d = na::distance_squared(&(*p).into(), &pos.into());
                    let acc = match acc {
                        None => d,
                        Some(d) => d,
                    };
                    Some(cmp(acc, d))
                })
                .map(|d| d.sqrt())
                .unwrap_or(0.)
        }
        #[derive(Debug)]
        struct UnitPosition {
            heading: f64,
            position: Vector2,
            altitude: Option<f64>,
        }
        /// Computed unit positions in template unit order. Units whose type
        /// has a component position are kept separately in `by_type`.
        #[derive(Debug)]
        struct GroupPosition {
            positions: VecDeque<UnitPosition>,
            by_type: FxHashMap<String, VecDeque<UnitPosition>>,
        }
        /// Translate (and for most locations rotate) the template's unit
        /// positions to `location`. Altitude is only kept for `InAir`.
        fn compute_unit_positions(
            spctx: &SpawnCtx,
            idx: &MizIndex,
            location: SpawnLoc,
            template: &Group,
        ) -> Result<GroupPosition> {
            let mut positions = template
                .units()?
                .into_iter()
                .map(|u| {
                    let u = u?;
                    Ok(UnitPosition {
                        heading: u.heading()?,
                        position: u.pos()?,
                        altitude: u.alt().unwrap_or(None),
                    })
                })
                .collect::<Result<VecDeque<_>>>()?;
            match location {
                SpawnLoc::InAir { pos, heading, altitude, speed: _ } => {
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let group_altitude = {
                        let (sum, i) = positions
                            .iter()
                            .filter_map(|p| p.altitude)
                            .fold((0., 0.), |(sum, i), a| (sum + a, i + 1.));
                        sum / i
                    };
                    for p in positions.iter_mut() {
                        p.position = p.position - group_center + pos;
                        p.heading = change_heading(p.heading, heading);
                        if let Some(a) = p.altitude {
                            p.altitude = Some(a - group_altitude + altitude);
                        }
                    }
                    rotate2d_gen(heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtPosWithCenter { pos, center } => {
                    for p in positions.iter_mut() {
                        p.position = p.position - center + pos;
                        p.altitude = None;
                    }
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtTrigger { name, group_heading } => {
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let pos = spctx.get_trigger_zone(idx, name.as_str())?.pos()?;
                    for p in positions.iter_mut() {
                        p.position = p.position - group_center + pos;
                        p.heading = change_heading(p.heading, group_heading);
                        p.altitude = None;
                    }
                    rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtPos { pos, offset_direction, group_heading } => {
                    // place the group beside pos in offset_direction, then push it
                    // along offset_direction so its closest unit is ~20m from pos
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let radius = distance(
                        group_center,
                        f64::max,
                        positions.iter().map(|p| &p.position),
                    );
                    for p in positions.iter_mut() {
                        p.position =
                            p.position - group_center + pos + radius * offset_direction;
                    }
                    rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    let offset_magnitude = 20.
                        - distance(pos, f64::min, positions.iter().map(|p| &p.position));
                    for p in positions.iter_mut() {
                        p.position = p.position + offset_magnitude * offset_direction;
                        p.heading = change_heading(p.heading, group_heading);
                        p.altitude = None;
                    }
                    Ok(GroupPosition { positions, by_type: FxHashMap::default() })
                }
                SpawnLoc::AtPosWithComponents { pos, group_heading, component_pos } => {
                    // units whose type is in component_pos are placed around that
                    // type's own position (keeping their layout relative to the
                    // centroid of their type), everything else around pos
                    let group_center = centroid2d(positions.iter().map(|p| p.position));
                    let center_by_typ: FxHashMap<String, Vector2> = {
                        let mut tbl = FxHashMap::default();
                        for unit in template.units()? {
                            let unit = unit?;
                            let pos = unit.pos()?;
                            let typ = unit.typ()?;
                            if component_pos.contains_key(&**typ) {
                                let (n, v) = tbl
                                    .entry(typ.clone())
                                    .or_insert_with(|| (0, Vector2::new(0., 0.)));
                                *v += pos;
                                *n += 1;
                            }
                        }
                        tbl.into_iter().map(|(k, (n, v))| (k, v / (n as f64))).collect()
                    };
                    let mut by_type: FxHashMap<String, VecDeque<UnitPosition>> =
                        FxHashMap::default();
                    positions.clear();
                    for unit in template.units()? {
                        let unit = unit?;
                        let typ = unit.typ()?;
                        let heading = unit.heading()?;
                        let position = unit.pos()?;
                        let group_center = match center_by_typ.get(&typ) {
                            None => group_center,
                            Some(pos) => *pos,
                        };
                        match component_pos.get(&typ) {
                            None => positions.push_back(UnitPosition {
                                position: position - group_center + pos,
                                heading: change_heading(heading, group_heading),
                                altitude: None,
                            }),
                            Some(pos) => by_type
                                .entry(typ.clone())
                                .or_default()
                                .push_back(UnitPosition {
                                    position: position - group_center + *pos,
                                    heading: change_heading(heading, group_heading),
                                    altitude: None,
                                }),
                        }
                    }
                    rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                        &mut p.position
                    });
                    for positions in by_type.values_mut() {
                        rotate2d_gen(group_heading, positions.make_contiguous(), |p| {
                            &mut p.position
                        });
                    }
                    Ok(GroupPosition { positions, by_type })
                }
            }
        }
        /// Fail if any position is in water
        fn check_water(
            land: &Land,
            positions: &VecDeque<UnitPosition>,
            positions_by_typ: &FxHashMap<String, VecDeque<UnitPosition>>,
        ) -> Result<()> {
            for pos in
                positions.iter().chain(positions_by_typ.values().flat_map(|v| v.iter()))
            {
                match land.get_surface_type(LuaVec2(pos.position))? {
                    SurfaceType::Land | SurfaceType::Road | SurfaceType::Runway => (),
                    SurfaceType::ShallowWater | SurfaceType::Water => {
                        bail!("you can't spawn this unit in water")
                    }
                }
            }
            Ok(())
        }
        /// Fail if any position is on land
        fn check_land(
            land: &Land,
            positions: &VecDeque<UnitPosition>,
            positions_by_typ: &FxHashMap<String, VecDeque<UnitPosition>>,
        ) -> Result<()> {
            for pos in
                positions.iter().chain(positions_by_typ.values().flat_map(|v| v.iter()))
            {
                match land.get_surface_type(LuaVec2(pos.position))? {
                    SurfaceType::ShallowWater | SurfaceType::Water => (),
                    SurfaceType::Land | SurfaceType::Road | SurfaceType::Runway => {
                        bail!("you can't spawn this unit on land")
                    }
                }
            }
            Ok(())
        }
        let land = Land::singleton(spctx.lua())?;
        let template_name = String::from(template_name);
        let template =
            spctx.get_template_ref(idx, GroupKind::Any, side, template_name.as_str())?;
        let mut gpos =
            compute_unit_positions(&spctx, idx, location.clone(), &template.group)?;
        let kind = GroupCategory::from_kind(template.category);
        let gid = GroupId::new();
        // naval spawn points need to be pre created in the miz, so they must be
        // spawned with the same name as the pre created group so that they move
        // to their destination.
        let group_name = if extra_tags.contains(UnitTag::NavalSpawnPoint) {
            template_name.clone()
        } else {
            String::from(format_compact!("{}-{}", template_name, gid))
        };
        // naval spawn points are classed as logi, so they count toward the
        // objective's logi
        let mut spawned = SpawnedGroup {
            id: gid,
            name: group_name.clone(),
            template_name: template_name.clone(),
            side,
            kind,
            origin,
            class: if extra_tags.contains(UnitTag::NavalSpawnPoint) {
                ObjGroupClass::Logi
            } else {
                ObjGroupClass::from(template_name.as_str())
            },
            units: SetS::new(),
            tags: UnitTags(BitFlags::empty()),
        };
        // first pass, compute the group tags so placement can be validated
        // before any units are added to the db
        for unit in template.group.units()?.into_iter() {
            let unit = unit?;
            let typ = unit.typ()?;
            let tags = *self
                .ephemeral
                .cfg
                .unit_classification
                .get(typ.as_str())
                .ok_or_else(|| anyhow!("unit type not classified {typ}"))?;
            let tags = UnitTags(tags.0 | extra_tags);
            spawned.tags.0.insert(tags.0);
        }
        match &location {
            SpawnLoc::AtPos { .. }
            | SpawnLoc::AtPosWithCenter { .. }
            | SpawnLoc::AtPosWithComponents { .. }
            | SpawnLoc::AtTrigger { .. } => {
                if let Some(tmpl) = self.ephemeral.cfg.crate_template.get(&side)
                    && &template_name == tmpl
                {
                    () // it's ok to spawn crates on ships
                } else if spawned.tags.contains(UnitTag::Boat) {
                    check_land(&land, &gpos.positions, &gpos.by_type)
                        .with_context(|| format_compact!("placing group {group_name}"))?
                } else {
                    check_water(&land, &gpos.positions, &gpos.by_type)
                        .with_context(|| format_compact!("placing group {group_name}"))?
                }
            }
            SpawnLoc::InAir { .. } => (),
        }
        for unit in template.group.units()?.into_iter() {
            let uid = UnitId::new();
            let unit = unit?;
            let typ = unit.typ()?;
            let tags = *self
                .ephemeral
                .cfg
                .unit_classification
                .get(typ.as_str())
                .ok_or_else(|| anyhow!("unit type not classified {typ}"))?;
            let tags = UnitTags(tags.0 | extra_tags);
            let template_name = unit.name()?;
            let unit_name = if extra_tags.contains(UnitTag::NavalSpawnPoint) {
                template_name.clone()
            } else {
                String::from(format_compact!("{}-{}", group_name, uid))
            };
            // positions were computed in template unit order, so popping in
            // the same order matches each unit to its position
            let pos = match gpos.by_type.get_mut(&typ) {
                None => gpos.positions.pop_front().unwrap(),
                Some(positions) => positions.pop_front().unwrap(),
            };
            // DCS 3d coordinates are (x, altitude, z), where 2d y maps to z
            let position = {
                let mut p = Position3::default();
                p.p.x = pos.position.x;
                p.p.y = match pos.altitude {
                    None => land.get_height(LuaVec2(pos.position))?,
                    Some(alt) => alt,
                };
                p.p.z = pos.position.y;
                p
            };
            let spawned_unit = SpawnedUnit {
                id: uid,
                group: gid,
                side,
                typ: Vehicle(typ),
                tags,
                name: unit_name.clone(),
                template_name,
                spawn_position: position,
                spawn_pos: pos.position,
                spawn_heading: pos.heading,
                position,
                pos: pos.position,
                heading: pos.heading,
                dead: false,
                moved: None,
                airborne_velocity: None,
            };
            spawned.units.insert_cow(uid);
            self.persisted.units.insert_cow(uid, spawned_unit);
            self.persisted.units_by_name.insert_cow(unit_name, uid);
        }
        match &mut spawned.origin {
            DeployKind::ObjectiveDeprecated | DeployKind::Objective { .. } => (),
            DeployKind::Action { spec, .. } => {
                self.persisted.actions.insert_cow(gid);
                match &spec.kind {
                    ActionKind::Drone(_) => {
                        self.persisted.jtacs.insert_cow(gid);
                    }
                    ActionKind::Awacs(_) => {
                        self.persisted.ewrs.insert_cow(gid);
                    }
                    _ => (),
                }
            }
            DeployKind::Crate { player, .. } => {
                self.persisted.crates.insert_cow(gid);
                self.persisted.players[player].crates.insert_cow(gid);
            }
            DeployKind::Deployed { spec, .. } => {
                self.persisted.deployed.insert_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.insert_cow(gid);
                }
                if spec.ewr.is_some() {
                    self.persisted.ewrs.insert_cow(gid);
                }
            }
            DeployKind::Troop { spec, .. } => {
                self.persisted.troops.insert_cow(gid);
                if spec.jtac.is_some() {
                    self.persisted.jtacs.insert_cow(gid);
                }
            }
        }
        self.persisted.groups.insert_cow(gid, spawned);
        self.persisted.groups_by_name.insert_cow(group_name, gid);
        self.persisted.groups_by_side.get_or_default_cow(side).insert_cow(gid);
        self.ephemeral.dirty();
        self.mark_group(&gid)?;
        Ok(gid)
    }

    /// [`Db::add_group`], then queue the group to spawn, either immediately or
    /// at time `delay` if specified.
    pub fn add_and_queue_group<'lua>(
        &mut self,
        spctx: &SpawnCtx,
        idx: &MizIndex,
        side: Side,
        location: SpawnLoc,
        template_name: &str,
        origin: DeployKind,
        extra_tags: BitFlags<UnitTag>,
        delay: Option<DateTime<Utc>>,
    ) -> Result<GroupId> {
        let gid = self.add_group(
            &spctx,
            idx,
            side,
            location,
            template_name,
            origin,
            extra_tags,
        )?;
        match delay {
            None => self.ephemeral.push_spawn(gid),
            Some(at) => self.ephemeral.delayspawnq.entry(at).or_default().push(gid),
        }
        Ok(gid)
    }

    /// Handle a DCS unit birth event.
    ///
    /// If the unit is one of ours (found by name) its DCS object id is
    /// recorded and a stat is published. Otherwise it is a player spawning
    /// into a slot. Unknown slots are dynamic slots, which are registered
    /// against the nearest objective (taking its owner's side) and validated
    /// now, destroying the unit if the player isn't allowed to fly it. A slot
    /// with no player in it is also destroyed.
    pub(crate) fn unit_born(
        &mut self,
        lua: MizLua,
        unit: &Unit,
        connected: &Connected,
    ) -> Result<BirthRes> {
        let id = unit.object_id()?;
        let name = unit.get_name()?;
        if let Some(uid) = self.persisted.units_by_name.get(name.as_str()) {
            let unit = unit!(self, uid)?;
            self.ephemeral.uid_by_object_id.insert(id.clone(), *uid);
            self.ephemeral.object_id_by_uid.insert(*uid, id.clone());
            self.ephemeral.units_potentially_close_to_enemies.insert(*uid);
            if unit.tags.contains(UnitTag::Driveable) {
                self.ephemeral.units_able_to_move.insert(*uid);
            }
            self.ephemeral.stat(Stat::Unit {
                id: EnId::Unit(*uid),
                gid: Some(unit.group),
                owner: unit.side,
                typ: stats::Unit { typ: unit.typ.clone(), tags: unit.tags },
                pos: stats::Pos {
                    pos: Coord::singleton(lua)?
                        .lo_to_ll(LuaVec3(Vector3::new(unit.pos.x, 0., unit.pos.y)))?,
                    velocity: unit.airborne_velocity.unwrap_or_default(),
                },
            });
            let gid = unit.group;
            if group_health!(self, gid)?.0 == 1 {
                self.mark_group(&gid)?
            }
            return Ok(BirthRes::None);
        }
        let slot = unit.slot()?;
        let (si, deferred_validate) = match self.ephemeral.slot_info.get(&slot) {
            Some(si) => (si, false),
            None => {
                // it's a dynamic slot
                let typ = Vehicle::from(unit.as_object()?.get_type_name()?);
                let pos = unit.get_ground_position()?;
                let obj =
                    Db::objective_near_point(&self.persisted.objectives, pos.0, |_| true)
                        .map(|(_, _, o)| o)
                        .ok_or_else(|| anyhow!("dynamic slot not near any objective"))?;
                let gid = unit.get_group()?.id()?;
                let gid = miz::GroupId::from(gid.inner());
                self.ephemeral.slot_info.insert(
                    slot,
                    SlotInfo {
                        typ,
                        unit_name: unit.get_name()?,
                        objective: obj.id,
                        ground_start: false,
                        miz_gid: gid,
                        side: obj.owner,
                    },
                );
                self.ephemeral.slot_by_miz_gid.insert(gid, slot);
                (&self.ephemeral.slot_info[&slot], true)
            }
        };
        let name = unit.get_player_name()?;
        let ifo = name.and_then(|name| connected.get_by_name(&name));
        let ucid = match ifo {
            Some(ifo) => ifo.ucid,
            None => {
                error!("slot {slot} born with no player in it");
                unit.clone().destroy()?;
                return Ok(BirthRes::None);
            }
        };
        let side = si.side;
        let typ = si.typ.clone();
        let objective = si.objective;
        let tags = *self
            .ephemeral
            .cfg
            .unit_classification
            .get(&typ)
            .unwrap_or(&UnitTags::default());
        if deferred_validate {
            match self.try_occupy_slot_deferred(Utc::now(), &ucid, slot) {
                SlotAuth::Yes(typ) => {
                    self.ephemeral.stat(Stat::Slot { id: ucid, slot, typ });
                }
                a => {
                    unit.clone().destroy()?;
                    return Ok(BirthRes::DynamicSlotDenied(ucid, a));
                }
            }
        }
        self.ephemeral.stat(Stat::Unit {
            id: EnId::Player(ucid),
            gid: None,
            owner: side,
            typ: stats::Unit { typ, tags },
            pos: stats::Pos {
                pos: Coord::singleton(lua)?.lo_to_ll(unit.get_point()?)?,
                velocity: Vector3::default(),
            },
        });
        self.player_entered_slot(lua, id, unit, slot, objective, ucid)
            .context("entering player into slot")?;
        Ok(BirthRes::OccupiedSlot(slot))
    }

    /// Handle a DCS static birth event, recording the object id of our statics
    pub fn static_born(&mut self, st: &StaticObject) -> Result<()> {
        let id = st.object_id()?;
        let name = st.get_name()?;
        if let Some(uid) = self.persisted.units_by_name.get(name.as_str()) {
            self.ephemeral.uid_by_static.insert(id, *uid);
        }
        Ok(())
    }

    /// Handle a DCS unit death.
    ///
    /// Player units are deslotted. Our units are marked dead and reset to
    /// their spawn position, and their objective's status is updated. When a
    /// deployed, troop, crate or action group is completely destroyed it is
    /// deleted, and any player who moved it (or the player who called the
    /// action, if it has a penalty) loses points.
    pub fn unit_dead(
        &mut self,
        id: &DcsOid<ClassUnit>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let uid = match self.ephemeral.unit_dead(&self.persisted, id) {
            None => return Ok(()),
            Some((uid, ucid)) => {
                if let Some(ucid) = ucid {
                    self.player_deslot(&ucid)
                }
                uid
            }
        };
        match self.persisted.units.get_mut_cow(&uid) {
            None => error!("unit_dead: missing unit {:?}", uid),
            Some(unit) => {
                unit.dead = true;
                unit.pos = unit.spawn_pos;
                unit.heading = unit.spawn_heading;
                unit.position = unit.spawn_position;
                self.ephemeral.dirty();
                let gid = unit.group;
                let health = group_health!(self, gid)?.0;
                if let Some(oid) = self.persisted.objectives_by_group.get(&gid).copied() {
                    self.update_objective_status(&oid, now)?;
                    self.ephemeral.units_potentially_close_to_enemies.remove(&uid);
                    if health == 0 {
                        if let Some(id) = self.ephemeral.group_marks.remove(&gid) {
                            self.ephemeral.msgs.delete_mark(id);
                        }
                    }
                }
                if self.persisted.deployed.contains(&gid)
                    || self.persisted.troops.contains(&gid)
                    || self.persisted.crates.contains(&gid)
                {
                    if health == 0 {
                        match &group!(self, gid)?.origin {
                            DeployKind::Troop {
                                player,
                                moved_by: Some((ucid, p)),
                                ..
                            }
                            | DeployKind::Deployed {
                                player,
                                moved_by: Some((ucid, p)),
                                ..
                            } => {
                                let owner = self.persisted.players[player].name.clone();
                                let ucid = ucid.clone();
                                let p = -(*p as i32);
                                let msg = format_compact!(
                                    "for the death of {gid} which was deployed by {owner} and moved by you"
                                );
                                self.adjust_points(&ucid, p, &msg)
                            }
                            DeployKind::Troop { .. }
                            | DeployKind::Deployed { .. }
                            | DeployKind::Action { .. }
                            | DeployKind::Crate { .. }
                            | DeployKind::Objective { .. }
                            | DeployKind::ObjectiveDeprecated => (),
                        }
                        self.delete_group(&gid)?
                    }
                }
                if self.persisted.actions.contains(&gid) {
                    if let DeployKind::Action { player, spec, .. } =
                        &group!(self, gid)?.origin
                    {
                        if self.group_health(&gid)?.0 == 0 {
                            if let Some((penalty, ucid)) = spec
                                .penalty
                                .and_then(|p| player.as_ref().map(|pl| (p, pl.clone())))
                            {
                                self.adjust_points(
                                    &ucid,
                                    -(penalty as i32),
                                    &format_compact!(
                                        "for the loss of action group {gid}"
                                    ),
                                )
                            }
                            self.delete_group(&gid)?
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle the death of one of our statics. Like [`Db::unit_dead`], but
    /// there is no position reset or points penalty.
    pub fn static_dead(
        &mut self,
        id: &DcsOid<ClassStatic>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if let Some(uid) = self.ephemeral.uid_by_static.remove(id) {
            match self.persisted.units.get_mut_cow(&uid) {
                None => error!("static_dead: missing unit {:?}", uid),
                Some(unit) => {
                    unit.dead = true;
                    let gid = unit.group;
                    self.ephemeral.dirty();
                    if let Some(oid) =
                        self.persisted.objectives_by_group.get(&gid).copied()
                    {
                        self.update_objective_status(&oid, now)?;
                    }
                    if self.persisted.deployed.contains(&gid)
                        || self.persisted.troops.contains(&gid)
                        || self.persisted.crates.contains(&gid)
                    {
                        if self.group_health(&gid)?.0 == 0 {
                            self.delete_group(&gid)?
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// (living units, total units) in the group
    pub fn group_health(&self, gid: &GroupId) -> Result<(usize, usize)> {
        group_health!(self, gid)
    }

    /// `side`'s deployed artillery groups within `artillery_mission_range`
    /// meters of `pos`
    pub fn artillery_near_point(
        &self,
        side: Side,
        pos: Vector2,
    ) -> SmallVec<[GroupId; 8]> {
        let range2 = (self.ephemeral.cfg.artillery_mission_range as f64).powi(2);
        let artillery = self
            .deployed()
            .filter_map(|group| {
                if group.tags.contains(UnitTag::Artillery) && group.side == side {
                    let center = self.group_center(&group.id).ok()?;
                    if na::distance_squared(&center.into(), &pos.into()) <= range2 {
                        Some(group.id)
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<SmallVec<[GroupId; 8]>>();
        artillery
    }

    /// `side`'s ALCM carrying action groups within `alcm_mission_range`
    /// meters of `pos`, with the ammo count of the first weapon on the
    /// group's first unit (0 if it can't be read).
    pub fn alcm_near_point(
        &self,
        side: Side,
        lua: MizLua,
        pos: Vector2,
    ) -> SmallVec<[(GroupId, i32); 8]> {
        let range2 = (self.ephemeral.cfg.alcm_mission_range as f64).powi(2);
        let alcm = self
            .actions()
            .filter_map(|group| {
                if group.tags.contains(UnitTag::ALCM) && group.side == side {
                    let center = self.group_center(&group.id).ok()?;
                    if na::distance_squared(
                        &pos.into(),
                        &na::Point2::new(center.x, center.y),
                    ) <= range2
                    {
                        let mut unit: Option<Unit> = None;
                        let mut ammo = 0;
                        if let Some(uid) = group.units.into_iter().next() {
                            if let Some(id) = self.ephemeral.object_id_by_uid.get(&uid) {
                                let instance = match unit.take() {
                                    Some(unit) => unit.change_instance(id),
                                    None => Unit::get_instance(lua, id),
                                };
                                ammo = (|| -> anyhow::Result<i32> {
                                    Ok(instance?.get_ammo()?.first()?.count()? as i32)
                                })()
                                .unwrap_or(0);
                            };
                        }

                        Some((group.id, ammo))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
            .collect::<SmallVec<[(GroupId, i32); 8]>>();
        alcm
    }

    /// Update the positions of a slice (about 1/16th) of the units able to
    /// move, starting at index `last`, to spread the cost over several
    /// calls. Returns the index to resume from (0 once all units have been
    /// covered) and the units whose DCS instance no longer exists.
    pub fn update_unit_positions_incremental(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        mut last: usize,
    ) -> Result<(usize, Vec<DcsOid<ClassUnit>>)> {
        let total = self.ephemeral.units_able_to_move.len();
        if last < total {
            let mut uids: SmallVec<[UnitId; 64]> = smallvec![];
            let elts = self.ephemeral.units_able_to_move.as_slice();
            let stop = last + max(1, total >> 4);
            while last < total && uids.len() < stop {
                uids.push(elts[last]);
                last += 1;
            }
            Ok((last, self.update_unit_positions(lua, now, &uids)?))
        } else {
            Ok((0, vec![]))
        }
    }

    /// Read the current position of each unit in `units` from DCS. Units that
    /// moved more than 1m are updated, flagged as potentially close to
    /// enemies, have a position stat published, and their group's mark is
    /// moved. Returns the object ids of units whose instance is no longer
    /// valid, so the caller can treat them as dead.
    pub fn update_unit_positions(
        &mut self,
        lua: MizLua,
        now: DateTime<Utc>,
        units: &[UnitId],
    ) -> Result<Vec<DcsOid<ClassUnit>>> {
        let coord = Coord::singleton(lua)?;
        let mut unit: Option<Unit> = None;
        let mut moved: SmallVec<[GroupId; 16]> = smallvec![];
        let mut dead: Vec<DcsOid<ClassUnit>> = vec![];
        for uid in units {
            let id = match self.ephemeral.object_id_by_uid.get(&uid) {
                Some(id) => id,
                None => {
                    warn!("update_unit_positions skipping unknown unit {uid}");
                    continue;
                }
            };
            // reuse the previous Unit handle rather than creating a new one
            let instance = match unit.take() {
                Some(unit) => unit.change_instance(id),
                None => Unit::get_instance(lua, id),
            };
            let instance = match instance {
                Ok(i) => i,
                Err(e) => {
                    warn!(
                        "update_unit_positions skipping invalid instance {uid}, {:?}",
                        e
                    );
                    dead.push(id.clone());
                    continue;
                }
            };
            let pos = instance.get_position()?;
            let spunit = unit_mut!(self, uid)?;
            if (spunit.position.p.0 - pos.p.0).magnitude_squared() > 1.0 {
                moved.push(spunit.group);
                spunit.moved = Some(now);
                spunit.position = pos;
                spunit.pos = Vector2::new(pos.p.x, pos.p.z);
                spunit.heading = azumith3d(pos.x.0);
                self.ephemeral.units_potentially_close_to_enemies.insert(*uid);
                let v = if spunit.tags.contains(UnitTag::Aircraft) && instance.in_air()? {
                    let v = instance.get_velocity()?.0;
                    spunit.airborne_velocity = Some(v);
                    Some(v)
                } else {
                    spunit.airborne_velocity = None;
                    None
                };
                self.ephemeral.stat(Stat::Position {
                    id: EnId::Unit(*uid),
                    pos: stats::Pos {
                        pos: coord.lo_to_ll(pos.p)?,
                        velocity: v.unwrap_or_default(),
                    },
                });
            }
            unit = Some(instance);
        }
        for gid in moved {
            self.ephemeral.dirty();
            self.mark_group(&gid)?;
        }
        Ok(dead)
    }
}
