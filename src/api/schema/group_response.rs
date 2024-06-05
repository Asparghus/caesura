use crate::api::{Group, Torrent};
use serde::{Deserialize, Serialize};

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupResponse {
    pub group: Group,
    pub torrents: Vec<Torrent>,
}
