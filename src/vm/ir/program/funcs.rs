//! Eager lowering output and on-demand decoding from immutable function archives.
//! A body is constructed at most once per instance, on the interpreter or JIT thread that needs it.

use super::*;

pub struct FuncTable {
    parts: Vec<Part>,
}

struct Part {
    end: usize,
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

/// Borrow a table's storage without initializing its demand-decoded bodies.
pub(crate) enum FuncPart<'a> {
    Owned(&'a [FuncBody]),
    Archived(&'a crate::vm::archive::functions::Functions),
}

impl Default for FuncTable {
    fn default() -> Self {
        Self::from(Vec::new())
    }
}

impl From<Vec<FuncBody>> for FuncTable {
    fn from(funcs: Vec<FuncBody>) -> Self {
        Self {
            parts: vec![Part {
                end: funcs.len(),
                storage: FuncStorage::Eager(funcs),
            }],
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
            parts: vec![Part {
                end: archive.len(),
                storage: FuncStorage::Lazy(LazyFuncs { archive, cells }),
            }],
        }
    }

    /// Layer ids already occupy consecutive ranges. Moving the owners preserves both those ids
    /// and any initialized decode cells; no function body needs to be read during the merge.
    pub(crate) fn append(&mut self, other: Self) {
        let offset = self.len();
        self.parts
            .extend(other.parts.into_iter().filter_map(|mut part| {
                let len = match &part.storage {
                    FuncStorage::Eager(funcs) => funcs.len(),
                    FuncStorage::Lazy(lazy) => lazy.archive.len(),
                };
                part.end += offset;
                (len != 0).then_some(part)
            }));
    }

    pub(crate) fn parts(&self) -> impl Iterator<Item = FuncPart<'_>> {
        self.parts.iter().map(|part| match &part.storage {
            FuncStorage::Eager(funcs) => FuncPart::Owned(funcs),
            FuncStorage::Lazy(lazy) => FuncPart::Archived(&lazy.archive),
        })
    }

    /// Settle every publish slot before fork. The child must never inherit a OnceLock whose
    /// initializer was a JIT thread that did not survive the fork.
    pub(crate) fn prepare_for_fork(&self) {
        for part in &self.parts {
            if let FuncStorage::Lazy(lazy) = &part.storage {
                for index in 0..lazy.cells.len() {
                    lazy.get(index);
                }
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn decoded_count(&self) -> usize {
        self.parts
            .iter()
            .map(|part| match &part.storage {
                FuncStorage::Eager(funcs) => funcs.len(),
                FuncStorage::Lazy(lazy) => lazy
                    .cells
                    .iter()
                    .filter(|cell| cell.get().is_some())
                    .count(),
            })
            .sum()
    }

    pub fn len(&self) -> usize {
        self.parts.last().map_or(0, |part| part.end)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&FuncBody> {
        if index >= self.len() {
            return None;
        }
        let part_index = self.parts.partition_point(|part| part.end <= index);
        let start = part_index.checked_sub(1).map_or(0, |i| self.parts[i].end);
        let index = index - start;
        Some(match &self.parts[part_index].storage {
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
        let FuncStorage::Eager(funcs) = &mut self.parts[0].storage else {
            unreachable!()
        };
        funcs.iter_mut()
    }

    pub fn push(&mut self, body: FuncBody) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.parts[0].storage else {
            unreachable!()
        };
        funcs.push(body);
        self.parts[0].end += 1;
    }

    pub fn drain_into(&mut self, out: &mut Vec<FuncBody>) {
        self.make_eager();
        let FuncStorage::Eager(funcs) = &mut self.parts[0].storage else {
            unreachable!()
        };
        out.append(funcs);
        self.parts[0].end = 0;
    }

    fn make_eager(&mut self) {
        if self.parts.len() == 1 && matches!(self.parts[0].storage, FuncStorage::Eager(_)) {
            return;
        }
        let mut funcs = Vec::with_capacity(self.len());
        for part in std::mem::take(&mut self.parts) {
            match part.storage {
                FuncStorage::Eager(mut owned) => funcs.append(&mut owned),
                FuncStorage::Lazy(lazy) => {
                    funcs.extend(
                        lazy.cells
                            .into_vec()
                            .into_iter()
                            .enumerate()
                            .map(|(i, cell)| {
                                cell.into_inner().unwrap_or_else(|| lazy.archive.decode(i))
                            }),
                    );
                }
            }
        }
        *self = Self::from(funcs);
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
