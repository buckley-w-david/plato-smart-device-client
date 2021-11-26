use crate::event::Event;
use std::fmt::Display;

pub struct Logger(u8);

pub const DEBUG: u8 = 3;
pub const VERBOSE: u8 = 2;
pub const STATUS: u8 = 1;
pub const ERROR: u8 = 0;

impl Logger {
    pub fn new(level: u8) -> Logger {
        Logger(level)
    }

    pub fn log(&self, msg: &[&dyn Display], level: u8) {
        if level <= self.0 {
            Event::Notify(
                &msg.iter()
                    .map(|d| d.to_string())
                    .collect::<Vec<String>>()
                    .join(" "),
            )
            .send();
        }
    }

    pub fn debug(&self, msg: &[&dyn Display]) {
        self.log(msg, DEBUG);
    }

    pub fn verbose(&self, msg: &[&dyn Display]) {
        self.log(msg, VERBOSE);
    }

    pub fn status(&self, msg: &[&dyn Display]) {
        self.log(msg, STATUS);
    }

    pub fn error(&self, msg: &[&dyn Display]) {
        self.log(msg, ERROR);
    }
}
