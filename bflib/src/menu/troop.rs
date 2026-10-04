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

//! F10 "Troops" radio menu for transport-capable aircraft.
//!
//! Lets pilots load infantry squads at friendly logistics hubs, unload them
//! into the field as spawned groups, extract deployed squads back aboard, or
//! return them to logistics for a points refund. The real work is done by the
//! `Db` cargo methods (`db/cargo.rs`); these handlers resolve the slot, call
//! into the db, and report the result. Success is announced to the whole
//! side, failures are shown only to the requesting group.
//!
//! Loading and unloading also pin the relevant JTAC id in the slot's JTAC
//! menu (a squad with a JTAC spec makes the carrying slot a JTAC while
//! aboard, and the deployed group a JTAC once unloaded) and rebuild that menu.

use super::{cargo, player_name, slot_for_group, ArgTuple};
use crate::{jtac::JtId, Context};
use anyhow::{Context as ErrContext, Result};
use bfprotocols::cfg::{Cfg, LimitEnforceTyp};
use compact_str::format_compact;
use dcso3::{
    coalition::Side, env::miz::GroupId, mission_commands::MissionCommands, MizLua, String,
};

/// Menu handler: load the squad named `arg.snd` into the aircraft of group `arg.fst`.
///
/// On success the carrying slot is pinned in that slot's JTAC menu (it only
/// becomes a live JTAC if the squad has a JTAC spec) and the logistics
/// objective the troops were loaded from is subscribed. The side is then told
/// how many of this squad type are deployed and what happens when the limit
/// is exceeded.
fn load_troops(lua: MizLua, arg: ArgTuple<GroupId, String>) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = slot_for_group(lua, ctx, &arg.fst).context("getting slot for group")?;
    match ctx.db.load_troops(lua, &slot, &arg.snd) {
        Ok((tr, oid)) => {
            let (n, oldest) = ctx
                .db
                .number_troops_deployed(side, &tr.name)
                .context("getting number of deployed troops")?;
            let player = player_name(&ctx.db, &slot);
            let sub = ctx.subscribed_jtac_menus.entry(slot).or_default();
            sub.pinned.insert(JtId::Slot(slot));
            sub.subscribed_objectives.insert(oid);
            super::jtac::init_jtac_menu_for_slot(ctx, lua, &slot)?;
            let enforce = match tr.limit_enforce {
                LimitEnforceTyp::DenyCrate => {
                    format_compact!("unloading will be denied when the limit is exceeded")
                }
                LimitEnforceTyp::DeleteOldest => match oldest {
                    Some(gid) => {
                        format_compact!(
                            "unloading will delete oldest, {gid}, when the limit is exceeded"
                        )
                    }
                    None => {
                        format_compact!("unloading will delete oldest when the limit is exceeded")
                    }
                },
            };
            let msg = format_compact!(
                "{player} loaded {}\n{n} of {} {} deployed, {}",
                tr.name,
                tr.limit,
                tr.name,
                enforce
            );
            ctx.db.ephemeral.msgs().panel_to_side(10, false, side, msg)
        }
        Err(e) => {
            ctx.db
                .ephemeral
                .msgs()
                .panel_to_group(10, false, arg.fst, format_compact!("{e}"))
        }
    }
    Ok(())
}

/// Menu handler: unload the most recently loaded squad from group `gid`'s
/// aircraft as a new ground group at its position (must be landed).
///
/// The new group is pinned in the slot's JTAC menu (in case it is a JTAC
/// squad) and the nearest objective, if any, is subscribed.
fn unload_troops(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    match ctx.db.unload_troops(lua, &ctx.idx, &slot) {
        Ok((tr, tgid, oid)) => {
            let player = player_name(&ctx.db, &slot);
            let sub = ctx.subscribed_jtac_menus.entry(slot).or_default();
            sub.pinned.insert(JtId::Group(tgid));
            if let Some(oid) = oid {
                sub.subscribed_objectives.insert(oid);
            }
            super::jtac::init_jtac_menu_for_slot(ctx, lua, &slot)?;
            let msg = format_compact!("{player} dropped {} troops into the field", tr.name);
            ctx.db.ephemeral.msgs().panel_to_side(10, false, side, msg)
        }
        Err(e) => ctx
            .db
            .ephemeral
            .msgs()
            .panel_to_group(10, false, gid, format_compact!("{e}")),
    }
    Ok(())
}

/// Menu handler: pick up a friendly deployed squad within crate load
/// distance of group `gid`'s aircraft, deleting the ground group.
fn extract_troops(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    match ctx.db.extract_troops(lua, &slot) {
        Ok(tr) => {
            let player = player_name(&ctx.db, &slot);
            let msg = format_compact!("{player} extracted {} troops from the field", tr.name);
            ctx.db.ephemeral.msgs().panel_to_side(10, false, side, msg)
        }
        Err(e) => ctx
            .db
            .ephemeral
            .msgs()
            .panel_to_group(10, false, gid, format_compact!("{e}")),
    }
    Ok(())
}

/// Menu handler: hand the most recently loaded squad back to friendly
/// logistics, refunding its point cost. Requires being landed near logistics.
fn return_troops(lua: MizLua, gid: GroupId) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let (side, slot) = slot_for_group(lua, ctx, &gid).context("getting slot for group")?;
    match ctx.db.return_troops(lua, &slot) {
        Ok(tr) => {
            let player = player_name(&ctx.db, &slot);
            let msg = format_compact!("{player} returned {} troops", tr.name);
            ctx.db.ephemeral.msgs().panel_to_side(10, false, side, msg)
        }
        Err(e) => ctx
            .db
            .ephemeral
            .msgs()
            .panel_to_group(10, false, gid, format_compact!("{e}")),
    }
    Ok(())
}

/// Build the "Troops" submenu for a player group: Unload/Extract/List/Return
/// commands plus a "Squads" submenu with one Load command per squad
/// configured for `side`. Does nothing if `side` has no troops configured.
pub(super) fn add_troops_menu_for_group(
    cfg: &Cfg,
    mc: &MissionCommands,
    side: &Side,
    group: GroupId,
) -> Result<()> {
    if let Some(squads) = cfg.troops.get(side) {
        let root = mc.add_submenu_for_group(group, "Troops".into(), None)?;
        mc.add_command_for_group(
            group,
            "Unload".into(),
            Some(root.clone()),
            unload_troops,
            group,
        )?;
        mc.add_command_for_group(
            group,
            "Extract".into(),
            Some(root.clone()),
            extract_troops,
            group,
        )?;
        mc.add_command_for_group(
            group,
            "List".into(),
            Some(root.clone()),
            cargo::list_current_cargo,
            group,
        )?;
        mc.add_command_for_group(
            group,
            "Return".into(),
            Some(root.clone()),
            return_troops,
            group,
        )?;
        let root = mc.add_submenu_for_group(group, "Squads".into(), Some(root))?;
        for sq in squads {
            let item = if sq.cost > 0 {
                format_compact!("Load {} squad ({} pts)", sq.name, sq.cost)
            } else {
                format_compact!("Load {} squad", sq.name)
            };
            mc.add_command_for_group(
                group,
                item.into(),
                Some(root.clone()),
                load_troops,
                ArgTuple {
                    fst: group,
                    snd: sq.name.clone(),
                },
            )?;
        }
    }
    Ok(())
}
