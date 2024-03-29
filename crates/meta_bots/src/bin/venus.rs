//! cex dex arbitrage bot

use ethers::prelude::*;
use gumdrop::Options;
use meta_address::{enums::Asset, get_rpc_info, get_token_info, TokenInfo};
use meta_bots::{
    venus::{
        check_arbitrage_status, notify_arbitrage_result, update_dex_swap_finalised_info,
        ArbitrageInstruction, ArbitragePair, CexInstruction, CexTradeInfo, DexInstruction,
        DexTradeInfo, SwapFinalisedInfo, CID,
    },
    VenusConfig,
};
use meta_cefi::{
    cefi_service::{AccessKey, CefiService, CexConfig},
    cex_currency_to_asset,
    model::CexEvent,
};
use meta_common::{
    enums::{CexExchange, DexExchange, Network},
    models::MarcketChange,
};
use meta_contracts::bindings::uniswapv3pool::SwapFilter;
use meta_dex::{DexBackend, DexService};
use meta_integration::Lark;
use meta_tracing::init_tracing;
use meta_util::{get_price_delta_in_bp, time::get_current_ts};
use rust_decimal::{prelude::FromPrimitive, Decimal};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc, Arc,
    },
    time::Duration,
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

lazy_static::lazy_static! {
    // static ref TOKIO_RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
    static ref ARBITRAGES: Arc<RwLock<BTreeMap<CID, ArbitragePair>>> = Arc::new(RwLock::new(BTreeMap::new())); // key is request id
    static ref TOTAL_PENDING_TRADES: AtomicU32 = AtomicU32::new(0);
}

pub const MIN_ASSET_BALANCE_MULTIPLIER: usize = 5;
static mut MIN_BASE_ASSET_BALANCE_AMT: Decimal = Decimal::ZERO;

type Spread = Arc<RwLock<Option<(Decimal, Decimal)>>>; //(bid, ask)

#[derive(Debug, Clone, Options)]
struct Opts {
    help: bool,

    #[options(help = "base token, such as USDT")]
    base_asset: Option<Asset>,

    #[options(help = "quote token, tokenIn, such as WBNB, BUSD")]
    quote_asset: Option<Asset>,

    #[options(help = "dex a, such as PANCAKE")]
    dex: Option<DexExchange>,

    #[options(help = "blockchain network, such as ETH, ARBI")]
    network: Option<Network>,

    #[options(help = "dex a, such as BISWAP")]
    cex: Option<CexExchange>,

    #[options(help = "path to your private key")]
    private_key_path: Option<PathBuf>,
}

pub const V3_FEE: u32 = 500u32;

/// will be invoked when a new cex trade or dex swap occurs
async fn handle_trade_update<M: Middleware>(dex_service: Arc<DexService<M>>, lark: Arc<Lark>) {
    let (should_stop, ret) = check_arbitrage_status(Arc::clone(&ARBITRAGES)).await;
    if should_stop {
        error!("should stop");
        std::process::exit(exitcode::DATAERR);
    }
    if let Some((cid, arbitrage_info)) = ret {
        notify_arbitrage_result(dex_service, Arc::clone(&ARBITRAGES), lark, cid, &arbitrage_info)
            .await;
    }
}

