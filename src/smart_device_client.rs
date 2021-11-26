use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::time::Duration;

use anyhow::{anyhow, Error};

use lazy_static::lazy_static;
use nix::sys::statvfs;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use regex::Regex;
use serde::Serialize;
use serde_json::{self, json};

use sha1::{Digest, Sha1};

use crate::error::PlatoSmartDeviceClientError;
use crate::message::Message;

const BROADCAST_PORTS: &[u16] = &[54982, 48123, 39001, 44044, 59678];
const BROADCAST_MSG: &[u8] = &[104, 101, 108, 108, 111];
const BROADCAST_REGEX: &str = r"calibre wireless device client \(on (.+)\);(\d+),(\d+)";

use const_format::concatcp;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");
const NAME: &'static str = env!("CARGO_PKG_NAME");
const DEVICE_NAME: &'static str = concatcp!(NAME, "(", VERSION, ")");

#[derive(FromPrimitive, Debug, Serialize)]
pub enum Opcode {
    NOOP = 12,
    OK = 0,
    BookDone = 11,
    CalibreBusy = 18,
    SetLibraryInfo = 19,
    DeleteBook = 13,
    DisplayMessage = 17,
    Error = 20,
    FreeSpace = 5,
    GetBookFileSegment = 14,
    GetBookMetadata = 15,
    GetBookCount = 6,
    GetDeviceInformation = 3,
    GetInitializationInfo = 9,
    SendBooklists = 7,
    SendBook = 8,
    SendBookMetadata = 16,
    SetCalibreDeviceInfo = 1,
    SetCalibreDeviceName = 2,
    TotalSpace = 4,
}

#[derive(Debug)]
pub struct SmartDeviceClient {
    addr: SocketAddr,
    password: Option<String>,
    save_path: PathBuf,
}

impl SmartDeviceClient {
    pub fn new(
        addr: Option<SocketAddr>,
        password: Option<String>,
        save_path: PathBuf,
    ) -> Result<SmartDeviceClient, Error> {
        let addr = addr.or(SmartDeviceClient::find_calibre_server()?).ok_or(
            PlatoSmartDeviceClientError::new("Unable to find calibre server address"),
        )?;

        Ok(SmartDeviceClient {
            addr,
            password,
            save_path,
        })
    }

    fn find_calibre_server() -> Result<Option<SocketAddr>, Error> {
        lazy_static! {
            static ref REGEX: Regex =
                Regex::new(BROADCAST_REGEX).expect("cannot parse broadcast response regex");
        }
        let mut calibre_addr = None;
        let addr = SocketAddr::from(([0, 0, 0, 0], 0));
        let broadcast_socket = UdpSocket::bind(addr)?;
        broadcast_socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        broadcast_socket.set_broadcast(true)?;
        for &port in BROADCAST_PORTS {
            let broadcast = SocketAddr::from(([255, 255, 255, 255], port));
            if let Ok(_size) = broadcast_socket.send_to(BROADCAST_MSG, broadcast) {
                let mut buf = [0; 1024];
                let (number_of_bytes, src_addr) = broadcast_socket.recv_from(&mut buf)?;
                let filled_buf = &mut buf[..number_of_bytes];
                calibre_addr = REGEX
                    .captures(std::str::from_utf8(filled_buf)?)
                    .and_then(|cap| {
                        Some(SocketAddr::new(
                            src_addr.ip(),
                            cap[3].parse().expect("Unable to parse port"),
                        ))
                    });
                break;
            }
        }
        Ok(calibre_addr)
    }

    // Use this to recieve stuff like files
    fn read_binary_from_net(mut stream: &TcpStream, length: usize) -> Result<Vec<u8>, Error> {
        let mut buffer = vec![0; length];
        let mut buf = &mut buffer[..];
        stream.read_exact(&mut buf)?;
        Ok(buffer)
    }

