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

//! Background work that must not run on the DCS simulation thread.
//!
//! The mission script runs inside DCS's Lua thread, where any blocking I/O
//! stalls the simulation. [`init`] spawns a dedicated OS thread running a
//! multi-threaded tokio runtime, and returns a channel sender. The game side
//! sends [`Task`]s down that channel (via `Context::do_bg_task`) and
//! [`background_loop`] executes them: saving and rotating the persisted
//! campaign state, saving the config, writing log lines, publishing perf
//! stats, and recording [`Stat`] events.
//!
//! Logging starts in file mode (`<writedir>/Logs/bfnext.txt`). Once the
//! campaign config is loaded, if it specifies a `netidx_base`, the loop
//! switches to netidx mode: logs, perf counters and stats are published
//! under `<netidx_base>/<sortie>`, and the admin RPCs in [`rpcs`] are
//! registered under `<netidx_base>/<sortie>/api`.

mod logpub;
mod perf;
mod rpcs;
mod statspub;

use crate::{admin::AdminCommand, db::persisted::Persisted};
use anyhow::{Context, Result, anyhow, bail};
use bfprotocols::{
    cfg::Cfg,
    perf::{Perf, PerfStat},
    stats::Stat,
};
use bytes::{BufMut, Bytes, BytesMut};
use chrono::prelude::*;
use compact_str::{CompactString, format_compact};
use crossbeam::queue::SegQueue;
use dcso3::perf::{Perf as ApiPerf, PerfStat as ApiPerfStat};
use fxhash::FxHashMap;
use log::error;
use logpub::LogPublisher;
use netidx::{
    chars::Chars,
    config::Config,
    path::Path as NetIdxPath,
    publisher::{Publisher, PublisherBuilder, Value},
};
use once_cell::sync::OnceCell;
use parking_lot::{Condvar, Mutex};
use perf::PubPerf;
use rpcs::Rpcs;
use serde::Serialize;
use simplelog::{LevelFilter, WriteLogger};
use statspub::Statspub;
use std::{
    cell::RefCell,
    env,
    ffi::OsStr,
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};
use tokio::{
    fs::File,
    io::AsyncWriteExt,
    runtime::Builder,
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    task,
};

thread_local! {
    /// Per thread accumulator for partial log lines written by the logger.
    static LOGBUF: RefCell<BytesMut> = RefCell::new(BytesMut::new());
}

/// The `io::Write` sink handed to simplelog. Rather than writing to disk on
/// the calling (possibly DCS) thread, complete lines are forwarded to the
/// background loop as [`Task::WriteLog`].
struct LogHandle(UnboundedSender<Task>);

impl io::Write for LogHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        LOGBUF.with_borrow_mut(|lbuf| {
            lbuf.extend_from_slice(buf);
            // the logger may emit a record in several write calls, only ship
            // the buffer once it ends with a newline so lines stay whole
            if lbuf.len() > 0 && lbuf[lbuf.len() - 1] == 0xA {
                self.0
                    .send(Task::WriteLog(lbuf.split().freeze()))
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "backend dead"))?;
            }
            Ok(buf.len())
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Serialize `db` to JSON, reusing a thread local buffer to avoid
/// reallocating for every save/stat.
fn encode<T: Serialize>(db: &T) -> Result<BytesMut> {
    thread_local! {
        static BUF: RefCell<BytesMut> = RefCell::new(BytesMut::new());
    }
    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        serde_json::to_writer((&mut *buf).writer(), db)?;
        Ok(buf.split())
    })
}

