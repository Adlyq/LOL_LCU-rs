use serde_json::Value;
use crate::lcu::api::{LcuClient, LcuApiError};

impl LcuClient {
    pub async fn get_player_loot(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-loot/v1/player-loot").await
    }

    pub async fn call_loot_recipe(
        &self,
        loot_id: &str,
        recipe_name: &str,
    ) -> Result<Value, LcuApiError> {
        self.post_json(
            &format!("/lol-loot/v1/recipes/{recipe_name}/craft?repeat=1"),
            Some(serde_json::json!([loot_id])),
        )
        .await
    }
}
