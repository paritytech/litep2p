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

use crate::Error;
use std::sync::Arc;

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
}

/// A registry for metrics.
pub trait MetricsRegistryT: Send + Sync {
    /// Register a new counter.
    fn register_counter(
        &self,
        name: &'static str,
        help: &'static str,
    ) -> Result<MetricCounter, Error>;

    /// Register a new gauge.
    fn register_gauge(&self, name: &'static str, help: &'static str) -> Result<MetricGauge, Error>;
}