/// Copy the existing save file at `path` (if any) to a timestamped backup,
/// then thin out old backups. The save itself is left in place, so there
/// is always a save at `path` until [`save`] atomically replaces it.
///
/// Backups are named `<file name><unix timestamp in seconds>` and live next
/// to `path`. Backups are grouped into age buckets (minute, ten minutes,
/// hour, day, week, 4 week "month", using the coarsest unit the age
/// exceeds) and only the newest backup in each bucket is kept. Backups less
/// than a minute old are never deleted. Errors are returned if the path has
/// no file name/parent or any filesystem operation fails.
fn rotate_state(path: &Path) -> Result<()> {
    if path.exists() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("save file with no name"))?;
        use std::fmt::Write;
        let now = Utc::now();
        let mut with_ts = PathBuf::from(path);
        let mut backup = CompactString::from(name);
        write!(backup, "{}", now.timestamp()).unwrap();
        with_ts.set_file_name(backup);
        // a hard link is cheap, fall back to a copy if the fs doesn't support it
        if fs::hard_link(path, &with_ts).is_err() {
            fs::copy(path, &with_ts)?;
        }
        let dir = path
            .parent()
            .ok_or_else(|| anyhow!("path has no parent dir"))?;
        let mut by_age: FxHashMap<i64, Vec<(i64, PathBuf)>> = FxHashMap::default();
        for file in fs::read_dir(dir)? {
            let file = file?;
            let fname = file.file_name();
            let fname = match fname.to_str() {
                Some(s) => s,
                None => continue,
            };
            let now = now.timestamp();
            let onemin = 60;
            let tenmin = 600;
            let hour = 3600;
            let day = 86400;
            let week = day * 7;
            let month = week * 4;
            if file.file_type()?.is_file() {
                if let Some(ts) = fname.strip_prefix(name) {
                    if let Ok(ts) = ts.parse::<i64>() {
                        let age = now - ts;
                        let file = PathBuf::from(file.path());
                        if age > month {
                            by_age
                                .entry((age / month) * month)
                                .or_default()
                                .push((ts, file));
                        } else if age > week {
                            by_age
                                .entry((age / week) * week)
                                .or_default()
                                .push((ts, file));
                        } else if age > day {
                            by_age
                                .entry((age / day) * day)
                                .or_default()
                                .push((ts, file));
                        } else if age > hour {
                            by_age
                                .entry((age / hour) * hour)
                                .or_default()
                                .push((ts, file));
                        } else if age > tenmin {
                            by_age
                                .entry((age / tenmin) * tenmin)
                                .or_default()
                                .push((ts, file));
                        } else if age > onemin {
                            by_age
                                .entry((age / onemin) * onemin)
                                .or_default()
                                .push((ts, file));
                        }
                    }
                }
            }
        }
        for (_, mut paths) in by_age {
            // newest first, then pop (delete) from the oldest end until one remains
            paths.sort_by_key(|(ts, _)| *ts);
            paths.reverse();
            while paths.len() > 1 {
                if let Some(path) = paths.pop() {
                    fs::remove_file(path.1)?;
                }
            }
        }
    }
    Ok(())
}

/// Write the encoded campaign state to `path`, zstd compressed (level 9).
///
/// The data is first written to `path` with a `.tmp` extension and synced
/// to disk, then the previous save is backed up (see [`rotate_state`]) and
/// the temp file is renamed over it. The rename replaces the save
/// atomically, so a crash at any point leaves either the old or the new
/// save at `path`, never a missing or truncated one. Rotation failures are
/// logged but don't fail the save.
async fn save(path: PathBuf, encoded: Bytes) -> Result<()> {
    task::spawn_blocking(move || {
        use std::fs::File;
        let mut tmp = PathBuf::from(&path);
        tmp.set_extension("tmp");
        let file = File::options()
            .write(true)
            .truncate(true)
            .create(true)
            .open(&tmp)?;
        let mut enc = zstd::stream::Encoder::new(file, 9)?;
        io::copy(&mut &*encoded, &mut enc)?;
        // finish explicitly, auto_finish would silently drop a failed final write
        let file = enc.finish()?;
        file.sync_all()?;
        drop(file);
        if let Err(e) = rotate_state(&path) {
            error!("failed to rotate backup files {e:?}")
        }
        fs::rename(tmp, path)?;
        Ok(())
    })
    .await?
}

/// Rename an existing log file at `path` to `<stem><utc timestamp>.<ext>`
/// (e.g. `bfnext20240101T120000Z.txt`) so each session starts a fresh log.
/// Failures are printed to stdout, since the logger isn't usable yet.
fn rotate_log(path: &Path) {
    if path.exists() {
        let ext = path
            .extension()
            .unwrap_or(&OsStr::new("ext"))
            .to_str()
            .unwrap_or("inv");
        let mut rotate_path = path.to_path_buf();
        rotate_path.set_extension("");
        let name = rotate_path
            .file_name()
            .unwrap_or(&OsStr::new("nameless"))
            .to_str()
            .unwrap_or("invalid");
        let ts = Utc::now()
            .to_rfc3339_opts(SecondsFormat::Secs, true)
            .chars()
            .filter(|c| c != &'-' && c != &':')
            .collect::<CompactString>();
        rotate_path.set_file_name(format_compact!("{name}{ts}.{ext}"));
        if let Err(e) = fs::rename(&path, &rotate_path) {
            println!(
                "could not rotate log file {:?} to {:?} {:?}",
                path, rotate_path, e
            )
        }
    }
}

