//! The F10 "Actions" menu.
//!
//! Actions are side-wide missions configured in the campaign (AI CAP,
//! AWACS, tankers, deployable drops, nukes, logistics repair, etc). Most
//! actions need a target location, which the player supplies by placing an
//! F10 map mark: each of the player's own marks with a unique label of at
//! most 24 characters becomes a menu entry. Waypoint style actions also
//! need a target group, chosen from the side's existing action groups (or
//! deployed groups and troops for `Move`).
//!
//! Because the menu contents depend on the player's current marks, the
//! slot initially only gets an "Actions>>" command; selecting it builds the
//! full menu (`add_action_menu`). If the player adds a mark while the menu
//! is expanded, it is collapsed back to "Actions>>" so it gets rebuilt.

use super::{ArgPent, ArgQuad, ArgTriple};
use crate::{
    Context,
    db::{
        actions::{ActionArgs, ActionCmd, WithObj, WithPos, WithPosAndGroup},
        group::DeployKind,
    },
    spawnctx::SpawnCtx,
};
use anyhow::{Context as ErrContext, Result, anyhow, bail};
use bfprotocols::{
    cfg::{Action, ActionKind},
    db::{group::GroupId as DbGid, objective::ObjectiveId},
    perf::{Perf, PerfInner},
};
use compact_str::format_compact;
use dcso3::{
    LuaVec3, MizLua, String, Vector2, Vector3,
    coalition::Side,
    env::miz::GroupId,
    mission_commands::{GroupCommandItem, GroupSubMenu, MissionCommands},
    net::{SlotId, Ucid},
    object::DcsObject,
    trigger::MarkId,
    world::World,
};
use fxhash::FxHashMap;
use std::sync::Arc;

/// Start an action for a player. On success the mark that targeted it (if
/// any) is deleted and the player's action menu is collapsed back to
/// "Actions>>".
fn run_action(
    ctx: &mut Context,
    perf: &mut PerfInner,
    lua: MizLua,
    side: Side,
    slot: SlotId,
    ucid: Ucid,
    mark: Option<MarkId>,
    cmd: ActionCmd,
) -> Result<()> {
    let spctx = SpawnCtx::new(lua)?;
    ctx.db.start_action(
        lua,
        perf,
        &spctx,
        &ctx.idx,
        &ctx.jtac,
        side,
        Some(ucid),
        cmd,
    )?;
    if let Some(mark) = mark {
        ctx.db.ephemeral.msgs().delete_mark(mark);
    }
    init_action_menu_for_slot(ctx, lua, &slot, &ucid)
}

