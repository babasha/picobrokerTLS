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
