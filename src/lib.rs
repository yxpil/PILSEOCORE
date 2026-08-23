//! PILSEOCORE 内核库
//!
//! SEO 自动域名穷举引擎 + 本地搜索引擎,纯 Rust 标准库实现(零第三方依赖)。
//! 本 crate 作为**内核**被其他软件嵌入使用;CLI / HTTP API / MCP 均为薄壳。
//!
//! 模块:
//! - [`engine`]   穷举引擎(字符集穷举 x 后缀大全 x 多DNS随机负载均衡 x 自动建站)
//! - [`dns`]      手写 DNS 报文构造/解析与多 DNS 并发探测
//! - [`enumerate`] 字符集穷举器
//! - [`site`]     SEO 网站生成
//! - [`index`]    站点扫描/标题提取/sitemap 生成/分块倒排索引
//! - [`search`]   搜索引擎(模糊匹配/联想/热点缓存/分块懒加载)
//! - [`server`]   HTTP API 服务(权限控制/管理员/扫描调度)
//! - [`http`]     手写 HTTP/1.1 服务器
//! - [`json`]     极简 JSON 库
//! - [`mcp`]      MCP(Model Context Protocol)Server
//! - [`ai`]       OpenAI 兼容 AI 接入
//! - [`auth`]     API/MCP token 签发与会话
//! - [`blacklist`] 内容指纹去重/雷同自动拉黑(SimHash)
//! - [`tokenizer`] BPE 分词器(81920 词表)
//! - [`config`]   配置加载

pub mod ai;
pub mod auth;
pub mod blacklist;
pub mod config;
pub mod dns;
pub mod engine;
pub mod enumerate;
pub mod http;
pub mod index;
pub mod json;
pub mod mcp;
pub mod search;
pub mod server;
pub mod site;
pub mod tokenizer;
