use std::{error::Error, sync::Arc};

use bebop::{Record, SliceWrapper};
use native_db::Database;
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message;

use crate::{
    schema::{Category, Data, Disconnect, Expired, Ping},
    Settings,
};

use super::traits::StringUtil;

#[cfg(feature = "rinf")]
#[cfg(not(feature = "debug"))]
#[macro_export]
macro_rules! log_debug {
    ($($rest:tt)*) => {
        if cfg!(feature = "rinf") {
            rinf::debug_print!($($rest)*);
        }
    };
}

#[cfg(not(feature = "rinf"))]
#[cfg(feature = "debug")]
#[macro_export]
macro_rules! log_debug {
    ($($rest:tt)*) => {
        if cfg!(feature = "debug") {
            log::debug!($($rest)*)
        }
    };
}

#[cfg(not(feature = "rinf"))]
#[cfg(not(feature = "debug"))]
#[macro_export]
macro_rules! log_debug {
    ($($rest:tt)*) => {
        if cfg!(debug_assertions) {
            println!($($rest)*)
        }
    };
}

#[cfg(feature = "rinf")]
#[cfg(not(feature = "debug"))]
#[macro_export]
macro_rules! log_error {
    ($($rest:tt)*) => {
        if cfg!(feature = "rinf") {
            rinf::debug_print!($($rest)*);
        }
    };
}

#[cfg(not(feature = "rinf"))]
#[cfg(feature = "debug")]
#[macro_export]
macro_rules! log_error {
    ($($rest:tt)*) => {
        if cfg!(feature = "debug") {
            log::error!($($rest)*)
        }
    };
}

#[cfg(not(feature = "rinf"))]
#[cfg(not(feature = "debug"))]
#[macro_export]
macro_rules! log_error {
    ($($rest:tt)*) => {
        if cfg!(debug_assertions) {
            println!($($rest)*)
        }
    };
}

pub async fn get_setting_by_key(
    db: Arc<RwLock<Database<'static>>>,
    key: String,
) -> Result<Option<Settings>, Box<dyn Error>> {
    let db = db.read().await;
    let reader = db.r_transaction()?;

    Ok(reader.get().primary::<Settings>(key)?)
}

pub async fn set_setting(
    db: Arc<RwLock<Database<'static>>>,
    settings: Settings,
) -> Result<bool, Box<dyn Error>> {
    let db = db.read().await;
    let reader = db.r_transaction()?;
    let writer = db.rw_transaction()?;

    let setting = reader
        .get()
        .primary::<Settings>(settings.key.copy_string())?;
    drop(reader);

    match setting {
        Some(setting) => {
            writer.update::<Settings>(setting, settings)?;
        }
        None => {
            writer.insert::<Settings>(settings)?;
        }
    }
    writer.commit()?;

    Ok(true)
}

pub fn make_ping_message(peer: &str) -> Message {
    let mut datas = Vec::new();
    Ping {
        peer,
        activations: 0,
    }
    .serialize(&mut datas)
    .unwrap();
    make_response_message(Category::Ping, datas)
}

pub fn make_response_message(category: Category, datas: Vec<u8>) -> Message {
    let mut result = Vec::new();
    Data {
        category: category as u16,
        datas: SliceWrapper::from_raw(&datas),
    }
    .serialize(&mut result)
    .unwrap();
    Message::Binary(result)
}

pub fn make_disconnect_message(peer: &str) -> Message {
    let mut datas = Vec::new();
    Disconnect { peer }.serialize(&mut datas).unwrap();
    make_response_message(Category::Disconnect, datas)
}

pub fn make_pong_message() -> Message {
    make_response_message(Category::Pong, Vec::new())
}

pub fn make_expired_output_message() -> Message {
    let mut datas = Vec::new();
    Expired { is_expired: true }.serialize(&mut datas).unwrap();
    make_response_message(Category::Expired, datas)
}
