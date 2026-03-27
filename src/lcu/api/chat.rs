use serde_json::json;
use serde_json::Value;
use crate::lcu::api::{LcuClient, LcuApiError};

impl LcuClient {
    /// 获取自己的聊天信息（jid / pid 等）。
    pub async fn get_chat_me(&self) -> Result<Value, LcuApiError> {
        self.get_json("/lol-chat/v1/me").await
    }

    /// 创建或复用与指定 pid（XMPP name）的单聊会话，返回 conversation id。
    pub async fn open_conversation(&self, pid: &str) -> Result<String, LcuApiError> {
        let v = self
            .post_json(
                "/lol-chat/v1/conversations",
                Some(json!({"pid": pid, "type": "chat"})),
            )
            .await?;
        v.get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_owned())
            .ok_or_else(|| LcuApiError::Other("open_conversation：响应缺少 id 字段".into()))
    }

    /// 向指定会话发送消息。
    pub async fn send_chat_message(
        &self,
        conversation_id: &str,
        body: &str,
    ) -> Result<(), LcuApiError> {
        self.post_json(
            &format!("/lol-chat/v1/conversations/{conversation_id}/messages"),
            Some(json!({"body": body, "type": "chat"})),
        )
        .await?;
        Ok(())
    }

    /// 向自己发送私信（仅自己可见）。
    pub async fn send_message_to_self(&self, body: &str) -> Result<(), LcuApiError> {
        let me = self.get_chat_me().await?;
        let pid = me
            .get("pid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LcuApiError::Other("get_chat_me：响应缺少 pid 字段".into()))?
            .to_owned();
        let conv_id = self.open_conversation(&pid).await?;
        self.send_chat_message(&conv_id, body).await
    }
}
