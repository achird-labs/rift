//! An imposter's recorded requests as `--datadir` keeps them in `{port}.requests.jsonl` (issue
//! #1608).
//!
//! Mountebank's datadir is its journal's live store: each recorded request is a file, written
//! before the request is matched, and a restart reads them all back. Rift keeps the journal in
//! memory and writes it behind instead, appending from the manager's flusher about once a second
//! and once more on a graceful stop, so the request path gains no write at all.
//!
//! The file is runtime state, not config: never part of `ImposterConfig`, so stored and replicated
//! config bytes, `GET /imposters` and `?replayable=true` are untouched, and a file that cannot be
//! read never refuses the imposter. It is JSON Lines: a header `{"version":1}`, then one
//! `{"flow":…,"request":…}` per entry in journal order, the request as `GET /imposters/:port` prints
//! it. The flow id is the one the entry was recorded under, which a scoped clear needs.
//!
//! An entry is journalled before it is matched and annotated after its response, so a flush holds
//! back at the first entry still without a status, for up to [`SETTLE_TICKS`] flushes; whatever
//! follows it waits too, so the file stays in journal order.
//!
//! Every write to the file holds [`JournalFile::state`]'s lock, and an imposter that has left the
//! manager is [detached](JournalFile::detach) under it first, so no write of a deleted or replaced
//! imposter can land after the delete removed its file (the #1124 shape). The lock is taken before
//! the journal's own, never after.

use crate::imposter::journal::{LocalJournal, RequestJournal as _};
use crate::imposter::types::RecordedRequest;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, BufRead as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The only version this engine writes and reads.
pub(crate) const REQUESTS_FILE_VERSION: u32 = 1;

/// How many flushes an entry still waiting for its response may hold the file back before it is
/// written without it.
pub(crate) const SETTLE_TICKS: u32 = 2;

/// [`SETTLE_TICKS`], for tests outside this module.
#[cfg(test)]
pub(crate) const fn settle_ticks() -> u32 {
    SETTLE_TICKS
}

/// The live size below which a file is never compacted, so a small journal is not rewritten for a
/// few evicted entries.
const COMPACT_FLOOR: u64 = 1024 * 1024;

/// How much a write serializes under the journal's read lock before it lets recorders in.
const CHUNK: usize = 4 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
struct Header {
    version: u32,
}

#[derive(Serialize)]
struct LineOut<'a> {
    flow: &'a str,
    request: &'a RecordedRequest,
}

#[derive(Deserialize)]
struct LineIn {
    flow: String,
    request: RecordedRequest,
}

/// Which entries a flush writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flush {
    /// The periodic flush: stops at an entry still waiting for its response.
    Tick,
    /// A graceful stop's: everything retained.
    Final,
}

/// What [`JournalFile::restore`] read back.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Restored {
    /// Entries recorded into the journal.
    pub(crate) entries: usize,
    /// Lines that could not be read (a bad header included), skipped with one warning.
    pub(crate) skipped: usize,
}

/// `{port}.requests.jsonl` for one imposter whose default journal is persisted.
pub(crate) struct JournalFile {
    port: u16,
    path: PathBuf,
    temp: PathBuf,
    journal: Arc<LocalJournal>,
    state: Mutex<FileState>,
}

impl std::fmt::Debug for JournalFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JournalFile")
            .field("port", &self.port)
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct FileState {
    /// The imposter has left the manager: nothing more is written.
    detached: bool,
    /// The next write replaces the file whole: this imposter has not written it yet, or what is on
    /// disk no longer matches `lines` (a failed or partial write, a deletion).
    rewrite: bool,
    /// The last journal index written. Every entry at or below it is ready by definition.
    persisted_through: u64,
    /// Each line on disk after the header, as (journal index, bytes with its newline).
    lines: VecDeque<(u64, u64)>,
    /// The sum of `lines`' bytes: what the file holds that the journal still retains.
    live_bytes: u64,
    /// The file's size, header included.
    file_bytes: u64,
    /// The entry the last flush stopped at, and how many flushes it has held back.
    waiting: Option<(u64, u32)>,
}

/// The per-flush rule for which entries are ready to write.
struct Cut<'s> {
    persisted_through: u64,
    flush: Flush,
    waiting: &'s mut Option<(u64, u32)>,
}

