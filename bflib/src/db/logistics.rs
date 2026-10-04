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

//! Warehouse logistics.
//!
//! When the campaign config has a `warehouse` section, every objective has a
//! [`Warehouse`] of equipment (airframes, weapons) and liquids (fuel) that
//! mirrors its DCS airbase warehouse. Each side's production is read from a
//! supply source warehouse in the miz at startup. On every logistics tick
//! (`tick` minutes) the hubs distribute stock to the objectives they supply
//! (each objective is supplied by its nearest friendly hub), and stock is
//! then balanced between hubs. Once `ticks_per_delivery` ticks have passed
//! since the last delivery, one delivery of production is first added to
//! each side's hubs.
//!
//! The work is spread across frames by a state machine ([`LogiStage`]) driven
//! by [`Db::logistics_step`]: read DCS warehouses into the db, compute
//! transfers, execute them, then write the db back to the DCS warehouses.

use super::{
    ephemeral::{Equipment, Production},
    objective::Objective,
    persisted::Persisted,
    Db, Map, MapS, SetS,
};
use crate::{admin::WarehouseKind, maybe, objective, objective_mut, Task};
use anyhow::{anyhow, bail, Context, Result};
use bfprotocols::{
    cfg::Vehicle,
    db::objective::{ObjectiveId, ObjectiveKind},
    perf::{Perf, PerfInner},
    stats::Stat,
};
use chrono::{prelude::*, Duration};
use compact_str::{format_compact, CompactString};
use dcso3::{
    airbase::Airbase,
    coalition::Side,
    object::DcsObject,
    perf::record_perf,
    warehouse::{self, LiquidType},
    world::World,
    MizLua, String, Vector2,
};
use fxhash::FxHashMap;
use log::{error, warn};
use serde_derive::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::{
    cmp::{max, min},
    collections::hash_map::Entry,
    mem,
    ops::{AddAssign, SubAssign},
    sync::Arc,
};
use tokio::sync::mpsc::UnboundedSender;

/// The state of the logistics state machine. Each call to
/// [`Db::logistics_step`] does a small amount of work and may advance the
/// stage. A normal tick runs Complete -> SyncFromWarehouses ->
/// ExecuteTransfers -> SyncToWarehouses -> Complete.
#[derive(Debug, Clone)]
pub enum LogiStage {
    /// Idle until `tick` minutes after `last_tick`
    Complete {
        last_tick: DateTime<Utc>,
    },
    /// Reading DCS warehouse inventories into the db, one objective per step.
    /// When empty, transfers are computed.
    SyncFromWarehouses {
        objectives: SmallVec<[ObjectiveId; 128]>,
    },
    /// Writing db inventories out to the DCS warehouses, one objective per step
    SyncToWarehouses {
        objectives: SmallVec<[ObjectiveId; 128]>,
    },
    /// Executing pending transfers within a time budget per step. When empty
    /// the logistics hubs are balanced.
    ExecuteTransfers {
        transfers: Vec<Transfer>,
    },
    /// Startup. Moves straight to syncing the db to the DCS warehouses.
    Init,
}

impl Default for LogiStage {
    fn default() -> Self {
        Self::Init
    }
}

/// The stock of one item in a warehouse. `+=` clamps to `capacity`, `-=`
/// clamps to 0.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub stored: u32,
    pub capacity: u32,
}

impl Inventory {
    /// The percent (capped at 100) of capacity stored, or None if the
    /// capacity is 0.
    pub fn percent(&self) -> Option<u8> {
        if self.capacity == 0 {
            None
        } else {
            let stored: f32 = self.stored as f32;
            let capacity: f32 = self.capacity as f32;
            Some(min(100, ((stored / capacity) * 100.) as u32) as u8)
        }
    }

    /// Remove `percent` (a fraction from 0 to 1) of the stored amount, but at
    /// least 1 if anything is stored. Returns the amount removed.
    pub fn reduce(&mut self, percent: f32) -> u32 {
        if self.stored == 0 {
            0
        } else {
            let taken = max(1, (self.stored as f32 * percent) as u32);
            self.stored -= taken;
            taken
        }
    }
}

impl AddAssign<u32> for Inventory {
    fn add_assign(&mut self, rhs: u32) {
        let qty = self.stored + rhs;
        if qty > self.capacity {
            self.stored = self.capacity
        } else {
            self.stored = qty
        }
    }
}

impl SubAssign<u32> for Inventory {
    fn sub_assign(&mut self, rhs: u32) {
        if rhs > self.stored {
            self.stored = 0
        } else {
            self.stored = self.stored - rhs;
        }
    }
}

/// The item a [`Transfer`] moves
#[derive(Debug, Clone)]
enum TransferItem {
    Equipment(String),
    Liquid(LiquidType),
}

/// A pending move of `amount` of `item` from `source` to `target`
#[derive(Debug, Clone)]
pub struct Transfer {
    source: ObjectiveId,
    target: ObjectiveId,
    amount: u32,
    item: TransferItem,
}

