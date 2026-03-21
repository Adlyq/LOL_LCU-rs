/// LCU 层：分为三个子模块
/// - connection : 从锁文件读取 port/auth，构建 reqwest client
/// - websocket  : WebSocket 事件循环与订阅分发
/// - api        : 对 LCU HTTP 端点的具体调用封装
pub mod api;
pub mod connection;
pub mod websocket;
