use crate::{
    bitfinex::{
        auth,
        common::{CONF_FLAG_SEQ_ALL, CONF_OB_CHECKSUM},
        errors::*,
        events::*,
        orders::OrderType,
    },
    WsBackendSender, WsMessage,
};

use error_chain::bail;
use meta_util::time::get_current_ts;
use rust_decimal::Decimal;
use serde_json::{from_str, json};
use std::{
    net::TcpStream,
    sync::{
        mpsc::{channel, Receiver, TryRecvError},
        Arc, RwLock,
    },
};
use tracing::{error, info};
use tungstenite::{connect, protocol::WebSocket, stream::MaybeTlsStream, Message};
use url::Url;

pub static INFO: &str = "info";
pub static SUBSCRIBED: &str = "subscribed";
pub static AUTH: &str = "auth";
pub static CONF: &str = "conf";
pub static CHECKSUM: &str = "cs";
pub static FUNDING_CREDIT_SNAPSHOT: &str = "fcs";
pub static WEBSOCKET_URL: &str = "wss://api.bitfinex.com/ws/2";
pub static DEAD_MAN_SWITCH_FLAG: u8 = 4;

pub trait BitfinexEventHandler {
    fn on_connect(&mut self, event: NotificationEvent);
    fn on_auth(&mut self, event: NotificationEvent);
    fn on_subscribed(&mut self, event: NotificationEvent);
    fn on_heart_beat(&mut self, channel: i32, data: String, seq: SEQUENCE);
    fn on_checksum(&mut self, event: i64);
    fn on_data_event(&mut self, event: DataEvent);
    fn on_error(&mut self, message: Error);
    fn as_any(&self) -> &dyn std::any::Any;
}

pub enum EventType {
    Funding,
    Trading,
}

pub struct WebSockets {
    // socket: Option<(WebSocket<MaybeTlsStream<TcpStream>>, Response)>,
    sender: WsBackendSender, // send request to backend
    // rx: mpsc::Receiver<WsMessage>,
    pub event_handler: Option<Arc<RwLock<Box<dyn BitfinexEventHandler>>>>,
}

impl WebSockets {
    pub fn new(hander: Box<dyn BitfinexEventHandler>) -> (WebSockets, SocketBackhand) {
        let wss: String = WEBSOCKET_URL.to_string();
        let url = Url::parse(&wss).unwrap();

        match connect(url) {
            Ok(answer) => {
                let (tx, rx) = channel::<WsMessage>();
                let sender = WsBackendSender { tx };

                let handler_box = Arc::new(RwLock::new(hander));
                // let handler: &'static Arc<RwLock<Box<dyn EventHandler>>> = &handler_box;
                let handle_clone = Arc::clone(&handler_box);
                let backhand = SocketBackhand::new(answer.0, rx, Some(handle_clone));
                let websockets =
                    WebSockets { sender, event_handler: Some(Arc::clone(&handler_box)) };
                (websockets, backhand)
            }
            Err(e) => {
                error!("error in connect socket {:?}", e);
                unreachable!()
            }
        }
    }

    // pub fn connect(&mut self) -> Result<()> {

    // }

