# PILSEOCORE

本地想自己建立搜索引擎？那么一个暴力的搜索全网编制的多线程SEO模块，你可能需要

**SEO 自动域名穷举引擎 + 本地搜索引擎** —— 字符集穷举域名 × 后缀大全 × 多DNS随机负载均衡并发探测 × 自动建站 × 本地索引与搜索。纯 Rust 标准库实现,**零第三方依赖**,单文件 0.7MB。

## 特性

### 穷举引擎
- 字符集穷举:`1234567890abcdefghijklmnopqrstuvwxyz`(数字优先),从 **1 位** 遍历到用户规定的位数
- 后缀大全:`config/tld.list` 空格分隔,严格按文件中的顺序遍历
- **335 个 DNS 随机负载均衡**(抓取自 dnsdaquan.com 大全):每域名随机抽取 N 个 DNS 并发查询,失败换组重试,避免单个服务器被薅秃
- DNS 健康预检:用冷门域名实测递归能力,剔除"残废"DNS
- 判定规则:任一 DNS 返回记录 = 已注册;其余(含无法判定)均视为可用候选并建站,另存 `uncertain.txt` 追溯
- 手写 DNS 报文构造/解析(qid 全局唯一 + 缓冲区清扫,杜绝响应串扰)

### 本地搜索引擎
- **自动分析站点**:扫描 `out/sites/` 提取标题/描述/关键词,自动生成每个站点的 `sitemap.xml`
- **分块倒排索引**:按词首字符分 36+1 块持久化,查询只路由加载相关块(懒加载)
- **模糊搜索**:中文单字+bigram 分词,编辑距离 ≤ 2 模糊匹配,字段加权排序
- **联想建议**:前缀匹配索引词 + 热点查询历史
- **热点缓存**:LRU 缓存(默认 1000 条,TTL 60s),重复查询 0ms 命中
- **Google 风格 Web UI**:彩色 logo、搜索框、联想下拉、结果列表
- **API + 文档**:`/api/search`、`/api/suggest`、`/api/status`、`/api/stats`、`/api/sitemap`、`/api/rebuild`,`/api/docs` 内置完整文档
- **MCP 支持**:stdio JSON-RPC,工具 `search`/`suggest`/`status`/`sitemap`/`rebuild`,AI 客户端可直接调用
- **AI 接入**:OpenAI 兼容端点(本地 llama-server/Ollama 等),搜索附加 AI 摘要

## 构建

```bash
cargo build --release
# 产物: target/release/pilseocore(.exe) 单文件 0.7MB
```

## 使用

```bash
# ---- 穷举模式 ----
pilseocore --max-len 2 --workers 128        # 穷举到 2 位,128 并发
pilseocore --dry-run --max-len 1            # 只穷举核对顺序(out/dryrun.txt)
pilseocore --check 1.com                    # 单域名探测注册状态

# ---- 本地搜索引擎 ----
pilseocore index                            # 扫描站点建索引 + 生成 sitemap.xml
pilseocore serve --port 8891                # 启动搜索引擎(浏览器打开 http://127.0.0.1:8891)
pilseocore search "智能家居"                # CLI 搜索
pilseocore mcp                              # MCP Server(stdio)
```

### 配置(config/engine.conf)

| 键 | 说明 | 默认 |
|---|---|---|
| `charset` | 穷举字符集 | `1234567890abcdefghijklmnopqrstuvwxyz` |
| `min_len` / `max_len` | 起始/最大位数 | `1` / `2` |
| `tld_file` | 后缀大全(空格分隔,按序遍历) | `config/tld.list` |
| `dns_file` | DNS 服务器大名单 | `config/dns.list`(335 个) |
| `dns_per_domain` | 每域名随机抽取 DNS 数 | `5` |
| `workers` | 并发线程数 | `64` |
| `dns_timeout_ms` | 单查询超时 | `2000` |
| `qtypes` | 探测记录类型 | `A AAAA NS` |
| `block_file` | 黑名单 | `config/back.list` |
| `build_sites` | 自动建站 | `true` |
| `server_port` | 搜索服务端口 | `8891` |
| `hot_cache_size` / `hot_cache_ttl` | 热点缓存容量/TTL | `1000` / `60` |
| `ai_enabled` | AI 摘要开关 | `false` |
| `ai_endpoint` / `ai_model` | AI 端点/模型 | 本地 llama-server |

### 输出

- `out/available.txt` / `out/registered.txt` / `out/errors.txt` / `out/uncertain.txt`
- `out/sites/<域名>/index.html` + `sitemap.xml`
- `data/index/` 分块索引(docs.json + blocks/block_XX.json)

## 注意

穷举组合爆炸:`36^N` × 后缀数。1 位=36,2 位=1296,3 位=46656,4 位=167 万,5 位=6000 万。**先小规模试跑再上量**。

已注册判定基于公共 DNS 视图,可用域名仍需在注册商处最终确认。