    // Don't judge me for this function
    fn read_command_from_net(mut stream: &TcpStream) -> Result<(Opcode, serde_json::Value), Error> {
        let mut length_buffer = vec![];
        // This is a god damned mess

        // I'm reading 1 byte at a time to simplify this
        // calibre source isn't much better and does it 2 at a time
        let mut buf = [0; 1];
        loop {
            match stream.read_exact(&mut buf) {
                Ok(_) => {
                    if buf[0] == 91 {
                        // 91 is '['
                        break;
                    }
                    length_buffer.push(buf[0]);
                }
                Err(e) => return Err(e.into()), // This looks weird
            };
        }
        let length: usize = std::str::from_utf8(&length_buffer)?.parse()?;
        let mut message_buffer = vec![0; length];
        message_buffer[0] = 91; // = '[';
        let mut buf = &mut message_buffer[1..]; // We use 1 here since the first character is already read in (the 91 from above)

        // Read the rest of the data into the message buffer
        stream.read_exact(&mut buf)?;

        // This dancing around with read_exact _feels_ wrong.
        // Maybe it's a rust thing, but it is strange that I can't seem to
        // read a specific number of bytes without doing it like this.
        let encoded_json = std::str::from_utf8(message_buffer.as_slice())?;
        let value: serde_json::Value = serde_json::from_str(encoded_json)?;
        let arr = value.as_array().ok_or(PlatoSmartDeviceClientError::new(
            "Unable to parse calibre response",
        ))?;

        let opcode = arr[0]
            .as_i64()
            .ok_or(PlatoSmartDeviceClientError::new("Opcode is not a number"))?;

        let message = arr[1].to_owned();

        let x: Opcode = FromPrimitive::from_i64(opcode)
            .ok_or(PlatoSmartDeviceClientError::new("Opcode not registered"))?;

        Ok((x, message))
    }

    pub fn serve(&self, sigterm: Arc<AtomicBool>, sender: Sender<Message>) -> Result<(), Error> {
        // TODO: Need to do non-blocking io for responsive sigterm handling
        //       Also need to figure out how to do some kind of select call or something
        //       to know when there is something to be read from the socket
        let stream = TcpStream::connect(&self.addr)?;
        stream.set_read_timeout(Some(Duration::from_secs(300)))?;

        loop {
            if sigterm.load(Ordering::Relaxed) {
                break;
            }

            let result = SmartDeviceClient::read_command_from_net(&stream);
            if let Ok((opcode, message)) = result {
                let response = match opcode {
                    Opcode::NOOP => self.on_noop(message, &sender),
                    Opcode::CalibreBusy => self.on_calibre_busy(message, &sender),
                    Opcode::SetLibraryInfo => self.on_set_library_info(message, &sender),
                    Opcode::DeleteBook => self.on_delete_book(message, &sender),
                    Opcode::DisplayMessage => self.on_display_message(message, &sender),
                    Opcode::FreeSpace => self.on_free_space(message, &sender),
                    Opcode::GetBookFileSegment => self.on_get_book_file_segment(message, &sender),
                    Opcode::GetBookMetadata => self.on_get_book_metadata(message, &sender),
                    Opcode::GetBookCount => self.on_get_book_count(message, &sender),
                    Opcode::GetDeviceInformation => {
                        self.on_get_device_information(message, &sender)
                    }
                    Opcode::GetInitializationInfo => {
                        self.on_get_initialization_info(message, &sender)
                    }
                    Opcode::SendBooklists => self.on_send_booklists(message, &sender),
                    Opcode::SendBook => self.on_send_book(message, &sender),
                    Opcode::SendBookMetadata => self.on_send_book_metadata(message, &sender),
                    Opcode::SetCalibreDeviceInfo => {
                        self.on_set_calibre_device_info(message, &sender)
                    }
                    Opcode::SetCalibreDeviceName => {
                        self.on_set_calibre_device_name(message, &sender)
                    }
                    Opcode::TotalSpace => self.on_total_space(message, &sender),
                    _ => Ok(None),
                };

                let send_result = match response {
                    Ok(Some(resp)) => SmartDeviceClient::send(&stream, (Opcode::OK, resp)),
                    Err(Some(resp)) => SmartDeviceClient::send(&stream, (Opcode::Error, resp)),
                    Err(None) => Err(anyhow!(format!("Error resposne to {:?}", opcode))),
                    _ => Ok(()),
                };
                if send_result.is_err() {
                    dbg!(&send_result);
                    sender
                        .send(Message::Notify(
                            "Error communicating with calibre...".to_string(),
                        ))
                        .unwrap();
                    // Should I exit here?
                    // sender.send(Message::Exit).unwrap();
                }
            } else {
                dbg!(&result);
                sender
                    .send(Message::Notify(
                        "Something went wrong! Exiting...".to_string(),
                    ))
                    .unwrap();
                sender.send(Message::Exit).unwrap();
            }
        }

        Ok(())
    }

