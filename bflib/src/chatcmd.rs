//! Chat command processing.
//!
//! Players interact with the campaign by typing commands into DCS chat
//! (e.g. `blue`, `-lives`, `-jtac 1 smoke`). [`process`] is the entry point
//! called from the chat hook; it dispatches each message to the matching
//! `*_command` handler. Replies are sent privately to the issuing player via
//! the message queue.
//!
//! Commands that need the mission scripting environment ([`MizLua`]) rather
//! than the hooks environment ([`HooksLua`]) can't be executed directly from
//! the chat hook. Those (actions and jtac commands) are queued on the
//! [`Context`] and later executed by [`run_action_commands`] and
//! [`run_jtac_commands`].

use crate::{
    Context,
    admin::{self, AdminCommand, Caller},
    bg::Task,
    db::{actions::ActionCmd, group::DeployKind, player::RegErr},
    jtac::JtId,
    lives,
    menu::{self, ArgQuad, ArgTriple, ArgTuple},
    msgq::MsgTyp,
    spawnctx::SpawnCtx,
};
use anyhow::{Context as ErrContext, Result, anyhow, bail};
use bfprotocols::{
    cfg::{Action, ActionKind},
    db::group::GroupId,
    perf::PerfInner,
    stats::Stat,
};
use chrono::{Duration, prelude::*};
use compact_str::{CompactString, format_compact};
use dcso3::{
    HooksLua, MizLua, String,
    coalition::Side,
    net::{Net, PlayerId},
};
use fxhash::FxBuildHasher;
use indexmap::IndexMap;
use log::{error, info};
use netidx::utils::Either;
use regex::Regex;
use smallvec::{SmallVec, smallvec};
use std::{mem, sync::Arc, sync::OnceLock};

/// Notify a player that they joined `side`, and announce it to everyone.
pub(crate) fn register_success(ctx: &mut Context, id: PlayerId, name: String, side: Side) {
    let msg = String::from(format_compact!(
        "Welcome to the {:?} team. You may only occupy slots belonging to your team. Good luck!",
        side
    ));
    ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
    ctx.db.ephemeral.msgs().send(
        MsgTyp::Chat(None),
        format_compact!("{} has joined {:?} team", name, side),
    );
}

/// Tell a player they tried to join the side they are already on.
pub(crate) fn register_already_on(ctx: &mut Context, id: PlayerId, side: Side) {
    ctx.db.ephemeral.msgs().send(
        MsgTyp::Chat(Some(id)),
        format_compact!("you are already on {:?} team!", side),
    )
}

/// Handle the `blue` / `red` chat commands, registering the player on a side.
///
/// If the player is already registered on the other side, they are told how
/// many side switches (if any) they have left and how to use `-switch`.
fn register_player(ctx: &mut Context, lua: HooksLua, id: PlayerId, msg: String) -> Result<String> {
    let ifo = ctx.connected.get_or_lookup_player_info(lua, id)?;
    let name = ifo.name.clone();
    let side = if msg.eq_ignore_ascii_case("blue") {
        Side::Blue
    } else if msg.eq_ignore_ascii_case("red") {
        Side::Red
    } else {
        bail!("side \"{msg}\" is not blue or red")
    };
    match ctx
        .db
        .register_player(ifo.ucid.clone(), ifo.name.clone(), side)
    {
        Ok(()) => register_success(ctx, id, name, side),
        Err(RegErr::AlreadyOn(side)) => register_already_on(ctx, id, side),
        Err(RegErr::AlreadyRegistered(side_switches, orig_side)) => {
            // side_switches is None when switching is unlimited, otherwise
            // it is the number of switches the player has remaining
            let msg = String::from(match side_switches {
                None => format_compact!(
                    "You are already on the {:?} team. You may switch sides by typing -switch {:?}.",
                    orig_side,
                    side
                ),
                Some(0) => format_compact!(
                    "You are already on {:?} team, and you may not switch sides.",
                    orig_side
                ),
                Some(1) => format_compact!(
                    "You are already on {:?} team. You may sitch sides 1 time by typing -switch {:?}.",
                    orig_side,
                    side
                ),
                Some(n) => format_compact!(
                    "You are already on {:?} team. You may switch sides {n} times. Type -switch {:?}.",
                    orig_side,
                    side
                ),
            });
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
        }
    }
    Ok("".into())
}

