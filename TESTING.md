# PILSEOCORE 测试说明

- 测试完成：是（2026-10-04）
- 测试日期：2026-10-04
- 测试内容：本仓库为零第三方依赖的 Rust 库，已有覆盖 crawler/dns/index/search/tokenizer/metasearch/stats/logger 等模块的单元测试。本次补强：① json.rs——手写 JSON 解析器对畸形/恶意载荷的拒绝（未闭合字符串、缺冒号、非法转义、控制字符、尾随垃圾、空输入），以及序列化时引号/反斜杠/换行转义（防止 JSON 字符串 breakout）；② auth.rs——会话 TTL 过期即失效、伪造/空 token 被拒、token 名空白归一；③ blacklist.rs——FNV-1a 哈希确定性、空文本与自海明距离；④ tests/integration.rs——从 crate 外部跨模块驱动公开 API（枚举器总数与终止、JSON 往返查询、畸形注入拒绝、哈希一致性）。注入测试集中在 json 解析器（不可信输入边界）。本仓库的"调度/钩子"为 TaskScheduler（仅元数据 + 到期计算，无回调注册/失败隔离语义）与 MCP tool 注册表（在 server 进程内、需运行环境），无可在无运行时下做"注册→触发→失败隔离"的纯钩子机制，故未单列钩子测试。
- 运行命令：`cargo test`（仓库根即 crate 根）
- 测试框架：Rust `#[cfg(test)]`（纯标准库，无测试依赖）
- 模型：豆包（Doubao）生成

## 测试布局

- **单元测试**：各 `src/*.rs` 内 `#[cfg(test)] mod tests`（原有 43 个通过 + 本次新增 7 个）。
- **集成测试**：`tests/integration.rs`（4 个），从 crate 外部使用公开 API。
- 本 crate 根 `lib.rs` 已将所有模块声明为 `pub mod`，无需改动源码即可在 `tests/` 中访问。

## 如何运行

```bash
cargo test            # 跑全部
cargo test --test integration   # 只跑集成测试
cargo test -- --ignored         # 跑需要真实网络/DNS 的探针（probe_stress/probe_timing）
```

## 预期结果

```
test result: ok. 50 passed; 0 failed; 2 ignored   (单元)
test result: ok. 4 passed;  0 failed; 0 ignored    (集成)
```

## 原有测试覆盖

仓库原本已有 45 个测试（43 通过 + 2 个 `#[ignore]` 的真实网络 DNS 探针），分布在 enumerate/json/blacklist/tasks/auth/crawler/dns/index/search/metasearch/stats/tokenizer/logger 等模块。本次新增 7 个单元 + 4 个集成，其中 4 个为注入/恶意输入用例。
