//! Eager lowering output and on-demand decoding from immutable function archives.
//! A body is constructed at most once per instance, on the interpreter or JIT thread that needs it.

use super::*;

pub struct FuncTable {
    storage: FuncStorage,
}

enum FuncStorage {
    Eager(Vec<FuncBody>),
    Lazy(LazyFuncs),
}

struct LazyFuncs {
    archive: std::sync::Arc<crate::vm::archive::functions::Functions>,
    cells: Box<[std::sync::OnceLock<FuncBody>]>,
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
    pub(crate) fn from_archive(
        archive: std::sync::Arc<crate::vm::archive::functions::Functions>,
    ) -> Self {
        let cells = (0..archive.len())
            .map(|_| std::sync::OnceLock::new())
            .collect();
        Self {
            storage: FuncStorage::Lazy(LazyFuncs { archive, cells }),
        }
    }

    /// Settle every publish slot before fork. The child must never inherit a OnceLock whose
    /// initializer was a JIT thread that did not survive the fork.
    pub(crate) fn prepare_for_fork(&self) {
        if let FuncStorage::Lazy(lazy) = &self.storage {
            for index in 0..lazy.cells.len() {
                lazy.get(index);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn decoded_count(&self) -> usize {
        match &self.storage {
            FuncStorage::Eager(funcs) => funcs.len(),
            FuncStorage::Lazy(lazy) => lazy
                .cells
                .iter()
                .filter(|cell| cell.get().is_some())
                .count(),
        }
    }

    pub fn len(&self) -> usize {
        match &self.storage {
            FuncStorage::Eager(funcs) => funcs.len(),
            FuncStorage::Lazy(lazy) => lazy.archive.len(),
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
        self.cells[index].get_or_init(|| self.archive.decode(index))
    }
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