/// Announce to everyone that a player has switched to `side`.
pub(crate) fn sideswitch_success(ctx: &mut Context, name: String, side: Side) {
    let msg = String::from(format_compact!("{} has switched to {:?}", name, side));
    ctx.db.ephemeral.msgs().send(MsgTyp::Chat(None), msg);
}

/// Handle `-switch blue` / `-switch red`. The player must be in spectators,
/// and the db enforces any side switch limits configured for the campaign.
fn sideswitch_player(
    ctx: &mut Context,
    lua: HooksLua,
    id: PlayerId,
    msg: String,
) -> Result<String> {
    let ifo = ctx.connected.get_or_lookup_player_info(lua, id)?;
    // switching while occupying a slot would leave the player in an enemy aircraft
    let (_, slot) = Net::singleton(lua)?.get_slot(id)?;
    if !slot.is_spectator() {
        bail!("you must be in spectators to switch sides")
    }
    let side = if msg.eq_ignore_ascii_case("-switch blue") {
        Side::Blue
    } else if msg.eq_ignore_ascii_case("-switch red") {
        Side::Red
    } else {
        bail!("side must be blue or red \"{msg}\"");
    };
    match ctx.db.sideswitch_player(&ifo.ucid, side) {
        Ok(()) => {
            let name = ifo.name.clone();
            sideswitch_success(ctx, name, side);
        }
        Err(e) => ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), e),
    }
    Ok("".into())
}

/// Handle `-lives`, reporting the player's remaining lives.
fn lives_command(ctx: &mut Context, id: PlayerId) -> Result<()> {
    let ifo = ctx
        .connected
        .get(&id)
        .ok_or_else(|| anyhow!("missing info for player {:?}", id))?;
    let msg = lives(&mut ctx.db, &ifo.ucid, None)?;
    ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg);
    Ok(())
}

/// Handle `-admin <command>`.
///
/// Non admins are silently ignored so the command's existence isn't revealed.
/// Parsed commands are queued on the context and executed later by the admin
/// module; `help` is answered immediately.
fn admin_command(ctx: &mut Context, id: PlayerId, cmd: &str) {
    let ifo = match ctx.connected.get(&id) {
        Some(ifo) => ifo,
        None => return,
    };
    if !ctx.db.ephemeral.cfg.admins.contains_key(&ifo.ucid) {
        return;
    }
    match cmd.parse::<AdminCommand>() {
        Err(e) => ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            format_compact!("parse error {:?}", e),
        ),
        Ok(AdminCommand::Help) => {
            for cmd in AdminCommand::help() {
                ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), *cmd);
            }
        }
        Ok(cmd) => {
            info!("queueing admin command {:?} from {:?}", cmd, ifo);
            ctx.admin_commands.push((Caller::Player(id), cmd))
        }
    }
}

/// Format a duration as `HH:MM:SS`. Hours are not wrapped at 24.
pub(super) fn format_duration(d: Duration) -> CompactString {
    let hrs = d.num_hours();
    let min = d.num_minutes() - hrs * 60;
    let sec = d.num_seconds() - hrs * 3600 - min * 60;
    format_compact!("{:02}:{:02}:{:02}", hrs, min, sec)
}

/// Handle `-time`, reporting how long until the scheduled server shutdown.
fn time_command(ctx: &mut Context, id: PlayerId, now: DateTime<Utc>) {
    match ctx.shutdown.as_ref() {
        None => ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            "The server isn't configured to restart automatically",
        ),
        Some(asd) => {
            let remains = format_duration(asd.when - now);
            ctx.db.ephemeral.msgs().send(
                MsgTyp::Chat(Some(id)),
                format_compact!("The server will shutdown in {remains}"),
            )
        }
    }
}

