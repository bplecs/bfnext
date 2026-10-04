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

//! Rate limited outgoing message queue.
//!
//! Everything the campaign shows to players, chat replies, on screen panel
//! messages, F10 map marks, and F10 map markup (circles, lines, text, etc.),
//! is queued here instead of being sent directly. The main timer loop in
//! `lib.rs` calls [`MsgQ::process`] once per tick (about once per second)
//! with `max_msgs_per_second` from the config, so a burst of messages, e.g.
//! redrawing every objective, can't stall the DCS frame.
//!
//! There are three priority queues, drained strictly in order:
//! - 0: chat and panel text messages
//! - 1: marks, text markup, and mark deletions
//! - 2: shape markup (circles, rects, quads, arrows) and markup updates
//!
//! A lower priority queue is only processed once every higher priority
//! queue is empty.

use dcso3::{
    Color, LuaVec3, String, Vector2, Vector3,
    coalition::Side,
    env::miz::{GroupId, UnitId},
    net::{Net, PlayerId},
    trigger::{Action, ArrowSpec, CircleSpec, MarkId, QuadSpec, RectSpec, SideFilter, TextSpec},
};
use log::error;
use std::collections::VecDeque;

/// Who should see an on screen (panel) text message
#[derive(Debug, Clone, Copy)]
pub enum PanelDest {
    All,
    Side(Side),
    Group(GroupId),
    Unit(UnitId),
}

/// Who should see an F10 map mark
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum MarkDest {
    All,
    Side(Side),
    Group(GroupId),
}

/// How and to whom a text message is delivered
#[derive(Debug, Clone)]
pub enum MsgTyp {
    /// a chat message, to everyone if None, otherwise privately to the
    /// specified player
    Chat(Option<PlayerId>),
    /// an on screen text message
    Panel {
        to: PanelDest,
        /// how long the message stays on screen in seconds
        display_time: i64,
        /// remove previous messages from the screen first
        clear_view: bool,
    },
    /// an F10 map mark with the message as its text
    Mark {
        id: MarkId,
        to: MarkDest,
        position: LuaVec3,
        read_only: bool,
    },
}

/// Something to display. Except for `Message` these are F10 map markup
/// operations, the `Set*` variants modify markup that was already drawn.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum Msg {
    Message {
        typ: MsgTyp,
        text: String,
    },
    Circle {
        id: MarkId,
        to: SideFilter,
        spec: CircleSpec,
        message: Option<String>,
    },
    Rect {
        id: MarkId,
        to: SideFilter,
        spec: RectSpec,
        message: Option<String>,
    },
    Quad {
        id: MarkId,
        to: SideFilter,
        spec: QuadSpec,
        message: Option<String>,
    },
    Text {
        id: MarkId,
        to: SideFilter,
        spec: TextSpec,
    },
    Arrow {
        id: MarkId,
        to: SideFilter,
        spec: ArrowSpec,
        message: Option<String>,
    },
    SetMarkupColor {
        id: MarkId,
        color: Color,
    },
    SetMarkupFillColor {
        id: MarkId,
        color: Color,
    },
    SetMarkupText {
        id: MarkId,
        text: String,
    },
    SetMarkupStart {
        id: MarkId,
        pos: LuaVec3,
    },
    SetMarkupEnd {
        id: MarkId,
        pos: LuaVec3,
    },
}

/// A queued operation
#[derive(Debug, Clone)]
pub enum Cmd {
    Send(Msg),
    /// remove a mark or markup from the F10 map
    DeleteMark(MarkId),
}

/// The message queue, one `VecDeque` per priority, index 0 is the highest
/// priority. Always has exactly three queues.
#[derive(Debug, Clone)]
pub struct MsgQ(Vec<VecDeque<Cmd>>);

impl Default for MsgQ {
    fn default() -> Self {
        MsgQ(vec![
            VecDeque::default(),
            VecDeque::default(),
            VecDeque::default(),
        ])
    }
}

impl MsgQ {
    fn send_with_priority<S: Into<String>>(&mut self, p: usize, typ: MsgTyp, text: S) {
        self.0[p].push_back(Cmd::Send(Msg::Message {
            typ,
            text: text.into(),
        }))
    }

    /// Queue a text message at the highest priority
    pub fn send<S: Into<String>>(&mut self, typ: MsgTyp, text: S) {
        self.send_with_priority(0, typ, text)
    }