impl Cut<'_> {
    fn ready(&mut self, index: u64, request: &RecordedRequest) -> bool {
        if index <= self.persisted_through || self.flush == Flush::Final || request.status.is_some()
        {
            return true;
        }
        match self.waiting {
            Some((waiting, held)) if *waiting == index => {
                if *held >= SETTLE_TICKS {
                    return true;
                }
                *held += 1;
                false
            }
            _ => {
                *self.waiting = Some((index, 1));
                false
            }
        }
    }
}

impl JournalFile {
    pub(crate) fn new(datadir: &Path, port: u16, journal: Arc<LocalJournal>) -> Self {
        Self {
            port,
            path: crate::imposter::datadir_file::requests_path(datadir, port),
            temp: crate::imposter::datadir_file::requests_temp_path(datadir, port),
            journal,
            state: Mutex::new(FileState {
                detached: false,
                rewrite: true,
                persisted_through: 0,
                lines: VecDeque::new(),
                live_bytes: 0,
                file_bytes: 0,
                waiting: None,
            }),
        }
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn journal(&self) -> &LocalJournal {
        &self.journal
    }

    /// Whether a flush has anything to do; cheap, so the flusher skips an idle imposter without
    /// leaving the runtime.
    pub(crate) fn has_work(&self) -> bool {
        let state = self.state.lock();
        !state.detached
            && (state.rewrite || self.journal.last_index(self.port) > state.persisted_through)
    }

    /// Stop writing: the imposter has left the manager. Waits for a write in progress.
    pub(crate) fn detach(&self) {
        self.state.lock().detached = true;
    }

    /// Undo [`detach`](Self::detach) for an imposter whose delete failed and that still serves;
    /// the next flush rewrites the file, which the failed delete may have removed.
    pub(crate) fn reattach(&self) {
        let mut state = self.state.lock();
        state.detached = false;
        state.rewrite = true;
    }

    /// Remove what an earlier imposter on the port left: this one starts with its own journal.
    pub(crate) fn reset(&self) -> io::Result<()> {
        let mut state = self.state.lock();
        state.lines.clear();
        state.live_bytes = 0;
        state.file_bytes = 0;
        state.persisted_through = 0;
        state.rewrite = true;
        if state.detached {
            return Ok(());
        }
        remove_files(&self.temp, &self.path)
    }

    /// Write behind what the journal recorded since the last flush; see [`Flush`]. Blocking.
    pub(crate) fn flush(&self, flush: Flush) -> io::Result<()> {
        let mut state = self.state.lock();
        if state.detached {
            return Ok(());
        }
        self.forget_evicted(&mut state);
        if state.rewrite || state.file_bytes > 2 * state.live_bytes.max(COMPACT_FLOOR) {
            return self.rewrite(&mut state, flush);
        }
        self.append(&mut state, flush)
    }

    /// `DELETE …/savedRequests`: remove the file, then run `clear` on the journal. A file that
    /// cannot be removed fails the clear with the journal untouched, so `Err` keeps meaning "not
    /// cleared" across a restart too (issue #330). `persist` is whether the imposter is a datadir one.
    pub(crate) fn clear(
        &self,
        persist: bool,
        clear: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let mut state = self.state.lock();
        if persist && !state.detached {
            remove_files(&self.temp, &self.path).map_err(|e| {
                anyhow::Error::new(e).context(format!(
                    "could not remove the recorded requests file {:?}",
                    self.path
                ))
            })?;
        }
        // Read before the clear: an entry recorded after it gets a higher index and is written.
        let through = self.journal.last_index(self.port);
        clear()?;
        state.lines.clear();
        state.live_bytes = 0;
        state.file_bytes = 0;
        state.persisted_through = through;
        state.waiting = None;
        state.rewrite = true;
        Ok(())
    }

    /// A scoped deletion (`retain`, `clear_flow`): run `delete` on the journal, then rewrite the
    /// file from what it kept, so a restart does not bring the deleted entries back. A failed rewrite
    /// is returned and left for the next flush to retry.
    pub(crate) fn deleting<R>(
        &self,
        persist: bool,
        delete: impl FnOnce() -> R,
    ) -> (R, io::Result<()>) {
        let mut state = self.state.lock();
        let result = delete();
        state.rewrite = true;
        if !persist || state.detached {
            return (result, Ok(()));
        }
        self.forget_evicted(&mut state);
        let written = self.rewrite(&mut state, Flush::Tick);
        (result, written)
    }

    /// Read `{port}.requests.jsonl` back into the journal, for an imposter loaded from its datadir
    /// file before it serves. Each entry is recorded as if live, so the journal's byte budget
    /// applies; the request count is not touched, as Mountebank's `numberOfRequests` starts over.
    /// A truncated last line (a crash mid-append) is dropped silently; any other unreadable line is
    /// skipped and counted. A file of another version is ignored whole. Blocking.
    pub(crate) fn restore(&self) -> io::Result<Restored> {
        let mut state = self.state.lock();
        let file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            // Domain-optional: an imposter that never recorded, or an older engine's datadir.
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Restored::default()),
            Err(e) => return Err(e),
        };
        let mut reader = io::BufReader::new(file);
        let mut restored = Restored::default();
        let mut line = Vec::new();
        let mut header = true;
        // Whether what is on disk differs from what `lines` will describe.
        let mut stale = false;
        let mut lines = VecDeque::new();
        let mut file_bytes = 0u64;
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            file_bytes += read as u64;
            let complete = line.last() == Some(&b'\n');
            if header {
                header = false;
                match serde_json::from_slice::<Header>(&line) {
                    Ok(h) if h.version == REQUESTS_FILE_VERSION => continue,
                    Ok(h) => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "unsupported version {} (this engine reads version \
                                 {REQUESTS_FILE_VERSION})",
                                h.version
                            ),
                        ));
                    }
                    // Not a header: skipped, and the lines after it are still read.
                    Err(_) => {
                        restored.skipped += 1;
                        stale = true;
                        continue;
                    }
                }
            }
            match serde_json::from_slice::<LineIn>(&line) {
                Ok(entry) => {
                    if let Some(index) =
                        self.journal
                            .record_indexed(self.port, &entry.flow, entry.request)
                    {
                        lines.push_back((index, read as u64));
                    }
                    restored.entries += 1;
                    // A last line with no newline: the next append would run into it.
                    stale |= !complete;
                }
                // A crash mid-append leaves a partial last line: expected, not worth a warning.
                Err(_) if !complete => stale = true,
                Err(_) => {
                    restored.skipped += 1;
                    stale = true;
                }
            }
        }
        if header {
            // Empty: nothing on disk to describe.
            stale = true;
        }
        state.live_bytes = lines.iter().map(|l| l.1).sum();
        state.persisted_through = lines.back().map_or(0, |l| l.0);
        state.lines = lines;
        state.file_bytes = file_bytes;
        state.rewrite = stale;
        self.forget_evicted(&mut state);
        if state.rewrite || state.live_bytes + header_len() as u64 != state.file_bytes {
            // The budget evicted some, or the file is not exactly its readable lines: compact once.
            self.rewrite(&mut state, Flush::Final)?;
        }
        Ok(restored)
    }

    /// Drop the lines whose entries the journal no longer retains (evicted by its budget).
    fn forget_evicted(&self, state: &mut FileState) {
        let oldest = self
            .journal
            .oldest_index(self.port)
            .unwrap_or_else(|| self.journal.last_index(self.port) + 1);
        while let Some(&(index, bytes)) = state.lines.front()
            && index < oldest
        {
            state.lines.pop_front();
            state.live_bytes -= bytes;
        }
    }

    /// Serialize each ready entry above `after` into `out`, in chunks, returning the lines written.
    fn emit(
        &self,
        after: u64,
        cut: &mut Cut<'_>,
        out: &mut impl io::Write,
    ) -> io::Result<Vec<(u64, u64)>> {
        let mut lines = Vec::new();
        let mut last = after;
        let mut buf = Vec::new();
        loop {
            let mut full = false;
            let mut failed = None;
            self.journal
                .scan_after(self.port, last, |index, flow, request| {
                    if !cut.ready(index, request) {
                        return false;
                    }
                    let start = buf.len();
                    if let Err(e) = serde_json::to_writer(&mut buf, &LineOut { flow, request }) {
                        failed = Some(e);
                        return false;
                    }
                    buf.push(b'\n');
                    lines.push((index, (buf.len() - start) as u64));
                    last = index;
                    full = buf.len() >= CHUNK;
                    !full
                });
            if let Some(e) = failed {
                return Err(io::Error::other(e));
            }
            out.write_all(&buf)?;
            buf.clear();
            if !full {
                return Ok(lines);
            }
        }
    }

    fn append(&self, state: &mut FileState, flush: Flush) -> io::Result<()> {
        if self.journal.last_index(self.port) <= state.persisted_through {
            return Ok(());
        }
        let mut file = match std::fs::OpenOptions::new().append(true).open(&self.path) {
            Ok(file) => file,
            // Removed behind this engine's back: start it over, header first.
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                state.rewrite = true;
                return self.rewrite(state, flush);
            }
            Err(e) => return Err(e),
        };
        let mut cut = Cut {
            persisted_through: state.persisted_through,
            flush,
            waiting: &mut state.waiting,
        };
        let written = self
            .emit(state.persisted_through, &mut cut, &mut file)
            .and_then(|lines| {
                if !lines.is_empty() {
                    file.sync_data()?;
                }
                Ok(lines)
            });
        match written {
            Ok(lines) => {
                for (index, bytes) in lines {
                    state.lines.push_back((index, bytes));
                    state.live_bytes += bytes;
                    state.file_bytes += bytes;
                    state.persisted_through = index;
                }
                Ok(())
            }
            Err(e) => {
                // Part of a line may be on disk: replace the file next time rather than append to it.
                state.rewrite = true;
                Err(e)
            }
        }
    }

    /// Replace the file with a header and every ready retained entry, through `{port}.requests.jsonl.tmp`;
    /// remove it instead when there is no entry to write.
    fn rewrite(&self, state: &mut FileState, flush: Flush) -> io::Result<()> {
        let written = self.write_temp(state, flush);
        let lines = match written {
            Ok(Some(lines)) => std::fs::rename(&self.temp, &self.path).map(|()| lines),
            Ok(None) => remove_files(&self.temp, &self.path).map(|()| Vec::new()),
            Err(e) => Err(e),
        };
        match lines {
            Ok(lines) => {
                state.live_bytes = lines.iter().map(|l| l.1).sum();
                state.file_bytes = if lines.is_empty() {
                    0
                } else {
                    state.live_bytes + header_len() as u64
                };
                if let Some(&(last, _)) = lines.last() {
                    state.persisted_through = state.persisted_through.max(last);
                }
                state.lines = lines.into();
                state.rewrite = false;
                Ok(())
            }
            Err(e) => {
                // Terminal last-resort: the caller gets the write's error, and a leftover temp file
                // is inert (never loaded, swept by the next start or replaced by the next rewrite).
                if let Err(cleanup) = std::fs::remove_file(&self.temp)
                    && cleanup.kind() != io::ErrorKind::NotFound
                {
                    tracing::warn!(error = %cleanup, temp = ?self.temp, "could not remove a failed write's temp file");
                }
                state.rewrite = true;
                Err(e)
            }
        }
    }

    /// The temp file with every ready retained entry, or `None` (and no temp file) when none is.
    fn write_temp(
        &self,
        state: &mut FileState,
        flush: Flush,
    ) -> io::Result<Option<Vec<(u64, u64)>>> {
        let mut file = io::BufWriter::new(std::fs::File::create(&self.temp)?);
        file.write_all(&header_line())?;
        let mut cut = Cut {
            persisted_through: state.persisted_through,
            flush,
            waiting: &mut state.waiting,
        };
        let lines = self.emit(0, &mut cut, &mut file)?;
        let file = file.into_inner().map_err(io::IntoInnerError::into_error)?;
        if lines.is_empty() {
            drop(file);
            std::fs::remove_file(&self.temp)?;
            return Ok(None);
        }
        file.sync_data()?;
        Ok(Some(lines))
    }
}

fn header_line() -> Vec<u8> {
    let mut line = serde_json::to_vec(&Header {
        version: REQUESTS_FILE_VERSION,
    })
    .expect("a header of one integer serializes");
    line.push(b'\n');
    line
}

fn header_len() -> usize {
    header_line().len()
}

/// Remove `temp` and `path`; an absent one is already the desired end state.
pub(crate) fn remove_files(temp: &Path, path: &Path) -> io::Result<()> {
    for path in [temp, path] {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