/// Handle `-balance`, reporting the player's points. Unregistered players get no reply.
fn balance_command(ctx: &mut Context, id: PlayerId) {
    if let Some(ifo) = ctx.connected.get(&id) {
        if let Some(player) = ctx.db.player(&ifo.ucid) {
            let points = player.points;
            ctx.db.ephemeral.msgs().send(
                MsgTyp::Chat(Some(id)),
                format_compact!("You have {points} points"),
            );
        }
    }
}

/// Handle `-transfer <amount> <target>`.
///
/// `target` is either a player name, or `objective:<name>` to transfer points
/// into an objective (airbase) instead of to another player.
fn transfer_command(ctx: &mut Context, id: PlayerId, s: &str) {
    // send a private chat reply to the issuing player
    macro_rules! reply {
        ($msg:tt) => {
            ctx.db
                .ephemeral
                .msgs()
                .send(MsgTyp::Chat(Some(id)), format_compact!($msg))
        };
    }
    if let Some(ifo) = ctx.connected.get(&id) {
        match s.split_once(" ") {
            None => reply!("transfer expected amount and target"),
            Some((amount, target)) => match amount.parse::<u32>() {
                Err(e) => reply!("transfer expected a number {e:?}"),
                Ok(amount) => match target.strip_prefix("objective:") {
                    Some(objective_name) => match admin::get_airbase(&ctx.db, objective_name) {
                        Err(e) => reply!("could not transfer to {objective_name}, {e:?}"),
                        Ok(oid) => {
                            match ctx
                                .db
                                .transfer_points(&ifo.ucid, Either::Right(oid), amount)
                            {
                                Err(e) => reply!("transfer failed {e:?}"),
                                Ok(()) => reply!("transfer complete"),
                            }
                        }
                    },
                    None => match admin::get_player_ucid(ctx, target) {
                        Err(e) => reply!("could not transfer to {target}, {e:?}"),
                        Ok(ucid) => {
                            match ctx
                                .db
                                .transfer_points(&ifo.ucid, Either::Left(&ucid), amount)
                            {
                                Err(e) => reply!("transfer failed {e:?}"),
                                Ok(()) => reply!("transfer complete"),
                            }
                        }
                    },
                },
            },
        }
    }
}