impl Transfer {
    /// Apply the transfer to the db (not the DCS warehouses) and publish the
    /// new inventory of both objectives as stats. Fails, without changing
    /// anything, if either objective doesn't exist.
    ///
    /// Transfers are computed ahead of time, and the source's stock may have
    /// dropped since (e.g. a player took off with it, or overlapping
    /// transfers were scheduled), so at most what the source currently holds
    /// is moved.
    fn execute(&self, db: &mut Persisted, to_bg: &Option<UnboundedSender<Task>>) -> Result<()> {
        // check the target first so a deleted target doesn't make the
        // supplies vanish from the source
        if db.objectives.get(&self.target).is_none() {
            bail!("no such objective {:?}", self.target)
        }
        let src = db
            .objectives
            .get_mut_cow(&self.source)
            .ok_or_else(|| anyhow!("no such objective {:?}", self.source))?;
        let amount = match &self.item {
            TransferItem::Equipment(name) => {
                let d = &mut src.warehouse.equipment[name].stored;
                let amount = min(self.amount, *d);
                *d -= amount;
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::EquipmentInventory {
                        id: src.id,
                        item: name.clone(),
                        amount: *d,
                    }));
                }
                amount
            }
            TransferItem::Liquid(name) => {
                let d = &mut src.warehouse.liquids[name].stored;
                let amount = min(self.amount, *d);
                *d -= amount;
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::LiquidInventory {
                        id: src.id,
                        item: *name,
                        amount: *d,
                    }));
                }
                amount
            }
        };
        let dst = db
            .objectives
            .get_mut_cow(&self.target)
            .ok_or_else(|| anyhow!("no such objective {:?}", self.target))?;
        match &self.item {
            TransferItem::Equipment(name) => {
                let d = &mut dst
                    .warehouse
                    .equipment
                    .get_or_default_cow(name.clone())
                    .stored;
                *d = d.saturating_add(amount);
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::EquipmentInventory {
                        id: dst.id,
                        item: name.clone(),
                        amount: *d,
                    }));
                }
            }
            TransferItem::Liquid(name) => {
                let d = &mut dst.warehouse.liquids.get_or_default_cow(*name).stored;
                *d = d.saturating_add(amount);
                if let Some(to_bg) = to_bg.as_ref() {
                    let _ = to_bg.send(Task::Stat(Stat::LiquidInventory {
                        id: dst.id,
                        item: *name,
                        amount: *d,
                    }));
                }
            }
        }
        Ok(())
    }
}

/// An objective's demand for one item while a hub's stock is being allocated
struct Needed<'a> {
    oid: &'a ObjectiveId,
    obj: &'a Objective,
    /// capacity - stored
    demanded: u32,
    allocated: u32,
}

/// The db side copy of an objective's warehouse
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Warehouse {
    /// Currently unused
    pub(super) base_equipment: Map<String, Inventory>,
    pub(super) equipment: Map<String, Inventory>,
    pub(super) liquids: MapS<LiquidType, Inventory>,
    /// The logistics hub that supplies this objective, if any
    pub(super) supplier: Option<ObjectiveId>,
    /// For logistics hubs, the objectives this hub supplies
    pub(super) destination: SetS<ObjectiveId>,
}

/// Write the db inventory of `obj` to its DCS warehouse
fn sync_obj_to_warehouse(obj: &Objective, warehouse: &warehouse::Warehouse) -> Result<()> {
    let perf = unsafe { Perf::get_mut() };
    let perf = Arc::make_mut(&mut perf.inner);
    for (item, inv) in &obj.warehouse.equipment {
        perf.logistics_items.insert((item.clone(), obj.id));
        warehouse
            .set_item(item.clone(), inv.stored)
            .context("setting item")?
    }
    for (name, inv) in &obj.warehouse.liquids {
        warehouse
            .set_liquid_amount(*name, inv.stored)
            .context("setting liquid")?
    }
    Ok(())
}

/// Read the stored amounts of the items `obj` tracks from its DCS warehouse.
/// Capacities are not changed.
fn sync_warehouse_to_obj(obj: &mut Objective, warehouse: &warehouse::Warehouse) -> Result<()> {
    for (name, inv) in obj.warehouse.equipment.iter_mut_cow() {
        inv.stored = warehouse.get_item_count(name.clone())?;
    }
    for (name, inv) in obj.warehouse.liquids.iter_mut_cow() {
        inv.stored = warehouse.get_liquid_amount(*name)?;
    }
    Ok(())
}

/// Get the warehouse of the airbase named `template` (a side's supply source)
fn get_supplier<'lua>(lua: MizLua<'lua>, template: String) -> Result<warehouse::Warehouse<'lua>> {
    Airbase::get_by_name(lua, template.clone())
        .with_context(|| format_compact!("getting airbase {}", template))?
        .get_warehouse()
        .context("getting warehouse")
}

