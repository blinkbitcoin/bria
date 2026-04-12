use thiserror::Error;

#[derive(Debug, Error)]
pub enum UtxoError {
    #[error("UtxoError - Sqlx: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("UtxoError - EntityError: {0}")]
    EntityError(#[from] crate::entity::EntityError),
    #[error("UtxoError - Utxo already settled")]
    UtxoAlreadySettledError,
    #[error("UtxoError - Utxo does not exist")]
    UtxoDoesNotExistError,
    #[error("UtxoError - Failed to reserve all utxos for batch (expected {expected}, reserved {reserved})")]
    ReserveUtxosMismatch { expected: usize, reserved: usize },
}