/// Handle `-delete <groupid>`, removing a group the player deployed.
///
/// Only groups the player themselves deployed may be deleted. Crates are
/// deleted without a refund. Deployed units and troops refund half their cost
/// (rounded up), either directly to the player or, if the group was paid for
/// from an objective's points, back through that objective.
fn delete_command(ctx: &mut Context, id: PlayerId, s: &str) {
    // send a private chat reply to the issuing player
    macro_rules! reply {
        ($msg:tt) => {
            ctx.db
                .ephemeral
                .msgs()
                .send(MsgTyp::Chat(Some(id)), format_compact!($msg))
        };
    }
    if let Some(ifo) = ctx.connected.get(&id) {
        match s.parse::<GroupId>() {
            Err(e) => reply!("delete expected a group id {e:?}"),
            Ok(id) => match ctx.db.group(&id) {
                Err(e) => reply!("could not get group {id} {e:?}"),
                Ok(group) => match &group.origin {
                    // ownership check first, the arms below can then assume
                    // the player owns the group
                    DeployKind::Crate { player, .. }
                    | DeployKind::Deployed { player, .. }
                    | DeployKind::Troop { player, .. }
                        if player != &ifo.ucid =>
                    {
                        reply!("group {id} wasn't deployed by you")
                    }
                    DeployKind::Action { .. } => reply!("can't delete an action group"),
                    DeployKind::Objective { .. } | DeployKind::ObjectiveDeprecated => {
                        reply!("can't delete an objective group")
                    }
                    DeployKind::Crate { .. } => match ctx.db.delete_group(&id) {
                        Err(e) => reply!("could not delete group {id} {e:?}"),
                        Ok(()) => reply!("deleted {id}"),
                    },
                    DeployKind::Deployed {
                        player,
                        spec,
                        moved_by: _,
                        cost_fraction,
                        origin,
                    } => {
                        // copy out what we need, since deleting the group
                        // invalidates the borrow of `group`
                        let player = player.clone();
                        let points = (spec.cost as f32 / 2.).ceil() as i32;
                        let cost_fraction = *cost_fraction;
                        let origin = *origin;
                        match ctx.db.delete_group(&id) {
                            Err(e) => reply!("could not delete group {id} {e:?}"),
                            Ok(()) => match origin {
                                // paid for by the player, refund them directly
                                None => {
                                    ctx.db.adjust_points(
                                        &player,
                                        points,
                                        &format_compact!("reclaimed {id}"),
                                    );
                                    reply!("deleted {id}")
                                }
                                // paid for from an objective, refund via the objective
                                Some(oid) => {
                                    ctx.db.refund_points(
                                        &player,
                                        oid,
                                        points as u32,
                                        cost_fraction,
                                        &format_compact!("reclaimed {id}"),
                                    );
                                    reply!("deleted {id}")
                                }
                            },
                        }
                    }
                    DeployKind::Troop {
                        player,
                        spec,
                        moved_by: _,
                        origin,
                        cost_fraction,
                    } => {
                        let player = player.clone();
                        let points = (spec.cost as f32 / 2.).ceil() as i32;
                        let cost_fraction = *cost_fraction;
                        let origin = *origin;
                        match ctx.db.delete_group(&id) {
                            Err(e) => reply!("could not delete group {id} {e:?}"),
                            Ok(()) => match origin {
                                None => {
                                    ctx.db.adjust_points(
                                        &player,
                                        points,
                                        &format_compact!("reclaimed {id}"),
                                    );
                                    reply!("deleted {id}")
                                }
                                Some(oid) => {
                                    ctx.db.refund_points(
                                        &player,
                                        oid,
                                        points as u32,
                                        cost_fraction,
                                        &format_compact!("reclaimed {id}"),
                                    );
                                    reply!("deleted {id}")
                                }
                            },
                        }
                    }
                },
            },
        }
    }
}

