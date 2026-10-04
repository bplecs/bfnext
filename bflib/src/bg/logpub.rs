//! Publish the campaign log over netidx.
//!
//! In netidx mode the log is still appended to the log file, and is also
//! published at `<base>/log` as a stream of line values. Each new
//! subscriber is first sent the whole log file, line by line, then receives
//! new lines as they are written. Lines are sent to individual subscribers
//! (`update_subscriber`) rather than as a shared current value, so each
//! subscriber sees every line exactly once.

use anyhow::Result;
use bytes::BytesMut;
use futures::{
    StreamExt,
    channel::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
    select_biased,
};
use fxhash::FxHashSet;
use log::error;
use netidx::{
    chars::Chars,
    path::Path,
    publisher::{Event, Publisher, Value},
};
use std::{io::SeekFrom, path::PathBuf, time::Duration};
use tokio::{
    fs::OpenOptions,
    io::{AsyncBufReadExt, AsyncSeekExt, AsyncWriteExt, BufReader},
    task,
};

/// Messages from [`LogPublisher`] to its [`logger_loop`] task.
enum ToLogger {
    /// Append a line (or lines) to the file and send it to subscribers.
    Log(Chars),
    /// Unpublish, close the file, then signal the sender.
    Close(oneshot::Sender<()>),
}

/// Own the log file and the published log value, handling subscriptions
/// and appends until closed or the input channel ends.
///
/// Returns an error if the file can't be opened, read or written, or the
/// path can't be published.
async fn logger_loop(
    publisher: Publisher,
    file_path: &PathBuf,
    netidx_path: Path,
    mut input: UnboundedReceiver<ToLogger>,
) -> Result<()> {
    let mut file = OpenOptions::new()
        .append(true)
        .create(true)
        .read(true)
        .write(true)
        .open(&file_path)
        .await?;
    let (tx, mut events) = mpsc::unbounded();
    let contents = publisher.publish(netidx_path, Value::Null)?;
    publisher.events_for_id(contents.id(), tx);
    let mut subs = FxHashSet::default();
    let mut batch = publisher.start_batch();
    let mut buf = String::new();
    let mut bytes = BytesMut::new();
    loop {
        // biased: handle pending (un)subscribes before appending new lines
        select_biased! {
            e = events.select_next_some() => match e {
                Event::Destroyed(_) => return Ok(()),
                Event::Unsubscribe(_, cl) => {
                    subs.remove(&cl);
                }
                Event::Subscribe(_, cl) => {
                    subs.insert(cl);
                    // replay the whole file to the new subscriber. the file is
                    // opened in append mode, so writes still go to the end
                    // regardless of where this leaves the cursor
                    file.seek(SeekFrom::Start(0)).await?;
                    let mut bufreader = BufReader::new(file);
                    buf.clear();
                    let mut n = 0;
                    loop {
                        if bufreader.read_line(&mut buf).await? == 0 {
                            break
                        }
                        bytes.extend_from_slice(buf.trim().as_bytes());
                        buf.clear();
                        // can't fail, the bytes came from a valid utf8 String
                        let chars = Chars::from_bytes(bytes.split().freeze()).unwrap();
                        contents.update_subscriber(&mut batch, cl, Value::String(chars));
                        n += 1;
                        // commit in chunks so a large log isn't buffered in
                        // one giant batch
                        if n >= 99 {
                            n = 0;
                            batch.commit(Some(Duration::from_secs(10))).await;
                            batch = publisher.start_batch();
                        }
                    }
                    file = bufreader.into_inner();
                    batch.commit(Some(Duration::from_secs(10))).await;
                    batch = publisher.start_batch();
                },
            },
            e = input.select_next_some() => match e {
                ToLogger::Log(b) => {
                    file.write_all_buf(&mut b.as_bytes()).await?;
                    bytes.extend_from_slice(b.trim().as_bytes());
                    let c = Chars::from_bytes(bytes.split().freeze()).unwrap();
                    for cl in &subs {
                        contents.update_subscriber(&mut batch, *cl, Value::String(c.clone()))
                    }
                    batch.commit(Some(Duration::from_secs(10))).await;
                    batch = publisher.start_batch();
                }
                ToLogger::Close(ch) => {
                    drop(contents);
                    drop(file);
                    let _ = ch.send(());
                    return Ok(())
                }
            },
            complete => return Ok(())
        }
    }
}

/// Handle to a background task that appends to `file_path` and publishes
/// the log at a netidx path. Cheap to clone; all clones feed the same task.
#[derive(Debug, Clone)]
pub struct LogPublisher(UnboundedSender<ToLogger>);

impl LogPublisher {
    /// Spawn the logger task. Must be called within a tokio runtime. Errors
    /// inside the task (e.g. failing to open the file) are only logged, they
    /// surface later as `append`/`close` failing because the task is gone.
    pub fn new(publisher: Publisher, file_path: &PathBuf, netidx_path: Path) -> Result<Self> {
        let (tx, rx) = mpsc::unbounded();
        let file_path = file_path.clone();
        task::spawn(async move {
            match logger_loop(publisher, &file_path, netidx_path, rx).await {
                Ok(()) => (),
                Err(e) => error!("{file_path:?} logger failed {e:?}"),
            }
        });
        Ok(Self(tx))
    }

    /// Queue `m` to be written. `m` should include its trailing newline;
    /// it is written to the file as is and published trimmed. Errors only if
    /// the logger task has exited.
    pub fn append(&self, m: Chars) -> Result<()> {
        Ok(self.0.unbounded_send(ToLogger::Log(m))?)
    }

    /// Ask the logger task to unpublish and close the file, and wait until it
    /// has. Lines queued before the close are written first.
    pub async fn close(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.0.unbounded_send(ToLogger::Close(tx))?;
        Ok(rx.await?)
    }
}
