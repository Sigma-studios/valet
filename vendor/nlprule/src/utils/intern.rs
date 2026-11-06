//! Shares identical values that loading creates many times over (the same
//! regex, part-of-speech table or word set in thousands of rules): one copy
//! behind an `Arc`. The pools hold weak references, so nothing is kept
//! alive by them.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, Weak};

pub(crate) type Pool<T> = Mutex<HashMap<u64, Vec<Weak<T>>>>;

/// The shared copy of `value` in `pool`, made if there is none.
pub(crate) fn intern<T: Hash + Eq>(pool: &Pool<T>, value: T) -> Arc<T> {
    let mut h = DefaultHasher::new();
    value.hash(&mut h);
    let key = h.finish();
    let mut map = pool.lock().unwrap_or_else(|e| e.into_inner());
    let list = map.entry(key).or_default();
    list.retain(|w| w.strong_count() > 0);
    for w in list.iter() {
        if let Some(shared) = w.upgrade() {
            if *shared == value {
                return shared;
            }
        }
    }
    let shared = Arc::new(value);
    list.push(Arc::downgrade(&shared));
    shared
}
