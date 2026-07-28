mod connection;
#[allow(unused_imports)] // public re-export, part of WsEvent::Connected's payload type
pub use connection::{WsConnector, WsEvent, WsSink};
