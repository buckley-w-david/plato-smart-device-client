use std::fs::{self, File};
use std::io::{self, Read, Write, BufReader};
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc::Sender, Arc};
use std::time::Duration;

use anyhow::{anyhow, Error};

use chrono::{DateTime, Utc};
use lazy_static::lazy_static;
use nix::sys::statvfs;
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use regex::Regex;
use serde::Serialize;
use serde_json::{self, json};

use sha1::{Digest, Sha1};

use crate::event;
use crate::logger;
use crate::message::Message;
use crate::metadata::Metadata;
use crate::types::{FileInfo, Info};

const BROADCAST_PORTS: &[u16] = &[54982, 48123, 39001, 44044, 59678];
const BROADCAST_MSG: &[u8] = &[104, 101, 108, 108, 111];
const BROADCAST_REGEX: &str = r"calibre wireless device client \(on (.+)\);(\d+),(\d+)";

use const_format::concatcp;

const VERSION: &'static str = env!("CARGO_PKG_VERSION");
const NAME: &'static str = env!("CARGO_PKG_NAME");
const DEVICE_NAME: &'static str = concatcp!(NAME, " (", VERSION, ")");

#[derive(FromPrimitive, Debug, Serialize)]
enum Opcode {
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

use thiserror::Error;

#[derive(Error, Debug)]
pub enum ClientError {
    #[error("An error occured carrying out a command from Calibre")]
    ProtocolError(serde_json::Value),
    #[error("An error occured doing our own thing")]
    ApplicationError(String),
    #[error("JSON!")]
    Json(#[from] serde_json::Error),
    #[error("Files!")]
    File(#[from] io::Error),
    #[error("Other")]
    Other(#[from] anyhow::Error),
}

pub struct SmartDeviceClient {
    addr: SocketAddr,
    password: Option<String>,
    save_path: PathBuf,
    library_path: PathBuf,
    logger: logger::Logger,
}

impl SmartDeviceClient {
    pub fn new(
        addr: Option<SocketAddr>,
        password: Option<String>,
        save_path: PathBuf,
        library_path: PathBuf,
        log_level: u8,
    ) -> Result<SmartDeviceClient, Error> {
        let addr = addr.or(SmartDeviceClient::find_calibre_server()?).ok_or(
            ClientError::ApplicationError("Unable to find calibre server address".into()),
        )?;

        let logger = logger::Logger::new(log_level);
        Ok(SmartDeviceClient {
            addr,
            password,
            save_path,
            library_path,
            logger,
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
        let mut value: serde_json::Value = serde_json::from_str(encoded_json)?;
        let arr = value.as_array_mut().ok_or(ClientError::ApplicationError(
            "Unable to parse calibre response".into(),
        ))?;

        let message = arr.remove(1);
        let encoded_opcode = arr.remove(0).as_i64().ok_or(ClientError::ApplicationError(
            "Opcode is not a number".into(),
        ))?;

        let opcode: Opcode = FromPrimitive::from_i64(encoded_opcode).ok_or(
            ClientError::ApplicationError("Opcode not registered".into()),
        )?;

        Ok((opcode, message))
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
                    Opcode::NOOP => self.on_noop(message, &sender, &stream),
                    Opcode::CalibreBusy => self.on_calibre_busy(message, &sender, &stream),
                    Opcode::SetLibraryInfo => self.on_set_library_info(message, &sender, &stream),
                    Opcode::DeleteBook => self.on_delete_book(message, &sender, &stream),
                    Opcode::DisplayMessage => self.on_display_message(message, &sender, &stream),
                    Opcode::FreeSpace => self.on_free_space(message, &sender, &stream),
                    Opcode::GetBookFileSegment => {
                        self.on_get_book_file_segment(message, &sender, &stream)
                    }
                    Opcode::GetBookMetadata => self.on_get_book_metadata(message, &sender, &stream),
                    Opcode::GetBookCount => self.on_get_book_count(message, &sender, &stream),
                    Opcode::GetDeviceInformation => {
                        self.on_get_device_information(message, &sender, &stream)
                    }
                    Opcode::GetInitializationInfo => {
                        self.on_get_initialization_info(message, &sender, &stream)
                    }
                    Opcode::SendBooklists => self.on_send_booklists(message, &sender, &stream),
                    Opcode::SendBook => self.on_send_book(message, &sender, &stream),
                    Opcode::SendBookMetadata => {
                        self.on_send_book_metadata(message, &sender, &stream)
                    }
                    Opcode::SetCalibreDeviceInfo => {
                        self.on_set_calibre_device_info(message, &sender, &stream)
                    }
                    Opcode::SetCalibreDeviceName => {
                        self.on_set_calibre_device_name(message, &sender, &stream)
                    }
                    Opcode::TotalSpace => self.on_total_space(message, &sender, &stream),
                    _ => Ok(None),
                };

                let send_result = match response {
                    Ok(Some(resp)) => SmartDeviceClient::send(&stream, (Opcode::OK, resp)),
                    Err(ClientError::ProtocolError(resp)) => {
                        SmartDeviceClient::send(&stream, (Opcode::Error, resp))
                    }
                    Err(e) => Err(e.into()),
                    _ => Ok(()),
                };
                if let Err(error) = send_result {
                    eprintln!("Error handling {:?}: {:#}.", opcode, error);
                    self.logger
                        .error(&[&format!("Error communicating with calibre... {:#}", error)]);
                    // Should I exit here?
                    // sender.send(Message::Exit).unwrap();
                }
            } else if let Err(error) = result {
                eprintln!("Error reading command {:#}.", error);
                self.logger
                    .error(&[&format!("Error communicating with calibre... {:#}", error)]);
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
        stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"NOOP", &message]);

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
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"CALIBRE_BUSY", &message]);
        Ok(None)
    }

