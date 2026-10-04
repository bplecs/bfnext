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

EWR SYSTEM CONFIGURATION:
The EWR system supports two modes controlled by the 'ewr_mode' configuration option:
- EwrMode::Original: Original implementation with immediate track updates and complex reporting timing
- EwrMode::Delayed: Modified implementation with configurable delay on track updates and simplified reporting

The delay is controlled by the 'ewr_delay' configuration option (in seconds, default: 60).
The default mode is EwrMode::Original to maintain backward compatibility.
*/

//! Early warning radar picture for players.
//!
//! Each side's EWR units (see [`Db::ewrs`]) track airborne players and AI
//! action aircraft within range and line of sight. [`Ewr::update_tracks`]
//! refreshes the per-side track tables and publishes detection stats, and
//! [`Ewr::where_chicken`] turns a side's tracks into BRAA style reports
//! (bearing, range, altitude, speed, heading, age) relative to a player.
//! Players can toggle automatic reports and choose metric or imperial units.

use crate::{
    db::{
        Db,
        player::{InstancedPlayer, Player},
    },
    landcache::LandCache,
};
use anyhow::Result;
use bfprotocols::{
    cfg::EwrMode,
    stats::{DetectionSource, EnId, Stat},
};
use chrono::prelude::*;
use dcso3::{
    MizLua, Position3, Vector2, Vector3, azumith2d_to, azumith3d, coalition::Side, land::Land,
    net::Ucid, radians_to_degrees,
};
use fxhash::FxHashMap;
use smallvec::{SmallVec, smallvec};
use std::fmt;

/// One line of an EWR report about a single contact.
///
/// Values are created in SI units (meters, m/s) with `units` set to
/// [`EwrUnits::Metric`], and are only meaningful for display after
/// [`GibBraa::convert`] has been applied.
#[derive(Debug, Clone, Copy)]
pub struct GibBraa {
    /// bearing from the player to the contact in degrees
    pub bearing: u16,
    pub range: u32,
    pub altitude: u32,
    /// the contact's heading in degrees
    pub heading: u16,
    pub speed: u16,
    /// seconds since the contact was last detected
    pub age: u16,
    pub units: EwrUnits,
    /// guards against converting twice
    converted: bool,
}

/// Column header matching the [`GibBraa`] display format.
pub const HEADER: &'static str = "BRG      RNG      ALT      SPD        HDG      AGE";

impl fmt::Display for GibBraa {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (range_u, altitude_u, _u) = match self.units {
            EwrUnits::Imperial => ("nm", "ft", "kts "),
            EwrUnits::Metric => ("km", "m ", "km/h"),
        };
        write!(
            f,
            "{:>6} {:>6}{} {:>6}{} {:>6}{} {:>6} {:>6}s",
            self.bearing,
            self.range,
            range_u,
            self.altitude,
            altitude_u,
            self.speed,
            _u,
            self.heading,
            self.age
        )
    }
}

impl GibBraa {
    /// Convert from SI units to display units, rounding speed to the nearest
    /// 100 and altitude to the nearest 100 (below 1000) or 1000. Metric gives
    /// km, m, km/h; imperial gives nm, ft, kts. Range is truncated. Only the
    /// first call has any effect.
    fn convert(&mut self, unit: EwrUnits) {
        if self.converted {
            return;
        }
        self.converted = true;
        match unit {
            EwrUnits::Metric => {
                self.range = self.range / 1000;
                // Round speed to nearest 100s in metric (km/h)
                self.speed = ((((self.speed as f64) * 3.6) / 100.0).round() * 100.0) as u16;
                // Round altitude: under 1000m to nearest 100s, 1000m+ to nearest 1000s
                if self.altitude < 1000 {
                    self.altitude = ((self.altitude as f64 / 100.0).round() * 100.0) as u32;
                } else {
                    self.altitude = ((self.altitude as f64 / 1000.0).round() * 1000.0) as u32;
                }
            }
            EwrUnits::Imperial => {
                self.range = self.range / 1852;
                self.altitude = (self.altitude as f64 * 3.38084) as u32;
                // Round speed to nearest 100s in imperial (kts)
                self.speed = ((((self.speed as f64) * 1.94384) / 100.0).round() * 100.0) as u16;
                // Round altitude: under 1000ft to nearest 100s, 1000ft+ to nearest 1000s
                if self.altitude < 1000 {
                    self.altitude = ((self.altitude as f64 / 100.0).round() * 100.0) as u32;
                } else {
                    self.altitude = ((self.altitude as f64 / 1000.0).round() * 1000.0) as u32;
                }
            }
        }
        self.units = unit;
    }
}

