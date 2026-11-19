// Copyright 2026 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Panic containment for `str0m`.
//!
//! [`Rtc`] mirrors the part of [`str0m::Rtc`] used by the transport. A panic in `str0m` discards
//! the instance, after which every call fails and the connection is closed through its regular
//! error paths. Has no effect when built with `panic = "abort"`.

use str0m::{
    channel::{ChannelConfig, ChannelId},
    config::Fingerprint,
    Input, Output, RtcError,
};

use std::{
    io,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::webrtc::rtc";

/// Error returned in place of a `str0m` panic.
fn panicked() -> RtcError {
    RtcError::Io(io::Error::other("str0m panicked"))
}

/// Run `f`, converting a panic into an error.
///
/// `f` must not leave anything but a discarded [`str0m::Rtc`] in an inconsistent state.
pub(super) fn guard<T>(f: impl FnOnce() -> T) -> Result<T, RtcError> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|payload| {
        // Fuzzers must see the panic.
        if cfg!(all(feature = "fuzz", not(test))) {
            resume_unwind(payload);
        }

        let reason = payload
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
            .unwrap_or("unknown");
        tracing::error!(target: LOG_TARGET, reason, "`str0m` panicked");

        panicked()
    })
}

/// [`str0m::Rtc`] that is discarded if it panics.
pub struct Rtc(Option<str0m::Rtc>);

impl Rtc {
    /// Create new [`Rtc`].
    pub fn new(rtc: str0m::Rtc) -> Self {
        Self(Some(rtc))
    }

    /// Call into `str0m`, discarding the instance if it panics.
    fn call<T>(&mut self, f: impl FnOnce(&mut str0m::Rtc) -> T) -> Result<T, RtcError> {
        let rtc = self.0.as_mut().ok_or_else(panicked)?;

        guard(|| f(rtc)).inspect_err(|_| self.0 = None)
    }

    /// Discard the instance as if `str0m` panicked.
    #[cfg(test)]
    pub(super) fn poison(&mut self) {
        let _ = self.call::<()>(|_| panic!("injected panic"));
    }

    /// See [`str0m::Rtc::poll_output`].
    pub fn poll_output(&mut self) -> Result<Output, RtcError> {
        self.call(|rtc| rtc.poll_output())?
    }

    /// See [`str0m::Rtc::handle_input`].
    pub fn handle_input(&mut self, input: Input<'_>) -> Result<(), RtcError> {
        self.call(|rtc| rtc.handle_input(input))?
    }

    /// See [`str0m::Rtc::is_alive`].
    pub fn is_alive(&self) -> bool {
        self.0.as_ref().is_some_and(|rtc| guard(|| rtc.is_alive()).unwrap_or(false))
    }

    /// See [`str0m::Rtc::is_connected`].
    pub fn is_connected(&self) -> bool {
        self.0.as_ref().is_some_and(|rtc| guard(|| rtc.is_connected()).unwrap_or(false))
    }

    /// See [`str0m::Rtc::disconnect`].
    pub fn disconnect(&mut self) {
        let _ = self.call(|rtc| rtc.disconnect());
    }

    /// See [`str0m::Rtc::channel`].
    pub fn channel(&mut self, id: ChannelId) -> Option<Channel<'_>> {
        let exists = self.call(|rtc| rtc.channel(id).is_some()).ok()?;

        exists.then_some(Channel { rtc: self, id })
    }

    /// See [`str0m::Rtc::direct_api`].
    pub fn direct_api(&mut self) -> DirectApi<'_> {
        DirectApi(self)
    }
}

/// Guarded counterpart of [`str0m::channel::Channel`].
pub struct Channel<'a> {
    rtc: &'a mut Rtc,
    id: ChannelId,
}