    // { event: 'conf', flags: CONF_FLAG_SEQ_ALL + CONF_OB_CHECKSUM }
    /// set configuration, defaults to seq and checksum
    pub fn conf(&mut self) {
        let msg = json!(
        {
            "event": "conf",
            "flags": CONF_FLAG_SEQ_ALL + CONF_OB_CHECKSUM
        });

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Trade, &msg.to_string()) {
            error!("conf error: {:?}", error_msg);
        }
    }

    // pub fn add_event_handler<H>(&mut self, handler: H)
    // where
    //     H: EventHandler + 'static,
    // {
    //     self.event_handler = Some(Box::new(handler));
    // }

    /// Authenticates the connection.
    ///
    /// The connection will be authenticated until it is disconnected.
    ///
    /// # Arguments
    ///
    /// * `api_key` - The API key
    /// * `api_secret` - The API secret
    /// * `dms` - Whether the dead man switch is enabled. If true, all account orders will be
    ///           cancelled when the socket is closed.
    pub fn auth<S>(&mut self, api_key: S, api_secret: S, dms: bool, filters: &[&str]) -> Result<()>
    where
        S: AsRef<str>,
    {
        let nonce = auth::generate_nonce()?;
        let auth_payload = format!("AUTH{}", nonce);
        let signature =
            auth::sign_payload(api_secret.as_ref().as_bytes(), auth_payload.as_bytes())?;

        let msg = json!({
            "event": "auth",
            "apiKey": api_key.as_ref(),
            "authSig": signature,
            "authNonce": nonce,
            "authPayload": auth_payload,
            "dms": if dms {Some(DEAD_MAN_SWITCH_FLAG)} else {None},
            "filters": filters,
        });

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Trade, &msg.to_string()) {
            error!("auth error: {:?}", error_msg);
        }

        Ok(())
    }

    pub fn subscribe_ticker<S>(&mut self, symbol: S, et: EventType)
    where
        S: Into<String>,
    {
        let local_symbol = self.format_symbol(symbol.into(), et);
        let msg = json!({"event": "subscribe", "channel": "ticker", "symbol": local_symbol });

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Stream, &msg.to_string()) {
            error!("subscribe_ticker error: {:?}", error_msg);
        }
    }

    pub fn subscribe_trades<S>(&mut self, symbol: S, et: EventType)
    where
        S: Into<String>,
    {
        let local_symbol = self.format_symbol(symbol.into(), et);
        let msg = json!({"event": "subscribe", "channel": "trades", "symbol": local_symbol });

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Trade, &msg.to_string()) {
            error!("subscribe_trades error: {:?}", error_msg);
        }
    }

    pub fn subscribe_candles<S>(&mut self, symbol: S, timeframe: S)
    where
        S: Into<String>,
    {
        let key: String = format!("trade:{}:t{}", timeframe.into(), symbol.into());
        let msg = json!({"event": "subscribe", "channel": "candles", "key": key });

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Trade, &msg.to_string()) {
            error!("subscribe_candles error: {:?}", error_msg);
        }
    }

    /// subscribe order book
    /// The Order Books channel allows you to keep track of the state of the Bitfinex order book.
    /// Tt is provided on a price aggregated basis with customizable precision.
    /// Upon connecting, you will receive a snapshot of the book
    /// followed by updates for any changes to the state of the book.
    /// # Arguments
    /// prec: Level of price aggregation (P0, P1, P2, P3, P4). The default is P0. P0 has 5 Number of significant figures;
    ///       while P4 has 1 Number of significant figures
    /// freq: Frequency of updates (F0, F1). F0=realtime / F1=2sec. The default is F0.
    /// len: Number of price points ("1", "25", "100", "250") [default="25"]
    pub fn subscribe_books<S, P, F>(&mut self, symbol: S, et: EventType, prec: P, freq: F, len: u32)
    where
        S: Into<String>,
        P: Into<String>,
        F: Into<String>,
    {
        let msg = json!(
        {
            "event": "subscribe",
            "channel": "book",
            "symbol": self.format_symbol(symbol.into(), et),
            "prec": prec.into(),
            "freq": freq.into(),
            "len": len
        });

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Trade, &msg.to_string()) {
            error!("subscribe_books error: {:?}", error_msg);
        }
    }

    pub fn submit_order<S>(&mut self, client_order_id: u128, symbol: S, qty: Decimal)
    where
        S: Into<String>,
    {
        let symbol_str: String = symbol.into();
        let qty_str: String = qty.to_string();
        info!("websockets submit order symbol: {:?}, qty {:?}", symbol_str, qty_str);
        let msg = json!(
        [
            0,
            "on", // order new
            null,
            {
                "gid": 0,
                "cid": client_order_id,
                "type": OrderType::EXCHANGE_MARKET.to_string(),
                "symbol": symbol_str,
                "amount": qty_str
                // "meta":option
            }
        ]);

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Trade, &msg.to_string()) {
            // self.error_hander(error_msg);
            error!(
                "submit_order error, order is: {:?}, error is: {:?}",
                msg.to_string(),
                error_msg
            );
        }
    }

    pub fn subscribe_raw_books<S>(&mut self, symbol: S, et: EventType)
    where
        S: Into<String>,
    {
        let msg = json!(
        {
            "event": "subscribe",
            "channel": "book",
            "prec": "R0",
            "pair": self.format_symbol(symbol.into(), et)
        });

        if let Err(error_msg) = self.sender.send(crate::MessageChannel::Stream, &msg.to_string()) {
            error!("subscribe_raw_books error: {:?}", error_msg);
        }
    }

    // fn error_hander(&mut self, error_msg: Error) {
    //     if let Some(ref mut h) = self.event_handler {
    //         h.on_error(error_msg);
    //     }
    // }

    fn format_symbol(&mut self, symbol: String, et: EventType) -> String {
        match et {
            EventType::Funding => format!("f{}", symbol),
            EventType::Trading => format!("t{}", symbol),
        }
    }
}

