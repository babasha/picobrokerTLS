use crate::session::registry::SessionRegistry;
use crate::session::state::SessionId;
use heapless::Vec;

pub fn topic_matches(filter: &str, pub_topic: &str) -> bool {
    if filter == pub_topic {
        return true;
    }

    if pub_topic.starts_with('$') && contains_wildcards(filter) {
        return false;
    }

    let mut filter_levels = filter.split('/');
    let mut topic_levels = pub_topic.split('/');

    loop {
        match (filter_levels.next(), topic_levels.next()) {
            (Some("+"), Some(level)) if !level.is_empty() => {}
            (Some(filter_level), Some(topic_level)) if filter_level == topic_level => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

pub fn collect_subscribers<
    const MAX_MATCHES: usize,
    const N: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
>(
    registry: &SessionRegistry<N, MAX_SUBS, MAX_INFLIGHT>,
    topic: &str,
) -> Vec<SessionId, MAX_MATCHES> {
    let mut subscribers = Vec::new();

    for (session_id, state) in registry.iter() {
        if state
            .subscriptions
            .iter()
            .any(|subscription| topic_matches(subscription.filter.as_str(), topic))
            && !subscribers.iter().any(|existing| *existing == session_id)
        {
            let _ = subscribers.push(session_id);
        }
    }

    subscribers
}

fn contains_wildcards(filter: &str) -> bool {
    filter.as_bytes().iter().any(|byte| *byte == b'+' || *byte == b'#')
}

#[cfg(test)]
mod tests {
    use super::{collect_subscribers, topic_matches};
    use crate::session::registry::SessionRegistry;
    use crate::session::state::{SessionState, Subscription};
    use heapless::String;
    use mqttrs::QoS;

    fn subscription(filter: &str, qos: QoS) -> Subscription {
        Subscription {
            filter: String::<128>::try_from(filter).unwrap(),
            qos,
        }
    }

    #[test]
    fn topic_matches_test_exact_true() {
        assert!(topic_matches("test", "test"));
    }

    #[test]
    fn topic_matches_test_exact_false() {
        assert!(!topic_matches("test", "other"));
    }

    #[test]
    fn topic_matches_multilevel_exact_true() {
        assert!(topic_matches("a/b/c", "a/b/c"));
    }

    #[test]
    fn topic_matches_multilevel_exact_false() {
        assert!(!topic_matches("a/b/c", "a/b/d"));
    }

    #[test]
    fn topic_matches_plus_cases() {
        assert!(topic_matches("a/+/c", "a/b/c"));
        assert!(topic_matches("a/+/c", "a/X/c"));
        assert!(!topic_matches("a/+/c", "a/b/d"));
        assert!(!topic_matches("a/+/c", "a/b/c/d"));
        assert!(topic_matches("a/+", "a/b"));
        assert!(!topic_matches("a/+", "a/b/c"));
        assert!(topic_matches("+", "a"));
        assert!(!topic_matches("+", "a/b"));
    }

    #[test]
    fn plus_does_not_match_empty_level() {
        assert!(!topic_matches("a/+/c", "a//c"));
        assert!(!topic_matches("+", ""));
    }

    #[test]
    fn realistic_smartbox_filter_matches() {
        assert!(topic_matches(
            "sb/+/device/+/state",
            "sb/house1/device/relay-1/state"
        ));
    }

    #[test]
    fn sys_topics_do_not_match_wildcards() {
        assert!(topic_matches("$SYS/broker", "$SYS/broker"));
        assert!(!topic_matches("+", "$SYS/broker"));
    }

    #[test]
    fn collect_subscribers_deduplicates_sessions_with_multiple_matching_filters() {
        let mut registry = SessionRegistry::<8, 8, 4>::new();

        let mut first = SessionState::new(String::<64>::try_from("mobile-app").unwrap(), 60);
        first
            .subscriptions
            .push(subscription("devices/+/temp", QoS::AtMostOnce))
            .unwrap();
        first
            .subscriptions
            .push(subscription("devices/kitchen/temp", QoS::AtLeastOnce))
            .unwrap();
        let first_id = registry.insert(first).unwrap();

        let mut second = SessionState::new(String::<64>::try_from("wall-panel").unwrap(), 60);
        second
            .subscriptions
            .push(subscription("devices/living/temp", QoS::AtMostOnce))
            .unwrap();
        let second_id = registry.insert(second).unwrap();

        let subscribers = collect_subscribers::<8, 8, 8, 4>(&registry, "devices/kitchen/temp");

        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0], first_id);
        assert!(!subscribers.iter().any(|session_id| *session_id == second_id));
    }
}