    // SET_LIBRARY_INFO requires a response
    fn on_set_library_info(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        // TODO actual implementation
        self.logger.debug(&[&"SET_LIBRARY_INFO", &message]);
        Ok(Some(json!({})))
    }

    // DELETE_BOOK requires a response
    // FIXME: This function shows a flaw in the client structure
    //        The DELETE_BOOK opcode expects a response to the call, then a response for each book
    fn on_delete_book(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"DELETE_BOOK", &message]);
        Ok(None)
    }

    // DISPLAY_MESSAGE kinda requires a response...
    // it does call with wait_for_response, but doesn't do anything with the result and wraps the
    // calls in try/except: pass
    fn on_display_message(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"DISPLAY_MESSAGE", &message]);
        Ok(Some(json!({})))
    }

    // FREE_SPACE requires a response
    fn on_free_space(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"FREE_SPACE", &message]);
        // FIXME: Should probably check the status on the save path that we own
        let info = statvfs::statvfs(&self.save_path);
        if let Ok(info) = info {
            let fbs = info.fragment_size() as u64;
            let free = info.blocks_available() as u64 * fbs;
            // let total = info.blocks() as u64 * fbs;
            Ok(Some(json!({ "free_space_on_device": free })))
        } else {
            Err(ClientError::ProtocolError(json!({
                "message": "Unable to get size information",
            })))
        }
    }

    // GET_BOOK_FILE_SEGMENT requires a response
    fn on_get_book_file_segment(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"GET_BOOK_FILE_SEGMENT", &message]);
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
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"GET_BOOK_METADATA", &message]);
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
    //
    //        Maybe I just leak the abstraction a little bit...
    fn on_get_book_count(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"GET_BOOK_COUNT", &message]);
        let e = event::Event::Search{ path: &self.save_path, query: "".to_string() };

        if let Some(event::Response::Search(search)) = e.send() {
            let count =  search.results.len();
            SmartDeviceClient::send(stream, (Opcode::OK, json!({
                "willStream": true,
                "willScan": true,
                "count": count,
            })))?;

            for info in search.results {
                // This shouldn't really use a '?'
                // calibre expects to be sent a specific number of metadata objects
                // If we don't do that, everything probably breaks
                SmartDeviceClient::send(stream, (Opcode::OK, json!({
                    "lpath": info.file.path,
                    "size": info.file.size,
                    "title": info.title,
                    "authors": info.author.split(", ").collect::<String>(),
                    "last_modified": info.added.to_rfc3339(),
                })))?;
            }

            Ok(None)
        } else {
             Ok(Some(json!({
                "willStream": true,
                "willScan": true,
                "count": 0,
            })))
        }
    }

    // GET_DEVICE_INFORMATION requires a response
    fn on_get_device_information(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"GET_DEVICE_INFORMATION", &message]);
        // TODO actual implementation
        let drive_info = self.save_path.join(".driveinfo.calibre");
        if drive_info.exists() {
            let file = File::open(drive_info)?;
            let reader = BufReader::new(file);

            let drive_info: serde_json::Value = serde_json::from_reader(reader)?;
            Ok(Some(json!({
                "device_info": {
                   "device_store_uuid": drive_info.get("device_store_uuid"),
                    "device_name": DEVICE_NAME,
                },
                "version": VERSION,
                "device_version": VERSION,
            })))
        } else {
            Ok(Some(json!({
                "device_info": {
                    "device_name": DEVICE_NAME,
                },
                "version": VERSION,
                "device_version": VERSION,
            })))
        }
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
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"GET_INITIALIZATION_INFO", &message]);
        // logger.debug("default GET_INITIALIZATION_INFO handler called: %s", payload)

        let init_info = json!({
            "appName": NAME,
            "acceptedExtensions": ["epub"], // TODO: more paths
            "cacheUsesLpaths": true,
            "canAcceptLibraryInfo": true,
            "canDeleteMultipleBooks": true,
            "canReceiveBookBinary": true,
            "canSendOkToSendbook": false, // TODO: probably actually true
            "canStreamBooks": true,
            "canStreamMetadata": true,
            "canUseCachedMetadata": false, // TODO: cache metadata
            "ccVersionNumber": VERSION,
            "coverHeight": 240,
            "deviceKind": "Kobo",
            "deviceName": DEVICE_NAME,
            "extensionPathLengths": {"epub": 37}, // TODO: more paths, also explanation of magic number
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
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"SEND_BOOKLISTS", &message]);
        // I imagine when I start giving reporting books that collection object wil be pretty big
        // message={'count': 0, 'collections': {}, 'willStreamMetadata': True, 'supportsSync': False}
        Ok(None)
    }

    // SEND_BOOK _can_ require a response, the canSendOkToSendbook key we return from GET_INITIALIZATION_INFO
    // Right now we send false for that, which lets us just start reading when we get SEND_BOOK
    // instead of having to return back to the event loop to send our response.
    //
    // I've kinda painted myself into a corner. If I want to keep this structure of a stateless
    // event loop then I have to let the handlers do more, including reading further data from the
    // stream, which I don't really like.
    fn on_send_book(
        &self,
        mut message: serde_json::Value,
        _sender: &Sender<Message>,
        stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"SEND_BOOK", &message]);
        let length =
            message
                .get("length")
                .and_then(|v| v.as_u64())
                .ok_or(ClientError::ProtocolError(
                    json!({"message": "You didn't send me a length!"}),
                ))?;

        let metadata: Metadata = serde_json::from_value(message["metadata"].take())?;

        let book = SmartDeviceClient::read_binary_from_net(stream, length as usize)?;

        let lpath =
            message
                .get("lpath")
                .and_then(|v| v.as_str())
                .ok_or(ClientError::ProtocolError(
                    json!({"message": "Bad or missing lpath"}),
                ))?;

        let epub_path = self.save_path.join(lpath);
        let mut file = File::create(&epub_path)?;
        file.write_all(&book[..])?;

        if let Ok(path) = epub_path.strip_prefix(&self.library_path) {
            let file_info = FileInfo {
                path: path.to_path_buf(),
                kind: "epub".to_string(),
                size: file.metadata().ok().map_or(0, |m| m.len()),
            };

            let info = Info {
                title: metadata.title,
                identifier: metadata.uuid,
                author: metadata.authors.join(", "),
                file: file_info,
                added: metadata.last_modified.into(),
            };

            event::Event::AddDocument(info).send();
        }

        Ok(None)
    }

    // SEND_BOOK_METADATA should _not_ send a response
    fn on_send_book_metadata(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"SEND_BOOK_METADATA", &message]);
        Ok(None)
    }

    // SET_CALIBRE_DEVICE_INFO requires a response
    fn on_set_calibre_device_info(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"SET_CALIBRE_DEVICE_INFO", &message]);
        // message={'device_name': 'Kobo', 'device_store_uuid': 'ae9fbd10-9ce0-49b9-95d8-f7a8e82d622a', 'location_code': 'main', 'last_library_uuid': '6b151ef5-03cd-4a9a-8d6b-e50387065299', 'calibre_version': '5.32.0', 'date_last_connected': '2021-11-26T18:41:03.851958+00:00', 'prefix': ''}
        let drive_info = self.save_path.join(".driveinfo.calibre");
        let mut file = File::create(&drive_info)?;
        file.write_all(serde_json::to_string(&message)?.as_bytes())?;

        Ok(Some(json!({})))
    }

    // SET_CALIBRE_DEVICE_NAME requires a response
    fn on_set_calibre_device_name(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"SET_CALIBRE_DEVICE_NAME", &message]);
        Ok(Some(json!({})))
    }

    // TOTAL_SPACE requires a response
    fn on_total_space(
        &self,
        message: serde_json::Value,
        _sender: &Sender<Message>,
        _stream: &TcpStream,
    ) -> Result<Option<serde_json::Value>, ClientError> {
        self.logger.debug(&[&"TOTAL_SPACE", &message]);
        let info = statvfs::statvfs(&self.save_path);
        if let Ok(info) = info {
            let fbs = info.fragment_size() as u64;
            let total = info.blocks() as u64 * fbs;
            Ok(Some(json!({ "total_space_on_device": total })))
        } else {
            Err(ClientError::ProtocolError(json!({
                "message": "Unable to get size information",
            })))
        }
    }
}
