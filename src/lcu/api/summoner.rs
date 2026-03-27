use serde_json::Value;
use crate::lcu::api::{LcuClient, LcuApiError};

impl LcuClient {
    pub async fn get_current_summoner(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-summoner/v1/current-summoner").await
    }
}