/// A unit of work sent from the game thread to the background loop.
#[derive(Debug)]
pub(super) enum Task {
    /// Encode and write a snapshot of the persisted campaign state to the
    /// path, rotating backups, then flush the stats archive.
    SaveState(PathBuf, Persisted),
    /// Delete the save file at the path (used when the campaign is reset).
    ResetState(PathBuf),
    /// The campaign config has been loaded. If it has a `netidx_base`, start
    /// the netidx publisher, register the admin RPCs and switch logging to
    /// netidx mode, all under `<netidx_base>/<sortie>`.
    CfgLoaded {
        sortie: dcso3::String,
        cfg: Arc<Cfg>,
        /// Queue the admin RPCs push commands onto. It is drained on the game
        /// thread by `admin::run_admin_commands`, which answers each command
        /// through the paired oneshot sender.
        admin_channel: Arc<SegQueue<(AdminCommand, oneshot::Sender<Value>)>>,
    },
    /// Save the config to the path.
    SaveConfig(PathBuf, Arc<Cfg>),
    /// One or more complete log lines (sent by [`LogHandle`]).
    WriteLog(Bytes),
    /// Log the current perf histograms and, in netidx mode, publish them.
    LogPerf {
        /// Number of connected players.
        players: usize,
        perf: Perf,
        api_perf: ApiPerf,
    },
    /// Shut down netidx publishing, set the bool to true and notify the
    /// condvar so the waiting game thread can proceed, then exit the loop.
    Shutdown(Arc<(Mutex<bool>, Condvar)>),
    /// Record a stat event in the stats archive (ignored in file mode).
    Stat(Stat),
}

/// Where logs, perf and stats go.
enum Logs {
    /// Netidx is configured: the log is published (and still written to the
    /// log file by [`LogPublisher`]), perf counters are published, and stats
    /// are recorded to a netidx archive.
    Netidx {
        publisher: Publisher,
        perf: PubPerf,
        stats: Statspub,
        log: LogPublisher,
    },
    /// No netidx: log lines are written to `log_path`, stats are dropped and
    /// perf is only written to the log.
    Files {
        log_path: PathBuf,
        /// `None` only transiently while switching to netidx.
        log_file: Option<File>,
        /// Directory the stats archive will use if we switch to netidx.
        stats_path: PathBuf,
    },
}

impl Logs {
    /// (Re)open the log file in file mode, appending to any existing
    /// contents. No-op in netidx mode.
    async fn open_files(&mut self) -> Result<()> {
        match self {
            Self::Netidx { .. } => Ok(()),
            Self::Files {
                log_path,
                log_file,
                stats_path: _,
            } => {
                *log_file = Some(
                    File::options()
                        .create(true)
                        .append(true)
                        .open(&log_path)
                        .await?,
                );
                Ok(())
            }
        }
    }

    /// Start in file mode, logging to `<write_dir>/Logs/bfnext.txt` after
    /// rotating away any previous log.
    async fn new(write_dir: &Path) -> Result<Self> {
        let stats_path = write_dir.join("Logs").join("stats");
        let log_path = write_dir.join("Logs").join("bfnext.txt");
        rotate_log(&log_path);
        let mut t = Self::Files {
            log_file: None,
            log_path,
            stats_path,
        };
        t.open_files().await?;
        Ok(t)
    }

    /// Write a log line. Errors if in file mode and the file isn't open.
    async fn write_log(&mut self, buf: Chars) -> Result<()> {
        match self {
            Self::Netidx { log, .. } => log.append(buf),
            Self::Files {
                log_file: Some(log_file),
                ..
            } => Ok(log_file.write_all_buf(&mut buf.as_bytes()).await?),
            Self::Files { .. } => bail!("log file is closed"),
        }
    }

    /// Append a stat to the archive, timestamped now. Dropped in file mode.
    fn write_stat(&mut self, stat: &Stat) -> Result<()> {
        match self {
            Self::Files { .. } => Ok(()),
            Self::Netidx { stats, .. } => stats.append(Utc::now(), stat),
        }
    }

