//! 管理员主动将应用可见通讯录同步到本地用户列表。

mod accounts;
mod client;
pub(crate) mod routes;
pub mod service;
#[cfg(test)]
mod tests;
