//! 移植自 Miri 的 MonoHashMap（rust-lang/miri, MIT/Apache-2.0）：
//! 共享引用下可插入的"单调" map。Allocation 装箱后地址稳定，
//! 因此可以在不持有 RefCell borrow 的情况下发出内部引用。

use std::borrow::Borrow;
use std::cell::RefCell;
use std::collections::hash_map::Entry;
use std::hash::Hash;

use rustc_const_eval::interpret::AllocMap;
use rustc_data_structures::fx::FxHashMap;

#[derive(Debug, Clone)]
pub struct MonoHashMap<K: Hash + Eq, V>(RefCell<FxHashMap<K, Box<V>>>);

impl<K: Hash + Eq, V> Default for MonoHashMap<K, V> {
    fn default() -> Self {
        MonoHashMap(RefCell::new(Default::default()))
    }
}

impl<K: Hash + Eq, V> AllocMap<K, V> for MonoHashMap<K, V> {
    #[inline(always)]
    fn contains_key<Q: ?Sized + Hash + Eq>(&mut self, k: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.0.get_mut().contains_key(k)
    }

    #[inline(always)]
    fn contains_key_ref<Q: ?Sized + Hash + Eq>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
    {
        self.0.borrow().contains_key(k)
    }

    #[inline(always)]
    fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.0.get_mut().insert(k, Box::new(v)).map(|x| *x)
    }

    #[inline(always)]
    fn remove<Q: ?Sized + Hash + Eq>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
    {
        self.0.get_mut().remove(k).map(|x| *x)
    }

    #[inline(always)]
    fn filter_map_collect<T>(&self, mut f: impl FnMut(&K, &V) -> Option<T>) -> Vec<T> {
        self.0.borrow().iter().filter_map(move |(k, v)| f(k, v)).collect()
    }

    #[inline(always)]
    fn get_or<E>(&self, k: K, vacant: impl FnOnce() -> Result<V, E>) -> Result<&V, E> {
        // 不能在调用 `vacant` 时持有 borrow_mut：它可能反过来查这张表。
        if let Some(v) = self.0.borrow().get(&k) {
            let val: *const V = &**v;
            // 安全性：val 指向 Box 内部；只要 &self 存活，条目不会被删除或移动。
            return unsafe { Ok(&*val) };
        }
        let new_val = Box::new(vacant()?);
        let val: *const V = &**self.0.borrow_mut().try_insert(k, new_val).ok().unwrap();
        unsafe { Ok(&*val) }
    }

    fn get(&self, k: K) -> Option<&V> {
        let val: *const V = &**self.0.borrow().get(&k)?;
        unsafe { Some(&*val) }
    }

    #[inline(always)]
    fn get_mut_or<E>(&mut self, k: K, vacant: impl FnOnce() -> Result<V, E>) -> Result<&mut V, E> {
        match self.0.get_mut().entry(k) {
            Entry::Occupied(e) => Ok(e.into_mut()),
            Entry::Vacant(e) => {
                let v = vacant()?;
                Ok(e.insert(Box::new(v)))
            }
        }
    }
}