/// What one side's EWR network knows about one aircraft.
#[derive(Debug, Clone, Copy, Default)]
struct Track {
    pos: Position3,
    /// velocity in m/s
    velocity: Vector3,
    last: DateTime<Utc>,          // Last detection time (for age calculation)
    last_update: DateTime<Utc>,   // Last data update time (for delay mechanism)
    /// the side the tracked aircraft belongs to
    side: Side,
    /// `detected` as of the previous update, used to publish only changes
    was_detected: bool,
    /// detected by an enemy EWR during the current update
    detected: bool,
}

/// Units used to display EWR reports.
#[derive(Debug, Clone, Copy)]
pub enum EwrUnits {
    Imperial,
    Metric,
}

impl Default for EwrUnits {
    fn default() -> Self {
        Self::Metric
    }
}

/// Per-player EWR preferences.
#[derive(Debug, Clone, Copy)]
struct PlayerState {
    /// whether automatic reports are on (forced reports ignore this)
    enabled: bool,
    units: EwrUnits,
    /// when the player was last sent a report
    last: DateTime<Utc>,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            enabled: true,
            units: EwrUnits::default(),
            last: DateTime::default(),
        }
    }
}

/// The EWR state for all sides. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct Ewr {
    /// EWR owning side -> tracked aircraft (of any side)
    tracks: FxHashMap<Side, FxHashMap<EnId, Track>>,
    player_state: FxHashMap<Ucid, PlayerState>,
}

impl Ewr {
    /// Update every side's tracks with the aircraft its EWRs can see.
    ///
    /// Candidates are airborne players and airborne units of AI action
    /// groups. An aircraft is seen by an EWR if it is within the EWR's range
    /// and has line of sight from 10m above the radar. A side's EWRs track
    /// friendly aircraft too, but only detection by an enemy EWR counts as
    /// "detected"; changes in that state are published as stats.
    ///
    /// In [`EwrMode::Delayed`] a track's position and velocity are only
    /// refreshed once every `ewr_delay` seconds, though its detection time
    /// is still updated on every sighting.
    pub fn update_tracks(
        &mut self,
        lua: MizLua,
        landcache: &mut LandCache,
        db: &Db,
        now: DateTime<Utc>,
        ewr_mode: EwrMode,
        ewr_delay: u32,
    ) -> Result<()> {
        let land = Land::singleton(lua)?;
        let aircraft: SmallVec<[(EnId, Side, Position3, Vector3); 128]> = {
            let players = db
                .instanced_players()
                .filter(|(_, _, inst)| inst.in_air)
                .map(|(ucid, player, inst)| {
                    (
                        EnId::Player(*ucid),
                        player.side,
                        inst.position,
                        inst.velocity,
                    )
                });
            let actions = db
                .persisted
                .actions
                .into_iter()
                .filter_map(|gid| db.persisted.groups.get(gid))
                .flat_map(|sg| {
                    sg.units
                        .into_iter()
                        .filter_map(|uid| db.persisted.units.get(uid).map(|u| (*uid, u)))
                        .filter_map(|(uid, su)| {
                            su.airborne_velocity
                                .map(|v| (EnId::Unit(uid), sg.side, su.position, v))
                        })
                });
            players.chain(actions).collect()
        };
        for tracks in self.tracks.values_mut() {
            for track in tracks.values_mut() {
                track.detected = false;
            }
        }
        for (mut ewr_pos, ewr_side, ewr) in db.ewrs() {
            let range = (ewr.range as f64).powi(2);
            let tracks = self.tracks.entry(ewr_side).or_default();
            ewr_pos.y += 10.; // factor in antenna height
            for (id, obj_side, pos, velocity) in &aircraft {
                let track = tracks.entry(*id).or_default();
                // already seen by another of this side's EWRs this update
                if track.last != now {
                    let dist = na::distance_squared(&ewr_pos.into(), &pos.p.0.into());
                    if dist <= range {
                        if landcache.is_visible(&land, dist.sqrt(), ewr_pos, pos.p.0)? {
                            match ewr_mode {
                                EwrMode::Original => {
                                    // Original implementation: update track data immediately
                                    track.pos = *pos;
                                    track.velocity = *velocity;
                                    track.last_update = now;
                                }
                                EwrMode::Delayed => {
                                    // Configurable delay: only update track data if the configured delay has passed since last update
                                    // For new tracks (last_update_time is epoch), update immediately
                                    let time_since_update = (now - track.last_update).num_seconds();
                                    if time_since_update >= ewr_delay as i64 || track.last_update == DateTime::<Utc>::UNIX_EPOCH {
                                        track.pos = *pos;
                                        track.velocity = *velocity;
                                        track.last_update = now;
                                    }
                                }
                            }
                            track.last = now;
                            track.side = *obj_side;
                            track.detected |= ewr_side != *obj_side;
                        }
                    }
                }
            }
        }
        for tracks in self.tracks.values_mut() {
            for (id, track) in tracks.iter_mut() {
                if track.was_detected != track.detected {
                    track.was_detected = track.detected;
                    db.ephemeral.stat(Stat::Detected {
                        id: *id,
                        detected: track.was_detected,
                        source: DetectionSource::EWR,
                    })
                }
            }
        }
        Ok(())
    }

