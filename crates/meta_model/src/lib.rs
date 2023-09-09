use meta_address::enums::Asset;
use meta_common::enums::Network;
use rust_decimal::Decimal;
use std::time::Instant;

#[derive(Debug, Default)]
pub struct ArbitrageOutcome {
    pub base_amount: Decimal,
    pub quote_amount: Decimal,
    pub price: Decimal,
    pub fee_token: Asset,
    pub fee_amount: Decimal,
    pub id: String,
    pub network: Option<Network>,
}

#[derive(Debug)]
pub struct ArbitrageSummary {
    pub datetime: String,
    pub base: Asset,
    pub quote: Asset,
    pub cex: ArbitrageOutcome,
    pub dex: ArbitrageOutcome,
}