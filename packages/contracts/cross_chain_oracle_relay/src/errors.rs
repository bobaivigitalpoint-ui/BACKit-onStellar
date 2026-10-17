use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum OracleRelayError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    UnauthorizedRelayer = 4,
    InvalidBlockHeader = 5,
    MerkleProofVerificationFailed = 6,
    PriceOutOfRange = 7,
    InsufficientSignatures = 8,
    BlockAlreadyRelayed = 9,
    /// No signers are configured for the source chain.
    UnsupportedChain = 10,
    /// The signer is not authorised for the source chain.
    UnauthorizedSigner = 11,
    /// This (chain, market) outcome has already been relayed.
    OutcomeAlreadyRelayed = 12,
    InvalidOutcome = 13,
    /// The outcome manager rejected the forwarded outcome.
    ForwardFailed = 14,
}
