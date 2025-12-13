// Copyright 2018 Parity Technologies (UK) Ltd.
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

#![allow(clippy::wrong_self_convention)]

use crate::{
    crypto::PublicKey,
    types::multihash::{Code, Multihash, MultihashDigest},
    Error,
};

use multiaddr::{Multiaddr, Protocol};
use rand::Rng;
use serde::{Deserialize, Serialize};

use std::{convert::TryFrom, fmt, str::FromStr};

/// Public keys with byte-lengths smaller than `MAX_INLINE_KEY_LENGTH` will be
/// automatically used as the peer id using an identity multihash.
const MAX_INLINE_KEY_LENGTH: usize = 42;

/// Identifier of a peer of the network.
///
/// The data is a CIDv0 compatible multihash of the protobuf encoded public key of the peer
/// as specified in [specs/peer-ids](https://github.com/libp2p/specs/blob/master/peer-ids/peer-ids.md).
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PeerId {
    multihash: Multihash,
}

impl fmt::Debug for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("PeerId").field(&self.to_base58()).finish()
    }
}

impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.to_base58().fmt(f)
    }
}

impl PeerId {
    /// Builds a `PeerId` from a public key.
    pub fn from_public_key(key: &PublicKey) -> PeerId {
        Self::from_public_key_protobuf(&key.to_protobuf_encoding())
    }

    /// Builds a `PeerId` from a public key in protobuf encoding.
    pub fn from_public_key_protobuf(key_enc: &[u8]) -> PeerId {
        let multihash = if key_enc.len() <= MAX_INLINE_KEY_LENGTH {
            // Use Identity hash (code 0x00) for small keys
            Multihash::wrap(0x00, key_enc).expect("Key size is within bounds")
        } else {
            // Use SHA-256 for larger keys
            Code::Sha2_256.digest(key_enc)
        };

        PeerId { multihash }
    }

    /// Parses a `PeerId` from bytes.
    pub fn from_bytes(data: &[u8]) -> Result<PeerId, Error> {
        let multihash = Multihash::from_bytes(data).map_err(|_| Error::InvalidData)?;
        PeerId::from_multihash(multihash).map_err(|_| Error::InvalidData)
    }

    /// Tries to turn a `Multihash` into a `PeerId`.
    ///
    /// If the multihash does not use a valid hashing algorithm for peer IDs,
    /// or the hash value does not satisfy the constraints for a hashed
    /// peer ID, it is returned as an `Err`.
    pub fn from_multihash(multihash: Multihash) -> Result<PeerId, Multihash> {
        match multihash.code() {
            // SHA-256
            0x12 => Ok(PeerId { multihash }),
            // Identity hash
            0x00 if multihash.digest().len() <= MAX_INLINE_KEY_LENGTH => Ok(PeerId { multihash }),
            _ => Err(multihash),
        }
    }

    /// Helper method to convert from multiaddr's old multihash type
    pub(crate) fn from_multiaddr_multihash(
        hash: multiaddr::multihash::Multihash,
    ) -> Result<PeerId, multiaddr::multihash::Multihash> {
        let bytes = hash.to_bytes();
        let new_multihash = Multihash::from_bytes(&bytes).map_err(|_| hash)?;
        PeerId::from_multihash(new_multihash).map_err(|_| hash)
    }

    /// Tries to extract a [`PeerId`] from the given [`Multiaddr`].
    ///
    /// In case the given [`Multiaddr`] ends with `/p2p/<peer-id>`, this function
    /// will return the encapsulated [`PeerId`], otherwise it will return `None`.
    pub fn try_from_multiaddr(address: &Multiaddr) -> Option<PeerId> {
        address.iter().last().and_then(|p| match p {
            Protocol::P2p(hash) => PeerId::from_multiaddr_multihash(hash).ok(),
            _ => None,
        })
    }

    /// Generates a random peer ID from a cryptographically secure PRNG.
    ///
    /// This is useful for randomly walking on a DHT, or for testing purposes.
    pub fn random() -> PeerId {
        let peer_id = rand::thread_rng().gen::<[u8; 32]>();
        PeerId {
            multihash: Multihash::wrap(0x00, &peer_id).expect("The digest size is never too large"),
        }
    }

