//! Sampled textures for the UI pass (white, font atlas, thumbnails) and the
//! byte-budgeted LRU behind the thumbnail cache.

use super::util::{Garbage, Image};
use ash::vk;
use std::collections::HashMap;

/// A sampled RGBA/R8 texture with its descriptor set.
pub(crate) struct Texture {
    pub image: Image,
    pub set: vk::DescriptorSet,
}

impl Texture {
    pub fn into_garbage(self, out: &mut Vec<Garbage>) {
        out.push(Garbage::DescriptorSet(self.set));
        out.push(self.image.into_garbage());
    }
}

struct Entry<T> {
    value: T,
    bytes: usize,
    last_used: u64,
}

/// Map from image id to value with a total byte budget; least recently used
/// entries are evicted first, but never ones touched in the current frame
/// (they may still be referenced by commands being recorded).
pub(crate) struct LruBudget<T> {
    map: HashMap<u64, Entry<T>>,
    used: usize,
    budget: usize,
}

impl<T> LruBudget<T> {
    pub fn new(budget: usize) -> Self {
        LruBudget {
            map: HashMap::new(),
            used: 0,
            budget,
        }
    }

    pub fn contains(&self, id: u64) -> bool {
        self.map.contains_key(&id)
    }

    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// Look up and mark used in `frame`.
    pub fn touch(&mut self, id: u64, frame: u64) -> Option<&T> {
        self.map.get_mut(&id).map(|e| {
            e.last_used = frame;
            &e.value
        })
    }

    /// Insert (replacing an existing value, which is returned in `evicted`)
    /// and evict LRU entries until within budget.
    pub fn insert(&mut self, id: u64, value: T, bytes: usize, frame: u64, evicted: &mut Vec<T>) {
        if let Some(old) = self.map.remove(&id) {
            self.used -= old.bytes;
            evicted.push(old.value);
        }
        self.map.insert(
            id,
            Entry {
                value,
                bytes,
                last_used: frame,
            },
        );
        self.used += bytes;
        while self.used > self.budget {
            let victim = self
                .map
                .iter()
                .filter(|(_, e)| e.last_used < frame)
                .min_by_key(|(_, e)| e.last_used)
                .map(|(&k, _)| k);
            match victim {
                Some(k) => {
                    let e = self.map.remove(&k).expect("victim present");
                    self.used -= e.bytes;
                    evicted.push(e.value);
                }
                None => break,
            }
        }
    }

    pub fn remove(&mut self, id: u64) -> Option<T> {
        self.map.remove(&id).map(|e| {
            self.used -= e.bytes;
            e.value
        })
    }

    pub fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.used = 0;
        self.map.drain().map(|(_, e)| e.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_evicts_oldest_within_budget() {
        let mut c = LruBudget::new(100);
        let mut ev = Vec::new();
        c.insert(1, "a", 40, 1, &mut ev);
        c.insert(2, "b", 40, 2, &mut ev);
        assert!(ev.is_empty());
        c.touch(1, 3);
        c.insert(3, "c", 40, 4, &mut ev);
        assert_eq!(ev, vec!["b"]);
        assert!(c.contains(1) && c.contains(3) && !c.contains(2));
        assert_eq!(c.used_bytes(), 80);
        // Replacing returns the old value.
        ev.clear();
        c.insert(3, "c2", 10, 5, &mut ev);
        assert_eq!(ev, vec!["c"]);
        assert_eq!(c.used_bytes(), 50);
        assert_eq!(c.remove(1), Some("a"));
        assert_eq!(c.used_bytes(), 10);
    }

    #[test]
    fn lru_never_evicts_current_frame() {
        let mut c = LruBudget::new(50);
        let mut ev = Vec::new();
        c.insert(1, 1, 40, 7, &mut ev);
        c.insert(2, 2, 40, 7, &mut ev);
        assert!(
            ev.is_empty(),
            "both used this frame; over budget is tolerated"
        );
        c.insert(3, 3, 10, 8, &mut ev);
        assert_eq!(ev.len(), 1, "evicts just enough to fit");
        assert_eq!(c.used_bytes(), 50);
        assert_eq!(c.drain().count(), 2);
        assert_eq!(c.used_bytes(), 0);
    }
}