    /// Write perf stats to the log, and in netidx mode publish them as a
    /// single batch.
    async fn log_perf(&self, players: usize, perf_stat: &PerfStat, api_perf_stat: &ApiPerfStat) {
        perf_stat.log();
        api_perf_stat.log();
        match self {
            Self::Files { .. } => (),
            Self::Netidx {
                publisher, perf, ..
            } => {
                let mut batch = publisher.start_batch();
                perf.update(&mut batch, players, perf_stat, api_perf_stat);
                batch.commit(None).await
            }
        }
    }

    /// Switch from file mode to netidx mode, publishing perf under `base`,
    /// stats under `base/stats` and the log under `base/log`. No-op if
    /// already in netidx mode.
    ///
    /// The log file is closed first because [`LogPublisher`] reopens it for
    /// appending. If any part of setup fails, the log file is reopened and
    /// we stay in file mode, returning the error.
    async fn switch_to_netidx(
        &mut self,
        publisher: Publisher,
        cfg: &Config,
        base: NetIdxPath,
    ) -> Result<()> {
        match self {
            Self::Netidx { .. } => Ok(()),
            Self::Files {
                log_path,
                log_file,
                stats_path,
            } => {
                drop(log_file.take());
                let go = || async {
                    let perf = PubPerf::new(
                        &publisher,
                        &base,
                        0,
                        &PerfStat::default(),
                        &ApiPerfStat::default(),
                    )
                    .context("starting pubperf")?;
                    let stats = Statspub::new(
                        publisher.clone(),
                        &cfg,
                        stats_path.clone(),
                        base.append("stats"),
                    )
                    .await
                    .context("starting stats pub")?;
                    let log = LogPublisher::new(publisher.clone(), log_path, base.append("log"))
                        .context("starting log pub")?;
                    Ok::<_, anyhow::Error>(Self::Netidx {
                        publisher: publisher.clone(),
                        perf,
                        stats,
                        log,
                    })
                };
                match go().await {
                    Ok(t) => {
                        *self = t;
                        Ok(())
                    }
                    Err(e) => {
                        if let Err(e) = self.open_files().await {
                            eprintln!("netidx init failed and reopening files also failed {e:?}")
                        }
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Flush the stats archive to disk (netidx mode only).
    fn flush_stats(&mut self) -> Result<()> {
        match self {
            Self::Files { .. } => Ok(()),
            Self::Netidx { stats, .. } => task::block_in_place(|| stats.flush()),
        }
    }

    /// Close the log publisher, flush stats and shut down the netidx
    /// publisher. Errors are ignored since we're exiting anyway.
    async fn shutdown(&mut self) {
        match self {
            Self::Files { .. } => (),
            Self::Netidx {
                publisher,
                log,
                stats,
                ..
            } => {
                let _ = log.close().await;
                let _ = task::block_in_place(|| stats.flush());
                publisher.clone().shutdown().await
            }
        }
    }
}

/// The main background task. Processes [`Task`]s in order until the channel
/// closes or a [`Task::Shutdown`] is received. Failures of individual tasks
/// are logged and do not stop the loop.
///
/// Panics if the initial log file can't be opened.
async fn background_loop(write_dir: PathBuf, mut rx: UnboundedReceiver<Task>) {
    let mut logs = Logs::new(&write_dir)
        .await
        .expect("could not open log files");
    // held only to keep the rpc procs published, dropping it unpublishes them
    let mut _rpcs: Option<Rpcs> = None;
    while let Some(msg) = rx.recv().await {
        match msg {
            Task::CfgLoaded {
                sortie,
                cfg,
                admin_channel,
            } => {
                if let Some(base) = cfg.netidx_base.as_ref() {
                    let base = base.append(&sortie);
                    let cfg = match Config::load_default() {
                        Ok(c) => c,
                        Err(e) => {
                            error!("failed to load netidx config {e:?}");
                            continue;
                        }
                    };
                    let publisher = match PublisherBuilder::new(cfg.clone()).build().await {
                        Ok(p) => p,
                        Err(e) => {
                            error!("failed to init netidx publisher {e:?}");
                            continue;
                        }
                    };
                    _rpcs = match Rpcs::new(&publisher, &admin_channel, &base).await {
                        Ok(r) => Some(r),
                        Err(e) => {
                            error!("failed to init rpcs {e:?}");
                            None
                        }
                    };
                    if let Err(e) = logs
                        .switch_to_netidx(publisher.clone(), &cfg, base.clone())
                        .await
                    {
                        eprintln!("failed to initialize netidx logs {e:?}")
                    }
                }
                match &logs {
                    Logs::Files { .. } => log::info!("log is in file mode"),
                    Logs::Netidx { .. } => log::info!("log is in netidx mode"),
                }
            }
            Task::SaveState(path, db) => {
                let encoded = match encode(&db) {
                    Ok(encoded) => encoded.freeze(),
                    Err(e) => {
                        error!("failed to encode save state {e:?}");
                        continue;
                    }
                };
                drop(db); // don't hold the db reference any longer than necessary
                if let Err(e) = save(path.clone(), encoded).await {
                    error!("failed to save state to {path:?}, {e:?}")
                }
                if let Err(e) = logs.flush_stats() {
                    error!("failed to flush stats {e:?}")
                }
            }
            Task::ResetState(path) => match fs::remove_file(&path) {
                Ok(()) => (),
                Err(e) => error!("failed to reset state {path:?}, {e:?}"),
            },
            Task::SaveConfig(path, cfg) => match cfg.save(&path) {
                Ok(()) => (),
                Err(e) => error!("failed to save config {e:?}"),
            },
            // log write failures go to stderr, logging them would recurse
            Task::WriteLog(buf) => match Chars::from_bytes(buf) {
                Err(e) => eprintln!("invalid unicode log {e:?}"),
                Ok(buf) => {
                    if let Err(e) = logs.write_log(buf).await {
                        eprintln!("could not write log line {e:?}")
                    }
                }
            },
            Task::LogPerf {
                players,
                perf,
                api_perf,
            } => {
                logs.log_perf(players, &perf.stat(), &api_perf.stat()).await;
            }
            Task::Shutdown(a) => {
                println!("starting netidx shutdown");
                logs.shutdown().await;
                println!("netidx shutdown complete");
                let &(ref lock, ref cvar) = &*a;
                let mut synced = lock.lock();
                *synced = true;
                cvar.notify_all();
                println!("condvar signaled, exiting background loop");
                break;
            }
            Task::Stat(st) => {
                if let Err(e) = logs.write_stat(&st) {
                    eprintln!("could not write stat {st:?} {e:?}")
                }
            }
        }
    }
}

/// The sender for the single background loop. The background thread and
/// global logger are only created once per process, even if the mission is
/// reloaded and [`init`] is called again.
static TXCOM: OnceCell<mpsc::UnboundedSender<Task>> = OnceCell::new();

/// Install the global logger, sending its output to the background loop.
/// The level comes from `RUST_LOG` (trace/debug/info/warn/error/off),
/// defaulting to debug if unset or unrecognized. Panics if a logger is
/// already installed.
fn setup_logger(tx: UnboundedSender<Task>) {
    let level = match env::var("RUST_LOG").ok().map(|s| s.to_ascii_lowercase()) {
        None => LevelFilter::Debug,
        Some(s) if &s == "trace" => LevelFilter::Trace,
        Some(s) if &s == "debug" => LevelFilter::Debug,
        Some(s) if &s == "info" => LevelFilter::Info,
        Some(s) if &s == "error" => LevelFilter::Error,
        Some(s) if &s == "warn" => LevelFilter::Warn,
        Some(s) if &s == "off" => LevelFilter::Off,
        Some(_) => LevelFilter::Debug,
    };
    WriteLogger::init(level, simplelog::Config::default(), LogHandle(tx))
        .expect("could not init logger")
}

/// Get the sender for the background loop, starting it on first call.
///
/// The first call installs the logger and spawns an OS thread running a
/// multi-threaded tokio runtime that drives [`background_loop`].
/// `write_dir` is the DCS write directory (e.g. `Saved Games/DCS`); it is
/// ignored on subsequent calls.
pub(super) fn init(write_dir: PathBuf) -> UnboundedSender<Task> {
    match TXCOM.get() {
        Some(tx) => tx.clone(),
        None => {
            let (tx, rx) = mpsc::unbounded_channel();
            TXCOM.set(tx.clone()).expect("txcom is already set");
            setup_logger(tx.clone());
            thread::spawn(move || {
                let rt = Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("could not initialize async runtime");
                rt.block_on(background_loop(write_dir, rx));
                println!("background thread exiting")
            });
            tx
        }
    }
}
