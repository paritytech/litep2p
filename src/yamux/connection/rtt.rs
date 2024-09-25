use std::sync::Arc;

use parking_lot::Mutex;
use std::time::{Duration, Instant};

use crate::yamux::{
    connection::Action,
    frame::{header::Ping, Frame},
};

const PING_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub(crate) struct Rtt(Arc<Mutex<RttInner>>);

impl Rtt {
    pub(crate) fn new() -> Self {
        Self(Arc::new(Mutex::new(RttInner {
            rtt: None,
            state: RttState::Waiting {
                next: Instant::now(),
            },
        })))
    }

    pub(crate) fn next_ping(&mut self) -> Option<Frame<Ping>> {
        let state = &mut self.0.lock().state;

        match state {
            RttState::AwaitingPong { .. } => return None,
            RttState::Waiting { next } =>
                if *next > Instant::now() {
                    return None;
                },
        }

        let nonce = rand::random();
        *state = RttState::AwaitingPong {
            sent_at: Instant::now(),
            nonce,
        };
        tracing::debug!("sending ping {nonce}");
        Some(Frame::ping(nonce))
    }

    pub(crate) fn handle_pong(&mut self, received_nonce: u32) -> Action {
        let inner = &mut self.0.lock();

        let (sent_at, expected_nonce) = match inner.state {
            RttState::Waiting { .. } => {
                tracing::error!("received unexpected pong {received_nonce}");
                return Action::Terminate(Frame::protocol_error());
            }
            RttState::AwaitingPong { sent_at, nonce } => (sent_at, nonce),
        };

        if received_nonce != expected_nonce {
            tracing::error!("received pong with {received_nonce} but expected {expected_nonce}");
            return Action::Terminate(Frame::protocol_error());
        }

        let rtt = sent_at.elapsed();
        inner.rtt = Some(rtt);
        tracing::debug!("received pong {received_nonce}, estimated round-trip-time {rtt:?}");

        inner.state = RttState::Waiting {
            next: Instant::now() + PING_INTERVAL,
        };

        return Action::None;
    }

    pub(crate) fn get(&self) -> Option<Duration> {
        self.0.lock().rtt
    }
}

#[cfg(test)]
impl quickcheck::Arbitrary for Rtt {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        Self(Arc::new(Mutex::new(RttInner::arbitrary(g))))
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
struct RttInner {
    state: RttState,
    rtt: Option<Duration>,
}

#[cfg(test)]
impl quickcheck::Arbitrary for RttInner {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        Self {
            state: RttState::arbitrary(g),
            rtt: if bool::arbitrary(g) {
                Some(Duration::arbitrary(g))
            } else {
                None
            },
        }
    }
}

#[derive(Debug)]
#[cfg_attr(test, derive(Clone))]
enum RttState {
    AwaitingPong { sent_at: Instant, nonce: u32 },
    Waiting { next: Instant },
}

#[cfg(test)]
impl quickcheck::Arbitrary for RttState {
    fn arbitrary(g: &mut quickcheck::Gen) -> Self {
        if bool::arbitrary(g) {
            RttState::AwaitingPong {
                sent_at: Instant::now(),
                nonce: u32::arbitrary(g),
            }
        } else {
            RttState::Waiting {
                next: Instant::now(),
            }
        }
    }
}