    /// Toggle automatic EWR reports for a player, returning the new state.
    pub fn toggle(&mut self, ucid: &Ucid) -> bool {
        let st = self.player_state.entry(ucid.clone()).or_default();
        st.enabled = !st.enabled;
        st.enabled
    }

    pub fn set_units(&mut self, ucid: &Ucid, units: EwrUnits) {
        self.player_state.entry(ucid.clone()).or_default().units = units;
    }

    /// Build an EWR report for a player from their side's tracks.
    ///
    /// Reports on friendly contacts if `friendly`, otherwise enemy contacts,
    /// excluding the player's own aircraft and anything not detected in the
    /// last 120 seconds (such tracks are also dropped). At most the 10
    /// closest contacts are returned, sorted by range, converted to the
    /// player's units.
    ///
    /// Returns an empty report if the player has reports disabled (unless
    /// `force`), or if it isn't time for a report yet. In
    /// [`EwrMode::Original`] a report is due after 60s, immediately when the
    /// closest contact is within 20km and fresh (<= 10s old), or after 30s
    /// when it is within 40km and fresh. In [`EwrMode::Delayed`] a report is
    /// due every `ewr_delay` seconds. `force` always produces a report.
    pub fn where_chicken(
        &mut self,
        now: DateTime<Utc>,
        friendly: bool,
        force: bool,
        ucid: &Ucid,
        player: &Player,
        inst: &InstancedPlayer,
        ewr_mode: EwrMode,
        ewr_delay: u32,
    ) -> SmallVec<[GibBraa; 64]> {
        let side = player.side;
        let pos = Vector2::new(inst.position.p.x, inst.position.p.z);
        let mut reports: SmallVec<[GibBraa; 64]> = smallvec![];
        let tracks = match self.tracks.get_mut(&side) {
            Some(t) => t,
            None => return reports,
        };
        let state = self.player_state.entry(ucid.clone()).or_default();
        if !force && !state.enabled {
            return reports;
        }
        let ownship = EnId::Player(*ucid);
        tracks.retain(|tucid, track| {
            let age = (now - track.last).num_seconds();
            let include = (friendly && track.side == side) || (!friendly && track.side != side);
            if include && age <= 120 && tucid != &ownship {
                let cpos = Vector2::new(track.pos.p.x, track.pos.p.z);
                let range = na::distance(&pos.into(), &cpos.into());
                let bearing = radians_to_degrees(azumith2d_to(pos, cpos));
                let heading = radians_to_degrees(azumith3d(track.pos.x.0));
                let speed = track.velocity.magnitude();
                let altitude = track.pos.p.y;
                reports.push(GibBraa {
                    range: range as u32,
                    heading: heading as u16,
                    altitude: altitude as u32,
                    bearing: bearing as u16,
                    age: age as u16,
                    speed: speed as u16,
                    units: EwrUnits::Metric,
                    converted: false,
                })
            }
            age <= 120
        });
        if reports.is_empty() {
            return reports;
        }
        reports.sort_by_key(|r| r.range);
        while reports.len() > 10 {
            reports.pop();
        }
        let since_last = (now - state.last).num_seconds();
        match ewr_mode {
            EwrMode::Original => {
                // Original reporting logic with complex timing rules
                if force
                    || since_last >= 60
                    || (reports[0].range <= 20000 && reports[0].age <= 10)
                    || (reports[0].range <= 40000 && reports[0].age <= 10 && since_last >= 30)
                {
                    state.last = now;
                    reports.iter_mut().for_each(|r| r.convert(state.units));
                    reports
                } else {
                    smallvec![]
                }
            }
            EwrMode::Delayed => {
                // With configurable track update delay, we can simplify the reporting logic
                // Reports are sent every delay period or when forced
                if force || since_last >= ewr_delay as i64 {
                    state.last = now;
                    reports.iter_mut().for_each(|r| r.convert(state.units));
                    reports
                } else {
                    smallvec![]
                }
            }
        }
    }
}