    fn send(mut stream: &TcpStream, response: (Opcode, serde_json::Value)) -> Result<(), Error> {
        let (opcode, value) = response;
        let s = json!([opcode as u8, value]).to_string();
        let mut calibre_encoded = s.len().to_string();
        calibre_encoded.push_str(&s);
        let bytes = calibre_encoded.as_bytes();

        Ok(stream.write_all(bytes)?)
    }

    // NOOP Sometimes returns a response
    fn on_noop(
        &self,
        message: serde_json::Value,
        sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // logger.debug("NOOP: %s", payload)

        // calibre wants to close the socket, time to disconnect
        if let Some(_ejecting) = message.get("ejecting") {
            sender.send(Message::Exit).unwrap();
            Ok(Some(json!({})))
        } else if let Some(count) = message.get("count") {
            // calibre announces the count of books that need more metadata
            Ok(None)
        } else if let Some(pri_key) = message.get("priKey") {
            // calibre requests more metadata for a book by its index
            // This might get weird, since calibre sends us a a bunch of these messages without
            // waiting for a response, then later does a read for each one it sent.
            // TODO
            // local book = CalibreMetadata:getBookMetadata(arg.priKey)
            // logger.dbg(string.format("sending book metadata %d/%d", self.current, self.pending))
            // self:sendJsonData('OK', book)
            // if self.current == self.pending then
            //     self.current = nil
            //     self.pending = nil
            //     return
            // end
            // self.current = self.current + 1
            // return
            Ok(Some(json!({})))
        } else {
            // keep-alive NOOP
            Ok(Some(json!({})))
        }
    }

    fn on_calibre_busy(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        Ok(None)
    }

    // SET_LIBRARY_INFO requires a response
    fn on_set_library_info(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // TODO actual implementation
        Ok(Some(json!({})))
    }

    // DELETE_BOOK requires a response
    // FIXME: This function shows a flaw in the client structure
    //        The DELETE_BOOK opcode expects a response to the call, then a response for each book
    fn on_delete_book(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        Ok(None)
    }

    // DISPLAY_MESSAGE kinda requires a response...
    // it does call with wait_for_response, but doesn't do anything with the result and wraps the
    // calls in try/except: pass
    fn on_display_message(
        &self,
        message: serde_json::Value,
        sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        sender.send(Message::Notify(message.to_string())).unwrap();
        Ok(Some(json!({})))
    }

    // FREE_SPACE requires a response
    fn on_free_space(
        &self,
        _message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // FIXME: Should probably check the status on the save path that we own
        let info = statvfs::statvfs(&self.save_path);
        if let Ok(info) = info {
            let fbs = info.fragment_size() as u64;
            let free = info.blocks_free() as u64 * fbs;
            // let total = info.blocks() as u64 * fbs;
            Ok(Some(json!({ "free_space_on_device": free })))
        } else {
            Err(Some(json!({
                "message": "Unable to get size information",
            })))
        }
    }

    // GET_BOOK_FILE_SEGMENT requires a response
    fn on_get_book_file_segment(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // message looks like this
        // {'lpath' : path, 'position': position,
        //  'thisBook': this_book, 'totalBooks': total_books,
        //  'canStream':True, 'canStreamBinary': True},
        // TODO need to actually know a file length
        Ok(Some(json!({
            "fileLength": 0
        })))
    }

    // cant' find any GET_BOOK_METADATA calls in the calibre source...
    fn on_get_book_metadata(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        Ok(None)
    }

