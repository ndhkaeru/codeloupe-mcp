//! Re-export the maintained implementation while satisfying Tantivy's 0.16 API requirement.
//! No implementation from the vulnerable 0.16 registry crate is included.
#![forbid(unsafe_code)]

pub use lru_fixed::*;

#[cfg(test)]
mod tests {
    use super::LruCache;
    use std::hash::{Hash, Hasher};
    use std::num::NonZeroUsize;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Key {
        id: usize,
        panic_on_drop: bool,
        trigger: Arc<AtomicBool>,
    }

    impl PartialEq for Key {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
        }
    }

    impl Eq for Key {}

    impl Hash for Key {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.id.hash(state);
        }
    }

    impl Drop for Key {
        fn drop(&mut self) {
            if self.panic_on_drop && self.trigger.swap(false, Ordering::SeqCst) {
                panic!("injected key destructor panic");
            }
        }
    }

    #[test]
    fn popping_a_panicking_key_leaves_the_cache_usable() {
        let trigger = Arc::new(AtomicBool::new(true));
        let key = |id, panic_on_drop| Key {
            id,
            panic_on_drop,
            trigger: Arc::clone(&trigger),
        };
        let mut cache = LruCache::new(NonZeroUsize::new(2).unwrap());
        cache.put(key(1, true), "one");
        assert!(catch_unwind(AssertUnwindSafe(|| cache.pop(&key(1, false)))).is_err());
        assert!(cache.is_empty());
        cache.put(key(2, false), "two");
        cache.put(key(3, false), "three");
        cache.put(key(4, false), "four");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&key(3, false)), Some(&"three"));
        assert_eq!(cache.pop(&key(4, false)), Some("four"));
    }
}
