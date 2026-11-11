# M6 完整测试与集成 - 进度报告

**阶段**: M6 - 完整测试与集成  
**状态**: 🔄 进行中  
**日期**: 2026-11-11

---

## 当前任务

### ✅ 已完成
1. **InstanceManager Storage 集成准备**
   - ✅ 添加 `storage: Option<Arc<Storage>>` 字段
   - ✅ 实现 `with_storage()` 构建器方法
   - ✅ 在 `launch()` 中调用 `save_instance_state()`
   - ✅ 在 `stop()` 中调用 `save_instance_state()`

2. **Storage 方法实现**
   - ✅ `save_instance_state()` - 保存实例状态
   - ✅ `get_instance_state()` - 查询实例状态
   - ✅ 使用 `Mutex<Connection>` 解决线程安全问题

### 🔄 进行中
1. **编译错误修复**
   - 问题: `RefCell` 不是线程安全的
   - 解决方案: 使用 `Mutex<Connection>` 包装
   - 状态: 修复中

### ⬜ 待完成
1. **完整编译通过**
2. **守护进程重启测试**
3. **实例生命周期测试**
4. **Storage 持久化验证**
5. **网络验证增强**
6. **文档完善**

---

## 技术细节

### Storage 线程安全改造

**问题**:
```rust
// 错误: Connection 包含 RefCell，不能跨线程
pub struct Storage {
    conn: Connection,  // ❌ 不是 Send + Sync
}
```

**解决方案**:
```rust
use std::sync::Mutex;

pub struct Storage {
    conn: Mutex<Connection>,  // ✅ 线程安全
}

impl Storage {
    pub fn save_instance_state(&self, ...) -> Result<i64> {
        let conn = self.conn.lock().unwrap();  // 获取锁
        conn.execute(...)?;
        Ok(conn.last_insert_rowid())
    }
}
```

### InstanceManager 集成

```rust
pub struct InstanceManager {
    instances: Arc<RwLock<HashMap<InstanceKind, Option<ProcessHandle>>>>,
    code: Arc<CodeAdapter>,
    desktop: Arc<DesktopAdapter>,
    vscode: Arc<VsCodeAdapter>,
    web: Arc<WebAdapter>,
    storage: Option<Arc<Storage>>,  // ✅ 新增
}

impl InstanceManager {
    pub fn with_storage(mut self, storage: Arc<Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    pub async fn launch(&self, kind: InstanceKind) -> Result<()> {
        let handle = adapter.launch().await?;
        
        // 保存到 Storage
        if let Some(ref storage) = self.storage {
            let kind_str = format!("{:?}", kind).to_lowercase();
            let _ = storage.save_instance_state(&kind_str, "running", Some(handle.root_pid));
        }
        
        // 存储句柄
        let mut instances = self.instances.write().await;
        instances.insert(kind, Some(handle));
        Ok(())
    }
}
```

---

## 下一步行动

1. **完成编译修复** - 确保所有 Mutex 调用正确
2. **重启守护进程** - 使用新的 Storage 集成版本
3. **运行完整测试** - 验证实例状态持久化
4. **编写 M6 测试脚本** - 自动化测试套件
5. **性能验证** - 检查 Storage 操作对性能的影响

---

**更新时间**: 2026-11-11  
**状态**: Storage 线程安全改造进行中