async fn run(config: VenusConfig) -> anyhow::Result<()> {
    debug!("run venus app with config: {:?}", config);
    unsafe {
        MIN_BASE_ASSET_BALANCE_AMT = Decimal::from_usize(MIN_ASSET_BALANCE_MULTIPLIER)
            .unwrap()
            .checked_mul(config.base_asset_quote_amt)
            .unwrap();
    }
    let rpc_info = get_rpc_info(config.network).unwrap();

    let rpc_provider = config.provider.provider.expect("need rpc provider");
    let rpc_url = rpc_info.ws_urls.get(&rpc_provider).unwrap();
    info!("rpc_url {:?}", rpc_url);
    let provider_ws = Provider::<Ws>::connect(rpc_url).await.expect("ws connect error");
    let provider_ws =
        provider_ws.interval(Duration::from_millis(config.provider.ws_interval_milli.unwrap()));
    let provider_ws = Arc::new(provider_ws);

    let last_block = provider_ws.get_block(BlockNumber::Latest).await?.unwrap().number.unwrap();

    let private_key = std::fs::read_to_string(config.account.private_key_path.unwrap())
        .unwrap()
        .trim()
        .to_string();
    let wallet: LocalWallet =
        private_key.parse::<LocalWallet>().unwrap().with_chain_id(rpc_info.chain_id);
    let wallet_address = wallet.address();
    let wallet = SignerMiddleware::new(Arc::clone(&provider_ws), wallet);
    let wallet = NonceManagerMiddleware::new(wallet, wallet_address);
    let wallet = Arc::new(wallet);

    let (base_token, quote_token) = (config.base_asset.into(), config.quote_asset.into());
    let base_token_info = get_token_info(base_token, config.network).unwrap();
    let quote_token_info = get_token_info(quote_token, config.network).unwrap();
    let (base_token_address, quote_token_address) =
        (base_token_info.address, quote_token_info.address);
    let base_token: TokenInfo = TokenInfo {
        token: base_token,
        decimals: base_token_info.decimals,
        network: config.network,
        address: base_token_address,
        unwrap_to: None,
        byte_code: None,
        code_hash: None,
        native: false,
    };
    let quote_token: TokenInfo = TokenInfo {
        token: quote_token,
        decimals: quote_token_info.decimals,
        network: config.network,
        address: quote_token_address,
        unwrap_to: None,
        byte_code: None,
        native: false,
        code_hash: None,
    };

    let (tx_market_change, rx_market_change) = mpsc::sync_channel::<MarcketChange>(1000);

    let (dex_service, mut dex_backend): (
        DexService<NonceManagerMiddleware<SignerMiddleware<Arc<Provider<Ws>>, LocalWallet>>>,
        DexBackend<Provider<Ws>>,
    ) = DexService::new(
        Arc::clone(&wallet),
        Arc::clone(&provider_ws),
        config.network,
        config.dex,
        base_token.clone(),
        config.base_asset_quote_amt,
        quote_token.clone(),
        V3_FEE,
        tx_market_change.clone(),
    );
    let dex_service = Arc::new(dex_service);

    let lark = Arc::new(Lark::new(config.lark.webhook));

    let (tx_cex_event, rx_cex_event) = mpsc::sync_channel::<CexEvent>(1000);
    let mut map = BTreeMap::new();
    let ak = match config.cex {
        CexExchange::BITFINEX => config.bitfinex.unwrap(),
        CexExchange::BINANCE => config.binance.unwrap(),
    };
    map.insert(
        config.cex,
        AccessKey { api_key: ak.api_key.to_string(), api_secret: ak.api_secret.to_string() },
    );
    let cex_config = CexConfig { keys: Some(map) };
    let cefi_service = CefiService::new(
        Some(cex_config),
        Some(tx_market_change.clone()),
        Some(tx_cex_event.clone()),
    );

    let cefi_service = Arc::new(RwLock::new(cefi_service));
    {
        let mut _g = cefi_service.write().await;
        (_g).connect_pair(config.cex, config.base_asset, config.quote_asset).await;
    }

    let (cex_spread, dex_spread): (Spread, Spread) =
        (Arc::new(RwLock::new(None)), Arc::new(RwLock::new(None)));

    match config.dex {
        DexExchange::UniswapV3 => {
            let pool = dex_service
                .dex_contracts
                .get_v3_pool(base_token_address, quote_token_address, V3_FEE)
                .await
                .unwrap();

            let (tx, mut rx) =
                tokio::sync::mpsc::unbounded_channel::<(TxHash, SwapFinalisedInfo)>();
            {
                // consume onchain swap event
                let dex_service_clone = Arc::clone(&dex_service);
                let lark_clone = Arc::clone(&lark);
                tokio::spawn(async move {
                    loop {
                        let maybe_hash = rx.recv().await;
                        if let Some((hash, _number)) = maybe_hash {
                            info!("receive onchain swap event with hash {:?}", hash);
                            handle_trade_update(dex_service_clone.clone(), Arc::clone(&lark_clone))
                                .await;
                        }
                    }
                });
            }

            {
                //TODO: to be moved to dex service; subscribing onchain swap event
                tokio::spawn(async move {
                    let v3_pool_swap_filter = pool
                        .event::<SwapFilter>()
                        .from_block(last_block)
                        .topic2(ValueOrArray::Value(H256::from(wallet_address)));

                    let mut my_swap_stream =
                        v3_pool_swap_filter.subscribe().await.unwrap().with_meta();
                    loop {
                        let next = my_swap_stream.next().await;
                        if let Some(log) = next {
                            let (swap_log, meta) = log.unwrap() as (SwapFilter, LogMeta);

                            info!(
                                "block: {:?}, hash: {:?}, address: {:?}, log {:?}",
                                meta.block_number, meta.transaction_hash, meta.address, swap_log
                            );
                            let swap_info =
                                SwapFinalisedInfo { block_number: meta.block_number.as_u64() };
                            update_dex_swap_finalised_info(
                                Arc::clone(&ARBITRAGES),
                                meta.transaction_hash,
                                swap_info.clone(),
                            )
                            .await;
                            let ret = tx.send((meta.transaction_hash, swap_info));
                            match ret {
                                Err(e) => error!("error in send swap event {:?}", e),
                                _ => {}
                            }
                        }
                    }
                });
            }

            {
                // listening to dex price change
                tokio::spawn(async move {
                    let _ = dex_backend.event_loop().await;
                });
            }
        }
        _ => {
            todo!()
        }
    }

    {
        let _arbitrages_map_cefi_trade = Arc::clone(&ARBITRAGES);
        let _provider_ws_cefi_trade = Arc::clone(&provider_ws);
        let dex_spread = Arc::clone(&dex_spread);
        let dex_service_clone = Arc::clone(&dex_service);
        tokio::spawn(async move {
            // subscribing cex event
            loop {
                let cex_event_ret = rx_cex_event.recv();
                if let Ok(cex_event) = cex_event_ret {
                    match cex_event {
                        CexEvent::Balance(wu) => {
                            info!("receive wallet update event {:?}", wu);
                            // TODO: use enum
                            if wu.wallet_type.eq("exchange") {
                                let asset = cex_currency_to_asset(config.cex, &wu.currency);
                                if asset.eq(&config.base_asset) {
                                    unsafe {
                                        if wu.balance.le(&MIN_BASE_ASSET_BALANCE_AMT) {
                                            warn!(
                                                "asset {:?} balance {:?} is below threshold {:?}",
                                                asset, wu.balance, MIN_BASE_ASSET_BALANCE_AMT
                                            );
                                            std::process::exit(exitcode::DATAERR);
                                        }
                                    }
                                }

                                if asset.eq(&config.quote_asset) {
                                    let _g = dex_spread.read().await;
                                    if let Some(p) = *_g {
                                        unsafe {
                                            let min_quote_amt =
                                                p.0.checked_mul(MIN_BASE_ASSET_BALANCE_AMT)
                                                    .unwrap();
                                            if wu.balance.le(&min_quote_amt) {
                                                warn!("asset {:?} balance {:?} is below threshold {:?}", asset, wu.balance, min_quote_amt);
                                                std::process::exit(exitcode::DATAERR);
                                            }
                                        }
                                    }

                                    drop(_g);
                                }
                            }
                        }
                        CexEvent::TradeExecution(trade) => {
                            info!("receive trade execution event {:?}", trade);
                            {
                                TOTAL_PENDING_TRADES.fetch_sub(1, Ordering::SeqCst);
                                let mut _g = _arbitrages_map_cefi_trade.write().await;
                                _g.entry(trade.client_order_id.into()).and_modify(|e| {
                                    e.cex.trade_info = Some(trade);
                                });
                            }; // drop _g
                            {
                                handle_trade_update(dex_service_clone.clone(), Arc::clone(&lark))
                                    .await;
                            }
                        }
                    }
                }
            }
        });
    };

    let (mut cex_bid, mut cex_ask, mut dex_bid, mut dex_ask): (
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
        Option<Decimal>,
    ) = (None, None, None, None);
    loop {
        if let Ok(change) = rx_market_change.recv() {
            // println!("receive market change: {:?}", change);
            if let Some(spread) = change.cex {
                {
                    let mut _g = cex_spread.write().await;
                    (*_g) = Some((spread.best_bid, spread.best_ask));
                    (cex_bid, cex_ask) = (Some(spread.best_bid), Some(spread.best_ask));
                }
            }
            if let Some(spread) = change.dex {
                {
                    let mut _g = dex_spread.write().await;
                    (*_g) = Some((spread.best_bid, spread.best_ask));
                    (dex_bid, dex_ask) = (Some(spread.best_bid), Some(spread.best_ask));
                }
            }

            if let (Some(cex_bid), Some(cex_ask), Some(dex_bid), Some(dex_ask)) =
                (cex_bid, cex_ask, dex_bid, dex_ask)
            {
                info!(
                    "current spread, cex_bid: {:?}, dex_ask: {:?}, dex_bid: {:?}, cex_ask {:?}",
                    cex_bid, dex_ask, dex_bid, cex_ask
                );

                if cex_bid > dex_ask {
                    let change = get_price_delta_in_bp(cex_bid, dex_ask);
                    if change > Decimal::from_u32(config.spread_diff_threshold).unwrap() {
                        info!(
                            "found a cross, cex bid {:?}, dex ask {:?}, price change {:?}",
                            cex_bid, dex_ask, change
                        );
                        let mut amount = config.base_asset_quote_amt;
                        amount.set_sign_negative(true);
                        let instraction = ArbitrageInstruction {
                            cex: CexInstruction {
                                venue: CexExchange::BITFINEX,
                                amount,
                                base_asset: config.base_asset,
                                quote_asset: config.quote_asset,
                            },
                            dex: DexInstruction {
                                network: config.network,
                                venue: DexExchange::UniswapV3,
                                amount: config.base_asset_quote_amt,
                                base_token: base_token.clone(),
                                quote_token: quote_token.clone(),
                                recipient: wallet_address,
                                fee: V3_FEE,
                            },
                        };

                        try_arbitrage(instraction, Arc::clone(&cefi_service), &dex_service).await;
                    }
                }

                if dex_bid > cex_ask {
                    let change = get_price_delta_in_bp(dex_bid, cex_ask);
                    if change > Decimal::from_u32(config.spread_diff_threshold).unwrap() {
                        // sell dex, buy cex
                        info!(
                            "found a cross, dex bid {:?}, cex ask {:?}, price change {:?}",
                            dex_bid, cex_ask, change
                        );
                        let mut amount = config.base_asset_quote_amt;
                        amount.set_sign_negative(true);
                        let instraction = ArbitrageInstruction {
                            cex: CexInstruction {
                                venue: config.cex,
                                amount: config.base_asset_quote_amt,
                                base_asset: config.base_asset,
                                quote_asset: config.quote_asset,
                            },
                            dex: DexInstruction {
                                network: config.network,
                                venue: config.dex,
                                amount,
                                base_token: base_token.clone(),
                                quote_token: quote_token.clone(),
                                recipient: wallet_address,
                                fee: V3_FEE,
                            },
                        };

                        try_arbitrage(instraction, Arc::clone(&cefi_service), &dex_service).await;
                    }
                }
            }
        }
    }
}

