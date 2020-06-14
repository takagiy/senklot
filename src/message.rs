use serde::{Deserialize, Serialize};

use crate::config::LocalTime;

#[derive(Serialize, Deserialize)]
pub enum UnlockResponse {
    Success {
        locked_at: LocalTime,
    },
    Fail {
        cause: String,
        unlocked_at: Option<LocalTime>,
    },
}
