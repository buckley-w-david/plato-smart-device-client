use std::fs;
use std::net::SocketAddr;
use std::path::Path;

use anyhow::{Context, Error};
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub host: Option<SocketAddr>,
    pub password: Option<String>,
    #[serde(default = "one")]
    pub log: u8,
}

// Is this really the correct way to put a default value?
fn one() -> u8 {
    1
}

pub fn load_toml<T, P: AsRef<Path>>(path: P) -> Result<T, Error>
where
    for<'a> T: Deserialize<'a>,
{
    let s = fs::read_to_string(path.as_ref())
        .with_context(|| format!("can't read file {}", path.as_ref().display()))?;
    toml::from_str(&s)
        .with_context(|| format!("can't parse TOML content from {}", path.as_ref().display()))
        .map_err(Into::into)
}
