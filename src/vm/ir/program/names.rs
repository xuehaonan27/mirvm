//! Function names borrowed from an immutable archive, or owned while lowering and merging.

use std::sync::Arc;

pub struct FunctionNames(Storage);

enum Storage {
    Owned(Vec<Box<str>>),
    Archived(Arc<crate::vm::archive::functions::Functions>),
}

impl Default for FunctionNames {
    fn default() -> Self {
        Vec::new().into()
    }
}

impl From<Vec<Box<str>>> for FunctionNames {
    fn from(names: Vec<Box<str>>) -> Self {
        Self(Storage::Owned(names))
    }
}

impl FromIterator<Box<str>> for FunctionNames {
    fn from_iter<T: IntoIterator<Item = Box<str>>>(iter: T) -> Self {
        iter.into_iter().collect::<Vec<_>>().into()
    }
}

impl FunctionNames {
    pub(crate) fn archived(archive: Arc<crate::vm::archive::functions::Functions>) -> Self {
        Self(Storage::Archived(archive))
    }

    pub fn len(&self) -> usize {
        match &self.0 {
            Storage::Owned(names) => names.len(),
            Storage::Archived(archive) => archive.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, index: usize) -> Option<&str> {
        match &self.0 {
            Storage::Owned(names) => names.get(index).map(AsRef::as_ref),
            Storage::Archived(archive) => {
                (index < archive.len()).then(|| archive.body(index).name.as_ref())
            }
        }
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &str> + Clone {
        (0..self.len()).map(|index| self.get(index).unwrap())
    }

    pub fn drain_into(&mut self, out: &mut Vec<Box<str>>) {
        match std::mem::take(&mut self.0) {
            Storage::Owned(mut names) => out.append(&mut names),
            Storage::Archived(archive) => {
                out.extend(archive.bodies().map(|body| Box::from(body.name.as_ref())))
            }
        }
    }

    pub fn push(&mut self, name: Box<str>) {
        if !matches!(self.0, Storage::Owned(_)) {
            self.0 = Storage::Owned(self.iter().map(Box::from).collect());
        }
        let Storage::Owned(names) = &mut self.0 else {
            unreachable!()
        };
        names.push(name);
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self::Owned(Vec::new())
    }
}

impl std::ops::Index<usize> for FunctionNames {
    type Output = str;
    fn index(&self, index: usize) -> &str {
        self.get(index).expect("FuncId outside name table")
    }
}

impl std::fmt::Debug for FunctionNames {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl serde::Serialize for FunctionNames {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> serde::Deserialize<'de> for FunctionNames {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <Vec<Box<str>> as serde::Deserialize>::deserialize(deserializer).map(Self::from)
    }
}