    // GET_BOOK_COUNT requires a response
    // FIXME: This function shows a flaw in the client structure
    //        The GET_BOOK_COUNT opcode expects a response to the call, then a response for each book
    //        At the very least this function is going to be pretty complex, and might require some
    //        refactoring. For now with count: 0 we don't trigger any of that
    //
    //        If I want to keep the client structure as-is I'll have to return an iterable of
    //        responses, with each being sent individually
    fn on_get_book_count(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // TODO actual implementation
        // TODO Need to look into what these options actually do
        // message={"canStream": true, "canScan": true, "willUseCachedMetadata": true, "supportsSync": false, "canSupportBookFormatSync": true}
        Ok(Some(json!({
                "willStream": true,
                "willScan": true,
                "count": 0,
            }
        )))
    }

    // GET_DEVICE_INFORMATION requires a response
    fn on_get_device_information(
        &self,
        _message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // TODO actual implementation
        Ok(Some(json!({
            "device_info": {
                // 'device_store_uuid' = CalibreMetadata.drive.device_store_uuid,
                "device_name": "Kobo",
            },
            "version": VERSION,
            "device_version": VERSION,
        })))
    }

    // I still think this auth protocol is weird
    fn get_password_hash(&self, challange: Option<&str>) -> String {
        let digest = if let Some(challange) = challange {
            if let Some(password) = &self.password {
                // create a Sha1 object
                let mut hasher = Sha1::new();

                let challange = challange.as_bytes();
                let password = password.as_bytes();

                hasher.update(password);
                hasher.update(challange);

                hasher.finalize().to_vec()
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        hex::encode(digest)
    }

    // GET_INITIALIZATION_INFO requires a response
    fn on_get_initialization_info(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // logger.debug("default GET_INITIALIZATION_INFO handler called: %s", payload)

        let init_info = json!({
            "appName": NAME,
            "acceptedExtensions": ["epub"],
            "cacheUsesLpaths": true,
            "canAcceptLibraryInfo": true,
            "canDeleteMultipleBooks": true,
            "canReceiveBookBinary": true,
            "canSendOkToSendbook": false, // TODO: probably actually true
            "canStreamBooks": true,
            "canStreamMetadata": true,
            "canUseCachedMetadata": true,
            "ccVersionNumber": VERSION,
            "coverHeight": 240,
            "deviceKind": "Kobo",
            "deviceName": DEVICE_NAME,
            "extensionPathLengths": [37],
            "passwordHash": self.get_password_hash(message.get("passwordChallenge").and_then(|v| v.as_str())),
            "maxBookContentPacketLen": 4096,
            "useUuidFileNames": false,
            "versionOK": true,
        });
        Ok(Some(init_info))
    }

    // SEND_BOOKLISTS should _not_ send a response
    fn on_send_booklists(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        // I imagine when I start giving reporting books that collection object wil be pretty big
        // message={'count': 0, 'collections': {}, 'willStreamMetadata': True, 'supportsSync': False}
        Ok(None)
    }

    // SEND_BOOK _can_ require a response, the canSendOkToSendbook key we return from GET_INITIALIZATION_INFO
    fn on_send_book(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        Ok(None)
    }

    // SEND_BOOK_METADATA should _not_ send a response
    fn on_send_book_metadata(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        Ok(None)
    }

    // SET_CALIBRE_DEVICE_INFO requires a response
    fn on_set_calibre_device_info(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        Ok(Some(json!({})))
    }

    // SET_CALIBRE_DEVICE_NAME requires a response
    fn on_set_calibre_device_name(
        &self,
        _message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        Ok(Some(json!({})))
    }

    // TOTAL_SPACE requires a response
    fn on_total_space(
        &self,
        _message: serde_json::Value,
        _sender: &Sender<Message>,
    ) -> Result<Option<serde_json::Value>, Option<serde_json::Value>> {
        let info = statvfs::statvfs(&self.save_path);
        if let Ok(info) = info {
            let fbs = info.fragment_size() as u64;
            let total = info.blocks() as u64 * fbs;
            Ok(Some(json!({ "total_space_on_device": total })))
        } else {
            Err(Some(json!({
                "message": "Unable to get size information",
            })))
        }
    }
}
