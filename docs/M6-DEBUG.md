# M6 问题诊断报告

**问题**: Storage 集成代码编译通过，但数据库记录为 0

## 症状
```bash
$ sqlite3 ~/.cim/cim.db "SELECT COUNT(*) FROM instance_state;"
0  # ❌ 没有任何记录
```

## 可能原因

### 1. 错误被静默忽略
```rust
// 当前代码
if let Some(ref storage) = self.storage {
    let kind_str = format!("{:?}", kind).to_lowercase();
    let _ = storage.save_instance_state(&kind_str, "running", Some(handle.root_pid));
    //  ^^^ 忽略所有错误！
}
```

**问题**: `let _ =` 会忽略所有错误，包括：
- 数据库锁定失败
- SQL 语法错误
- 权限问题

### 2. Storage 未传递到 InstanceManager
可能的问题：
- `main.rs` 创建了 Storage 但未正确传递
- `api::create_router()` 签名不匹配
- `with_storage()` 调用被跳过

### 3. InstanceManager 仍在使用旧代码
- 守护进程可能使用了旧的二进制
- 需要确认重新编译和重启

## 诊断步骤

1. **检查 manager.rs 是否有 save_instance_state 调用**
2. **检查守护进程日志是否有 Storage 错误**
3. **手动测试 Storage API**
4. **添加日志记录确认 Storage 调用**

---

**日期**: 2026-11-11
**状态**: 诊断中
