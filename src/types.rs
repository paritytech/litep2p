// Copyright 2023 litep2p developers
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

pub mod protocol;

/// Substream ID.
pub type SubstreamId = usize;

/// Request ID.
pub type RequestId = usize;

/// Connection ID.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct ConnectionId(usize);

impl ConnectionId {
    /// Create new [`ConnectionId`].
    pub fn new() -> Self {
        ConnectionId(0usize)
    }

    /// Get next [`ConnectionID`].
    pub fn next(&mut self) -> ConnectionId {
        let connection_id = self.0;
        self.0 += 1usize;

        ConnectionId(connection_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_id_works() {
        let mut connection_id = ConnectionId::new();
        assert_eq!(connection_id, ConnectionId(0));

        let next_connection_id = connection_id.next();
        assert_eq!(next_connection_id, ConnectionId(0));

        let next_connection_id = connection_id.next();
        assert_eq!(next_connection_id, ConnectionId(1));
    }
}
