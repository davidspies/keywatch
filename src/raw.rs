use std::{collections::HashSet, future::pending, hash::Hash, mem, time::Duration};

use list_ordered_hashmap::{Entry, OrderedHashMap};
use tokio::time::Instant;

use crate::Update;

pub(super) struct Raw<K, V> {
    cooldown: Duration,
    cooldown_keys: OrderedHashMap<K, (Instant, Option<V>)>,
    pending_updates: OrderedHashMap<K, Update<V>>,
    keys_receiver_has: HashSet<K>,
}

impl<K, V> Raw<K, V> {
    pub(super) fn new(starting_keys: HashSet<K>, cooldown: Duration) -> Self {
        Self {
            cooldown,
            cooldown_keys: OrderedHashMap::new(),
            pending_updates: OrderedHashMap::new(),
            keys_receiver_has: starting_keys,
        }
    }
}

impl<K: Clone + Eq + Hash, V> Raw<K, V> {
    pub(super) fn send(&mut self, key: K, update: Update<V>) -> Option<V> {
        self.process_cooldown_keys(Instant::now());
        let Self {
            cooldown: _,
            cooldown_keys,
            pending_updates,
            keys_receiver_has,
        } = self;
        match update {
            Update::Add(v) => match cooldown_keys.entry(key) {
                Entry::Occupied(occupied_entry) => occupied_entry.into_mut().1.replace(v),
                Entry::Vacant(vacant_entry) => {
                    let key = vacant_entry.into_key();
                    let replaced = pending_updates.insert(key, Update::Add(v));
                    replaced.and_then(Update::into_option)
                }
            },
            Update::Delete => match pending_updates.entry(key) {
                Entry::Occupied(occupied_entry) => {
                    debug_assert!(!cooldown_keys.contains_key(occupied_entry.key()));
                    let replaced = if keys_receiver_has.contains(occupied_entry.key()) {
                        mem::replace(occupied_entry.into_mut(), Update::Delete)
                    } else {
                        occupied_entry.remove()
                    };
                    replaced.into_option()
                }
                Entry::Vacant(vacant_entry) => {
                    if !keys_receiver_has.contains(vacant_entry.key()) {
                        return None;
                    }
                    let mut replaced = None;
                    if let Some((_, v)) = cooldown_keys.get_mut(vacant_entry.key()) {
                        replaced = v.take();
                    }
                    vacant_entry.insert(Update::Delete);
                    replaced
                }
            },
        }
    }

    pub(super) fn clear(&mut self) {
        let Self {
            cooldown: _,
            cooldown_keys,
            pending_updates,
            keys_receiver_has,
        } = self;
        let now = Instant::now();
        loop {
            let Some(&(t, _)) = cooldown_keys.values().next() else {
                break;
            };
            if t > now {
                break;
            }
            cooldown_keys.pop_front();
        }
        cooldown_keys.for_each_mut(|_, (_, v)| *v = None);
        pending_updates.clear();
        for k in keys_receiver_has.iter() {
            pending_updates.insert(k.clone(), Update::Delete);
        }
    }

    pub(super) fn try_recv(&mut self) -> Option<(K, Update<V>)> {
        let now = Instant::now();
        self.process_cooldown_keys(now);
        let update = self.pending_updates.pop_front()?;
        self.update_receiver(&update, now);
        Some(update)
    }

    pub(super) fn try_recv_now(&mut self) -> Option<(K, Update<V>)> {
        let now = Instant::now();
        self.process_cooldown_keys(now);
        let update = match self.pending_updates.pop_front() {
            Some(update) => update,
            None => loop {
                let (k, (_, v)) = self.cooldown_keys.pop_front()?;
                if let Some(v) = v {
                    break (k, Update::Add(v));
                }
            },
        };
        self.update_receiver(&update, now);
        Some(update)
    }

    pub(super) fn wait_next_cooldown(&self) -> impl Future<Output = ()> + Send + 'static {
        let next_cooldown = self.cooldown_keys.values().next().map(|&(t, _)| t);
        async move {
            match next_cooldown {
                Some(t) => tokio::time::sleep_until(t).await,
                None => pending().await,
            }
        }
    }

    fn process_cooldown_keys(&mut self, now: Instant) {
        loop {
            let Some(&(t, _)) = self.cooldown_keys.values().next() else {
                break;
            };
            if t > now {
                break;
            }
            let (k, (_, v)) = self.cooldown_keys.pop_front().unwrap();
            if let Some(v) = v {
                self.pending_updates.insert(k, Update::Add(v));
            }
        }
    }

    fn update_receiver(&mut self, (k, v): &(K, Update<V>), now: Instant) {
        let Self {
            cooldown,
            cooldown_keys,
            pending_updates: _,
            keys_receiver_has,
        } = self;
        match v {
            Update::Add(_) => {
                if *cooldown > Duration::ZERO {
                    cooldown_keys.insert(k.clone(), (now + *cooldown, None));
                }
                keys_receiver_has.insert(k.clone());
            }
            Update::Delete => {
                keys_receiver_has.remove(k);
            }
        }
    }
}