    /// Returns a raw bytes representation of this `PeerId`.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.multihash.to_bytes()
    }

    /// Returns a base-58 encoded string of this `PeerId`.
    pub fn to_base58(&self) -> String {
        bs58::encode(self.to_bytes()).into_string()
    }

    /// Checks whether the public key passed as parameter matches the public key of this `PeerId`.
    ///
    /// Returns `None` if this `PeerId`s hash algorithm is not supported when encoding the
    /// given public key, otherwise `Some` boolean as the result of an equality check.
    pub fn is_public_key(&self, public_key: &PublicKey) -> Option<bool> {
        let enc = public_key.to_protobuf_encoding();
        let expected_multihash = if enc.len() <= MAX_INLINE_KEY_LENGTH {
            Multihash::wrap(0x00, &enc).ok()?
        } else {
            Code::Sha2_256.digest(&enc)
        };
        Some(expected_multihash == self.multihash)
    }
}

impl From<PublicKey> for PeerId {
    fn from(key: PublicKey) -> PeerId {
        PeerId::from_public_key(&key)
    }
}

impl From<&PublicKey> for PeerId {
    fn from(key: &PublicKey) -> PeerId {
        PeerId::from_public_key(key)
    }
}

impl TryFrom<Vec<u8>> for PeerId {
    type Error = Vec<u8>;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        PeerId::from_bytes(&value).map_err(|_| value)
    }
}

impl TryFrom<Multihash> for PeerId {
    type Error = Multihash;

    fn try_from(value: Multihash) -> Result<Self, Self::Error> {
        PeerId::from_multihash(value)
    }
}

impl AsRef<Multihash> for PeerId {
    fn as_ref(&self) -> &Multihash {
        &self.multihash
    }
}

impl AsRef<multiaddr::multihash::Multihash> for PeerId {
    fn as_ref(&self) -> &multiaddr::multihash::Multihash {
        // SAFETY: Both Multihash types have the same memory layout (they're both Multihash<64>)
        // We can safely transmute between them for read-only operations
        unsafe { std::mem::transmute(&self.multihash) }
    }
}

impl From<PeerId> for Multihash {
    fn from(peer_id: PeerId) -> Self {
        peer_id.multihash
    }
}

impl From<PeerId> for multiaddr::multihash::Multihash {
    fn from(peer_id: PeerId) -> Self {
        // Convert by serializing to bytes and deserializing with the old version
        multiaddr::multihash::Multihash::from_bytes(&peer_id.multihash.to_bytes())
            .expect("Valid multihash conversion")
    }
}

impl TryFrom<multiaddr::multihash::Multihash> for PeerId {
    type Error = multiaddr::multihash::Multihash;

    fn try_from(multihash: multiaddr::multihash::Multihash) -> Result<Self, Self::Error> {
        PeerId::from_multiaddr_multihash(multihash)
    }
}

impl From<PeerId> for Vec<u8> {
    fn from(peer_id: PeerId) -> Self {
        peer_id.to_bytes()
    }
}

impl Serialize for PeerId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.to_base58())
        } else {
            serializer.serialize_bytes(&self.to_bytes()[..])
        }
    }
}

impl<'de> Deserialize<'de> for PeerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::*;

        struct PeerIdVisitor;

        impl Visitor<'_> for PeerIdVisitor {
            type Value = PeerId;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                write!(f, "valid peer id")
            }

            fn visit_bytes<E>(self, v: &[u8]) -> Result<Self::Value, E>
            where
                E: Error,
            {
                PeerId::from_bytes(v).map_err(|_| Error::invalid_value(Unexpected::Bytes(v), &self))
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: Error,
            {
                PeerId::from_str(v).map_err(|_| Error::invalid_value(Unexpected::Str(v), &self))
            }
        }

        if deserializer.is_human_readable() {
            deserializer.deserialize_str(PeerIdVisitor)
        } else {
            deserializer.deserialize_bytes(PeerIdVisitor)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("base-58 decode error: {0}")]
    B58(#[from] bs58::decode::Error),
    #[error("decoding multihash failed")]
    MultiHash,
}

