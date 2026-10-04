/*
Copyright 2024 Eric Stokes.

This file is part of dcso3.

dcso3 is free software: you can redistribute it and/or modify it under
the terms of the MIT License.

dcso3 is distributed in the hope that it will be useful, but WITHOUT
ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or
FITNESS FOR A PARTICULAR PURPOSE.
*/

//! DCS object attributes.
//!
//! DCS classifies object types with string attributes (e.g. `"Tanks"`,
//! `"SAM SR"`), which can be tested with `Object:hasAttribute` and appear in
//! object descriptions. [`Attribute`] names the known ones and
//! [`Attributes`] is a table of attribute flags.

use super::{as_tbl, String};
use crate::{wrapped_table, string_enum};
use anyhow::Result;
use mlua::{prelude::*, Value};
use serde_derive::{Serialize, Deserialize};
use std::ops::Deref;

// A DCS attribute name. Attribute strings not listed here convert to
// `Attribute::Custom`.
string_enum!(Attribute, u8, [
    PlaneCarrier => "plane_carrier",
    NoTailTrail => "no_tail_trail",
    Cord => "cord",
    SkiJump => "ski_jump",
    Catapult => "catapult",
    LowReflectionVessel => "low_reflection_vessel",
    AAFlak => "AA_flak",
    AAMissile => "AA_missile",
    CruiseMissiles => "Cruise missiles",
    AntiShipMissiles => "Anti-Ship missiles",
    Missiles => "Missiles",
    Fighters => "Fighters",
    Interceptors => "Interceptors",
    MultiroleFighters => "Multirole fighters",
    Bombers => "Bombers",
    Battleplanes => "Battleplanes",
    AWACS => "AWACS",
    Tankers => "Tankers",
    Aux => "Aux",
    Transports => "Transports",
    StrategicBombers => "Strategic bombers",
    UAVs => "UAVs",
    AttackHelicopters => "Attack helicopters",
    TransportHelicopters => "Transport helicopters",
    Planes => "Planes",
    Helicopters => "Helicopters",
    Cars => "Cars",
    Trucks => "Trucks",
    Infantry => "Infantry",
    Tanks => "Tanks",
    Artillery => "Artillery",
    MLRS => "MLRS",
    IFV => "IFV",
    APC => "APC",
    Fortifications => "Fortifications",
    ArmedVehicles => "Armed vehicles",
    StaticAAA => "Static AAA",
    MobileAAA => "Mobile AAA",
    SAM_SR => "SAM SR",
    SAM_TR => "SAM TR",
    SAM_LL => "SAM LL",
    SAM_CC => "SAM CC",
    SAM_AUX => "SAM AUX",
    SR_SAM => "SR SAM",
    MR_SAM => "MR SAM",
    LR_SAM => "LR SAM",
    SAMElements => "SAM elements",
    IRGuidedSAM => "IR Guided SAM",
    SAM => "SAM",
    SAMRelated => "SAM related",
    AAA => "AAA",
    EWR => "EWR",
    AirDefenceVehicles => "Air Defence vehicles",
    MANPADS => "MANPADS",
    MANPADS_AUX => "MANPADS AUX",
    UnarmedVehicles => "Unarmed vehicles",
    ArmedGroundUnits => "Armed ground units",
    ArmedAirDefence => "Armed Air Defence",
    AirDefence => "Air Defence",
    AircraftCarriers => "Aircraft Carriers",
    Cruisers => "Cruisers",
    Destroyers => "Destroyers",
    Frigates => "Frigates",
    Corvettes => "Corvettes",
    HeavyArmedShips => "Heavy armed ships",
    LightArmedShips => "Light armed ships",
    ArmedShips => "Armed ships",
    UnarmedShips => "Unarmed ships",
    Air => "Air",
    GroundVehicles => "Ground vehicles",
    Ships => "Ships",
    Buildings => "Buildings",
    HeavyArmoredUnits => "HeavyArmoredUnits",
    ATGM => "ATGM",
    OldTanks => "Old Tanks",
    ModernTanks => "Modern Tanks",
    LightArmoredUnits => "LightArmoredUnits",
    RocketAttackValidAirDefence => "Rocket Attack Valid AirDefence",
    BattleAirplanes => "Battle airplanes",
    All => "All",
    InfantryCarriers => "Infantry carriers",
    Vehicles => "Vehicles",
    GroundUnits => "Ground Units",
    GroundUnitsNonAirdefence => "Ground Units Non Airdefence",
    ArmoredVehicles => "Armored vehicles",
    AntiAirArmedVehicles => "AntiAir Armed Vehicles",
    Airfields => "Airfields",
    Heliports => "Heliports",
    GrassAirfields => "Grass Airfields",
    Point => "Point",
    NonArmoredUnits => "NonArmoredUnits",
    NonAndLightArmoredUnits => "NonAndLightArmoredUnits",
    HumanVehicle => "human_vehicle",
    RADAR_BAND1_FOR_ARM => "RADAR_BAND1_FOR_ARM",
    RADAR_BAND2_FOR_ARM => "RADAR_BAND2_FOR_ARM",
    Prone => "Prone",
    DetectionByAWACS => "DetectionByAWACS",
    Datalink => "Datalink",
    CustomAimPoint => "CustomAimPoint",
    IndirectFire => "Indirect fire",
    Refuelable => "Refuelable",
    Weapon => "Weapon",
    Shell => "Shell",
    Rocket => "Rocket",
    Bomb => "Bomb",
    Missile => "Missile"
]);

// A table mapping attribute name to a boolean flag. Used, for example, by
// the radio usage options in `controller`.
wrapped_table!(Attributes, None);

impl<'lua> Attributes<'lua> {
    /// Create an empty attribute table
    pub fn new(lua: &'lua Lua) -> Result<Self> {
        Ok(Self {
            t: lua.create_table()?,
            lua
        })
    }

    /// The flag for `attr`. A missing attribute reads as `false`.
    pub fn get(&self, attr: Attribute) -> Result<bool> {
        Ok(self.t.get(attr)?)
    }

    /// Set the flag for `attr`
    pub fn set(&self, attr: Attribute, val: bool) -> Result<()> {
        Ok(self.t.set(attr, val)?)
    }
}
