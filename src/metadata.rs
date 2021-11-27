use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// TODO: Verify these types
// FIXME: Most of these are probably actually Option<T>
#[derive(Serialize, Deserialize, Debug)]
pub struct Metadata {
    pub uuid: String,
    pub lpath: String,
    #[serde(with = "datetime_format")]
    pub last_modified: DateTime<Utc>,
    pub size: usize,
    pub title: String,
    pub authors: Vec<String>,
    pub tags: Vec<String>,
    pub series: Option<String>,
    pub series_index: Option<f64>,
}


mod datetime_format {
    use chrono::{DateTime, Utc};
    use serde::{self, Deserialize, Deserializer, Serializer};

    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Utc>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        s.parse::<DateTime<Utc>>().map_err(serde::de::Error::custom)
    }

    pub fn serialize<S>(date: &DateTime<Utc>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&date.to_rfc3339())
    }
}