/// Start an action that targets a map position (`pos`, from the mark).
/// Fails if the action kind doesn't take just a position.
fn do_pos_action(
    ctx: &mut Context,
    perf: &mut PerfInner,
    lua: MizLua,
    side: Side,
    slot: SlotId,
    ucid: Ucid,
    name: String,
    pos: LuaVec3,
    mark: MarkId,
    action: Action,
) -> Result<()> {
    // DCS 3d (x, alt, z) to the 2d map plane
    let pos = Vector2::new(pos.0.x, pos.0.z);
    let args = match &action.kind {
        ActionKind::Attackers(cfg) => ActionArgs::Attackers(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Sead(cfg) => ActionArgs::Sead(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Awacs(cfg) => ActionArgs::Awacs(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::CruiseMissileSpawn(cfg) => ActionArgs::CruiseMissileSpawn(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Deployable(cfg) => ActionArgs::Deployable(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Drone(cfg) => ActionArgs::Drone(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Fighters(cfg) => ActionArgs::Fighters(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Tanker(cfg) => ActionArgs::Tanker(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Paratrooper(cfg) => ActionArgs::Paratrooper(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Nuke(cfg) => ActionArgs::Nuke(WithPos {
            cfg: cfg.clone(),
            pos,
        }),
        ActionKind::Bomber(_)
        | ActionKind::LogisticsTransfer(_)
        | ActionKind::LogisticsRepair(_)
        | ActionKind::Move(_)
        | ActionKind::Rtb
        | ActionKind::TankerWaypoint
        | ActionKind::CruiseMissileWaypoint
        | ActionKind::AwacsWaypoint
        | ActionKind::FighersWaypoint
        | ActionKind::DroneWaypoint
        | ActionKind::AttackersWaypoint
        | ActionKind::SeadWaypoint => bail!("invalid action type for this menu item"),
    };
    let cmd = ActionCmd { name, action, args };
    run_action(ctx, perf, lua, side, slot, ucid, Some(mark), cmd)
}

/// Look up the player's side, current slot, and the action called `name`
/// for their side. Fails if the player, slot, or action doesn't exist.
fn side_slot_action(ctx: &mut Context, ucid: &Ucid, name: &str) -> Result<(Side, SlotId, Action)> {
    let player = ctx
        .db
        .player(ucid)
        .ok_or_else(|| anyhow!("no such player"))?;
    let side = player.side;
    let slot = player
        .current_slot
        .as_ref()
        .map(|(slot, _)| *slot)
        .ok_or_else(|| anyhow!("missing slot"))?;
    let action = ctx
        .db
        .ephemeral
        .cfg
        .actions
        .get(&side)
        .ok_or_else(|| anyhow!("missions actions for {side}"))?
        .get(name)
        .ok_or_else(|| anyhow!("missing action {}", name))?
        .clone();
    Ok((side, slot, action))
}

/// Menu callback for position actions. `arg` is (player, action name, mark
/// position, mark id). The outcome is reported to the player in a 10 second
/// message panel.
fn run_pos_action(lua: MizLua, arg: ArgQuad<Ucid, String, LuaVec3, MarkId>) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let perf = Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner);
    let (side, slot, action) = side_slot_action(ctx, &arg.fst, &arg.snd)?;
    match do_pos_action(
        ctx,
        perf,
        lua,
        side,
        slot,
        arg.fst,
        arg.snd.clone(),
        arg.trd,
        arg.fth,
        action,
    ) {
        Ok(()) => ctx.db.ephemeral.panel_to_player(
            &ctx.db.persisted,
            10,
            &arg.fst,
            format_compact!("action {} started", arg.snd),
        ),
        Err(e) => ctx.db.ephemeral.panel_to_player(
            &ctx.db.persisted,
            10,
            &arg.fst,
            format_compact!("could not start {}, {e:?}", arg.snd),
        ),
    }
    Ok(())
}

/// Start an action that targets an existing `group` and a map position,
/// e.g. sending a tanker to a new waypoint or moving deployed units. Fails
/// if the action kind doesn't take a position and group.
fn do_pos_group_action(
    ctx: &mut Context,
    perf: &mut PerfInner,
    lua: MizLua,
    side: Side,
    slot: SlotId,
    ucid: Ucid,
    name: String,
    pos: LuaVec3,
    group: DbGid,
    mark: MarkId,
    action: Action,
) -> Result<()> {
    let pos = Vector2::new(pos.0.x, pos.0.z);
    let args = match &action.kind {
        ActionKind::TankerWaypoint => ActionArgs::TankerWaypoint(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::AwacsWaypoint => ActionArgs::AwacsWaypoint(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::CruiseMissileWaypoint => ActionArgs::CruiseMissileWaypoint(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::FighersWaypoint => ActionArgs::FightersWaypoint(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::DroneWaypoint => ActionArgs::DroneWaypoint(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::AttackersWaypoint => ActionArgs::AttackersWaypoint(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::SeadWaypoint => ActionArgs::SeadWaypoint(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::Move(cfg) => ActionArgs::Move(WithPosAndGroup {
            cfg: cfg.clone(),
            pos,
            group,
        }),
        ActionKind::Rtb => ActionArgs::Rtb(WithPosAndGroup {
            cfg: (),
            pos,
            group,
        }),
        ActionKind::Attackers(_)
        | ActionKind::Sead(_)
        | ActionKind::Awacs(_)
        | ActionKind::Deployable(_)
        | ActionKind::CruiseMissileSpawn(_)
        | ActionKind::Drone(_)
        | ActionKind::Fighters(_)
        | ActionKind::Tanker(_)
        | ActionKind::Paratrooper(_)
        | ActionKind::Nuke(_)
        | ActionKind::Bomber(_)
        | ActionKind::LogisticsTransfer(_)
        | ActionKind::LogisticsRepair(_) => bail!("invalid action type for this menu item"),
    };
    let cmd = ActionCmd { name, action, args };
    run_action(ctx, perf, lua, side, slot, ucid, Some(mark), cmd)
}

/// Menu callback for position + group actions. `arg` is (player, action
/// name, mark position, target group, mark id). The outcome is reported to
/// the player in a 10 second message panel.
fn run_pos_group_action(
    lua: MizLua,
    arg: ArgPent<Ucid, String, LuaVec3, DbGid, MarkId>,
) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let perf = Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner);
    let (side, slot, action) = side_slot_action(ctx, &arg.fst, &arg.snd)?;
    match do_pos_group_action(
        ctx,
        perf,
        lua,
        side,
        slot,
        arg.fst,
        arg.snd.clone(),
        arg.trd,
        arg.fth,
        arg.pnt,
        action,
    ) {
        Ok(()) => ctx.db.ephemeral.panel_to_player(
            &ctx.db.persisted,
            10,
            &arg.fst,
            format_compact!("action {} started for {}", arg.snd, arg.fth),
        ),
        Err(e) => ctx.db.ephemeral.panel_to_player(
            &ctx.db.persisted,
            10,
            &arg.fst,
            format_compact!("could not start {} for {}, {e:?}", arg.snd, arg.fth),
        ),
    }
    Ok(())
}

/// Start an action that targets an objective (currently only logistics
/// repair). Fails for any other action kind.
fn do_objective_action(
    ctx: &mut Context,
    perf: &mut PerfInner,
    lua: MizLua,
    side: Side,
    slot: SlotId,
    ucid: Ucid,
    name: String,
    oid: ObjectiveId,
    action: Action,
) -> Result<()> {
    let args = match &action.kind {
        ActionKind::LogisticsRepair(cfg) => ActionArgs::LogisticsRepair(WithObj {
            cfg: cfg.clone(),
            oid,
        }),
        ActionKind::TankerWaypoint
        | ActionKind::AwacsWaypoint
        | ActionKind::CruiseMissileWaypoint
        | ActionKind::FighersWaypoint
        | ActionKind::DroneWaypoint
        | ActionKind::AttackersWaypoint
        | ActionKind::Attackers(_)
        | ActionKind::Sead(_)
        | ActionKind::Awacs(_)
        | ActionKind::CruiseMissileSpawn(_)
        | ActionKind::Deployable(_)
        | ActionKind::Drone(_)
        | ActionKind::Fighters(_)
        | ActionKind::Tanker(_)
        | ActionKind::Paratrooper(_)
        | ActionKind::Nuke(_)
        | ActionKind::Bomber(_)
        | ActionKind::LogisticsTransfer(_)
        | ActionKind::Rtb
        | ActionKind::Move(_)
        | ActionKind::SeadWaypoint => bail!("invalid action type for this menu item"),
    };
    let cmd = ActionCmd { name, action, args };
    run_action(ctx, perf, lua, side, slot, ucid, None, cmd)
}

/// Menu callback for objective actions. `arg` is (player, action name,
/// objective). The outcome is reported to the player in a message panel.
fn run_objective_action(lua: MizLua, arg: ArgTriple<Ucid, String, ObjectiveId>) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let perf = Arc::make_mut(&mut unsafe { Perf::get_mut() }.inner);
    let (side, slot, action) = side_slot_action(ctx, &arg.fst, &arg.snd)?;
    match do_objective_action(
        ctx,
        perf,
        lua,
        side,
        slot,
        arg.fst,
        arg.snd.clone(),
        arg.trd,
        action,
    ) {
        Ok(()) => ctx.db.ephemeral.panel_to_player(
            &ctx.db.persisted,
            10,
            &arg.fst,
            format_compact!("action {} started", arg.snd),
        ),
        Err(e) => ctx.db.ephemeral.panel_to_player(
            &ctx.db.persisted,
            10,
            &arg.fst,
            format_compact!("could not start {}, {e:?}", arg.snd),
        ),
    }
    Ok(())
}

/// Callback for the "Actions>>" command: replace it with the full Actions
/// submenu for the player. `arg` is (player, miz group, slot).
///
/// Each configured action for the player's side gets a submenu (titled with
/// its point cost, if any) listing valid targets: the player's marks,
/// target groups then marks, or friendly objectives, depending on the
/// action kind. Bomber and logistics transfer actions are not offered here.
/// DCS menus hold a limited number of items, so every level is paginated
/// with "Next>>" submenus after 8 entries.
fn add_action_menu(lua: MizLua, arg: ArgTriple<Ucid, GroupId, SlotId>) -> Result<()> {
    let ctx = unsafe { Context::get_mut() };
    let mc = MissionCommands::singleton(lua)?;
    let world = World::singleton(lua)?;
    mc.remove_command_for_group(arg.snd, vec!["Actions>>".into()].into())?;
    let mut root = mc.add_submenu_for_group(arg.snd, "Actions".into(), None)?;
    let player = ctx
        .db
        .player(&arg.fst)
        .ok_or_else(|| anyhow!("unknown player"))?;
    let actions = ctx
        .db
        .ephemeral
        .cfg
        .actions
        .get(&player.side)
        .ok_or_else(|| anyhow!("no actions for {}", player.side))?;
    /// a player mark, keyed by its text in `marks`
    struct Mk {
        id: MarkId,
        pos: Vector3,
        /// how many of the player's marks have this text
        count: usize,
    }
    let mut marks: FxHashMap<String, Mk> = FxHashMap::default();
    for mk in world.get_mark_panels()? {
        let mk = mk?;
        if let Some(unit) = mk.initiator.as_ref() {
            let id = unit.object_id()?;
            if let Some(ucid) = ctx.db.player_in_unit(false, &id) {
                if ucid == arg.fst && mk.text.len() <= 24 {
                    marks
                        .entry(mk.text.clone())
                        .or_insert_with(|| Mk {
                            id: mk.id,
                            pos: mk.pos.0,
                            count: 0,
                        })
                        .count += 1;
                }
            }
        }
    }
    // marks with duplicate text would be ambiguous menu entries
    marks.retain(|_, mk| mk.count == 1);
    let add_pos = |root: GroupSubMenu, name: String| -> Result<()> {
        for (text, mk) in &marks {
            mc.add_command_for_group(
                arg.snd,
                text.clone(),
                Some(root.clone()),
                run_pos_action,
                ArgQuad {
                    fst: arg.fst,
                    snd: name.clone(),
                    trd: LuaVec3(mk.pos),
                    fth: mk.id,
                },
            )?;
        }
        Ok(())
    };
    // if `action` the candidate groups are the side's action groups,
    // otherwise its deployed groups and troops
    let add_pos_group = |mut root: GroupSubMenu, name: String, action: bool| -> Result<()> {
        let iter: Box<dyn Iterator<Item = &DbGid>> = if action {
            Box::new(ctx.db.persisted.actions.into_iter())
        } else {
            Box::new(
                ctx.db
                    .persisted
                    .deployed
                    .into_iter()
                    .chain(ctx.db.persisted.troops.into_iter()),
            )
        };
        let mut n = 0;
        for gid in iter {
            if n >= 8 {
                root = mc.add_submenu_for_group(arg.snd, "Next>>".into(), Some(root))?;
                n = 0;
            }
            let group = ctx.db.group(gid)?;
            if group.side != player.side {
                continue;
            }
            let key = match &group.origin {
                DeployKind::Action { name, .. } => {
                    if action {
                        Some(name.clone())
                    } else {
                        None
                    }
                }
                DeployKind::Deployed { spec, .. } => {
                    if !action {
                        Some(spec.path.last().unwrap().clone())
                    } else {
                        None
                    }
                }
                DeployKind::Troop { spec, .. } => {
                    if !action {
                        Some(format_compact!("{} Troop", spec.name).into())
                    } else {
                        None
                    }
                }
                DeployKind::Crate { .. }
                | DeployKind::Objective { .. }
                | DeployKind::ObjectiveDeprecated => None,
            };
            if let Some(key) = key {
                let root = mc.add_submenu_for_group(
                    arg.snd,
                    format_compact!("{gid}({key})").into(),
                    Some(root.clone()),
                )?;
                for (text, mk) in &marks {
                    mc.add_command_for_group(
                        arg.snd,
                        text.clone(),
                        Some(root.clone()),
                        run_pos_group_action,
                        ArgPent {
                            fst: arg.fst,
                            snd: name.clone(),
                            trd: LuaVec3(mk.pos),
                            fth: *gid,
                            pnt: mk.id,
                        },
                    )?;
                }
            }
            n += 1;
        }
        Ok(())
    };
    let add_objective = |mut root: GroupSubMenu, name: String| -> Result<()> {
        let mut n = 0;
        for (oid, obj) in ctx.db.objectives() {
            if obj.owner == player.side {
                if n >= 8 {
                    root = mc.add_submenu_for_group(arg.snd, "Next>>".into(), Some(root))?;
                    n = 0;
                }
                mc.add_command_for_group(
                    arg.snd,
                    obj.name.clone(),
                    Some(root.clone()),
                    run_objective_action,
                    ArgTriple {
                        fst: arg.fst,
                        snd: name.clone(),
                        trd: *oid,
                    },
                )?;
                n += 1;
            }
        }
        Ok(())
    };
    let mut n = 0;
    for (name, action) in actions {
        if n >= 8 {
            root = mc.add_submenu_for_group(arg.snd, "Next>>".into(), Some(root))?;
            n = 0;
        }
        let title = if action.cost > 0 {
            String::from(format_compact!("{name}({} pts)", action.cost))
        } else {
            name.clone()
        };
        match &action.kind {
            ActionKind::Bomber(_) | ActionKind::LogisticsTransfer(_) => (),
            ActionKind::AttackersWaypoint
            | ActionKind::SeadWaypoint
            | ActionKind::AwacsWaypoint
            | ActionKind::Rtb
            | ActionKind::CruiseMissileWaypoint
            | ActionKind::FighersWaypoint
            | ActionKind::TankerWaypoint
            | ActionKind::DroneWaypoint => {
                let root = mc.add_submenu_for_group(arg.snd, title, Some(root.clone()))?;
                add_pos_group(root.clone(), name.clone(), true)?
            }
            ActionKind::Move(_) => {
                let root = mc.add_submenu_for_group(arg.snd, title, Some(root.clone()))?;
                add_pos_group(root.clone(), name.clone(), false)?
            }
            ActionKind::Attackers(_)
            | ActionKind::Sead(_)
            | ActionKind::Awacs(_)
            | ActionKind::Deployable(_)
            | ActionKind::CruiseMissileSpawn(_)
            | ActionKind::Drone(_)
            | ActionKind::Fighters(_)
            | ActionKind::Tanker(_)
            | ActionKind::Paratrooper(_)
            | ActionKind::Nuke(_) => {
                let root = mc.add_submenu_for_group(arg.snd, title, Some(root.clone()))?;
                add_pos(root.clone(), name.clone())?
            }
            ActionKind::LogisticsRepair(_) => {
                let root = mc.add_submenu_for_group(arg.snd, title, Some(root.clone()))?;
                add_objective(root.clone(), name.clone())?
            }
        }
        n += 1;
    }
    // the menu is now expanded, so it must be reset when new marks appear
    ctx.subscribed_action_menus.insert(arg.trd);
    Ok(())
}

/// Reset the slot's action menu to the single collapsed "Actions>>"
/// command, which builds the full menu when selected.
pub(crate) fn init_action_menu_for_slot(
    ctx: &mut Context,
    lua: MizLua,
    slot: &SlotId,
    ucid: &Ucid,
) -> Result<()> {
    let mc = MissionCommands::singleton(lua)?;
    let si = ctx
        .db
        .ephemeral
        .get_slot_info(slot)
        .context("getting slot info")?;
    ctx.subscribed_action_menus.remove(slot);
    mc.remove_command_for_group(si.miz_gid, GroupCommandItem::from(vec!["Actions>>".into()]))?;
    mc.remove_submenu_for_group(si.miz_gid, GroupSubMenu::from(vec!["Actions".into()]))?;
    mc.add_command_for_group(
        si.miz_gid,
        "Actions>>".into(),
        None,
        add_action_menu,
        ArgTriple {
            fst: *ucid,
            snd: si.miz_gid,
            trd: *slot,
        },
    )?;
    Ok(())
}
