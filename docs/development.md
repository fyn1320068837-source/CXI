# CIM 开发指南

## 开发环境

### 必需工具

- Rust 1.80+ (已通过 Homebrew 安装)
- macOS 13+
- sing-box (安装脚本自动下载)

### 可选工具

- `tart` - VM 虚拟化 (用于 Desktop 实例 VM 模式)
- `jq` - JSON 处理 (测试脚本使用)

## 本地开发

### 首次设置

```bash
cd ~/CXI

# 构建项目
cargo build

# 运行测试
cargo test

# 格式化代码
cargo fmt

# Lint 检查
cargo clippy
```

### 运行守护进程

```bash
# 开发模式（带详细日志）
RUST_LOG=debug cargo run -- daemon

# 在另一个终端打开控制台
open http://localhost:8080
```

### 初始化数据库

```bash
cargo run -- init-db
```

这会创建 `~/.cim/cim.db` SQLite 数据库。

## 代码结构

### 模块职责

- **main.rs**: CLI 入口，子命令路由
- **api/**: HTTP/WebSocket API 服务器
- **proxy/**: sing-box 生命周期管理
- **instance/**: 四个实例适配器的实现
- **verifier/**: lsof 网络验证逻辑
- **storage/**: SQLite + Keychain 抽象
- **geoip/**: GeoIP 查询服务

### 添加新实例类型

1. 在 `src/instance/` 创建新文件
2. 实现 `InstanceAdapter` trait
3. 在 `src/instance/mod.rs` 导出
4. 在 API 和前端添加对应路由

### 添加 API 端点

1. 在 `src/api/mod.rs` 定义路由和处理函数
2. 添加请求/响应结构体（derive Serialize/Deserialize）
3. 更新 `web/index.html` 前端调用

## 测试

### 单元测试

```bash
# 运行所有测试
cargo test

# 运行特定模块
cargo test storage::tests

# 显示输出
cargo test -- --nocapture
```

### 集成测试

```bash
# 启动守护进程
cargo run -- daemon &
DAEMON_PID=$!

# 运行测试脚本
./tests/chaos/inject-proxy-failure.sh

# 停止守护进程
kill $DAEMON_PID
```

### 手动测试

参考 `TESTING.md` 的验收测试矩阵。

## 调试

### 日志级别

```bash
# 调试级别
RUST_LOG=debug cargo run -- daemon

# 特定模块
RUST_LOG=cim::proxy=trace cargo run -- daemon

# 仅错误
RUST_LOG=error cargo run -- daemon
```

### 查看数据库

```bash
sqlite3 ~/.cim/cim.db

# 查看表结构
.schema

# 查看代理配置
SELECT * FROM proxy_profile;

# 查看验证记录
SELECT * FROM validation_evidence ORDER BY timestamp DESC LIMIT 10;

# 退出
.quit
```

### 查看 Keychain

```bash
# 查找 CIM 密码条目
security find-generic-password -s com.cim.proxy

# 查看密码 (需要授权)
security find-generic-password -s com.cim.proxy -w
```

守护进程启动时优先复用 `~/.cim/sing-box/config.json` 中权限为 0600 的密码，只有首次迁移或该配置缺少密码时才查询 Keychain。这样 launchd 重启和普通 release 更新不会重复触发 macOS 授权弹窗；需要更新代码时使用 `./scripts/restart-daemon.sh`，不要反复执行安装脚本。

### 网络调试

```bash
# 检查监听端口
lsof -i :8080
lsof -i :7890
lsof -i :7891

# 测试代理连接
curl --proxy socks5://127.0.0.1:7891 https://api.ipify.org

# 抓包分析
sudo tcpdump -i any -n 'tcp port 7891' -w /tmp/proxy.pcap
```

## 性能分析

### CPU 分析

```bash
# 安装 flamegraph
cargo install flamegraph

# 生成火焰图
cargo flamegraph --bin cim -- daemon
```

### 内存分析

```bash
# 使用 valgrind (需要 Intel Mac 或 Rosetta)
valgrind --tool=massif target/debug/cim daemon

# 或使用 Instruments (macOS)
instruments -t Leaks target/debug/cim daemon
```

## 贡献指南

### 分支策略

- `main`: 稳定版本
- `dev`: 开发主线
- `feature/*`: 功能分支
- `fix/*`: 修复分支

### 提交规范

```
<type>(<scope>): <subject>

<body>

<footer>
```

Type:
- `feat`: 新功能
- `fix`: 修复
- `docs`: 文档
- `style`: 格式
- `refactor`: 重构
- `test`: 测试
- `chore`: 构建/工具

示例:
```
feat(verifier): implement lsof connection parsing

- Parse ESTABLISHED TCP connections
- Classify as PROXIED/LOCAL/DIRECT
- Handle edge cases for IPv6

Closes #123
```

### 代码审查清单

- [ ] 所有测试通过
- [ ] `cargo fmt` 已运行
- [ ] `cargo clippy` 无警告
- [ ] 文档已更新
- [ ] CHANGELOG 已添加条目
- [ ] 无硬编码密码/IP
- [ ] 错误处理完善
- [ ] 日志级别合理

## 发布流程

### 版本号

遵循 [Semantic Versioning](https://semver.org/):
- MAJOR: 不兼容的 API 变更
- MINOR: 向后兼容的新功能
- PATCH: 向后兼容的修复

### 发布步骤

1. 更新 `Cargo.toml` 版本号
2. 更新 `CHANGELOG.md`
3. 提交: `git commit -m "chore: bump version to X.Y.Z"`
4. 打标签: `git tag vX.Y.Z`
5. 构建 release: `cargo build --release`
6. 测试 release 二进制
7. 推送: `git push && git push --tags`

### 分发

```bash
# 构建 universal binary (arm64 + x86_64)
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

# 合并为 universal binary
lipo -create \
  target/aarch64-apple-darwin/release/cim \
  target/x86_64-apple-darwin/release/cim \
  -output cim-universal

# 打包
tar czf cim-v1.0.0-macos-universal.tar.gz \
  cim-universal \
  install.sh \
  uninstall.sh \
  README.md \
  config.toml.example
```

## 常见问题

### 编译错误: security-framework

确保 Xcode Command Line Tools 已安装:
```bash
xcode-select --install
```

### 链接错误: libsqlite3

Homebrew 的 SQLite 库路径:
```bash
export LIBRARY_PATH=/opt/homebrew/lib:$LIBRARY_PATH
```

### 运行时错误: Address already in use

端口 8080 被占用:
```bash
# 查找占用进程
lsof -i :8080

# 杀掉进程或更改配置
# 修改 config.toml 中的 listen_addr
```

## 参考资料

- [Rust 异步编程](https://rust-lang.github.io/async-book/)
- [axum 文档](https://docs.rs/axum/)
- [tokio 文档](https://docs.rs/tokio/)
- [rusqlite 文档](https://docs.rs/rusqlite/)
- [macOS Security Framework](https://developer.apple.com/documentation/security)
