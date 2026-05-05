use super::state::SessionId;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;

pub type SessionSignal = Signal<CriticalSectionRawMutex, ()>;
pub type SessionSignals<const N: usize> = [SessionSignal; N];

pub const fn new_session_signals<const N: usize>() -> SessionSignals<N> {
    [const { Signal::new() }; N]
}

pub fn signal_session<const N: usize>(signals: &SessionSignals<N>, session_id: SessionId) {
    if let Some(signal) = signals.get(session_id) {
        signal.signal(());
    }
}

pub fn reset_session_signal<const N: usize>(signals: &SessionSignals<N>, session_id: SessionId) {
    if let Some(signal) = signals.get(session_id) {
        signal.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::{new_session_signals, reset_session_signal, signal_session};

    #[test]
    fn reset_session_signal_clears_queued_notification() {
        let signals = new_session_signals::<2>();

        signal_session(&signals, 1);
        assert!(signals[1].signaled());

        signal_session(&signals, 1);
        reset_session_signal(&signals, 1);

        assert!(!signals[1].signaled());
    }
}
