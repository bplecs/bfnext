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

//! The campaign database.
//!
//! [`Db`] holds all campaign state and is split in two halves:
//! - [`Persisted`]: everything that survives a server restart (objectives, groups, units,
//!   players, ...). It is periodically snapshotted via [`Db::maybe_snapshot`] and written to
//!   a zstd compressed json save file, which [`Db::load`] reads back.
//! - [`Ephemeral`]: runtime only state (config, indexes, spawn/despawn queues, slot
//!   occupancy, cargo, message queue, dirty flag, ...) that is rebuilt on startup.
//!
//! The submodules extend `Db` with `impl` blocks for each area of the campaign (players,
//! cargo, logistics, objectives, ...). This module also defines the persistent map/set type
//! aliases and a set of lookup macros used throughout the crate.

extern crate nalgebra as na;
use self::{group::DeployKind, persisted::Persisted};
use crate::{bg::Task, db::ephemeral::Ephemeral, jtac::JtId};
use anyhow::{Result, anyhow};
use bfprotocols::{
    cfg::{
        Action, ActionKind, AwacsCfg, Cfg, Deployable, DeployableEwr, DeployableJtac, DroneCfg,
        Troop,
    },
    db::{
        group::{GroupId, UnitId},
        objective::ObjectiveId,
    },
};
use dcso3::{
    Vector3, centroid3d,
    coalition::Side,
    env::miz::{Miz, MizIndex},
};
use std::{cmp::max, fs::File, path::Path, sync::Arc};
use tokio::sync::mpsc::UnboundedSender;

pub mod actions;
pub mod cargo;
pub mod ephemeral;
pub mod group;
pub mod logistics;
pub mod markup;
pub mod mizinit;
pub mod objective;
pub mod persisted;
pub mod player;

// persistent (copy on write, structurally shared) maps and sets. Cloning is cheap, which is
// what makes snapshotting `Persisted` affordable. The suffix selects the chunk size: none =
// 256, M = 64, S = 16; smaller chunks suit smaller collections.
pub type Map<K, V> = immutable_chunkmap::map::Map<K, V, 256>;
pub type MapM<K, V> = immutable_chunkmap::map::Map<K, V, 64>;
pub type MapS<K, V> = immutable_chunkmap::map::Map<K, V, 16>;

pub type Set<K> = immutable_chunkmap::set::Set<K, 256>;
pub type SetM<K> = immutable_chunkmap::set::Set<K, 64>;
pub type SetS<K> = immutable_chunkmap::set::Set<K, 16>;

/// Description of one JTAC source, as produced by [`Db::jtacs`].
pub struct JtDesc {
    /// position of the jtac (group centroid, or the player's aircraft)
    pub pos: Vector3,
    pub id: JtId,
    pub side: Side,
    pub spec: DeployableJtac,
    /// true for airborne jtacs (drones and jtac capable player aircraft)
    pub air: bool,
}

/// Look up `$id` in map `$t`, returning `Result<&V>` with a "no such `$name`" error if absent.
#[macro_export]
macro_rules! maybe {
    ($t:expr, $id:expr, $name:expr) => {
        $t.get(&$id)
            .ok_or_else(|| anyhow!("no such {} {:?}", $name, $id))
    };
}

/// Like [`maybe!`] but returns a mutable (copy on write) reference.
#[macro_export]
macro_rules! maybe_mut {
    ($t:expr, $id:expr, $name:expr) => {
        $t.get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such {} {:?}", $name, $id))
    };
}

// the following macros take a `Db` (or anything with a `persisted` field) and look up a
// unit, group, or objective, returning an `anyhow::Result` instead of an `Option`.

#[macro_export]
macro_rules! unit {
    ($t:expr, $id:expr) => {
        $t.persisted
            .units
            .get(&$id)
            .ok_or_else(|| anyhow!("no such unit {:?}", $id))
    };
}

#[macro_export]
macro_rules! unit_mut {
    ($t:expr, $id:expr) => {
        $t.persisted
            .units
            .get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such unit {:?}", $id))
    };
}

