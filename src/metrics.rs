// Copyright 2020 Parity Technologies (UK) Ltd.
// Copyright 2024 litep2p developers
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

//! A generic module for handling the metrics exposed by litep2p.
//!
//! Contains the traits and types that are used to define and interact with metrics.

use crate::{utils::futures_stream::FuturesStream, Error};
use futures::{Stream, StreamExt};
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

pub type MetricCounter = Arc<dyn MetricCounterT>;

pub type MetricGauge = Arc<dyn MetricGaugeT>;

pub type MetricsRegistry = Arc<dyn MetricsRegistryT>;

/// Represents a metric that can only go up.
pub trait MetricCounterT: Send + Sync {
    /// Increment the counter by `value`.
    fn inc(&self, value: u64);
}

/// Represents a metric that can arbitrarily go up and down.
pub trait MetricGaugeT: Send + Sync {
    /// Set the gauge to `value`.
    fn set(&self, value: u64);

    /// Increment the gauge.
    fn inc(&self);

    /// Decrement the gauge.
    fn dec(&self);

    /// Add `value` to the gauge.
    fn add(&self, value: u64);

    /// Subtract `value` from the gauge.
    fn sub(&self, value: u64);
}

/// A registry for metrics.
pub trait MetricsRegistryT: Send + Sync {
    /// Register a new counter.
    fn register_counter(&self, name: String, help: String) -> Result<MetricCounter, Error>;

    /// Register a new gauge.
    fn register_gauge(&self, name: String, help: String) -> Result<MetricGauge, Error>;
}

/// A scope for metrics that modifies a provided gauge in an RAII fashion.
///
/// The gauge is incremented when constructed and decremented when the object is dropped.
#[derive(Clone)]
pub struct ScopeGaugeMetric {
    inner: MetricGauge,
}

impl ScopeGaugeMetric {
    /// Create a new [`ScopeGaugeMetric`].
    pub fn new(inner: MetricGauge) -> Self {
        inner.inc();
        ScopeGaugeMetric { inner }
    }
}

impl Drop for ScopeGaugeMetric {
    fn drop(&mut self) {
        self.inner.dec();
    }
}

/// Wrapper around [`FuturesStream`] that provides information to the given metric.
#[derive(Default)]
pub struct MeteredFuturesStream<F> {
    stream: FuturesStream<F>,
    metric: Option<MetricGauge>,
}

impl<F> MeteredFuturesStream<F> {
    pub fn new(metric: Option<MetricGauge>) -> Self {
        MeteredFuturesStream {
            stream: FuturesStream::new(),
            metric,
        }
    }

    pub fn push(&mut self, future: F) {
        if let Some(ref metric) = self.metric {
            metric.inc();
        }

        self.stream.push(future);
    }

    /// Number of futures in the stream.
    pub fn len(&self) -> usize {
        self.stream.len()
    }

    /// Returns `true` if the stream is empty.
    pub fn is_empty(&self) -> bool {
        self.stream.len() == 0
    }
}

impl<F: Future> Stream for MeteredFuturesStream<F> {
    type Item = <F as Future>::Output;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.stream.poll_next_unpin(cx);
        if result.is_ready() {
            if let Some(ref metric) = self.metric {
                metric.dec();
            }
        }
        result
    }
}

impl<F> Drop for MeteredFuturesStream<F> {
    fn drop(&mut self) {
        if let Some(ref metric) = self.metric {
            metric.sub(self.len() as u64);
        }
    }
}