/// Send the player a usage line for each action available to their side.
///
/// In the usage strings `<key>` is the text of a map mark point, and `<group>`
/// is the id of a group previously spawned by an action.
fn action_help(ctx: &mut Context, actions: &IndexMap<String, Action, FxBuildHasher>, id: PlayerId) {
    for (name, action) in actions {
        let msg = match &action.kind {
            ActionKind::Attackers(_) => Some(format_compact!(
                "{name}: <key> | Spawn ai attackers. cost {}",
                action.cost
            )),
            ActionKind::AttackersWaypoint => Some(format_compact!(
                "{name}: <group> <key> | Move ai attackers. cost {}",
                action.cost
            )),
            ActionKind::Sead(_) => Some(format_compact!(
                "{name}: <key> | Spawn ai sead units. cost {}",
                action.cost
            )),
            ActionKind::SeadWaypoint => Some(format_compact!(
                "{name}: <group> <key> | Move ai sead units. cost {}",
                action.cost
            )),
            ActionKind::Move(_) => Some(format_compact!(
                "{name}: <group> <key> | Move a ground unit. cost {}",
                action.cost
            )),
            ActionKind::Rtb => Some(format_compact!(
                "{name}: <group> <key> | RTB an air asset manually. cost {}",
                action.cost
            )),
            ActionKind::Awacs(_) => Some(format_compact!(
                "{name}: <key> | Spawn an awacs at key, a mark point. cost {}",
                action.cost
            )),
            ActionKind::AwacsWaypoint => Some(format_compact!(
                "{name}: <group> <key> | Move an awacs to key, a mark point. Group is the awacs group. cost {}",
                action.cost
            )),
            // bombers are called via -jtac <id> bomber, not directly
            ActionKind::Bomber(_) => None,
            ActionKind::CruiseMissileSpawn(_) => Some(format_compact!(
                "{name}: <key> | Spawn a cruise missile bomber at key, a mark point. cost {}",
                action.cost
            )),
            ActionKind::CruiseMissileWaypoint => Some(format_compact!(
                "{name}: <group> <key> | Move a cruise missile bomber to key, a mark point. Group is the bomber group. cost {}",
                action.cost
            )),
            ActionKind::Deployable(d) => Some(format_compact!(
                "{name}: <key> | Ai deploy a {} at key a mark point. cost {}",
                d.name,
                action.cost
            )),
            ActionKind::Drone(_) => Some(format_compact!(
                "{name}: <key> | Spawn a drone at key a mark point. cost {}",
                action.cost
            )),
            ActionKind::DroneWaypoint => Some(format_compact!(
                "{name}: <group> <key> | Move a drone to key, a mark point. Group is the drone group. cost {}",
                action.cost
            )),
            ActionKind::FighersWaypoint => Some(format_compact!(
                "{name}: <group> <key> | Move an a figher group to key, a mark point. Group is the fighter group. cost {}",
                action.cost
            )),
            ActionKind::Fighters(_) => Some(format_compact!(
                "{name}: <key> | Spawn ai fighters at key, a mark point. cost {}",
                action.cost
            )),
            ActionKind::LogisticsRepair(_) => Some(format_compact!(
                "{name}: <objective> | Start a logistics repair mission to objective. cost {}",
                action.cost
            )),
            ActionKind::LogisticsTransfer(_) => Some(format_compact!(
                "{name}: <from> <to> | Start a logistics transfer mission between from and to. cost {}",
                action.cost
            )),
            ActionKind::Nuke(_) => Some(format_compact!(
                "{name}: <key> | Nuke key, a mark point. cost {}",
                action.cost
            )),
            ActionKind::Paratrooper(d) => Some(format_compact!(
                "{name}: <key> | Drop {} troops at key, a mark point. cost {}",
                d.name,
                action.cost
            )),
            ActionKind::Tanker(_) => Some(format_compact!(
                "{name}: <key> | Spawn a tanker at key, a mark point. cost {}",
                action.cost
            )),
            ActionKind::TankerWaypoint => Some(format_compact!(
                "{name}: <group> <key> | Move a tanker to key. Group is the tanker group. cost {}",
                action.cost
            )),
        };
        if let Some(msg) = msg {
            ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg)
        }
    }
}

/// Handle `-action <name> <args>`.
///
/// `help` is answered immediately. Anything else is queued and executed later
/// by [`run_action_commands`], because spawning requires the mission
/// environment.
fn action_command(ctx: &mut Context, id: PlayerId, cmd: &str) {
    if cmd.trim().eq_ignore_ascii_case("help") {
        if let Some(ifo) = ctx.connected.get(&id) {
            if let Some(player) = ctx.db.player(&ifo.ucid) {
                // clone the Arc so we can borrow ctx mutably in action_help
                let cfg = Arc::clone(&ctx.db.ephemeral.cfg);
                if let Some(actions) = cfg.actions.get(&player.side) {
                    action_help(ctx, actions, id)
                }
            }
        }
    } else {
        ctx.action_commands.push((id, String::from(cmd)))
    }
}

/// Execute all action commands queued by [`action_command`].
///
/// Each command is parsed against the issuing player's side and started; the
/// player is told whether it succeeded. Commands from players who have since
/// disconnected or are unregistered are dropped.
pub(super) fn run_action_commands(
    ctx: &mut Context,
    perf: &mut PerfInner,
    lua: MizLua,
) -> Result<()> {
    let spctx = SpawnCtx::new(lua).context("creating spawn ctx")?;
    for (id, s) in ctx.action_commands.drain(..) {
        if let Some(ifo) = ctx.connected.get(&id) {
            if let Some(player) = ctx.db.player(&ifo.ucid) {
                let ucid = ifo.ucid.clone();
                let side = player.side;
                let r = match ActionCmd::parse(&mut ctx.db, lua, side, &s) {
                    Err(e) => Err(e),
                    Ok(cmd) => ctx.db.start_action(
                        lua,
                        perf,
                        &spctx,
                        &ctx.idx,
                        &ctx.jtac,
                        side,
                        Some(ucid),
                        cmd,
                    ),
                };
                let msg = match r {
                    Err(e) => format_compact!("could not run action {s}: {e:?}"),
                    Ok(()) => format_compact!("action {s} started"),
                };
                ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), msg)
            }
        }
    }
    Ok(())
}

