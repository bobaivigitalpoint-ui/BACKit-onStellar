use soroban_sdk::contracterror;

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum OracleMarketplaceError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    OracleAlreadyRegistered = 4,
    OracleNotFound = 5,
    OracleNotActive = 6,
    InsufficientStake = 7,
    CooldownActive = 8,
    InvalidFee = 9,
    CallNotFound = 10,
    OracleNotSelectedForCall = 11,
    AlreadyRated = 12,
    InvalidRating = 13,
    /// No bond token has been configured.
    BondTokenNotSet = 40,
    InvalidBond = 41,
    ProviderNotFound = 42,
    ProviderAlreadyRegistered = 43,
    /// The call already has a hired provider.
    AlreadyHired = 44,
    HireNotFound = 45,
    /// The hire was already paid or slashed.
    HireClosed = 46,
    InvalidSlash = 47,
    /// Bond withdrawal needs the cooldown to have elapsed.
    WithdrawalLocked = 48,
    /// Bond withdrawal needs every pending hire to be settled.
    PendingHires = 49,
}