impl Db {
    /// Build each side's production (the per delivery amount of every item)
    /// from the contents of its `supply_source` warehouse, if it hasn't been
    /// built yet. Also checks that every produced aircraft has a threatened
    /// distance and life type configured.
    fn init_resource_map(&mut self, lua: MizLua) -> Result<()> {
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            None => return Ok(()),
            Some(w) => w,
        };
        if self.ephemeral.production_by_side.is_empty() {
            let map =
                warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
            map.for_each(|name, typ| {
                for side in Side::ALL {
                    let template = match whcfg.supply_source.get(&side) {
                        Some(tmpl) => tmpl,
                        None => continue, // side didn't produce anything, bummer
                    };
                    let w = get_supplier(lua, template.clone())
                        .with_context(|| format_compact!("getting supplier {template}"))?;
                    let production =
                        Arc::make_mut(self.ephemeral.production_by_side.entry(side).or_default());
                    let qty = w
                        .get_item_count(name.clone())
                        .with_context(|| format_compact!("getting {name} from the warehouse"))?;
                    if qty > 0 {
                        production
                            .equipment
                            .insert(name.clone(), Equipment { production: qty });
                        let category = typ.category().context("getting category")?;
                        if category.is_aircraft() {
                            let vehicle = Vehicle::from(name.clone());
                            self.ephemeral
                                .cfg
                                .check_vehicle_has_threat_distance(&vehicle)?;
                            self.ephemeral.cfg.check_vehicle_has_life_type(&vehicle)?;
                        }
                    }
                    for name in LiquidType::ALL {
                        let qty = w.get_liquid_amount(name).context("getting liquid amount")?;
                        if qty > 0 {
                            production.liquids.insert(name, qty);
                        }
                    }
                }
                Ok(())
            })
            .context("iterating resource map")?
        }
        Ok(())
    }

    /// Set up an empty warehouse for a new FARP, with airbase capacity for
    /// everything its owner produces.
    pub(super) fn init_farp_warehouse(&mut self, oid: &ObjectiveId) -> Result<()> {
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            Some(cfg) => cfg,
            None => return Ok(()),
        };
        let obj = objective_mut!(self, oid)?;
        let production = match self.ephemeral.production_by_side.get(&obj.owner) {
            Some(q) => Arc::clone(q),
            None => return Ok(()),
        };
        for (name, equip) in &production.equipment {
            let inv = Inventory {
                stored: 0,
                capacity: equip.production * whcfg.airbase_max,
            };
            obj.warehouse.equipment.insert_cow(name.clone(), inv);
        }
        for (name, qty) in &production.liquids {
            let inv = Inventory {
                stored: 0,
                capacity: qty * whcfg.airbase_max,
            };
            obj.warehouse.liquids.insert_cow(*name, inv);
        }
        Ok(())
    }

    /// Initialize the warehouses of every objective for a new campaign,
    /// filled to capacity for everything the owner produces. Capacity is the
    /// production amount times `hub_max` for logistics hubs or
    /// `airbase_max` otherwise.
    pub(super) fn init_warehouses(&mut self, lua: MizLua) -> Result<()> {
        self.init_resource_map(lua)
            .context("initializing resource map")?;
        let cfg = &self.ephemeral.cfg;
        let whcfg = match cfg.warehouse.as_ref() {
            Some(cfg) => cfg,
            None => return Ok(()),
        };
        for side in Side::ALL {
            let production = match self.ephemeral.production_by_side.get(&side) {
                None => continue,
                Some(q) => Arc::clone(q),
            };
            for (name, equip) in &production.equipment {
                for (oid, obj) in self.persisted.objectives.iter_mut_cow() {
                    if obj.owner == side {
                        let hub = self.persisted.logistics_hubs.contains(&oid);
                        let capacity = whcfg.capacity(hub, equip.production);
                        let inv = obj.warehouse.equipment.get_or_default_cow(name.clone());
                        inv.capacity = capacity;
                        inv.stored = capacity;
                    }
                }
            }
            for (name, qty) in &production.liquids {
                for (oid, obj) in self.persisted.objectives.iter_mut_cow() {
                    if obj.owner == side {
                        let hub = self.persisted.logistics_hubs.contains(&oid);
                        let capacity = whcfg.capacity(hub, *qty);
                        let inv = obj.warehouse.liquids.get_or_default_cow(*name);
                        inv.capacity = capacity;
                        inv.stored = capacity;
                    }
                }
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    /// Called after the db is loaded. Associates each DCS airbase with the
    /// objective whose zone contains it (disabling auto capture and setting
    /// its coalition), zeroes the equipment of airbases outside any objective
    /// (except FARP pad templates), and adjusts objective warehouses for
    /// changes in production since the save (removing items no longer
    /// produced and updating capacities). Then updates supply status and
    /// supply lines. Fails if an objective has no airbase, or has more than
    /// one.
    pub(super) fn setup_warehouses_after_load(&mut self, lua: MizLua) -> Result<()> {
        self.init_resource_map(lua)
            .context("initializing resource map")?;
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            Some(cfg) => cfg,
            None => return Ok(()),
        };
        let map = warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
        let world = World::singleton(lua).context("getting world")?;
        let mut load_and_sync_airbases = || -> Result<()> {
            world
                .get_airbases()
                .context("getting airbases")?
                .for_each(|airbase| {
                    let airbase = airbase.context("getting airbase")?;
                    let name = airbase.as_object()?.get_name()?;
                    log::info!("setting up airbase {name}");
                    if !airbase.is_exist()? {
                        return Ok(()); // can happen when farps get recycled
                    }
                    let pos3 = airbase.get_point().context("getting airbase position")?;
                    let pos = Vector2::new(pos3.x, pos3.z);
                    airbase
                        .auto_capture(false)
                        .context("setting airbase autocapture")?;
                    let oid = self
                        .persisted
                        .objectives
                        .into_iter()
                        .find(|(_, obj)| obj.zone.contains(pos));
                    let w = airbase
                        .get_warehouse()
                        .context("getting airbase warehouse")?;
                    let (oid, obj) = match oid {
                        Some((oid, obj)) => {
                            airbase
                                .set_coalition(obj.owner)
                                .context("setting airbase owner")?;
                            (*oid, obj)
                        }
                        None if !self.ephemeral.global_pad_templates.contains(&name) => {
                            map.for_each(|name, _| {
                                w.set_item(name, 0).context("zeroing item")?;
                                Ok(())
                            })?;
                            return Ok(());
                        }
                        None => {
                            log::info!("airbase {name} has no objective");
                            return Ok(());
                        }
                    };
                    match self.ephemeral.airbase_by_oid.entry(oid) {
                        Entry::Vacant(e) => {
                            e.insert(airbase.object_id().context("getting airbase object_id")?);
                        }
                        Entry::Occupied(_) => {
                            bail!("multiple airbases inside the trigger zone of {}", obj.name)
                        }
                    }
                    Ok(())
                })
        };
        load_and_sync_airbases().context("loading and syncing airbases")?;
        let mut adjust_warehouses_for_miz_changes = || -> Result<()> {
            for (oid, obj) in self.persisted.objectives.iter_mut_cow() {
                let mut del_eq: SmallVec<[String; 8]> = smallvec![];
                let mut del_l: SmallVec<[LiquidType; 4]> = smallvec![];
                if let Some(prod) = self.ephemeral.production_by_side.get(&obj.owner) {
                    let hub = self.persisted.logistics_hubs.contains(oid);
                    for (name, _) in &obj.warehouse.equipment {
                        if !prod.equipment.contains_key(name) {
                            del_eq.push(name.clone());
                        }
                    }
                    for name in del_eq {
                        obj.warehouse.equipment.remove_cow(&name);
                    }
                    for (liq, _) in &obj.warehouse.liquids {
                        if !prod.liquids.contains_key(liq) {
                            del_l.push(*liq);
                        }
                    }
                    for liq in del_l {
                        obj.warehouse.liquids.remove_cow(&liq);
                    }
                    for (name, eqip) in &prod.equipment {
                        let capacity = whcfg.capacity(hub, eqip.production);
                        let inv = obj.warehouse.equipment.get_or_default_cow(name.clone());
                        inv.capacity = capacity;
                    }
                    for (name, prod) in &prod.liquids {
                        let capacity = whcfg.capacity(hub, *prod);
                        let inv = obj.warehouse.liquids.get_or_default_cow(*name);
                        inv.capacity = capacity;
                    }
                }
            }
            Ok(())
        };
        adjust_warehouses_for_miz_changes().context("adjusting warehouses for miz changes")?;
        let mut missing = vec![];
        for (oid, obj) in &self.persisted.objectives {
            if !self.ephemeral.airbase_by_oid.contains_key(oid) {
                missing.push(obj.name.clone());
            }
        }
        if !missing.is_empty() {
            bail!("objectives missing a warehouse {:?}", missing)
        }
        self.update_supply_status()
            .context("updating supply status")?;
        self.setup_supply_lines()
            .context("setting up supply lines")?;
        Ok(())
    }

    /// True while a logistics tick has changed the db's inventories but not
    /// yet written them all to DCS (or after loading, before the first
    /// write). While this is true the db is authoritative and the tick will
    /// write it to every DCS warehouse. Otherwise the DCS warehouses are
    /// authoritative, since players may have used stock since the last sync.
    pub(super) fn logistics_db_ahead(&self) -> bool {
        match self.ephemeral.logistics_stage {
            LogiStage::Init
            | LogiStage::ExecuteTransfers { .. }
            | LogiStage::SyncToWarehouses { .. } => true,
            LogiStage::Complete { .. } | LogiStage::SyncFromWarehouses { .. } => false,
        }
    }

    /// Stock objectives `oids` (just built or captured) from their logistics
    /// hubs now, rather than waiting for the next logistics tick, and get
    /// the result into DCS.
    ///
    /// The db inventories of `oids` must already be current (the caller
    /// reads or initializes them). Only `oids` and the hubs that supply them
    /// are read from or written to DCS, so stock used at other objectives
    /// since the last tick isn't overwritten. If a tick has db changes not
    /// yet in DCS (see [`Db::logistics_db_ahead`]) nothing is read, and the
    /// tick writes the result out. Failures are logged; if a hub can't be
    /// read, no supplies are moved but `oids` are still written.
    pub(super) fn stock_objectives_now(&mut self, lua: MizLua, oids: &[ObjectiveId]) -> Result<()> {
        if self.ephemeral.cfg.warehouse.is_none() {
            return Ok(());
        }
        let ahead = self.logistics_db_ahead();
        // the hubs supplying oids, excluding any in oids, whose db state the
        // caller has already set
        let mut hubs: SmallVec<[ObjectiveId; 4]> = smallvec![];
        for oid in oids {
            if let Some(hub) = objective!(self, oid)?.warehouse.supplier {
                if !oids.contains(&hub) && !hubs.contains(&hub) {
                    hubs.push(hub)
                }
            }
        }
        // read what the hubs really hold, so transfers come from current stock
        let mut hubs_ok = true;
        if !ahead {
            for hub in &hubs {
                if let Err(e) = self.sync_warehouse_to_objective(lua, *hub) {
                    error!("failed to sync hub {hub} from warehouse {e:?}");
                    hubs_ok = false;
                }
            }
        }
        if hubs_ok {
            match self.deliver_supplies_to(Some(oids)) {
                Err(e) => error!("failed to compute supplies for {oids:?} {e:?}"),
                Ok(transfers) => {
                    for tr in transfers {
                        if let Err(e) = tr.execute(&mut self.persisted, &self.ephemeral.to_bg) {
                            error!("executing transfer {:?} {e:?}", tr)
                        }
                    }
                }
            }
        } else {
            // a hub's db stock is stale, writing it would overwrite DCS
            hubs.clear();
        }
        let to_write = oids.iter().chain(hubs.iter()).copied();
        if ahead {
            // Init and ExecuteTransfers write every objective once they
            // reach SyncToWarehouses, but a sync already underway may have
            // passed these objectives
            if let LogiStage::SyncToWarehouses { objectives } = &mut self.ephemeral.logistics_stage {
                objectives.extend(to_write);
            }
        } else {
            for oid in to_write {
                if let Err(e) = self.sync_objective_to_warehouse(lua, oid) {
                    error!("failed to sync objective {oid} to warehouse {e:?}")
                }
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    /// Make the next logistics tick start immediately (if idle)
    pub fn admin_tick_now(&mut self) {
        match &mut self.ephemeral.logistics_stage {
            LogiStage::Init
            | LogiStage::SyncFromWarehouses { .. }
            | LogiStage::SyncToWarehouses { .. }
            | LogiStage::ExecuteTransfers { .. } => (),
            LogiStage::Complete { last_tick } => {
                *last_tick = DateTime::<Utc>::MIN_UTC;
            }
        }
    }

    /// Make the next logistics tick start immediately and deliver production
    pub fn admin_deliver_now(&mut self) {
        self.admin_tick_now();
        self.persisted.logistics_ticks_since_delivery = u32::MAX;
    }

    /// Advance the logistics state machine by one step (see [`LogiStage`]).
    /// Called frequently, each call does a bounded amount of work. Does
    /// nothing if the warehouse system isn't configured.
    pub fn logistics_step(
        &mut self,
        lua: MizLua,
        perf: &mut PerfInner,
        ts: DateTime<Utc>,
    ) -> Result<()> {
        if let Some(wcfg) = self.ephemeral.cfg.warehouse.as_ref() {
            let freq = Duration::minutes(wcfg.tick as i64);
            let ticks_per_delivery = wcfg.ticks_per_delivery;
            let start_ts = Utc::now();
            match &mut self.ephemeral.logistics_stage {
                LogiStage::Init => {
                    let objectives = self
                        .persisted
                        .objectives
                        .into_iter()
                        .map(|(id, _)| *id)
                        .collect();
                    self.ephemeral.logistics_stage = LogiStage::SyncToWarehouses { objectives }
                }
                LogiStage::Complete { last_tick } if ts - *last_tick >= freq => {
                    let objectives = self
                        .persisted
                        .objectives
                        .into_iter()
                        .map(|(id, _)| *id)
                        .collect();
                    self.ephemeral.logistics_stage = LogiStage::SyncFromWarehouses { objectives };
                }
                LogiStage::Complete { last_tick: _ } => (),
                LogiStage::SyncFromWarehouses { objectives } => match objectives.pop() {
                    Some(oid) => {
                        let start_ts = Utc::now();
                        if let Err(e) = self.sync_warehouse_to_objective(lua, oid) {
                            error!("failed to sync objective {oid} from warehouse {:?}", e)
                        }
                        record_perf(&mut perf.logistics_sync_from, start_ts);
                    }
                    None => {
                        // all warehouses are read, decide what moves this tick
                        let sts = Utc::now();
                        let transfers = if self.persisted.logistics_ticks_since_delivery
                            >= ticks_per_delivery
                        {
                            self.persisted.logistics_ticks_since_delivery = 0;
                            let v = match self.deliver_production() {
                                Ok(v) => v,
                                Err(e) => {
                                    error!("failed to deliver production {:?}", e);
                                    vec![]
                                }
                            };
                            record_perf(&mut perf.logistics_deliver, sts);
                            v
                        } else {
                            self.persisted.logistics_ticks_since_delivery += 1;
                            let v = match self.deliver_supplies_from_logistics_hubs() {
                                Ok(v) => v,
                                Err(e) => {
                                    error!("failed to deliver supplies from hubs {:?}", e);
                                    vec![]
                                }
                            };
                            record_perf(&mut perf.logistics_distribute, sts);
                            v
                        };
                        self.ephemeral.logistics_stage = LogiStage::ExecuteTransfers { transfers };
                    }
                },
                LogiStage::ExecuteTransfers { transfers } if transfers.is_empty() => {
                    let st = Utc::now();
                    self.balance_logistics_hubs()?;
                    let objectives = self
                        .persisted
                        .objectives
                        .into_iter()
                        .map(|(id, _)| *id)
                        .collect();
                    self.ephemeral.logistics_stage = LogiStage::SyncToWarehouses { objectives };
                    record_perf(&mut perf.logistics_transfer, st);
                }
                LogiStage::ExecuteTransfers { transfers } => {
                    let st = Utc::now();
                    while let Some(tr) = transfers.pop() {
                        if let Err(e) = tr.execute(&mut self.persisted, &self.ephemeral.to_bg) {
                            error!("executing transfer {:?} {e:?}", tr)
                        }
                        // limit the time spent per frame, the rest run next step
                        if Utc::now() - st > Duration::milliseconds(6) {
                            break;
                        }
                    }
                    record_perf(&mut perf.logistics_transfer, st);
                }
                LogiStage::SyncToWarehouses { objectives } => match objectives.pop() {
                    None => self.ephemeral.logistics_stage = LogiStage::Complete { last_tick: ts },
                    Some(oid) => {
                        let start_ts = Utc::now();
                        if let Err(e) = self.sync_objective_to_warehouse(lua, oid) {
                            error!("failed to sync objective {oid} to warehouse {:?}", e)
                        }
                        record_perf(&mut perf.logistics_sync_to, start_ts);
                    }
                },
            }
            record_perf(&mut perf.logistics, start_ts);
        }
        Ok(())
    }

    /// Convert a just captured objective's warehouse to its new owner. Items
    /// the new owner produces get the owner's capacity (existing stock is
    /// kept). Items only the other side produces are emptied and get zero
    /// capacity.
    pub(super) fn capture_warehouse(&mut self, lua: MizLua, oid: ObjectiveId) -> Result<()> {
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            Some(cfg) => cfg,
            None => return Ok(()),
        };
        let obj = objective_mut!(self, oid)?;
        let other_production = match self.ephemeral.production_by_side.get(&obj.owner.opposite()) {
            Some(q) => Arc::clone(q),
            None => Arc::new(Production::default()),
        };
        let production = match self.ephemeral.production_by_side.get(&obj.owner) {
            Some(q) => Arc::clone(q),
            None => return Ok(()),
        };
        let map = warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
        let hub = obj.kind.is_hub();
        map.for_each(|name, _| {
            match production.equipment.get(&name) {
                Some(equip) => {
                    let inv = obj.warehouse.equipment.get_or_default_cow(name);
                    inv.capacity = whcfg.capacity(hub, equip.production);
                }
                None => {
                    if let Some(_) = other_production.equipment.get(&name) {
                        let inv = obj.warehouse.equipment.get_or_default_cow(name);
                        inv.stored = 0;
                        inv.capacity = 0;
                    }
                }
            }
            Ok(())
        })?;
        for name in LiquidType::ALL {
            match production.liquids.get(&name) {
                Some(qty) => {
                    let inv = obj.warehouse.liquids.get_or_default_cow(name);
                    inv.capacity = whcfg.capacity(hub, *qty);
                }
                None => {
                    if let Some(_) = other_production.liquids.get(&name) {
                        let inv = obj.warehouse.liquids.get_or_default_cow(name);
                        inv.stored = 0;
                        inv.capacity = 0;
                    }
                }
            }
        }
        Ok(())
    }

    /// The nearest logistics hub with the same owner as `obj`, or None if
    /// there is none or `obj` is detached from logistics.
    pub(super) fn compute_supplier(&self, obj: &Objective) -> Result<Option<ObjectiveId>> {
        Ok(self
            .persisted
            .logistics_hubs
            .into_iter()
            .fold(Ok::<_, anyhow::Error>(None), |acc, id| {
                let logi = objective!(self, id)?;
                if obj.logistics_detached || logi.owner != obj.owner {
                    acc
                } else {
                    let dist =
                        na::distance_squared(&obj.zone.pos().into(), &logi.zone.pos().into());
                    match acc {
                        Err(e) => Err(e),
                        Ok(None) => Ok(Some((dist, *id))),
                        Ok(Some((pdist, _))) if dist < pdist => Ok(Some((dist, *id))),
                        Ok(Some((dist, id))) => Ok(Some((dist, id))),
                    }
                }
            })?
            .map(|(_, id)| id))
    }

    /// Recompute which hub supplies each non hub objective, rebuilding every
    /// hub's `destination` set. Hubs whose destinations changed get their
    /// map markup redrawn.
    pub fn setup_supply_lines(&mut self) -> Result<()> {
        let mut suppliers: SmallVec<[(ObjectiveId, Option<ObjectiveId>); 64]> = smallvec![];
        for (oid, obj) in &self.persisted.objectives {
            match obj.kind {
                ObjectiveKind::Logistics => (),
                ObjectiveKind::Airbase | ObjectiveKind::Farp { .. } | ObjectiveKind::Fob => {
                    let hub = self.compute_supplier(obj)?;
                    suppliers.push((*oid, hub));
                }
            }
        }
        let mut current: FxHashMap<ObjectiveId, SetS<ObjectiveId>> = FxHashMap::default();
        for oid in &self.persisted.logistics_hubs {
            let obj = objective_mut!(self, oid)?;
            current.insert(*oid, mem::take(&mut obj.warehouse.destination));
        }
        for (oid, supplier) in suppliers {
            let obj = objective_mut!(self, oid)?;
            obj.warehouse.supplier = supplier;
            if let Some(id) = supplier {
                objective_mut!(self, id)?
                    .warehouse
                    .destination
                    .insert_cow(oid);
            }
        }
        for (oid, current) in current {
            let obj = objective!(self, oid)?;
            if obj.warehouse.destination != current {
                self.ephemeral.create_objective_markup(&self.persisted, obj)
            }
        }
        Ok(())
    }

    /// Add one delivery of each side's production to its logistics hubs
    /// (clamped to capacity), then compute the hub to objective transfers.
    pub fn deliver_production(&mut self) -> Result<Vec<Transfer>> {
        if self.ephemeral.cfg.warehouse.is_none() {
            return Ok(vec![]);
        }
        self.setup_supply_lines()
            .context("setting up supply lines")?;
        let mut deliver_produced_supplies = || -> Result<()> {
            for side in Side::ALL {
                let production = match self.ephemeral.production_by_side.get(&side) {
                    Some(e) => e,
                    None => continue,
                };
                for oid in &self.persisted.logistics_hubs {
                    let logi = objective_mut!(self, oid)?;
                    if logi.owner == side {
                        for (name, inv) in logi.warehouse.equipment.iter_mut_cow() {
                            if let Some(eq) = production.equipment.get(name) {
                                *inv += eq.production;
                            }
                        }
                        for (name, inv) in logi.warehouse.liquids.iter_mut_cow() {
                            if let Some(pr) = production.liquids.get(name) {
                                *inv += *pr;
                            }
                        }
                    }
                }
            }
            Ok(())
        };
        deliver_produced_supplies().context("delivering produced supplies")?;
        self.ephemeral.dirty();
        self.deliver_supplies_from_logistics_hubs()
            .context("delivering supplies from logistics hubs")
    }

    /// Refresh the db's stored count of one vehicle type at an objective from
    /// its DCS warehouse. Does nothing if the objective doesn't track it.
    pub fn sync_vehicle_at_obj(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        typ: Vehicle,
    ) -> Result<()> {
        let obj = objective_mut!(self, oid)?;
        let id = maybe!(self.ephemeral.airbase_by_oid, oid, "airbase")?;
        let wh = Airbase::get_instance(lua, id)
            .context("getting airbase")?
            .get_warehouse()
            .context("getting warehouse")?;
        if let Some(inv) = obj.warehouse.equipment.get_mut_cow(&typ.0) {
            inv.stored = wh.get_item_count(typ.0).context("getting item")?;
            self.ephemeral.dirty();
        }
        Ok(())
    }

    /// Compute transfers from each logistics hub to the friendly objectives it
    /// supplies that are below 100% supply or fuel. Transfers are computed,
    /// not executed.
    ///
    /// For each item the hub has, the destinations are sorted by how much
    /// they have (least first) and the hub's stock is handed out round robin
    /// in chunks of 1/8th of what remains, until the stock runs out or all
    /// demand (capacity - stored) is met.
    pub fn deliver_supplies_from_logistics_hubs(&mut self) -> Result<Vec<Transfer>> {
        self.deliver_supplies_to(None)
    }

    /// Like [`Db::deliver_supplies_from_logistics_hubs`], but if `only` is
    /// given, only compute transfers to those objectives.
    fn deliver_supplies_to(&mut self, only: Option<&[ObjectiveId]>) -> Result<Vec<Transfer>> {
        self.update_supply_status()
            .context("updating supply status")?;
        let mut transfers: Vec<Transfer> = vec![];
        for lid in &self.persisted.logistics_hubs {
            let logi = objective!(self, lid)?;
            let mut needed: SmallVec<[Needed; 64]> = logi
                .warehouse
                .destination
                .into_iter()
                .filter(|oid| only.map_or(true, |only| only.contains(oid)))
                .filter_map(|oid| Some((oid, self.persisted.objectives.get(oid)?)))
                .filter(|(_, obj)| logi.owner == obj.owner && (obj.supply < 100 || obj.fuel < 100))
                .map(|(oid, obj)| Needed {
                    oid,
                    obj,
                    demanded: 0,
                    allocated: 0,
                })
                .collect();
            macro_rules! schedule_transfers {
                ($typ:expr, $from:ident, $get:ident) => {
                    for (name, inv) in &logi.warehouse.$from {
                        if inv.stored == 0 {
                            continue;
                        }
                        needed.sort_by(|n0, n1| {
                            let i0 = n0.obj.$get(name);
                            let i1 = n1.obj.$get(name);
                            i0.stored.cmp(&i1.stored)
                        });
                        let mut total_demanded = 0;
                        for n in &mut needed {
                            let inv = n.obj.$get(name);
                            let demanded = if inv.stored <= inv.capacity {
                                inv.capacity - inv.stored
                            } else {
                                0
                            };
                            total_demanded += demanded;
                            n.demanded = demanded;
                            n.allocated = 0;
                        }
                        let mut have = inv.stored;
                        let mut total_filled = 0;
                        while have > 0 && total_filled < total_demanded {
                            for n in &mut needed {
                                if have == 0 {
                                    break;
                                }
                                let allocation = max(1, have >> 3);
                                let amount = min(allocation, n.demanded - n.allocated);
                                n.allocated += amount;
                                total_filled += amount;
                                have -= amount;
                            }
                        }
                        for n in &needed {
                            if n.allocated > 0 {
                                transfers.push(Transfer {
                                    source: *lid,
                                    target: *n.oid,
                                    amount: n.allocated,
                                    item: $typ(name.clone()),
                                })
                            }
                        }
                    }
                };
            }
            schedule_transfers!(TransferItem::Equipment, equipment, get_equipment);
            schedule_transfers!(TransferItem::Liquid, liquids, get_liquids);
        }
        Ok(transfers)
    }

    /// Even out stock between each side's logistics hubs (if it has at least
    /// two), moving items from hubs above the mean to hubs below it. Unlike
    /// the other transfers these are executed immediately.
    fn balance_logistics_hubs(&mut self) -> Result<()> {
        struct Needed<'a> {
            oid: &'a ObjectiveId,
            obj: &'a Objective,
            had: u32,
            have: u32,
        }
        for side in Side::ALL {
            let mut transfers: Vec<Transfer> = vec![];
            macro_rules! schedule_transfers {
                ($typ:expr, $from:ident, $get:ident) => {{
                    let mut needed: SmallVec<[Needed; 16]> = self
                        .persisted
                        .logistics_hubs
                        .into_iter()
                        .filter_map(|lid| {
                            let obj = &self.persisted.objectives[lid];
                            if obj.owner != side {
                                None
                            } else {
                                Some(Needed {
                                    oid: lid,
                                    obj,
                                    had: 0,
                                    have: 0,
                                })
                            }
                        })
                        .collect();
                    if needed.len() < 2 {
                        continue;
                    }
                    let items = needed[0].obj.warehouse.$from.clone();
                    for (name, _) in &items {
                        let mean = {
                            let sum: u32 = needed
                                .iter_mut()
                                .map(|n| {
                                    n.have = n.obj.$get(name).stored;
                                    n.had = n.have;
                                    n.had
                                })
                                .sum();
                            sum / needed.len() as u32
                        };
                        // too little stock to be worth balancing
                        if mean >> 2 == 0 {
                            continue;
                        }
                        // fill the poorest hubs (from the front) by taking from
                        // the richest (from the back)
                        needed.sort_by(|n0, n1| n0.had.cmp(&n1.had));
                        let mut take = needed.len() - 1;
                        for i in 0..needed.len() {
                            if needed[i].have + 1 >= mean {
                                break;
                            }
                            while needed[i].have + 1 < mean {
                                while take > i && needed[take].have <= mean {
                                    take -= 1;
                                }
                                if take == i {
                                    break;
                                }
                                let need = mean - needed[i].have;
                                let available = needed[take].have - mean;
                                let xfer = min(need, available);
                                needed[i].have += xfer;
                                needed[take].have -= xfer;
                                transfers.push(Transfer {
                                    source: *needed[take].oid,
                                    target: *needed[i].oid,
                                    amount: xfer,
                                    item: $typ(name.clone()),
                                });
                            }
                        }
                    }
                }};
            }
            schedule_transfers!(TransferItem::Equipment, equipment, get_equipment);
            schedule_transfers!(TransferItem::Liquid, liquids, get_liquids);
            for tr in transfers.drain(..) {
                tr.execute(&mut self.persisted, &self.ephemeral.to_bg)
                    .with_context(|| format_compact!("executing transfer {:?}", tr))?
            }
            self.ephemeral.dirty();
        }
        self.update_supply_status()?;
        Ok(())
    }

    /// Recompute every objective's `supply` and `fuel` percentages as the
    /// average fill percent of its equipment and liquids (items with zero
    /// capacity are ignored), publishing a stat when they change.
    pub(super) fn update_supply_status(&mut self) -> Result<()> {
        for (_, obj) in self.persisted.objectives.iter_mut_cow() {
            let current_supply = obj.supply;
            let current_fuel = obj.fuel;
            let mut n = 0;
            let mut sum: u32 = 0;
            for (_, inv) in &obj.warehouse.equipment {
                if let Some(pct) = inv.percent() {
                    sum += pct as u32;
                    n += 1;
                }
            }
            obj.supply = if n == 0 { 0 } else { (sum / n) as u8 };
            n = 0;
            sum = 0;
            for (_, inv) in &obj.warehouse.liquids {
                if let Some(pct) = inv.percent() {
                    sum += pct as u32;
                    n += 1;
                }
            }
            obj.fuel = if n == 0 { 0 } else { (sum / n) as u8 };
            if current_supply != obj.supply || current_fuel != obj.fuel {
                self.ephemeral.stat(Stat::ObjectiveSupply {
                    id: obj.id,
                    supply: obj.supply,
                    fuel: obj.fuel,
                });
            }
        }
        self.ephemeral.dirty();
        Ok(())
    }

    /// Read an objective's DCS warehouse into the db. Returns the objective
    /// and the warehouse handle. Fails if the objective has no airbase.
    pub fn sync_warehouse_to_objective<'lua>(
        &mut self,
        lua: MizLua<'lua>,
        oid: ObjectiveId,
    ) -> Result<(&mut Objective, warehouse::Warehouse<'lua>)> {
        let obj = objective_mut!(self, oid)?;
        let airbase = self
            .ephemeral
            .airbase_by_oid
            .get(&oid)
            .ok_or_else(|| anyhow!("no logistics for objective {}", obj.name))?;
        let warehouse = Airbase::get_instance(lua, &airbase)
            .context("getting airbase")?
            .get_warehouse()
            .context("getting warehouse")?;
        sync_warehouse_to_obj(obj, &warehouse).context("syncing warehouse to objective")?;
        Ok((obj, warehouse))
    }

    /// Write an objective's db inventory to its DCS warehouse. Returns the
    /// objective and the warehouse handle. Fails if the objective has no
    /// airbase.
    pub fn sync_objective_to_warehouse<'lua>(
        &mut self,
        lua: MizLua<'lua>,
        oid: ObjectiveId,
    ) -> Result<(&mut Objective, warehouse::Warehouse<'lua>)> {
        let obj = objective_mut!(self, oid)?;
        let airbase = self
            .ephemeral
            .airbase_by_oid
            .get(&oid)
            .ok_or_else(|| anyhow!("no logistics for objective {}", obj.name))?;
        let warehouse = Airbase::get_instance(lua, &airbase)
            .context("getting airbase")?
            .get_warehouse()
            .context("getting warehouse")?;
        sync_obj_to_warehouse(obj, &warehouse).context("syncing warehouse to objective")?;
        Ok((obj, warehouse))
    }

    /// Move supplies between two friendly objectives (a supply transfer
    /// crate). For every item `from` has, move `supply_transfer_size` percent
    /// of its stock (at least 1), limited to the space left at `to`. Both
    /// warehouses are synced from DCS first and written back afterward.
    pub fn transfer_supplies(
        &mut self,
        lua: MizLua,
        from: ObjectiveId,
        to: ObjectiveId,
    ) -> Result<()> {
        if from == to {
            bail!("you can't transfer supplies to the same objective")
        }
        let whcfg = match self.ephemeral.cfg.warehouse.as_ref() {
            Some(whcfg) => whcfg,
            None => return Ok(()),
        };
        let size = whcfg.supply_transfer_size as f32 / 100.;
        let side = objective!(self, from)?.owner;
        if side != objective!(self, to)?.owner {
            bail!("can't transfer supply from an enemy objective")
        }
        let mut transfers: SmallVec<[Transfer; 128]> = smallvec![];
        let (_, from_wh) = self
            .sync_warehouse_to_objective(lua, from)
            .context("syncing from objective")?;
        let (_, to_wh) = self
            .sync_warehouse_to_objective(lua, to)
            .context("syncing to objective")?;
        let from_obj = objective!(self, from)?;
        let to_obj = objective!(self, to)?;
        macro_rules! compute {
            ($src:ident, $typ:ident) => {
                for (name, inv) in &from_obj.warehouse.$src {
                    if inv.stored > 0 {
                        let needed = match to_obj.warehouse.$src.get(name) {
                            None => 0,
                            Some(inv) => {
                                if inv.capacity >= inv.stored {
                                    inv.capacity - inv.stored
                                } else {
                                    0
                                }
                            }
                        };
                        let amount = min(needed, max(1, (inv.stored as f32 * size) as u32));
                        transfers.push(Transfer {
                            amount,
                            source: from,
                            target: to,
                            item: TransferItem::$typ(name.clone()),
                        });
                    }
                }
            };
        }
        compute!(equipment, Equipment);
        compute!(liquids, Liquid);
        for tr in transfers {
            tr.execute(&mut self.persisted, &self.ephemeral.to_bg)?
        }
        sync_obj_to_warehouse(objective!(self, from)?, &from_wh)?;
        sync_obj_to_warehouse(objective!(self, to)?, &to_wh)?;
        self.update_supply_status()
            .context("updating supply status")?;
        Ok(())
    }

    /// Admin command to remove `amount` percent (0-100) of every produced
    /// item from an objective's inventory, syncing with DCS before and after.
    pub fn admin_reduce_inventory(
        &mut self,
        lua: MizLua,
        oid: ObjectiveId,
        amount: u8,
    ) -> Result<()> {
        if amount > 100 {
            bail!("enter a percentage")
        }
        let percent = amount as f32 / 100.;
        let production = match self
            .ephemeral
            .production_by_side
            .get(&objective!(self, oid)?.owner)
        {
            Some(p) => Arc::clone(p),
            None => return Ok(()),
        };
        let (obj, warehouse) = self
            .sync_warehouse_to_objective(lua, oid)
            .with_context(|| format_compact!("syncing warehouses to {oid}"))?;
        for name in production.equipment.keys() {
            if let Some(inv) = obj.warehouse.equipment.get_mut_cow(name) {
                inv.reduce(percent);
            }
        }
        for liq in production.liquids.keys() {
            if let Some(inv) = obj.warehouse.liquids.get_mut_cow(&liq) {
                inv.reduce(percent);
            }
        }
        sync_obj_to_warehouse(obj, &warehouse).context("syncing from warehouse")?;
        self.update_supply_status()
            .context("updating supply status")?;
        self.ephemeral.dirty();
        Ok(())
    }

    /// Admin command to write an objective's inventory to the log, either as
    /// seen by DCS (non zero items only) or as recorded in the db
    /// (stored/capacity).
    pub fn admin_log_inventory(
        &mut self,
        lua: MizLua,
        kind: WarehouseKind,
        oid: ObjectiveId,
    ) -> Result<()> {
        use std::fmt::Write;
        match kind {
            WarehouseKind::DCS => {
                let abid = self
                    .ephemeral
                    .airbase_by_oid
                    .get(&oid)
                    .ok_or_else(|| anyhow!("no airbase for {oid}"))?;
                let wh = Airbase::get_instance(lua, &abid)
                    .context("getting airbase")?
                    .get_warehouse()
                    .context("getting warehouse")?;
                let map =
                    warehouse::Warehouse::get_resource_map(lua).context("getting resource map")?;
                let mut msg = CompactString::new("");
                map.for_each(|name, _| {
                    let qty = wh
                        .get_item_count(name.clone())
                        .with_context(|| format_compact!("getting {name} count from warehouse"))?;
                    if qty > 0 {
                        write!(msg, "{name}, {qty}\n")?
                    }
                    Ok(())
                })?;
                for name in LiquidType::ALL {
                    let qty = wh.get_liquid_amount(name).with_context(|| {
                        format_compact!("getting liquid {:?} from warehouse", name)
                    })?;
                    if qty > 0 {
                        write!(msg, "{:?}, {qty}\n", name)?
                    }
                }
                warn!("{msg}")
            }
            WarehouseKind::Objective => {
                let obj = objective!(self, oid)?;
                let mut msg = CompactString::new("");
                for (name, inv) in &obj.warehouse.equipment {
                    write!(msg, "{name}, {}/{}\n", inv.stored, inv.capacity)?
                }
                for (name, inv) in &obj.warehouse.liquids {
                    write!(msg, "{:?}, {}/{}\n", name, inv.stored, inv.capacity)?
                }
                warn!("{msg}")
            }
        }
        Ok(())
    }
}
