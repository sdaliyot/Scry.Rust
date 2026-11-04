//! Generic helpers. Each is used with more than one concrete type argument somewhere in the app,
//! which is the case the registry generator has to discover from call sites.

pub trait Describe {
    fn describe(&self) -> String;
}

/// Used as `describe::<Item>` and `describe::<Stats>`.
pub fn describe<T: Describe>(value: &T) -> String {
    let type_name = std::any::type_name::<T>().rsplit("::").next().unwrap_or("?");
    format!("[{type_name}] {}", value.describe())
}

/// Used as `clamp_all::<i64>` and `clamp_all::<f64>`.
pub fn clamp_all<T: PartialOrd + Copy>(values: &mut [T], low: T, high: T) {
    for value in values.iter_mut() {
        if *value < low {
            *value = low;
        } else if *value > high {
            *value = high;
        }
    }
}

/// A tiny fixed-capacity cache, used as `Cache<u32, String>`.
#[cfg_attr(feature = "scry", derive(scry::Reflect))]
pub struct Cache<K, V> {
    entries: Vec<(K, V)>,
    capacity: usize,
    hits: u64,
    misses: u64,
}

impl<K: PartialEq, V: Clone> Cache<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self { entries: Vec::with_capacity(capacity), capacity, hits: 0, misses: 0 }
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        match self.entries.iter().find(|(k, _)| k == key) {
            Some((_, value)) => {
                self.hits += 1;
                Some(value.clone())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    pub fn put(&mut self, key: K, value: V) {
        if self.entries.len() == self.capacity {
            self.entries.remove(0);
        }
        self.entries.push((key, value));
    }
}

pub fn format_cents(cents: i64) -> String {
    format!("${}.{:02}", cents / 100, (cents % 100).abs())
}

/// A small linear congruential generator, so the demo needs no dependencies.
pub fn next_random(seed: u64) -> u64 {
    seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407) >> 1
}