#[macro_export]
macro_rules! unit_by_name {
    ($t:expr, $name:expr) => {
        $t.persisted
            .units_by_name
            .get($name)
            .and_then(|id| $t.persisted.units.get(id))
            .ok_or_else(|| anyhow!("no such unit {}", $name))
    };
}

#[macro_export]
macro_rules! group {
    ($t:expr, $id:expr) => {
        $t.persisted
            .groups
            .get(&$id)
            .ok_or_else(|| anyhow!("no such group {:?}", $id))
    };
}

#[macro_export]
macro_rules! group_mut {
    ($t:expr, $id:expr) => {
        $t.persisted
            .groups
            .get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such group {:?}", $id))
    };
}

#[macro_export]
macro_rules! group_by_name {
    ($t:expr, $name:expr) => {
        $t.persisted
            .groups_by_name
            .get($name)
            .and_then(|id| $t.persisted.groups.get(id))
            .ok_or_else(|| anyhow!("no such group {}", $name))
    };
}

#[macro_export]
macro_rules! objective {
    ($t:expr, $id:expr) => {
        $t.persisted
            .objectives
            .get(&$id)
            .ok_or_else(|| anyhow!("no such objective {:?}", $id))
    };
}

#[macro_export]
macro_rules! objective_mut {
    ($t:expr, $id:expr) => {
        $t.persisted
            .objectives
            .get_mut_cow(&$id)
            .ok_or_else(|| anyhow!("no such objective {:?}", $id))
    };
}

/// Evaluates to `Result<(alive, total)>`, the number of living units in group `$gid` and
/// its total number of units. Uses `?` internally, so it must be used in a fn returning
/// `Result`.
#[macro_export]
macro_rules! group_health {
    ($t:expr, $gid:expr) => {{
        let group = group!($t, $gid)?;
        let mut alive = 0;
        for uid in &group.units {
            if !unit!($t, uid)?.dead {
                alive += 1;
            }
        }
        Ok::<_, anyhow::Error>((alive, group.units.len()))
    }};
}

/// The campaign database, see the module docs.
#[derive(Debug, Default)]
pub struct Db {
    /// state saved to disk and restored across restarts
    pub persisted: Persisted,
    /// runtime only state, rebuilt on startup
    pub ephemeral: Ephemeral,
}

impl Db {
    /// Load a campaign from the zstd compressed json save file at `path`.
    ///
    /// The global objective/group/unit id sequences are advanced past the saved values so
    /// new ids never collide with existing ones, then the ephemeral state is initialized
    /// from the mission and `cfg`. Errors if the file can't be opened or decoded.
    pub fn load(
        miz: &Miz,
        idx: &MizIndex,
        to_bg: UnboundedSender<Task>,
        cfg: Arc<Cfg>,
        path: &Path,
    ) -> Result<Self> {
        let file = File::open(&path)
            .map_err(|e| anyhow!("failed to open save file {:?}, {:?}", path, e))?;
        let file = zstd::stream::Decoder::new(file)?;
        let persisted: Persisted = serde_json::from_reader(file)
            .map_err(|e| anyhow!("failed to decode save file {:?}, {:?}", path, e))?;
        let mut db = Db {
            persisted,
            ephemeral: Ephemeral::default(),
        };
        ObjectiveId::setseq(max(db.persisted.oid, ObjectiveId::seq()));
        GroupId::setseq(max(db.persisted.gid, GroupId::seq()));
        UnitId::setseq(max(db.persisted.uid, UnitId::seq()));
        db.ephemeral.set_cfg(miz, idx, cfg, to_bg)?;
        Ok(db)
    }

    /// If anything changed since the last call (the dirty flag is set), clear the flag, record
    /// the current id sequences, and return a (cheap, structurally shared) clone of the
    /// persisted state for saving. Returns `None` if nothing changed.
    pub fn maybe_snapshot(&mut self) -> Option<Persisted> {
        if self.ephemeral.take_dirty() {
            self.persisted.oid = ObjectiveId::seq();
            self.persisted.gid = GroupId::seq();
            self.persisted.uid = UnitId::seq();
            Some(self.persisted.clone())
        } else {
            None
        }
    }

