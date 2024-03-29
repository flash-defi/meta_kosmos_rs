use serde::Deserialize;
use strum::{AsRefStr, Display, EnumCount, EnumIter, EnumString, EnumVariantNames};

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    AsRefStr,         // AsRef<str>, fmt::Display and serde::Serialize
    EnumVariantNames, // Chain::VARIANTS
    EnumString,       // FromStr, TryFrom<&str>
    EnumIter,         // Chain::iter
    EnumCount,        // Chain::COUNT
    // TryFromPrimitive, // TryFrom<u64>
    Deserialize,
    Display,
)]
pub enum CexExchange {
    #[strum(ascii_case_insensitive, serialize = "BITFINEX")]
    BITFINEX,

    #[strum(ascii_case_insensitive, serialize = "BINANCE")]
    BINANCE,
}

impl Default for CexExchange {
    fn default() -> Self {
        CexExchange::BITFINEX
    }
}
