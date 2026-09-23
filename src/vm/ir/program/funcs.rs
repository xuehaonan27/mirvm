//! The function table: the eager bodies and the lazily decoded ones, the worker that decodes them
//! off the hot path, the heat order it records, and the iteration and serde views a compiled
//! artifact is persisted through.

use super::*;

/// The function table has two ownership modes. Lower and images hold decoded bodies in an ordinary
/// `Vec`; a `.mirvm` package holds only the immutable byte snapshot taken at load time, the per-function
/// slice bounds, and on-demand publish slots, decoding bodies lazily in a background worker. Readers
/// see one interface either way, through `len`/`get`/`index`/`iter`.
pub struct FuncTable {
    storage: FuncStorage,
}

enum FuncStorage {
    Eager(Vec<FuncBody>),
    Lazy(LazyFuncs),
}

struct LazyFuncs {
    state: std::sync::Arc<DecodeState>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct FuncBlob {
    pub start: usize,
    pub end: usize,
    pub expected_hash: u128,
}

struct DecodeState {
    map: std::sync::Arc<[u8]>,
    blobs: Box<[FuncBlob]>,
    cells: Box<[std::sync::OnceLock<Result<FuncBody, String>>]>,
    queue: std::sync::Mutex<DecodeQueue>,
    ready: std::sync::Condvar,
    done: std::sync::Condvar,
    access: std::sync::Mutex<(Vec<u32>, std::collections::BTreeSet<u32>)>,
    heat_path: std::path::PathBuf,
}

#[derive(Default)]
pub(in crate::vm::ir) struct DecodeQueue {
    pub(in crate::vm::ir) demand: std::collections::VecDeque<usize>,
    pub(in crate::vm::ir) predicted: std::collections::VecDeque<usize>,
    pub(in crate::vm::ir) queued: std::collections::BTreeSet<usize>,
    pub(in crate::vm::ir) stop: bool,
}

impl DecodeQueue {
    pub(in crate::vm::ir) fn request_demand(&mut self, index: usize) {
        if self.queued.insert(index) {
            self.demand.push_back(index);
        } else if let Some(at) = self.predicted.iter().position(|item| *item == index) {
            self.predicted.remove(at);
            self.demand.push_back(index);
        }
    }

    pub(in crate::vm::ir) fn pop_next(&mut self) -> Option<usize> {
        let index = self
            .demand
            .pop_front()
            .or_else(|| self.predicted.pop_front())?;
        self.queued.remove(&index);
        Some(index)
    }
}

impl Default for FuncTable {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl From<Vec<FuncBody>> for FuncTable {
    fn from(funcs: Vec<FuncBody>) -> Self {
        Self {
            storage: FuncStorage::Eager(funcs),
        }
    }
}

impl FromIterator<FuncBody> for FuncTable {
    fn from_iter<T: IntoIterator<Item = FuncBody>>(iter: T) -> Self {
        Self::from(iter.into_iter().collect::<Vec<_>>())
    }
}

impl FuncTable {
    pub(crate) fn from_bytes(
        map: std::sync::Arc<[u8]>,
        blobs: Vec<FuncBlob>,
        heat_path: std::path::PathBuf,
    ) -> Self {
        let predicted = read_heat_order(&heat_path, blobs.len());
        let state = std::sync::Arc::new(DecodeState {
            map,
            cells: (0..blobs.len())
                .map(|_| std::sync::OnceLock::new())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            blobs: blobs.into_boxed_slice(),
            queue: std::sync::Mutex::new(DecodeQueue {
                predicted: predicted.iter().copied().collect(),
                queued: predicted.into_iter().collect(),
                ..DecodeQueue::default()
            }),
            ready: std::sync::Condvar::new(),
            done: std::sync::Condvar::new(),
            access: std::sync::Mutex::new((Vec::new(), std::collections::BTreeSet::new())),
            heat_path,
        });
        let worker_state = std::sync::Arc::clone(&state);
        let worker = std::thread::Builder::new()
            .name("mirvm-decode".into())
            .spawn(move || decode_worker(worker_state))
            .ok();
        if worker.is_some() {
            state.ready.notify_one();
        }
        Self {
            storage: FuncStorage::Lazy(LazyFuncs { state, worker }),
        }
    }

    pub fn len(&self) -> usize {
        match &self.storage {
            FuncStorage::Eager(funcs) => funcs.len(),
            FuncStorage::Lazy(lazy) => lazy.state.blobs.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&FuncBody> {
        if index >= self.len() {
            return None;
        }
        Some(match &self.storage {
            FuncStorage::Eager(funcs) => &funcs[index],
            FuncStorage::Lazy(lazy) => lazy.get(index),
        })
    }

    pub fn iter(&self) -> FuncIter<'_> {
        FuncIter {
            funcs: self,
            next: 0,
        }
    }

    pub(super) fn iter_mut(&mut self) -> std::slice::IterMut<'_, FuncBody> {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        funcs.iter_mut()
    }