async fn try_arbitrage<'a, M: Middleware + 'static>(
    instruction: ArbitrageInstruction,
    cefi_service_ptr: Arc<RwLock<CefiService>>,
    dex_service_ref: &DexService<M>,
) {
    let total = TOTAL_PENDING_TRADES.load(Ordering::Relaxed);
    if total > 5 {
        warn!("total pending number of trades are {:?}, skip trade for now", total);
        return;
    }
    let _total = TOTAL_PENDING_TRADES.fetch_add(1, Ordering::SeqCst);
    let client_order_id = get_current_ts().as_millis();

    info!("start arbitrage with instruction {:?}", instruction);
    info!(
        "start send cex trade, venue {:?}, base_asset {:?}, quote_asset {:?}, amount {:?}",
        instruction.cex.venue,
        instruction.cex.base_asset,
        instruction.cex.quote_asset,
        instruction.cex.amount
    );

    {
        let mut _g = ARBITRAGES.write().await;
        let date_time = chrono::Utc::now();
        _g.insert(
            client_order_id,
            ArbitragePair {
                datetime: date_time,
                base: instruction.cex.base_asset,
                quote: instruction.cex.quote_asset,
                cex: CexTradeInfo { venue: instruction.cex.venue, trade_info: None },
                dex: DexTradeInfo {
                    network: instruction.dex.network,
                    venue: instruction.dex.venue,
                    tx_hash: None,
                    base_token_info: instruction.dex.base_token.clone(),
                    quote_token_info: instruction.dex.quote_token.clone(),
                    v3_fee: Some(instruction.dex.fee),
                    created: date_time,
                    finalised_info: None,
                },
            },
        );
    }

    {
        let mut _cex = cefi_service_ptr.write().await;
        (_cex)
            .submit_order(
                client_order_id,
                instruction.cex.venue,
                instruction.cex.base_asset,
                instruction.cex.quote_asset,
                instruction.cex.amount,
            )
            .await;
        info!("end send cex trade");
    }

    match instruction.dex.venue {
        DexExchange::UniswapV3 => {
            let ret = dex_service_ref
                .submit_order(
                    instruction.dex.base_token,
                    instruction.dex.quote_token,
                    instruction.dex.amount,
                    instruction.dex.fee,
                    instruction.dex.recipient,
                )
                .await;
            match ret {
                Ok(hash) => {
                    let mut _g = ARBITRAGES.write().await;
                    info!("send dex order success {:?}", hash);
                    _g.entry(client_order_id).and_modify(|e| {
                        e.dex.tx_hash = Some(hash);
                    });
                }
                Err(e) => error!("error in send dex order {:?}", e),
            }
        }
        _ => unimplemented!(),
    }
    info!("end send dex trade");
}

