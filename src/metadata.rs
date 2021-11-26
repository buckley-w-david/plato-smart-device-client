use serde::{Deserialize, Serialize};

// TODO: Verify these types
#[derive(Serialize, Deserialize, Debug)]
pub struct Metadata {
    uuid: String,
    lpath: String,
    last_modified: String,
    size: usize,
    title: String,
    authors: Vec<String>,
    tags: Vec<String>,
    series: String,
    series_index: usize,
}