impl Channel<'_> {
    /// See [`str0m::channel::Channel::write`].
    #[must_use = "Whether the buffer was accepted by the write()"]
    pub fn write(&mut self, binary: bool, buf: &[u8]) -> Result<bool, RtcError> {
        let id = self.id;

        self.rtc.call(|rtc| match rtc.channel(id) {
            Some(mut channel) => channel.write(binary, buf),
            None => Err(RtcError::Io(io::ErrorKind::NotConnected.into())),
        })?
    }

    /// See [`str0m::channel::Channel::set_buffered_amount_low_threshold`].
    pub fn set_buffered_amount_low_threshold(&mut self, threshold: usize) {
        let id = self.id;

        let _ = self.rtc.call(|rtc| {
            if let Some(mut channel) = rtc.channel(id) {
                channel.set_buffered_amount_low_threshold(threshold);
            }
        });
    }
}

/// Guarded counterpart of [`str0m::change::DirectApi`].
pub struct DirectApi<'a>(&'a mut Rtc);

impl DirectApi<'_> {
    /// See [`str0m::change::DirectApi::create_data_channel`].
    ///
    /// Returns `None` if `str0m` panicked.
    pub fn create_data_channel(&mut self, config: ChannelConfig) -> Option<ChannelId> {
        self.0.call(|rtc| rtc.direct_api().create_data_channel(config)).ok()
    }

    /// See [`str0m::change::DirectApi::close_data_channel`].
    pub fn close_data_channel(&mut self, id: ChannelId) {
        let _ = self.0.call(|rtc| rtc.direct_api().close_data_channel(id));
    }

    /// See [`str0m::change::DirectApi::local_dtls_fingerprint`].
    ///
    /// Returns `None` if `str0m` panicked.
    pub fn local_dtls_fingerprint(&mut self) -> Option<Fingerprint> {
        self.0.call(|rtc| rtc.direct_api().local_dtls_fingerprint().clone()).ok()
    }

    /// See [`str0m::change::DirectApi::remote_dtls_fingerprint`].
    pub fn remote_dtls_fingerprint(&mut self) -> Option<Fingerprint> {
        self.0
            .call(|rtc| rtc.direct_api().remote_dtls_fingerprint().cloned())
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::webrtc::certificate::DtlsCertificate;
    use std::time::Instant;
    use str0m::channel::Reliability;

    fn rtc() -> Rtc {
        let certificate = DtlsCertificate::new().expect("certificate generation to succeed");

        Rtc::new(
            str0m::Rtc::builder()
                .set_ice_lite(true)
                .set_dtls_cert(certificate.into())
                .build(Instant::now()),
        )
    }

    fn channel_config() -> ChannelConfig {
        ChannelConfig {
            label: "test".to_string(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        }
    }

    #[test]
    fn guard_converts_panic_into_error() {
        assert_eq!(guard(|| 1337).unwrap(), 1337);
        assert!(guard::<()>(|| panic!("injected panic")).is_err());
        assert!(guard::<()>(|| std::panic::panic_any(1337)).is_err());
    }

    #[test]
    fn calls_reach_live_rtc() {
        let mut rtc = rtc();

        assert!(rtc.is_alive());
        assert!(rtc.poll_output().is_ok());
        assert!(rtc.handle_input(Input::Timeout(Instant::now())).is_ok());
        assert!(rtc.direct_api().local_dtls_fingerprint().is_some());
        assert!(rtc.direct_api().create_data_channel(channel_config()).is_some());

        rtc.disconnect();
        assert!(!rtc.is_alive());
    }

    #[test]
    fn panic_discards_rtc() {
        let mut rtc = rtc();
        let channel_id = rtc.direct_api().create_data_channel(channel_config()).unwrap();

        rtc.poison();

        assert!(rtc.0.is_none());
        assert!(!rtc.is_alive());
        assert!(!rtc.is_connected());
        assert!(rtc.poll_output().is_err());
        assert!(rtc.handle_input(Input::Timeout(Instant::now())).is_err());
        assert!(rtc.channel(channel_id).is_none());
        assert!(rtc.direct_api().create_data_channel(channel_config()).is_none());
        assert!(rtc.direct_api().local_dtls_fingerprint().is_none());
        assert!(rtc.direct_api().remote_dtls_fingerprint().is_none());

        // Must stay no-ops.
        rtc.direct_api().close_data_channel(channel_id);
        rtc.disconnect();
    }
}