async fn main_impl() -> anyhow::Result<()> {
    let opts = Opts::parse_args_default_or_exit();
    println!("opts: {:?}", opts);

    let mut app_config = VenusConfig::try_new().expect("parsing config error");
    if let Some(network) = opts.network {
        app_config.network = network;
    }
    if let Some(dex) = opts.dex {
        app_config.dex = dex;
    }
    if let Some(cex) = opts.cex {
        app_config.cex = cex;
    }
    if let Some(asset) = opts.base_asset {
        app_config.base_asset = asset;
    }
    if let Some(asset) = opts.quote_asset {
        app_config.quote_asset = asset;
    }

    if let Some(pk_path) = opts.private_key_path {
        app_config.account.private_key_path = Some(pk_path);
    }

    app_config.log.file_name_prefix.push('_');
    app_config.log.file_name_prefix.push_str(app_config.base_asset.as_ref());
    app_config.log.file_name_prefix.push_str(app_config.quote_asset.as_ref());
    let _guard = init_tracing(app_config.log.clone().into());

    debug!("venus config: {:?}", app_config);
    run(app_config).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    match main_impl().await {
        Ok(_) => {
            std::process::exit(exitcode::OK);
        }
        Err(e) => {
            error!("run Error: {}", e);
            std::process::exit(exitcode::DATAERR);
        }
    }
}

