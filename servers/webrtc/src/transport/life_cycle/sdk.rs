use str0m::IceConnectionState;

use super::{TransportLifeCycle, TransportLifeCycleEvent};

const CONNECT_TIMEOUT: u64 = 10000;
const RECONNECT_TIMEOUT: u64 = 3000;

#[derive(Debug)]
pub enum State {
    New { at_ms: u64 },
    Connected { datachannel: bool, at_ms: u64 },
    Reconnecting { datachannel: bool, at_ms: u64 },
    Failed,
    Closed,
}

pub struct SdkTransportLifeCycle {
    state: State,
}

impl SdkTransportLifeCycle {
    pub fn new(now_ms: u64) -> Self {
        log::info!("[SdkTransportLifeCycle] new");
        Self { state: State::New { at_ms: now_ms } }
    }
}

impl TransportLifeCycle for SdkTransportLifeCycle {
    fn on_tick(&mut self, now_ms: u64) -> Option<TransportLifeCycleEvent> {
        match self.state {
            State::New { at_ms } => {
                if at_ms + CONNECT_TIMEOUT < now_ms {
                    log::info!("[SdkTransportLifeCycle] on webrtc connect timeout => switched to Failed");
                    self.state = State::Failed;
                    Some(TransportLifeCycleEvent::ConnectError)
                } else {
                    None
                }
            }
            State::Connected { datachannel, at_ms } => None,
            State::Reconnecting { datachannel, at_ms } => {
                if at_ms + RECONNECT_TIMEOUT < now_ms {
                    log::info!("[SdkTransportLifeCycle] on webrtc reconnect timeout => switched to Failed");
                    self.state = State::Failed;
                    Some(TransportLifeCycleEvent::Failed)
                } else {
                    None
                }
            }
            State::Failed => None,
            State::Closed => None,
        }
    }

    fn on_webrtc_connected(&mut self, now_ms: u64) -> Option<TransportLifeCycleEvent> {
        self.state = State::Connected { datachannel: false, at_ms: now_ms };
        log::info!("[SdkTransportLifeCycle] on webrtc connected => switched to {:?}", self.state);
        None
    }

    fn on_ice_state(&mut self, now_ms: u64, ice: IceConnectionState) -> Option<TransportLifeCycleEvent> {
        let res = match (&self.state, ice) {
            (State::Connected { datachannel: dc, at_ms: _ }, IceConnectionState::Disconnected) => {
                self.state = State::Reconnecting { datachannel: *dc, at_ms: now_ms };
                Some(TransportLifeCycleEvent::Reconnecting)
            }
            (State::Reconnecting { datachannel: dc, at_ms: _ }, IceConnectionState::Completed) => {
                self.state = State::Connected { datachannel: *dc, at_ms: now_ms };
                Some(TransportLifeCycleEvent::Reconnected)
            }
            (State::Reconnecting { datachannel: dc, at_ms: _ }, IceConnectionState::Connected) => {
                self.state = State::Connected { datachannel: *dc, at_ms: now_ms };
                Some(TransportLifeCycleEvent::Reconnected)
            }
            _ => None,
        };

        if res.is_some() {
            log::info!("[SdkTransportLifeCycle] on ice state {:?} => switched to {:?}", ice, self.state);
        }
        res
    }

    fn on_data_channel(&mut self, now_ms: u64, connected: bool) -> Option<TransportLifeCycleEvent> {
        let res = match (connected, &self.state) {
            (true, State::Connected { datachannel: false, at_ms: _ }) => {
                self.state = State::Connected { datachannel: true, at_ms: now_ms };
                Some(TransportLifeCycleEvent::Connected)
            }
            (false, _) => {
                self.state = State::Closed;
                Some(TransportLifeCycleEvent::Closed)
            }
            _ => None,
        };
        if res.is_some() {
            log::info!("[SdkTransportLifeCycle] on datachannel connected {} => switched to {:?}", connected, self.state);
        }
        res
    }
}

//TODO test this