    /// Delete a mark or markup. Any queued updates to `did` are dropped. If
    /// the shape that creates `did` is still queued it is dropped too and
    /// no delete is sent, since DCS never saw it. Otherwise a delete is
    /// queued at priority 1.
    pub fn delete_mark(&mut self, did: MarkId) {
        // false if we removed the queued creation of did
        let mut push = true;
        let mut remove = |pri: usize| {
            self.0[pri].retain(|cmd| match cmd {
                Cmd::DeleteMark(_) => true,
                Cmd::Send(msg) => match msg {
                    Msg::Message { .. } => true,
                    Msg::Circle { id, .. }
                    | Msg::Rect { id, .. }
                    | Msg::Quad { id, .. }
                    | Msg::Text { id, .. }
                    | Msg::Arrow { id, .. } => {
                        if *id == did {
                            push = false;
                            false
                        } else {
                            true
                        }
                    }
                    Msg::SetMarkupColor { id, .. }
                    | Msg::SetMarkupFillColor { id, .. }
                    | Msg::SetMarkupText { id, .. }
                    | Msg::SetMarkupStart { id, .. }
                    | Msg::SetMarkupEnd { id, .. } => *id != did,
                },
            })
        };
        remove(0);
        remove(1);
        remove(2);
        if push {
            self.0[1].push_back(Cmd::DeleteMark(did))
        }
    }

    /// Queue an F10 mark visible to everyone at `position` (a 2d map
    /// position, x and z in DCS coordinates). Returns the mark id, which
    /// can later be passed to `delete_mark`.
    #[allow(dead_code)]
    pub fn mark_to_all<S: Into<String>>(
        &mut self,
        position: Vector2,
        read_only: bool,
        text: S,
    ) -> MarkId {
        let id = MarkId::new();
        self.send_with_priority(
            1,
            MsgTyp::Mark {
                id,
                to: MarkDest::All,
                position: LuaVec3(Vector3::new(position.x, 0., position.y)),
                read_only,
            },
            text,
        );
        id
    }

    /// Like `mark_to_all`, but only visible to `side`
    pub fn mark_to_side<S: Into<String>>(
        &mut self,
        side: Side,
        position: Vector2,
        read_only: bool,
        text: S,
    ) -> MarkId {
        let id = MarkId::new();
        self.send_with_priority(
            1,
            MsgTyp::Mark {
                id,
                to: MarkDest::Side(side),
                position: LuaVec3(Vector3::new(position.x, 0., position.y)),
                read_only,
            },
            text,
        );
        id
    }

    /// Like `mark_to_all`, but only visible to `group`
    #[allow(dead_code)]
    pub fn mark_to_group<S: Into<String>>(
        &mut self,
        group: GroupId,
        position: Vector2,
        read_only: bool,
        text: S,
    ) -> MarkId {
        let id = MarkId::new();
        self.send_with_priority(
            1,
            MsgTyp::Mark {
                id,
                to: MarkDest::Group(group),
                position: LuaVec3(Vector3::new(position.x, 0., position.y)),
                read_only,
            },
            text,
        );
        id
    }

    /// Queue an on screen message for everyone. `display_time` is in
    /// seconds, `clear_view` removes previous messages first. The other
    /// `panel_to_*` functions are the same but for a narrower audience.
    #[allow(dead_code)]
    pub fn panel_to_all<S: Into<String>>(&mut self, display_time: i64, clear_view: bool, text: S) {
        self.send_with_priority(
            0,
            MsgTyp::Panel {
                to: PanelDest::All,
                display_time,
                clear_view,
            },
            text,
        )
    }

    pub fn panel_to_side<S: Into<String>>(
        &mut self,
        display_time: i64,
        clear_view: bool,
        side: Side,
        text: S,
    ) {
        self.send_with_priority(
            0,
            MsgTyp::Panel {
                to: PanelDest::Side(side),
                display_time,
                clear_view,
            },
            text,
        )
    }

    pub fn panel_to_group<S: Into<String>>(
        &mut self,
        display_time: i64,
        clear_view: bool,
        group: GroupId,
        text: S,
    ) {
        self.send_with_priority(
            0,
            MsgTyp::Panel {
                to: PanelDest::Group(group),
                display_time,
                clear_view,
            },
            text,
        )
    }

    pub fn panel_to_unit<S: Into<String>>(
        &mut self,
        display_time: i64,
        clear_view: bool,
        unit: UnitId,
        text: S,
    ) {
        self.send_with_priority(
            0,
            MsgTyp::Panel {
                to: PanelDest::Unit(unit),
                display_time,
                clear_view,
            },
            text,
        )
    }

    /// Queue drawing a circle on the F10 map for the sides in `to`. The
    /// caller allocates `id` so it can update or delete the shape later.
    /// The other shape functions work the same way.
    pub fn circle_to_all(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: CircleSpec,
        message: Option<String>,
    ) {
        self.0[2].push_back(Cmd::Send(Msg::Circle {
            id,
            to,
            spec,
            message,
        }))
    }

    #[allow(dead_code)]
    pub fn rect_to_all(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: RectSpec,
        message: Option<String>,
    ) {
        self.0[2].push_back(Cmd::Send(Msg::Rect {
            id,
            to,
            spec,
            message,
        }))
    }

    pub fn quad_to_all(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: QuadSpec,
        message: Option<String>,
    ) {
        self.0[2].push_back(Cmd::Send(Msg::Quad {
            id,
            to,
            spec,
            message,
        }))
    }

    /// Queue a text label on the F10 map. Unlike the other shapes this is
    /// queued at priority 1.
    pub fn text_to_all(&mut self, to: SideFilter, id: MarkId, spec: TextSpec) {
        self.0[1].push_back(Cmd::Send(Msg::Text { id, to, spec }))
    }