/// Handle `-bind <token>`, linking the player's ucid to a web gui account.
///
/// The token is a UUID issued by the web gui. Once it is validated here, the
/// binding is sent to the stats db in the background.
fn bind_command(ctx: &mut Context, id: PlayerId, s: &str) {
    // compiled once on first use, matches a lowercase hyphenated UUID
    static RX: OnceLock<Regex> = OnceLock::new();
    match ctx.connected.get(&id) {
        None => ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            "You must register first. Type red or blue in chat",
        ),
        Some(ifo) => {
            let rx = RX.get_or_init(|| {
                Regex::new("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
                    .unwrap()
            });
            let s = s.trim();
            if !rx.is_match(s) {
                ctx.db
                    .ephemeral
                    .msgs()
                    .send(MsgTyp::Chat(Some(id)), "Invalid token")
            } else {
                ctx.db
                    .ephemeral
                    .msgs()
                    .send(MsgTyp::Chat(Some(id)), "Success");
                ctx.do_bg_task(Task::Stat(Stat::Bind {
                    id: ifo.ucid,
                    token: s.into(),
                }))
            }
        }
    }
}

/// Handle `-jtac <id> <cmd>`.
///
/// `help` is answered immediately. Otherwise the jtac id is parsed and the
/// command is queued for [`run_jtac_commands`], since jtac operations need the
/// mission environment.
fn jtac_command(ctx: &mut Context, id: PlayerId, s: &str) {
    if s.trim().eq_ignore_ascii_case("help") {
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> autoshift");
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> pointer");
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> shift");
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> status");
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> smoke");
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> code <code>");
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> arty <id|all> <n>");
        ctx.db
            .ephemeral
            .msgs()
            .send(MsgTyp::Chat(Some(id)), " -jtac <id> bomber [mission]");
    } else if let Some((jtid, cmd)) = s.split_once(" ") {
        if let Ok(jtid) = jtid.parse::<JtId>() {
            ctx.jtac_commands.push((id, jtid, cmd.into()));
        } else {
            ctx.db
                .ephemeral
                .msgs()
                .send(MsgTyp::Chat(Some(id)), format_compact!("invalid jtac id {jtid}"));
        }
    } else {
        ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            "expected -jtac <id> <cmd>, see -jtac <help>",
        );
    }
}

