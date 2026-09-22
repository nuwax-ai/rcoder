# Contract: HTTP 语义（对外不变，除空仓库修复）

**Feature**: `001-structured-error-classification`  
**Date**: 2026-09-22

## 不变
- 错误信封：`{ code, displayCode, message, data, tid, success }`
- 重试策略对外表现（4xx 不重试 / 5xx 重试）——只改内部判据
- Gateway `code=not_found` 回退行为
- CLI 退出码集合

## 变更（行为修正，来自 FR-005）

### `GET /api/git/log`（及 file-server 对等接口）

**Before**（缺陷）  
空仓库时：
```json
{ "code": "UNKNOWN_ERROR", "message": "git head_id: Branch 'refs/heads/main' does not have any commits", "success": false }
```

**After**
```json
{ "success": true, "logId": "…", "commits": [], "total": 0 }
```

**测试**  
- 打开仅有 `git init` 的工作区 → 200 + `commits=[]`  
- 有提交后 → 列表非空且回归分页

## 明确非目标
- 不改错误码表、不改 i18n 文案
- 不改 Java `UNKNOWN_ERROR` 包装规则（file-server 不再返回该误报后自然消失）