    /// Iterate over all EWR capable groups (deployed EWRs and AWACS actions), yielding the
    /// group centroid, its side, and its EWR config.
    pub fn ewrs(&self) -> impl Iterator<Item = (Vector3, Side, &DeployableEwr)> {
        self.persisted.ewrs.into_iter().filter_map(|gid| {
            let group = self.persisted.groups.get(gid)?;
            match &group.origin {
                DeployKind::Crate { .. }
                | DeployKind::Objective { .. }
                | DeployKind::ObjectiveDeprecated
                | DeployKind::Troop { .. } => None,
                DeployKind::Action {
                    spec:
                        Action {
                            kind: ActionKind::Awacs(AwacsCfg { ewr, .. }),
                            ..
                        },
                    ..
                }
                | DeployKind::Deployed {
                    spec: Deployable { ewr: Some(ewr), .. },
                    ..
                } => {
                    let pos = centroid3d(
                        group
                            .units
                            .into_iter()
                            .map(|u| self.persisted.units[u].position.p.0),
                    );
                    Some((pos, group.side, ewr))
                }
                DeployKind::Action { .. } | DeployKind::Deployed { .. } => None,
            }
        })
    }

    /// Iterate over every JTAC source in the campaign:
    /// - ground jtac groups (deployed jtacs and jtac troops)
    /// - drone actions (airborne)
    /// - player aircraft whose type is configured as an airborne jtac
    /// - player aircraft carrying a jtac capable troop as cargo (treated as ground jtacs)
    pub fn jtacs<'a>(&'a self) -> impl Iterator<Item = JtDesc> + 'a {
        self.persisted
            .jtacs
            .into_iter()
            .filter_map(|gid| {
                let group = self.persisted.groups.get(gid)?;
                let pos = centroid3d(
                    group
                        .units
                        .into_iter()
                        .filter_map(|u| self.persisted.units.get(u).map(|u| u.position.p.0)),
                );
                match &group.origin {
                    DeployKind::Troop {
                        spec:
                            Troop {
                                jtac: Some(jtac), ..
                            },
                        ..
                    }
                    | DeployKind::Deployed {
                        spec:
                            Deployable {
                                jtac: Some(jtac), ..
                            },
                        ..
                    } => Some(JtDesc {
                        pos,
                        id: JtId::Group(*gid),
                        side: group.side,
                        spec: *jtac,
                        air: false,
                    }),
                    DeployKind::Action {
                        spec:
                            Action {
                                kind: ActionKind::Drone(DroneCfg { jtac, .. }),
                                ..
                            },
                        ..
                    } => Some(JtDesc {
                        pos,
                        id: JtId::Group(*gid),
                        side: group.side,
                        spec: *jtac,
                        air: true,
                    }),
                    DeployKind::Crate { .. }
                    | DeployKind::Action { .. }
                    | DeployKind::Objective { .. }
                    | DeployKind::ObjectiveDeprecated
                    | DeployKind::Troop { .. }
                    | DeployKind::Deployed { .. } => None,
                }
            })
            .chain(self.instanced_players().filter_map(|(_, p, inst)| {
                // instanced_players only yields players with a current slot
                let slot = p.current_slot.as_ref().unwrap().0;
                let pos = inst.position.p.0;
                let id = JtId::Slot(slot);
                match self.ephemeral.cfg.airborne_jtacs.get(&inst.typ) {
                    Some(jt) => Some(JtDesc {
                        pos,
                        id,
                        side: p.side,
                        spec: *jt,
                        air: true,
                    }),
                    None => match self.ephemeral.cargo.get(&slot) {
                        None => None,
                        Some(cargo) => {
                            for it in &cargo.troops {
                                if let Some(jt) = &it.troop.jtac {
                                    return Some(JtDesc {
                                        pos,
                                        id,
                                        side: p.side,
                                        spec: *jt,
                                        air: false,
                                    });
                                }
                            }
                            None
                        }
                    },
                }
            }))
    }
}
