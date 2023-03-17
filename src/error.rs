use crate::types::PeerId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Peer `{0}` does not exist")]
    PeerDoesntExist(PeerId),
}