    pub fn arrow_to(
        &mut self,
        to: SideFilter,
        id: MarkId,
        spec: ArrowSpec,
        message: Option<String>,
    ) {
        self.0[2].push_back(Cmd::Send(Msg::Arrow {
            id,
            to,
            spec,
            message,
        }))
    }

    /// Change the line color of existing markup `id`. The other
    /// `set_markup_*` functions likewise modify existing markup.
    pub fn set_markup_color(&mut self, id: MarkId, color: Color) {
        self.0[2].push_back(Cmd::Send(Msg::SetMarkupColor { id, color }))
    }

    #[allow(dead_code)]
    pub fn set_markup_fill_color(&mut self, id: MarkId, color: Color) {
        self.0[2].push_back(Cmd::Send(Msg::SetMarkupFillColor { id, color }))
    }

    pub fn set_markup_text(&mut self, id: MarkId, text: String) {
        self.0[2].push_back(Cmd::Send(Msg::SetMarkupText { id, text }))
    }

    pub fn set_markup_pos_start(&mut self, id: MarkId, pos: LuaVec3) {
        self.0[2].push_back(Cmd::Send(Msg::SetMarkupStart { id, pos }))
    }

    pub fn set_markup_pos_end(&mut self, id: MarkId, pos: LuaVec3) {
        self.0[2].push_back(Cmd::Send(Msg::SetMarkupEnd { id, pos }))
    }

    /// Total number of queued commands across all priorities
    pub fn len(&self) -> usize {
        self.0.iter().fold(0, |acc, q| acc + q.len())
    }

    /// Send up to `max_rate` queued commands to DCS, highest priority
    /// first. Errors are logged and the failed command is dropped.
    pub fn process(&mut self, max_rate: usize, net: &Net, act: &Action) {
        for _ in 0..max_rate {
            let cmd = match self.0[0].pop_front() {
                Some(cmd) => cmd,
                None => match self.0[1].pop_front() {
                    Some(cmd) => cmd,
                    None => match self.0[2].pop_front() {
                        Some(cmd) => cmd,
                        None => return,
                    },
                },
            };
            let res = match cmd {
                Cmd::DeleteMark(id) => act.remove_mark(id),
                Cmd::Send(Msg::Message { typ, text }) => match typ {
                    MsgTyp::Mark {
                        id,
                        to,
                        position,
                        read_only,
                    } => match to {
                        MarkDest::All => act.mark_to_all(id, text, position, read_only, None),
                        MarkDest::Side(side) => {
                            act.mark_to_coalition(id, text, position, side, read_only, None)
                        }
                        MarkDest::Group(group) => {
                            act.mark_to_group(id, text, position, group, read_only, None)
                        }
                    },
                    MsgTyp::Chat(to) => match to {
                        None => net.send_chat(text, true),
                        // sent as if from player id 1, the server
                        Some(id) => net.send_chat_to(text, id, Some(PlayerId::from(1))),
                    },
                    MsgTyp::Panel {
                        to,
                        display_time,
                        clear_view,
                    } => match to {
                        PanelDest::All => act.out_text(text, display_time, clear_view),
                        PanelDest::Group(gid) => {
                            act.out_text_for_group(gid, text, display_time, clear_view)
                        }
                        PanelDest::Side(side) => {
                            act.out_text_for_coalition(side, text, display_time, clear_view)
                        }
                        PanelDest::Unit(uid) => {
                            act.out_text_for_unit(uid, text, display_time, clear_view)
                        }
                    },
                },
                Cmd::Send(Msg::Circle {
                    id,
                    to,
                    spec,
                    message,
                }) => act.circle_to_all(to, id, spec, message),
                Cmd::Send(Msg::Rect {
                    id,
                    to,
                    spec,
                    message,
                }) => act.rect_to_all(to, id, spec, message),
                Cmd::Send(Msg::Quad {
                    id,
                    to,
                    spec,
                    message,
                }) => act.quad_to_all(to, id, spec, message),
                Cmd::Send(Msg::Text { id, to, spec }) => act.text_to_all(to, id, spec),
                Cmd::Send(Msg::Arrow {
                    id,
                    to,
                    spec,
                    message,
                }) => act.arrow_to_all(to, id, spec, message),
                Cmd::Send(Msg::SetMarkupColor { id, color }) => act.set_markup_color(id, color),
                Cmd::Send(Msg::SetMarkupFillColor { id, color }) => {
                    act.set_markup_fill_color(id, color)
                }
                Cmd::Send(Msg::SetMarkupStart { id, pos }) => {
                    act.set_markup_position_start(id, pos)
                }
                Cmd::Send(Msg::SetMarkupEnd { id, pos }) => act.set_markup_position_end(id, pos),
                Cmd::Send(Msg::SetMarkupText { id, text }) => act.set_markup_text(id, text),
            };
            if let Err(e) = res {
                error!("could not send message {:?}", e)
            }
        }
    }
}
