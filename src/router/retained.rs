use crate::router::topic_matches;
use heapless::{String, Vec};
use mqttrs::QoS;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedEntry {
    pub topic: String<128>,
    pub payload: Vec<u8, 512>,
    pub qos: QoS,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetainedError {
    TopicTooLong,
    PayloadTooLarge,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedStore<const N: usize> {
    entries: Vec<RetainedEntry, N>,
}

impl<const N: usize> Default for RetainedStore<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RetainedStore<N> {
    pub const fn new() -> Self {
        Self { entries: Vec::new() }
    }

    pub fn set(&mut self, topic: &str, payload: &[u8], qos: QoS) -> Result<(), RetainedError> {
        let topic = String::<128>::try_from(topic).map_err(|_| RetainedError::TopicTooLong)?;

        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| entry.topic.as_str() == topic.as_str())
        {
            if payload.is_empty() {
                self.entries.remove(index);
                return Ok(());
            }

            let payload =
                Vec::<u8, 512>::from_slice(payload).map_err(|_| RetainedError::PayloadTooLarge)?;
            self.entries[index].payload = payload;
            self.entries[index].qos = qos;
            return Ok(());
        }

        if payload.is_empty() {
            return Ok(());
        }

        let payload =
            Vec::<u8, 512>::from_slice(payload).map_err(|_| RetainedError::PayloadTooLarge)?;

        self.entries
            .push(RetainedEntry { topic, payload, qos })
            .map_err(|_| RetainedError::Full)
    }

    /// Iterates over every retained entry. Intended for firmware-side serialisation
    /// (e.g. persisting the snapshot to NVS before shutdown).
    pub fn iter(&self) -> impl Iterator<Item = &RetainedEntry> {
        self.entries.iter()
    }

    pub fn matching<'a>(&'a self, filter: &'a str) -> impl Iterator<Item = &'a RetainedEntry> + 'a {
        self.entries
            .iter()
            .filter(move |entry| topic_matches(filter, entry.topic.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::{RetainedError, RetainedStore};
    use mqttrs::QoS;
    use std::vec;
    use std::vec::Vec;

    #[test]
    fn set_new_entry_increases_len() {
        let mut store = RetainedStore::<64>::new();

        store.set("a/b", &[1, 2, 3], QoS::AtMostOnce).unwrap();

        assert_eq!(store.len(), 1);
    }

    #[test]
    fn set_existing_entry_updates_payload_and_qos() {
        let mut store = RetainedStore::<64>::new();
        store.set("a/b", &[1, 2, 3], QoS::AtMostOnce).unwrap();

        store.set("a/b", &[4, 5, 6], QoS::AtLeastOnce).unwrap();

        let matches: Vec<_> = store.matching("a/b").collect();
        assert_eq!(store.len(), 1);
        assert_eq!(matches[0].payload.as_slice(), &[4, 5, 6]);
        assert_eq!(matches[0].qos, QoS::AtLeastOnce);
    }

    #[test]
    fn set_empty_payload_deletes_existing_entry() {
        let mut store = RetainedStore::<64>::new();
        store.set("a/b", &[1, 2, 3], QoS::AtMostOnce).unwrap();

        store.set("a/b", &[], QoS::AtMostOnce).unwrap();

        assert_eq!(store.len(), 0);
    }

    #[test]
    fn set_empty_payload_for_missing_entry_is_noop() {
        let mut store = RetainedStore::<64>::new();

        store.set("a/b", &[], QoS::AtMostOnce).unwrap();

        assert_eq!(store.len(), 0);
    }

    #[test]
    fn set_new_entry_over_capacity_returns_full() {
        let mut store = RetainedStore::<64>::new();
        for idx in 0..64 {
            let topic = std::format!("a/{idx}");
            store.set(&topic, &[idx as u8], QoS::AtMostOnce).unwrap();
        }

        assert_eq!(
            store.set("a/overflow", &[0xFF], QoS::AtMostOnce),
            Err(RetainedError::Full)
        );
    }

    #[test]
    fn set_existing_entry_when_full_updates_in_place() {
        let mut store = RetainedStore::<64>::new();
        for idx in 0..64 {
            let topic = std::format!("a/{idx}");
            store.set(&topic, &[idx as u8], QoS::AtMostOnce).unwrap();
        }

        store.set("a/10", &[9, 9], QoS::AtLeastOnce).unwrap();

        let matches: Vec<_> = store.matching("a/10").collect();
        assert_eq!(store.len(), 64);
        assert_eq!(matches[0].payload.as_slice(), &[9, 9]);
        assert_eq!(matches[0].qos, QoS::AtLeastOnce);
    }

    #[test]
    fn matching_plus_finds_only_single_level_matches() {
        let mut store = RetainedStore::<64>::new();
        store.set("a/b", &[1], QoS::AtMostOnce).unwrap();
        store.set("a/c", &[2], QoS::AtMostOnce).unwrap();
        store.set("a/b/c", &[3], QoS::AtMostOnce).unwrap();

        let matches: Vec<_> = store.matching("a/+").map(|entry| entry.topic.as_str()).collect();

        assert_eq!(matches, vec!["a/b", "a/c"]);
    }

    #[test]
    fn matching_exact_finds_only_exact_topic() {
        let mut store = RetainedStore::<64>::new();
        store.set("a/b", &[1], QoS::AtMostOnce).unwrap();
        store.set("a/c", &[2], QoS::AtMostOnce).unwrap();

        let matches: Vec<_> = store.matching("a/b").map(|entry| entry.topic.as_str()).collect();

        assert_eq!(matches, vec!["a/b"]);
    }

    #[test]
    fn iter_returns_all_entries() {
        let mut store = RetainedStore::<64>::new();
        store.set("a/b", &[1], QoS::AtMostOnce).unwrap();
        store.set("c/d", &[2], QoS::AtLeastOnce).unwrap();

        let topics: Vec<_> = store.iter().map(|e| e.topic.as_str()).collect();

        assert_eq!(topics, vec!["a/b", "c/d"]);
    }

    #[test]
    fn matching_missing_topic_returns_empty_iterator() {
        let mut store = RetainedStore::<64>::new();
        store.set("a/b", &[1], QoS::AtMostOnce).unwrap();

        assert_eq!(store.matching("missing/topic").count(), 0);
    }
}