impl FromStr for PeerId {
    type Err = ParseError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes = bs58::decode(s).into_vec()?;
        PeerId::from_bytes(&bytes).map_err(|_| ParseError::MultiHash)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crypto::ed25519::Keypair, types::multihash::Multihash, PeerId};
    use multiaddr::{Multiaddr, Protocol};

    #[test]
    fn multihash_layout_compatibility() {
        // Verify that both Multihash types have the same memory layout
        // This is critical for the safety of the transmute in AsRef implementation
        use std::mem::{align_of, size_of};

        assert_eq!(
            size_of::<crate::types::multihash::Multihash>(),
            size_of::<multiaddr::multihash::Multihash>(),
            "Multihash types must have the same size"
        );

        assert_eq!(
            align_of::<crate::types::multihash::Multihash>(),
            align_of::<multiaddr::multihash::Multihash>(),
            "Multihash types must have the same alignment"
        );
    }

    #[test]
    fn transmute_peer_id_wth_identity_code() {
        // Test that the transmute actually works correctly by creating a peer ID
        // and verifying we can get the same bytes through both types
        let peer_id = PeerId::random();
        let bytes_new: Vec<u8> =
            AsRef::<crate::types::multihash::Multihash>::as_ref(&peer_id).to_bytes();
        let bytes_old: Vec<u8> =
            AsRef::<multiaddr::multihash::Multihash>::as_ref(&peer_id).to_bytes();

        assert_eq!(
            bytes_new, bytes_old,
            "Transmuted Multihash must preserve data"
        );
    }

    #[test]
    fn transmute_peer_id_with_sha256_code() {
        // Test that the transmute works correctly with non-zero multihash code.
        let payload = b"1337";
        let multihash = Code::Sha2_256.digest(payload);
        let peer_id = PeerId::try_from(multihash).expect("sha256 is supported by `PeerId`");
        let bytes_new: Vec<u8> =
            AsRef::<crate::types::multihash::Multihash>::as_ref(&peer_id).to_bytes();
        let bytes_old: Vec<u8> =
            AsRef::<multiaddr::multihash::Multihash>::as_ref(&peer_id).to_bytes();

        assert_eq!(
            bytes_new, bytes_old,
            "Transmuted Multihash must preserve data"
        );
    }

    #[test]
    fn multihash_sha256_conversion() {
        // Test conversion for both Identity and SHA-256 hashed peer IDs
        // Ed25519 keys are 32 bytes, which when protobuf-encoded are < 42 bytes, so they use
        // Identity

        // Test with Ed25519 key (should use Identity hash, code 0x00)
        let keypair = Keypair::generate();
        let peer_id = keypair.public().to_peer_id();

        let multihash: &crate::types::multihash::Multihash = peer_id.as_ref();
        let hash_code = multihash.code();

        // Ed25519 uses Identity (0x00), but verify conversion works for whatever code is used
        assert!(
            hash_code == 0x00 || hash_code == 0x12,
            "Should use either Identity (0x00) or SHA-256 (0x12), got: 0x{:x}",
            hash_code
        );

        // Test conversion to old multihash type
        let old_multihash = multiaddr::multihash::Multihash::from(peer_id);
        assert_eq!(
            old_multihash.code(),
            hash_code,
            "Converted multihash should preserve hash code"
        );

        // Verify bytes are preserved
        let bytes_new = multihash.to_bytes();
        let bytes_old = old_multihash.to_bytes();
        assert_eq!(bytes_new, bytes_old, "Multihash bytes must be preserved");

        // Test conversion back from old multihash type
        let peer_id_back = PeerId::try_from(old_multihash).unwrap();
        assert_eq!(
            peer_id, peer_id_back,
            "Round-trip conversion must preserve PeerId"
        );

        // Test with a manually created SHA-256 peer ID
        // Create a large key by using a 50-byte buffer which exceeds MAX_INLINE_KEY_LENGTH (42)
        let large_key = vec![0x42u8; 50];
        let peer_id_sha256 = PeerId::from_public_key_protobuf(&large_key);

        let multihash_sha256: &crate::types::multihash::Multihash = peer_id_sha256.as_ref();
        assert_eq!(
            multihash_sha256.code(),
            0x12,
            "Large key should use SHA-256 hash"
        );

        // Test SHA-256 conversion
        let old_multihash_sha256 = multiaddr::multihash::Multihash::from(peer_id_sha256);
        assert_eq!(
            old_multihash_sha256.code(),
            0x12,
            "Converted SHA-256 multihash should preserve code"
        );

        let bytes_new_sha256 = multihash_sha256.to_bytes();
        let bytes_old_sha256 = old_multihash_sha256.to_bytes();
        assert_eq!(
            bytes_new_sha256, bytes_old_sha256,
            "SHA-256 multihash bytes must be preserved"
        );
    }

    #[test]
    fn peer_id_is_public_key() {
        let key = Keypair::generate().public();
        let peer_id = key.to_peer_id();
        assert_eq!(peer_id.is_public_key(&key.into()), Some(true));
    }

    #[test]
    fn peer_id_into_bytes_then_from_bytes() {
        let peer_id = Keypair::generate().public().to_peer_id();
        let second = PeerId::from_bytes(&peer_id.to_bytes()).unwrap();
        assert_eq!(peer_id, second);
    }

    #[test]
    fn peer_id_to_base58_then_back() {
        let peer_id = Keypair::generate().public().to_peer_id();
        let second: PeerId = peer_id.to_base58().parse().unwrap();
        assert_eq!(peer_id, second);
    }

    #[test]
    fn random_peer_id_is_valid() {
        for _ in 0..5000 {
            let peer_id = PeerId::random();
            assert_eq!(peer_id, PeerId::from_bytes(&peer_id.to_bytes()).unwrap());
        }
    }

    #[test]
    fn peer_id_from_multiaddr() {
        let address = "[::1]:1337".parse::<std::net::SocketAddr>().unwrap();
        let peer = PeerId::random();
        let address = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(multiaddr::multihash::Multihash::from(peer)));

        assert_eq!(peer, PeerId::try_from_multiaddr(&address).unwrap());
    }

    #[test]
    fn peer_id_from_multiaddr_no_peer_id() {
        let address = "[::1]:1337".parse::<std::net::SocketAddr>().unwrap();
        let address = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()));

        assert!(PeerId::try_from_multiaddr(&address).is_none());
    }

    #[test]
    fn peer_id_from_bytes() {
        let peer = PeerId::random();
        let bytes = peer.to_bytes();

        assert_eq!(PeerId::try_from(bytes).unwrap(), peer);
    }

    #[test]
    fn peer_id_as_multihash() {
        let peer = PeerId::random();
        let multihash = Multihash::from(peer);

        assert_eq!(&multihash, peer.as_ref());
        assert_eq!(PeerId::try_from(multihash).unwrap(), peer);
    }

    #[test]
    fn serialize_deserialize() {
        let peer = PeerId::random();
        let serialized = serde_json::to_string(&peer).unwrap();
        let deserialized = serde_json::from_str(&serialized).unwrap();

        assert_eq!(peer, deserialized);
    }

    #[test]
    fn invalid_multihash() {
        fn test() -> crate::Result<PeerId> {
            let bytes = [
                0x16, 0x20, 0x64, 0x4b, 0xcc, 0x7e, 0x56, 0x43, 0x73, 0x04, 0x09, 0x99, 0xaa, 0xc8,
                0x9e, 0x76, 0x22, 0xf3, 0xca, 0x71, 0xfb, 0xa1, 0xd9, 0x72, 0xfd, 0x94, 0xa3, 0x1c,
                0x3b, 0xfb, 0xf2, 0x4e, 0x39, 0x38,
            ];

            PeerId::from_multihash(Multihash::from_bytes(&bytes).unwrap()).map_err(From::from)
        }
        let _error = test().unwrap_err();
    }
}
