//! A bounded map that keeps the entries in recent use.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;

/// A map holding at most `2 × generation` entries.
///
/// Inserts go to the current generation, and a hit in the previous one
/// moves the entry back into the current. When the current generation
/// fills it becomes the previous one and the old previous is dropped, so
/// an entry not used for a whole generation is forgotten. Every
/// operation is O(1). The mover's directory caches use it so that their
/// memory does not grow with the number of directories in the tree: a
/// forgotten directory costs one more LOOKUP or MKDIR the next time it
/// is needed, never a wrong answer.
#[derive(Debug)]
pub(crate) struct Generational<K, V> {
    current: HashMap<K, V>,
    previous: HashMap<K, V>,
    generation: usize,
}

impl<K: Eq + Hash, V: Clone> Generational<K, V> {
    pub(crate) fn new(generation: usize) -> Self {
        Self {
            current: HashMap::new(),
            previous: HashMap::new(),
            generation: generation.max(1),
        }
    }

    /// The value for `key`, moved into the current generation if it was
    /// in the previous one.
    pub(crate) fn get<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(value) = self.current.get(key) {
            return Some(value.clone());
        }
        let (key, value) = self.previous.remove_entry(key)?;
        self.insert(key, value.clone());
        Some(value)
    }

    pub(crate) fn insert(&mut self, key: K, value: V) {
        if self.current.len() >= self.generation && !self.current.contains_key(&key) {
            self.previous = std::mem::take(&mut self.current);
        }
        self.previous.remove(&key);
        self.current.insert(key, value);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.current.len() + self.previous.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holds_at_most_two_generations() {
        let mut c = Generational::new(100);
        for i in 0..10_000u32 {
            c.insert(i, i);
            assert!(c.len() <= 200, "{} entries after {i}", c.len());
        }
        // The latest inserts are all still there; the oldest are gone.
        assert!((9_900..10_000).all(|i| c.get(&i) == Some(i)));
        assert_eq!(c.get(&0), None);
    }

    #[test]
    fn an_entry_in_use_survives_rotation() {
        let mut c = Generational::new(10);
        c.insert(String::from("/root"), 1);
        for i in 0..1_000 {
            c.insert(format!("/d{i}"), 0);
            // Touched once a generation, the hot entry is never dropped.
            if i % 5 == 0 {
                assert_eq!(c.get("/root"), Some(1), "after {i}");
            }
        }
        assert_eq!(c.get("/d0"), None, "an entry nobody touched is forgotten");
    }

    #[test]
    fn a_hit_in_the_previous_generation_moves_it_forward() {
        let mut c = Generational::new(2);
        c.insert("a", 1);
        c.insert("b", 2);
        c.insert("c", 3); // rotates: previous = {a, b}, current = {c}
        assert_eq!(c.get("a"), Some(1)); // a moves to current
        c.insert("d", 4); // rotates: previous = {c, a}, current = {d}; b dropped
        assert_eq!(c.get("b"), None);
        assert_eq!(c.get("a"), Some(1));
        assert_eq!(c.get("c"), Some(3));
    }

    #[test]
    fn reinserting_a_key_keeps_one_copy() {
        let mut c = Generational::new(2);
        c.insert("a", 1);
        c.insert("b", 2);
        c.insert("c", 3); // previous = {a, b}
        c.insert("a", 9); // into current, removed from previous
        assert_eq!(c.len(), 3);
        assert_eq!(c.get("a"), Some(9));
    }
}