/// Execute a single queued jtac command on behalf of player `id`.
///
/// Each sub command maps onto the equivalent F10 jtac menu function, so chat
/// and menu behave identically. Player errors (bad arguments, enemy jtac,
/// etc.) are reported to the player and return `Ok`; only internal failures
/// return `Err`.
fn run_jtac_command(
    ctx: &mut Context,
    lua: MizLua,
    id: PlayerId,
    jtid: JtId,
    cmd: String,
) -> Result<()> {
    // shadows log::error! in this function: report the formatted message to
    // the player and return early from run_jtac_command with Ok
    macro_rules! error {
        ($msg:literal) => {error!($msg,)};
        ($msg:literal, $($arg:expr),*) => {{
            ctx.db
                .ephemeral
                .msgs()
                .send(MsgTyp::Chat(Some(id)), format_compact!($msg, $($arg),*));
            return Ok(());
        }};
    }
    let ucid = ctx
        .connected
        .get(&id)
        .ok_or_else(|| anyhow!("unknown player"))?
        .ucid;
    let side = match ctx.db.player(&ucid) {
        Some(player) => player.side,
        None => error!("no such player {ucid}"),
    };
    let jtac = match ctx.jtac.get(&jtid) {
        Err(_) => error!("no such jtac {jtid}"),
        Ok(jtac) => {
            if jtac.side() != side {
                error!("you can't give orders to enemy jtacs")
            }
            jtac
        }
    };
    if let Some(_) = cmd.strip_prefix("autoshift") {
        let arg = ArgTuple {
            fst: ucid,
            snd: jtid,
        };
        menu::jtac::jtac_toggle_auto_shift(lua, arg)?;
    } else if let Some(_) = cmd.strip_prefix("shift") {
        let arg = ArgTuple {
            fst: ucid,
            snd: jtid,
        };
        menu::jtac::jtac_shift(lua, arg)?;
    } else if let Some(_) = cmd.strip_prefix("status") {
        // depending on the player's preference the status report goes to
        // the whole side (fst = None) or only to the requesting player
        let panel_to_side = ctx
            .db
            .player(&ucid)
            .map(|p| p.jtac_or_spectators)
            .unwrap_or(true);
        let arg = ArgTuple {
            fst: (!panel_to_side).then_some(ucid),
            snd: jtid,
        };
        menu::jtac::jtac_status(lua, arg)?
    } else if let Some(_) = cmd.strip_prefix("smoke") {
        let arg = ArgTuple {
            fst: ucid,
            snd: jtid,
        };
        menu::jtac::jtac_smoke_target(lua, arg)?
    } else if let Some(_) = cmd.strip_prefix("pointer") {
        let arg = ArgTuple {
            fst: ucid,
            snd: jtid,
        };
        menu::jtac::jtac_toggle_ir_pointer(lua, arg)?
    } else if let Some(s) = cmd.strip_prefix("bomber") {
        let name = s.trim();
        let name = if name != "" {
            Some(String::from(name))
        } else {
            // no mission named, use the first bomber action configured for the side
            let bomber_missions = ctx.db.ephemeral.cfg.actions.get(&side);
            bomber_missions.iter().find_map(|acts| {
                acts.iter().find_map(|(n, a)| match a.kind {
                    ActionKind::Bomber(_) => Some(n.clone()),
                    _ => None,
                })
            })
        };
        match name {
            None => error!("no bomber mission(s)"),
            Some(name) => {
                let arg = ArgTriple {
                    fst: jtid,
                    snd: ucid,
                    trd: name,
                };
                menu::jtac::call_bomber(lua, arg)?
            }
        }
    } else if let Some(s) = cmd.strip_prefix("code ") {
        let code = match s.parse::<u16>() {
            Ok(c) => c,
            Err(_) => {
                ctx.db
                    .ephemeral
                    .msgs()
                    .send(MsgTyp::Chat(Some(id)), format_compact!("invalid laser code {s}"));
                return Ok(());
            }
        };
        let arg = ArgTriple {
            fst: jtid,
            snd: code,
            trd: ucid,
        };
        menu::jtac::jtac_set_code(lua, arg)?
    } else if let Some(arty) = cmd.strip_prefix("arty ") {
        if let Some((aid, n)) = arty.split_once(" ") {
            // either a single artillery group id, or "all" for every
            // artillery group within range of the jtac
            let aids: SmallVec<[GroupId; 8]> = match aid.parse::<GroupId>() {
                Ok(id) => smallvec![id],
                Err(_) => {
                    if aid == "all" {
                        SmallVec::from_iter(jtac.nearby_artillery().into_iter().copied())
                    } else {
                        error!("invalid arty group id {aid}")
                    }
                }
            };
            let n = match n.parse::<u8>() {
                Ok(n) => n,
                Err(_) => error!("expected a number of shots between 0 and 255"),
            };
            for aid in aids {
                let arg = ArgQuad {
                    fst: jtid,
                    snd: aid,
                    trd: n,
                    fth: ucid,
                };
                menu::jtac::jtac_artillery_mission(lua, arg)?
            }
        } else {
            error!("arty expected <id> and <n>")
        }
    } else {
        error!("invalid jtac command {cmd}")
    }
    Ok(())
}