#[cfg(test)]
mod test {

    // #[test]
    // fn test_check_arbitrage_status() {
    //     let mut map: BTreeMap<CID, ArbitragePair> = BTreeMap::new();
    //     map.insert(
    //         u128::from(100u32),
    //         ArbitragePair {
    //             base: Asset::ARB,
    //             quote: Asset::USD,
    //             cex: CexTradeInfo { venue: CexExchange::BITFINEX, trade_info: None },
    //             dex: DexTradeInfo {
    //                 network: Network::ARBI,
    //                 venue: DexExchange::UniswapV3,
    //                 tx_hash: None,
    //                 base_token_info: TokenInfo {
    //                     token: Token::ARB,
    //                     decimals: 18,
    //                     network: Network::ARBI,
    //                     address: address_from_str("0x89dbEA2B8c120a60C086a5A7f73cF58261Cb9c44"),
    //                 },
    //                 quote_token_info: TokenInfo {
    //                     token: Token::USD,
    //                     decimals: 6,
    //                     network: Network::ARBI,
    //                     address: address_from_str("0x89dbEA2B8c120a60C086a5A7f73cF58261Cb9c44"),
    //                 },
    //                 v3_fee: None,
    //             },
    //         },
    //     );
    //     map.insert(
    //         u128::from(200u32),
    //         ArbitragePair {
    //             base: Asset::ARB,
    //             quote: Asset::USD,
    //             cex: CexTradeInfo {
    //                 venue: CexExchange::BITFINEX,
    //                 trade_info: Some(TradeExecutionUpdate::default()),
    //             },
    //             dex: DexTradeInfo {
    //                 network: Network::ARBI,
    //                 venue: DexExchange::UniswapV3,
    //                 tx_hash: None,
    //                 base_token_info: TokenInfo {
    //                     token: Token::ARB,
    //                     decimals: 18,
    //                     network: Network::ARBI,
    //                     address: address_from_str("0x89dbEA2B8c120a60C086a5A7f73cF58261Cb9c44"),
    //                 },
    //                 quote_token_info: TokenInfo {
    //                     token: Token::USD,
    //                     decimals: 6,
    //                     network: Network::ARBI,
    //                     address: address_from_str("0x89dbEA2B8c120a60C086a5A7f73cF58261Cb9c44"),
    //                 },
    //                 v3_fee: None,
    //             },
    //         },
    //     );
    //     let map = Arc::new(std::sync::RwLock::new(map));
    //     let output = check_arbitrage_status(map.clone());
    //     assert!(output.is_none());

    //     {
    //         let mut _g = map.write().unwrap();
    //         _g.entry(u128::from(200u32)).and_modify(|e| {
    //             (*e).dex.tx_hash = Some(tx_hash_from_str(
    //                 "0xcba0d4fc27a32aaddece248d469beb430e29c1e6fecdd5db3383e1c8b212cdeb",
    //             ))
    //         });
    //     }
    //     let output = check_arbitrage_status(map.clone());
    //     assert!(output.is_some());
    //     assert_eq!(output.unwrap().0, u128::from(200u32));
    // }
}