unsafe impl Send for SocketBackhand {}
unsafe impl Sync for SocketBackhand {}

pub struct SocketBackhand {
    rx: Receiver<WsMessage>,
    pub socket: WebSocket<MaybeTlsStream<TcpStream>>,
    event_handler: Option<Arc<RwLock<Box<dyn BitfinexEventHandler>>>>,
}

impl SocketBackhand {
    pub fn new(
        socket: WebSocket<MaybeTlsStream<TcpStream>>,
        rx: Receiver<WsMessage>,
        event_handler: Option<Arc<RwLock<Box<dyn BitfinexEventHandler>>>>,
    ) -> Self {
        Self { rx, socket, event_handler }
    }

    pub fn event_loop(&mut self) -> Result<()> {
        loop {
            loop {
                match self.rx.try_recv() {
                    Ok(msg) => match msg {
                        WsMessage::Text(_, text) => {
                            let time = get_current_ts().as_millis();
                            info!("socket write message {:?}, time: {:?}", text, time);
                            let ret = self.socket.write_message(Message::Text(text));
                            match ret {
                                Err(e) => {
                                    error!("error in socket write {:?}", e);
                                }
                                Ok(()) => {}
                            }
                        }
                        WsMessage::Close => {
                            info!("socket close");
                            return self.socket.close(None).map_err(|e| e.into());
                        }
                    },
                    Err(TryRecvError::Disconnected) => {
                        bail!("Disconnected")
                    }
                    Err(TryRecvError::Empty) => {
                        break;
                    }
                }
            }

            let message = self.socket.read_message()?;

            match message {
                Message::Text(text) => {
                    // println!("got msg: {:?}", text);
                    if let Some(ref mut h) = self.event_handler {
                        let mut _g_ret = h.write();
                        match _g_ret {
                            Ok(mut _g) => {
                                if text.contains(INFO) {
                                    let event: NotificationEvent = from_str(&text)?;
                                    _g.on_connect(event);
                                } else if text.contains(SUBSCRIBED) {
                                    let event: NotificationEvent = from_str(&text)?;
                                    _g.on_subscribed(event);
                                } else if text.contains(AUTH) {
                                    let event: NotificationEvent = from_str(&text)?;
                                    _g.on_auth(event);
                                } else if text.contains(CONF) {
                                    info!("got conf msg: {:?}", text);
                                } else {
                                    // debug!("receive raw event text: {:?}", text);
                                    let event: DataEvent = from_str(&text)?;
                                    _g.on_data_event(event);
                                }
                            }
                            Err(_e) => {
                                error!("error in acquire wirte lock");
                            }
                        }
                    }
                }
                Message::Binary(_) => {}
                Message::Ping(_) | Message::Pong(_) => {}
                Message::Close(e) => {
                    bail!(format!("Disconnected {:?}", e));
                }
                _ => {}
            }
        }
    }
}