/// Execute all jtac commands queued by [`jtac_command`].
pub(super) fn run_jtac_commands(ctx: &mut Context, lua: MizLua) -> Result<()> {
    // take the queue so run_jtac_command can borrow ctx mutably
    let cmds = mem::take(&mut ctx.jtac_commands);
    for (id, jtid, cmd) in cmds {
        run_jtac_command(ctx, lua, id, jtid, cmd)?
    }
    Ok(())
}

/// Handle `-help`, listing player commands. Admins also see the `-admin` command.
fn help_command(ctx: &mut Context, id: PlayerId) {
    let admin = match ctx.connected.get(&id) {
        None => false,
        Some(ifo) => ctx.db.ephemeral.cfg.admins.contains_key(&ifo.ucid),
    };
    for cmd in [
        " blue: join the blue team",
        " red: join the red team",
        " -switch <color>: side switch to <color>",
        " -lives: display your current lives",
        " -time: how long until server restart",
        " -balance: show your points balance",
        " -transfer <amount> [<player> | objective:<objective>]: transfer points to another player or objective",
        " -delete <groupid>: delete a group you deployed for a partial refund",
        " -action <name> <args>: perform an action, -action help for a list of actions",
        " -bind <token>: bind your ucid to the specified token (for the web gui)",
        " -jtac <jtid> <cmd>",
        " -help: show this help message",
    ] {
        ctx.db.ephemeral.msgs().send(MsgTyp::Chat(Some(id)), cmd)
    }
    if admin {
        ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            " -admin <command>: run admin commands, -admin help for details",
        );
    }
}

/// Entry point for chat messages sent by player `id`.
///
/// Recognized commands return an empty string and ordinary chat is returned
/// unchanged. Note the caller (`on_player_try_send_chat` in lib.rs) currently
/// discards the returned string; only an `Err` suppresses the chat message.
pub(super) fn process(
    ctx: &mut Context,
    lua: HooksLua,
    now: DateTime<Utc>,
    id: PlayerId,
    msg: String,
) -> Result<String> {
    if msg.eq_ignore_ascii_case("blue") || msg.eq_ignore_ascii_case("red") {
        register_player(ctx, lua, id, msg)
    } else if msg.eq_ignore_ascii_case("-switch blue") || msg.eq_ignore_ascii_case("-switch red") {
        sideswitch_player(ctx, lua, id, msg)
    } else if msg.eq_ignore_ascii_case("-lives") {
        if let Err(e) = lives_command(ctx, id) {
            error!("lives command failed for player {:?} {:?}", id, e);
        }
        Ok("".into())
    } else if msg.eq_ignore_ascii_case("-time") {
        time_command(ctx, id, now);
        Ok("".into())
    } else if let Some(msg) = msg.strip_prefix("-admin ") {
        admin_command(ctx, id, msg);
        Ok("".into())
    } else if let Some(msg) = msg.strip_prefix("-action ") {
        action_command(ctx, id, msg);
        Ok("".into())
    } else if msg.starts_with("-balance") {
        balance_command(ctx, id);
        Ok("".into())
    } else if let Some(s) = msg.strip_prefix("-transfer ") {
        transfer_command(ctx, id, s);
        Ok("".into())
    } else if let Some(s) = msg.strip_prefix("-delete ") {
        delete_command(ctx, id, s);
        Ok("".into())
    } else if let Some(s) = msg.strip_prefix("-bind ") {
        bind_command(ctx, id, s);
        Ok("".into())
    } else if let Some(s) = msg.strip_prefix("-jtac ") {
        jtac_command(ctx, id, s);
        Ok("".into())
    } else if msg.starts_with("-help") {
        help_command(ctx, id);
        Ok("".into())
    } else if msg.starts_with("-")
        // unknown dash commands, and words players commonly type expecting
        // a command, get the help text instead of being posted to chat
        || msg.as_str() == "help"
        || msg.as_str() == "points"
        || msg.as_str() == "credits"
    {
        ctx.db.ephemeral.msgs().send(
            MsgTyp::Chat(Some(id)),
            format_compact!(" {msg} is not a valid command. Valid commands follow."),
        );
        help_command(ctx, id);
        Ok("".into())
    } else {
        Ok(msg)
    }
}
