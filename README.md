# PILSEOCORE

**作者:yxpil(笔名,可能非本人真实姓名)** · © 2026 PILSEOCORE Project。本项目保留作者署名权——使用、修改、分发(含商用)须保留本声明及作者信息;删除/篡改作者声明将被视为违反项目协议。

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
- **聚合搜索(元搜索)**:本地索引搜不到时,自动借用必应/百度/360搜索/搜狗/谷歌/中国搜索,每引擎翻页抓取 3 页(不只第一页),结果去重缓存到本地引擎,分页展示;管理后台可启停各引擎、添加自定义引擎(URL 模板 `{q}` 查询词、`{p}` 翻页);`engine.conf` 可配置 `meta_proxy`(如 `127.0.0.1:7890`)使谷歌等经代理可达
- **节日 LOGO**:管理后台"前端入口"可上传 SVG/PNG 节日 LOGO(春节/国庆/中秋等),搜索页 Logo 即时替换,更真实的搜索引擎体验
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
pilseocore serve --port 8896                # 启动搜索引擎(浏览器打开 http://127.0.0.1:8896)
pilseocore search "智能家居"                # CLI 搜索
pilseocore mcp                              # MCP Server(stdio)
```

### 权限控制(管理员 / 用户)

服务区分两种角色:

| 角色 | 认证 | 权限 |
|---|---|---|
| 普通用户 | 无需认证 | 搜索、联想、状态、站点地图、API 文档、Web UI(只读) |
| 管理员 | **账号密码登录** Web UI,或使用**管理员签发的 token** | 全部 + 触发穷举遍历、配置后缀/DNS 列表、重建索引、签发/撤销 token |

**登录**:管理员在 Web UI 点"管理",用 `admin_user` / `admin_pass`(engine.conf)登录。
**Token 签发**:登录后可在管理面板"签发 token"(命名、撤销,存于 `data/tokens.json`),token 给 **API 客户端与 MCP** 使用——像 GitHub personal access token。

```bash
# 1) 登录拿会话 token
curl -X POST http://127.0.0.1:8896/api/auth/login \
  -H "Content-Type: application/json" -d '{"username":"admin","password":"你的密码"}'
# => {"token":"<会话token>",...}

# 2) 签发 API/MCP token
curl -X POST http://127.0.0.1:8896/api/admin/tokens \
  -H "Authorization: Bearer <会话token>" -H "Content-Type: application/json" \
  -d '{"name":"mcp-server"}'
# => {"token":"<完整token,仅此一次>",...}

# 3) 用签发 token 调用管理 API
curl -X POST http://127.0.0.1:8896/api/admin/scan \
  -H "Authorization: Bearer <签发token>" -H "Content-Type: application/json" \
  -d '{"max_len":2,"workers":64}'

# 4) MCP 用签发 token 启动(或环境变量 PILSEO_TOKEN)
pilseocore mcp --token <签发token>
```

`admin_user`/`admin_pass` 留空则管理功能整体禁用(管理接口返回 403)。

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
| `server_port` | 搜索服务端口 | `8896` |
| `hot_cache_size` / `hot_cache_ttl` | 热点缓存容量/TTL | `1000` / `60` |
| `ai_enabled` | AI 摘要开关 | `false` |
| `ai_endpoint` / `ai_model` | AI 端点/模型 | 本地 llama-server |
| `admin_user` / `admin_pass` | 管理员账号密码(登录 Web UI) | `admin` / `pilseo_admin_2026` |
| API/MCP token | 登录后签发(存 data/tokens.json) | 空 |

### 输出

- `out/available.txt` / `out/registered.txt` / `out/errors.txt` / `out/uncertain.txt`
- `out/sites/<域名>/index.html` + `sitemap.xml`
- `data/index/` 分块索引(docs.json + blocks/block_XX.json)

## 注意

穷举组合爆炸:`36^N` × 后缀数。1 位=36,2 位=1296,3 位=46656,4 位=167 万,5 位=6000 万。**先小规模试跑再上量**。

已注册判定基于公共 DNS 视图,可用域名仍需在注册商处最终确认。

## 协议与作者

- **作者**:yxpil(笔名,可能非本人真实姓名)。保留作者署名权——使用、修改、分发(含商用)须保留作者声明。
- **协议全文**:见 [LICENSE](LICENSE)。核心条款:必须保留作者署名,删除/篡改作者声明视为违反协议;软件按"现状"提供,作者不对使用产生的损失负责;二次分发须附 LICENSE 原文并注明修改者。
- 本项目为纯 Rust 标准库实现,零第三方依赖。
