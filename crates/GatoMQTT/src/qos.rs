use mqttrs::QoS;

pub const fn qos_rank(qos: QoS) -> u8 {
    match qos {
        QoS::AtMostOnce => 0,
        QoS::AtLeastOnce => 1,
        QoS::ExactlyOnce => 2,
    }
}

pub fn max_qos(lhs: QoS, rhs: QoS) -> QoS {
    if qos_rank(lhs) >= qos_rank(rhs) {
        lhs
    } else {
        rhs
    }
}

pub fn min_qos(lhs: QoS, rhs: QoS) -> QoS {
    if qos_rank(lhs) <= qos_rank(rhs) {
        lhs
    } else {
        rhs
    }
}
