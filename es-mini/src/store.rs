//! The indices behind the API, each behind its own lock, so writes to different indices and
//! reads proceed concurrently. Locks are taken in one order: the map, then indices by name.

use es_index::aliases::{Alias, IndexMeta};
use es_index::{Index, Indices};
use std::collections::BTreeMap;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

type Map = BTreeMap<String, Slot>;

/// An index behind its lock, beside the metadata index expressions resolve against, so
/// resolving locks no index. Only [`Exclusive`] changes that metadata.
pub(crate) struct Slot {
    aliases: BTreeMap<String, Alias>,
    closed: bool,
    index: RwLock<Index>,
}

impl IndexMeta for Slot {
    fn aliases(&self) -> &BTreeMap<String, Alias> {
        &self.aliases
    }
    fn closed(&self) -> bool {
        self.closed
    }
}

pub(crate) struct Store(RwLock<Map>);

impl Store {
    pub fn new(indices: Indices) -> Self {
        Self(RwLock::new(wrap(indices)))
    }

    /// Shared access: no index is created or deleted, and no alias or setting changes, until
    /// it is dropped. Documents change only through [`Shared::write`].
    pub fn read(&self) -> Shared<'_> {
        Shared(self.0.read().expect("lock poisoned"))
    }

    /// Exclusive access to every index, for changes to the set of indices or their metadata.
    pub fn write(&self) -> Exclusive<'_> {
        let mut map = self.0.write().expect("lock poisoned");
        let indices = std::mem::take(&mut *map)
            .into_iter()
            .map(|(name, slot)| (name, slot.index.into_inner().expect("lock poisoned")))
            .collect();
        Exclusive { map, indices }
    }
}

fn wrap(indices: Indices) -> Map {
    indices
        .into_iter()
        .map(|(name, ix)| {
            let (aliases, closed) = (ix.aliases.clone(), ix.closed);
            (
                name,
                Slot {
                    aliases,
                    closed,
                    index: RwLock::new(ix),
                },
            )
        })
        .collect()
}

pub(crate) struct Shared<'a>(RwLockReadGuard<'a, Map>);

/// Every index's metadata, for resolving index expressions without locking any index.
impl std::ops::Deref for Shared<'_> {
    type Target = Map;
    fn deref(&self) -> &Map {
        &self.0
    }
}

impl Shared<'_> {
    /// Every index, read-locked, so it waits for writes in progress on any of them.
    pub fn all(&self) -> BTreeMap<String, Locked<'_>> {
        self.lock(self.0.keys())
    }

    /// Indices `names`, read-locked; other indices stay free. Drop them before calling
    /// [`Shared::write`] on one of them.
    pub fn lock<'n>(
        &self,
        names: impl IntoIterator<Item = &'n String>,
    ) -> BTreeMap<String, Locked<'_>> {
        (names.into_iter())
            .filter_map(|name| Some((name.clone(), self.read(name)?)))
            .collect()
    }

    /// Index `name`, read-locked; other indices stay free.
    pub fn read(&self, name: &str) -> Option<Locked<'_>> {
        Some(Locked(
            self.0.get(name)?.index.read().expect("lock poisoned"),
        ))
    }

    /// Index `name`, write-locked; other indices stay free. Its aliases and `closed` must not
    /// change: only [`Store::write`] changes them.
    pub fn write(&self, name: &str) -> Option<RwLockWriteGuard<'_, Index>> {
        Some(self.0.get(name)?.index.write().expect("lock poisoned"))
    }
}

/// A read-locked index.
pub(crate) struct Locked<'a>(RwLockReadGuard<'a, Index>);

impl std::ops::Deref for Locked<'_> {
    type Target = Index;
    fn deref(&self) -> &Index {
        &self.0
    }
}

impl std::borrow::Borrow<Index> for Locked<'_> {
    fn borrow(&self) -> &Index {
        &self.0
    }
}

impl IndexMeta for Locked<'_> {
    fn aliases(&self) -> &BTreeMap<String, Alias> {
        &self.0.aliases
    }
    fn closed(&self) -> bool {
        self.0.closed
    }
}

/// Every index, moved out of its lock while the map is write-locked.
pub(crate) struct Exclusive<'a> {
    map: RwLockWriteGuard<'a, Map>,
    indices: Indices,
}

impl std::ops::Deref for Exclusive<'_> {
    type Target = Indices;
    fn deref(&self) -> &Indices {
        &self.indices
    }
}

impl std::ops::DerefMut for Exclusive<'_> {
    fn deref_mut(&mut self) -> &mut Indices {
        &mut self.indices
    }
}

impl Drop for Exclusive<'_> {
    fn drop(&mut self) {
        *self.map = wrap(std::mem::take(&mut self.indices));
    }
}
