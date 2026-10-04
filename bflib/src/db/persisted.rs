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

//! The persisted half of the campaign database.
//!
//! [`Persisted`] holds all campaign state that must survive a server restart:
//! spawned groups and units, objectives, players, and the various indexes over
//! them. [`super::Db::maybe_snapshot`] clones it when the db is dirty, and the
//! clone is written to the save file by a background task;
//! [`super::Db::load`] reads it back. The maps are persistent (copy on write)
//! chunk maps, so cloning a snapshot is cheap.
//!
//! Fields added after the initial format carry `#[serde(default)]` so that
//! older save files still load.

use super::{
    group::{SpawnedGroup, SpawnedUnit},
    objective::Objective,
    player::Player,
    Map, MapM, MapS, Set, SetM, SetS,
};
use bfprotocols::db::{
    group::{GroupId, UnitId},
    objective::ObjectiveId,
};
use dcso3::{coalition::Side, net::Ucid, String};
use serde_derive::{Deserialize, Serialize};

/// All campaign state that is saved to disk.
///
/// The `*_by_*` maps and the per-kind sets are secondary indexes over
/// `groups`, `units`, and `objectives`; they must be kept in sync whenever a
/// group, unit, or objective is added or removed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Persisted {
    /// Every spawned group in the campaign, of any origin.
    pub groups: Map<GroupId, SpawnedGroup>,
    /// Every spawned unit in the campaign.
    pub units: Map<UnitId, SpawnedUnit>,
    /// DCS group name -> campaign group id.
    pub groups_by_name: Map<String, GroupId>,
    /// DCS unit name -> campaign unit id.
    pub units_by_name: Map<String, UnitId>,
    pub groups_by_side: MapS<Side, Set<GroupId>>,
    /// Groups unpacked from crates by players (`DeployKind::Deployed`).
    pub deployed: SetM<GroupId>,
    /// Objectives that are player built FARPs.
    pub farps: SetS<ObjectiveId>,
    /// Cargo crate groups currently in the world (`DeployKind::Crate`).
    pub crates: SetM<GroupId>,
    /// Troop groups unloaded by players (`DeployKind::Troop`).
    pub troops: SetM<GroupId>,
    /// Groups that act as a jtac (deployed/troop jtacs and drone actions).
    pub jtacs: SetM<GroupId>,
    /// Groups that provide EWR coverage (deployed EWRs and awacs actions).
    pub ewrs: SetS<GroupId>,
    /// Groups spawned by actions (`DeployKind::Action`).
    #[serde(default)]
    pub actions: SetS<GroupId>,
    pub objectives: MapM<ObjectiveId, Objective>,
    pub objectives_by_name: MapM<String, ObjectiveId>,
    /// Maps each objective owned group to the objective it belongs to.
    pub objectives_by_group: MapM<GroupId, ObjectiveId>,
    /// Every registered player, keyed by their DCS ucid.
    pub players: Map<Ucid, Player>,
    /// Objectives that act as logistics hubs, distributing supplies to the
    /// objectives they serve.
    #[serde(default)]
    pub logistics_hubs: SetS<ObjectiveId>,
    /// Number of nukes used so far this campaign. Used to scale the cost of
    /// further nukes.
    #[serde(default)]
    pub nukes_used: u32,
    /// Logistics ticks since production was last delivered. When it reaches
    /// the configured ticks per delivery, production is delivered and it is
    /// reset to 0. Setting it to `u32::MAX` forces a delivery on the next
    /// tick.
    #[serde(default)]
    pub logistics_ticks_since_delivery: u32,
    /// The objective id sequence at the time of the snapshot, restored on
    /// load so ids are never reused across restarts.
    #[serde(default)]
    pub oid: i64,
    /// The group id sequence at the time of the snapshot (see `oid`).
    #[serde(default)]
    pub gid: i64,
    /// The unit id sequence at the time of the snapshot (see `oid`).
    #[serde(default)]
    pub uid: i64,
    /// Whether the one time v0 save format migration has run (converts
    /// `DeployKind::ObjectiveDeprecated` groups and marks units not on their
    /// objective's owning side as dead).
    #[serde(default)]
    pub migrated_v0: bool,
}

impl Persisted {
    pub fn players(&self) -> &Map<Ucid, Player> {
        &self.players
    }
}