    pub fn push(&mut self, body: FuncBody) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        funcs.push(body);
    }

    pub fn drain_into(&mut self, out: &mut Vec<FuncBody>) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.storage else {
            unreachable!()
        };
        out.append(funcs);
    }

    pub(crate) fn flush_heat_order(&self) {
        if let FuncStorage::Lazy(lazy) = &self.storage {
            write_heat_order(&lazy.state);
        }
    }

    fn make_eager(&mut self) {
        if matches!(self.storage, FuncStorage::Eager(_)) {
            return;
        }
        let funcs = self.iter().cloned().collect();
        self.storage = FuncStorage::Eager(funcs);
    }
}

impl LazyFuncs {
    fn get(&self, index: usize) -> &FuncBody {
        record_access(&self.state, index as u32);
        if self.worker.is_none() {
            decode_one(&self.state, index);
        } else if self.state.cells[index].get().is_none() {
            let mut queue = self.state.queue.lock().unwrap();
            if self.state.cells[index].get().is_none() {
                queue.request_demand(index);
                self.state.ready.notify_one();
                while self.state.cells[index].get().is_none() {
                    queue = self.state.done.wait(queue).unwrap();
                }
            }
        }
        match self.state.cells[index]
            .get()
            .expect("function decode slot not published")
        {
            Ok(body) => body,
            Err(error) => panic!("verified function failed during lazy decode: {error}"),
        }
    }
}

impl Drop for LazyFuncs {
    fn drop(&mut self) {
        write_heat_order(&self.state);
        {
            let mut queue = self.state.queue.lock().unwrap();
            queue.stop = true;
            self.state.ready.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn decode_worker(state: std::sync::Arc<DecodeState>) {
    loop {
        let index = {
            let mut queue = state.queue.lock().unwrap();
            loop {
                if queue.stop {
                    return;
                }
                if let Some(index) = queue.pop_next() {
                    break index;
                }
                queue = state.ready.wait(queue).unwrap();
            }
        };
        decode_one(&state, index);
        state.done.notify_all();
    }
}

fn decode_one(state: &DecodeState, index: usize) {
    if state.cells[index].get().is_some() {
        return;
    }
    let blob = state.blobs[index];
    let bytes = &state.map[blob.start..blob.end];
    let decoded = if func_blob_hash(bytes) != blob.expected_hash {
        Err(format!(
            "function {index} changed after package verification"
        ))
    } else {
        postcard::from_bytes(bytes)
            .map_err(|error| format!("function {index} decode failed: {error}"))
    };
    let _ = state.cells[index].set(decoded);
}

fn func_blob_hash(data: &[u8]) -> u128 {
    let fnv = |prefix: &[u8]| {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in prefix.iter().chain(data) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    };
    ((fnv(&[]) as u128) << 64) | u128::from(fnv(b"\x01mirvmar"))
}

fn record_access(state: &DecodeState, id: u32) {
    let mut access = state.access.lock().unwrap();
    if access.1.insert(id) {
        access.0.push(id);
    }
}

fn read_heat_order(path: &std::path::Path, count: usize) -> Vec<usize> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    text.split_ascii_whitespace()
        .filter_map(|value| value.parse::<usize>().ok())
        .filter(|id| *id < count && seen.insert(*id))
        .collect()
}

fn write_heat_order(state: &DecodeState) {
    let access = state.access.lock().unwrap();
    if access.0.is_empty() {
        return;
    }
    let Some(dir) = state.heat_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let body = access
        .0
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    // Best effort: a heat file that cannot be published only costs the next run its learned
    // order, so the failure is swallowed rather than reported.
    let _ = crate::store::publish_bytes(&state.heat_path, body.as_bytes());
}

pub struct FuncIter<'a> {
    funcs: &'a FuncTable,
    next: usize,
}

impl<'a> Iterator for FuncIter<'a> {
    type Item = &'a FuncBody;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.funcs.get(self.next)?;
        self.next += 1;
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.funcs.len().saturating_sub(self.next);
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for FuncIter<'_> {}

impl<'a> IntoIterator for &'a FuncTable {
    type Item = &'a FuncBody;
    type IntoIter = FuncIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl std::ops::Index<usize> for FuncTable {
    type Output = FuncBody;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("FuncId outside function table")
    }
}

impl std::fmt::Debug for FuncTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl serde::Serialize for FuncTable {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> serde::Deserialize<'de> for FuncTable {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Vec<FuncBody> as serde::Deserialize>::deserialize(deserializer).map(Self::from)
    }
}
