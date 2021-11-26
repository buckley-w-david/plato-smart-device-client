mod error;
mod event;
mod logger;
mod message;
mod settings;
mod smart_device_client;
mod metadata;
mod types;

use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::channel, Arc};
use std::time::Duration;
use std::thread;

use anyhow::{format_err, Context, Error};

use event::{Event, Response};
use logger::Logger;
use message::Message;
use smart_device_client::SmartDeviceClient;

const SETTINGS_PATH: &str = "Settings.toml";

fn main() -> Result<(), Error> {
    let mut args = env::args().skip(1);
    let library_path = PathBuf::from(
        args.next()
            .ok_or_else(|| format_err!("missing argument: library path"))?,
    );
    let save_path = PathBuf::from(
        args.next()
            .ok_or_else(|| format_err!("missing argument: save path"))?,
    );
    let wifi = args
        .next()
        .ok_or_else(|| format_err!("missing argument: wifi status"))
        .and_then(|v| v.parse::<bool>().map_err(Into::into))?;
    let online = args
        .next()
        .ok_or_else(|| format_err!("missing argument: online status"))
        .and_then(|v| v.parse::<bool>().map_err(Into::into))?;

    let settings = settings::load_toml::<settings::Settings, _>(SETTINGS_PATH)
        .with_context(|| format!("can't load settings from {}", SETTINGS_PATH))?;

    let settings::Settings { log, .. } = settings;
    let logger = Logger::new(log);
    logger.debug(&[&"Starting plato-smart-device-client!"]);

    if !online {
        logger.debug(&[&"Not online!"]);
        if !wifi {
            logger.debug(&[&"Wifi is off!"]);
            logger.status(&[&"Establishing a network connection."]);
            Event::SetWifi(true).send();
        } else {
            logger.debug(&[&"Wifi is on!"]);
            logger.status(&[&"Waiting for the network to come up."]);
            // Throw away network coming up event
            let _event = Response::receive();
        }
    }

    if !save_path.exists() {
        logger.debug(&[&"Creating save directory"]);
        fs::create_dir(&save_path)?;
    }

    let settings::Settings { host, password, .. } = settings;

    let sigterm = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&sigterm))?;

    let (sender, receiver) = channel();

    let calibre_addr: Option<SocketAddr> = host;

    let sigterm2 = sigterm.clone();
    let handle = thread::spawn(move || {
        let client = SmartDeviceClient::new(calibre_addr, password, save_path, log).unwrap();
        client.serve(sigterm2, sender).unwrap();
    });

    loop {
        if sigterm.load(Ordering::Relaxed) {
            break;
        }

        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Message::Exit) => {
                sigterm.store(true, Ordering::Relaxed);
            }
            Err(_) => (),
        }
    }

    handle.join().unwrap();
    logger.status(&[&"Bye bye!"]);

    Ok(())
}
