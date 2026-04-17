use crate::router::topic_matches;
use crate::qos::max_qos;
use crate::session::registry::SessionRegistry;
use crate::session::state::SessionId;
use heapless::Vec;
use mqttrs::QoS;

pub fn find_subscribers<
    const N: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
>(
    registry: &SessionRegistry<N, MAX_SUBS, MAX_INFLIGHT>,
    topic: &str,
    sender_id: SessionId,
) -> Vec<(SessionId, QoS), 16> {
    let mut subscribers = Vec::<(SessionId, QoS), 16>::new();

    for (session_id, session) in registry.iter() {
        if session_id == sender_id {
            continue;
        }

        let mut matched_qos = None;
        for subscription in &session.subscriptions {
            if topic_matches(subscription.filter.as_str(), topic) {
                matched_qos = Some(match matched_qos {
                    Some(current) => max_qos(current, subscription.qos),
                    None => subscription.qos,
                });
            }
        }

        if let Some(qos) = matched_qos {
            let _ = subscribers.push((session_id, qos));
        }
    }

    subscribers
}

pub fn find_all_subscribers<
    const N: usize,
    const MAX_SUBS: usize,
    const MAX_INFLIGHT: usize,
>(
    registry: &SessionRegistry<N, MAX_SUBS, MAX_INFLIGHT>,
    topic: &str,
) -> Vec<(SessionId, QoS), 16> {
    let mut subscribers = Vec::<(SessionId, QoS), 16>::new();

    for (session_id, session) in registry.iter() {
        let mut matched_qos = None;
        for subscription in &session.subscriptions {
            if topic_matches(subscription.filter.as_str(), topic) {
                matched_qos = Some(match matched_qos {
                    Some(current) => max_qos(current, subscription.qos),
                    None => subscription.qos,
                });
            }
        }

        if let Some(qos) = matched_qos {
            let _ = subscribers.push((session_id, qos));
        }
    }

    subscribers
}

#[cfg(test)]
mod tests {
    use super::{find_all_subscribers, find_subscribers};
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

    fn session(
        client_id: &str,
        subscriptions: &[(&str, QoS)],
    ) -> SessionState<8, 4> {
        let mut session = SessionState::new(String::<64>::try_from(client_id).unwrap(), 60);
        for (filter, qos) in subscriptions {
            session
                .subscriptions
                .push(subscription(filter, *qos))
                .unwrap();
        }
        session
    }

    #[test]
    fn excludes_sender_from_delivery_list() {
        let mut registry = SessionRegistry::<8, 8, 4>::new();
        let sender_id = registry
            .insert(session("sender", &[("devices/+/temp", QoS::AtMostOnce)]))
            .unwrap();
        let receiver_id = registry
            .insert(session("receiver", &[("devices/+/temp", QoS::AtLeastOnce)]))
            .unwrap();

        let subscribers = find_subscribers(&registry, "devices/kitchen/temp", sender_id);

        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0], (receiver_id, QoS::AtLeastOnce));
    }

    #[test]
    fn collects_matching_subscribers_only() {
        let mut registry = SessionRegistry::<8, 8, 4>::new();
        let sender_id = registry.insert(session("sender", &[])).unwrap();
        let exact_id = registry
            .insert(session("exact", &[("devices/kitchen/temp", QoS::AtMostOnce)]))
            .unwrap();
        let wildcard_id = registry
            .insert(session("wildcard", &[("devices/+/temp", QoS::AtLeastOnce)]))
            .unwrap();
        let _other_id = registry
            .insert(session("other", &[("devices/living/humidity", QoS::ExactlyOnce)]))
            .unwrap();

        let subscribers = find_subscribers(&registry, "devices/kitchen/temp", sender_id);

        assert_eq!(subscribers.len(), 2);
        assert_eq!(subscribers[0], (exact_id, QoS::AtMostOnce));
        assert_eq!(subscribers[1], (wildcard_id, QoS::AtLeastOnce));
    }

    #[test]
    fn multiple_matching_filters_use_maximum_qos_for_session() {
        let mut registry = SessionRegistry::<8, 8, 4>::new();
        let sender_id = registry.insert(session("sender", &[])).unwrap();
        let matched_id = registry
            .insert(session(
                "receiver",
                &[
                    ("devices/+/temp", QoS::AtMostOnce),
                    ("devices/kitchen/temp", QoS::ExactlyOnce),
                    ("devices/#", QoS::AtLeastOnce),
                ],
            ))
            .unwrap();

        let subscribers = find_subscribers(&registry, "devices/kitchen/temp", sender_id);

        assert_eq!(subscribers.len(), 1);
        assert_eq!(subscribers[0], (matched_id, QoS::ExactlyOnce));
    }

    #[test]
    fn returns_empty_when_no_matching_subscribers() {
        let mut registry = SessionRegistry::<8, 8, 4>::new();
        let sender_id = registry.insert(session("sender", &[])).unwrap();
        let _ = registry
            .insert(session("receiver", &[("devices/living/humidity", QoS::AtMostOnce)]))
            .unwrap();

        let subscribers = find_subscribers(&registry, "devices/kitchen/temp", sender_id);

        assert!(subscribers.is_empty());
    }

    #[test]
    fn find_all_includes_every_matching_session_without_exclusion() {
        let mut registry = SessionRegistry::<8, 8, 4>::new();
        let a = registry
            .insert(session("a", &[("devices/+/temp", QoS::AtMostOnce)]))
            .unwrap();
        let b = registry
            .insert(session("b", &[("devices/kitchen/temp", QoS::AtLeastOnce)]))
            .unwrap();

        let subscribers = find_all_subscribers(&registry, "devices/kitchen/temp");

        assert_eq!(subscribers.len(), 2);
        assert!(subscribers.iter().any(|(id, _)| *id == a));
        assert!(subscribers.iter().any(|(id, _)| *id == b));
    }

    #[test]
    fn find_all_returns_empty_when_no_matching_subscribers() {
        let mut registry = SessionRegistry::<8, 8, 4>::new();
        let _ = registry
            .insert(session("a", &[("other/topic", QoS::AtMostOnce)]))
            .unwrap();

        let subscribers = find_all_subscribers(&registry, "devices/kitchen/temp");

        assert!(subscribers.is_empty());
    }
}
