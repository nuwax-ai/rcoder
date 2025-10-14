# RCoder Agent 测试平台

这是一个专门用于测试 RCoder 平台 AI Agent 功能的 Web 界面。基于 Next.js 14 和 Tailwind CSS 构建，提供现代化的用户界面来与不同类型的 Agent 进行交互。

## 功能特性

### 🤖 多 Agent 支持
- **Codex Agent**: 使用 ACP 协议和 MPMC 架构的高性能 Agent
- **Claude Agent**: 基于 Claude Code CLI 的 Agent
- **Proxy Agent**: 通过 ACP 代理管理器进行路由的 Agent

### 🔄 实时通信
- 基于 Server-Sent Events (SSE) 的实时进度监控
- WebSocket 风格的实时消息推送
- 连接状态自动管理和重连

### 💬 聊天界面
- 现代化的聊天 UI 设计
- 消息历史记录管理
- 复制消息功能
- 支持 Shift+Enter 换行

### 📊 会话管理
- 自动会话创建和管理
- 会话 ID 追踪和显示
- 支持用户和项目 ID 关联

## 技术栈

- **框架**: Next.js 14 (App Router)
- **语言**: TypeScript
- **样式**: Tailwind CSS
- **UI 组件**: shadcn/ui + Radix UI
- **图标**: Lucide React
- **HTTP 客户端**: Fetch API
- **实时通信**: Server-Sent Events

## 项目结构

```
src/
├── app/                    # Next.js App Router
│   └── page.tsx           # 主页面
├── components/            # React 组件
│   ├── chat/              # 聊天相关组件
│   │   └── ChatInterface.tsx
│   └── ui/                # shadcn/ui 组件
│       ├── button.tsx
│       ├── card.tsx
│       ├── textarea.tsx
│       ├── select.tsx
│       ├── badge.tsx
│       ├── scroll-area.tsx
│       └── separator.tsx
├── hooks/                 # React Hooks
│   └── use-rcoder-api.ts
└── lib/                   # 工具库
    ├── rcoder-api.ts      # RCoder API 客户端
    └── utils.ts           # 通用工具函数
```

## 快速开始

### 1. 安装依赖

```bash
npm install
# 或
yarn install
# 或
pnpm install
```

### 2. 环境配置

复制 `.env.local` 文件并根据需要修改配置：

```env
NEXT_PUBLIC_API_BASE_URL=http://localhost:3000
NEXT_PUBLIC_APP_NAME="RCoder Agent 测试平台"
NEXT_PUBLIC_APP_VERSION="1.0.0"
NODE_ENV=development
```

### 3. 启动开发服务器

```bash
npm run dev
# 或
yarn dev
# 或
pnpm dev
```

访问 [http://localhost:3000](http://localhost:3000) 查看应用。

### 4. 确保 RCoder 后端服务运行

确保 RCoder 后端服务在 `http://localhost:3000` 端口运行，或修改 `.env.local` 中的 `NEXT_PUBLIC_API_BASE_URL` 指向正确的后端地址。

## API 接口

### 聊天接口

- `POST /chat` - 发送消息给 Codex Agent
- `POST /chat/proxy` - 发送消息给 Proxy Agent
- `POST /chat/multipart` - 发送包含文件的聊天消息

### 会话管理

- `GET /sessions/{session_id}` - 获取会话信息
- `GET /progress/{session_id}` - SSE 进度流

## 组件使用说明

### ChatInterface 组件

主要的聊天界面组件，支持以下属性：

```tsx
interface ChatInterfaceProps {
  userId?: string;        // 用户 ID
  projectId?: string;     // 项目 ID
  onSessionChange?: (sessionId: string) => void; // 会话变更回调
}
```

使用示例：

```tsx
<ChatInterface
  userId="test-user"
  projectId="test-project"
  onSessionChange={(sessionId) => console.log('Session changed:', sessionId)}
/>
```

### useRCoderAPI Hook

提供 RCoder API 访问功能的 React Hook：

```tsx
const {
  loading,
  error,
  sendMessage,
  sendMessageProxy,
  getSession,
  uploadFile,
  subscribeToProgress,
  unsubscribeFromProgress,
} = useRCoderAPI();
```

## 开发指南

### 添加新的 UI 组件

项目使用 shadcn/ui 组件库。添加新组件的步骤：

1. 从 shadcn/ui 官网复制组件代码
2. 创建对应的 `.tsx` 文件在 `src/components/ui/` 目录
3. 确保安装了所需的依赖

### 自定义主题

修改 `tailwind.config.js` 文件来自定义主题：

```js
module.exports = {
  theme: {
    extend: {
      colors: {
        border: "hsl(var(--border))",
        background: "hsl(var(--background))",
        // ... 其他颜色配置
      },
    },
  },
}
```

### 类型定义

所有 API 相关的类型定义都在 `src/lib/rcoder-api.ts` 文件中：

- `ChatMessage` - 聊天消息类型
- `ChatRequest` - 聊天请求类型
- `ChatResponse` - 聊天响应类型
- `ProgressEvent` - 进度事件类型
- `SessionInfo` - 会话信息类型

## 故障排除

### 常见问题

1. **连接失败**: 检查 RCoder 后端服务是否运行在正确端口
2. **CORS 错误**: 确保后端配置了正确的 CORS 设置
3. **组件样式问题**: 检查是否正确安装了 Tailwind CSS 和相关依赖

### 调试模式

启用调试模式：

```bash
RUST_LOG=debug npm run dev
```

## 贡献指南

1. Fork 项目
2. 创建功能分支 (`git checkout -b feature/AmazingFeature`)
3. 提交更改 (`git commit -m 'Add some AmazingFeature'`)
4. 推送到分支 (`git push origin feature/AmazingFeature`)
5. 打开 Pull Request

## 许可证

本项目基于 MIT 许可证开源。详情请查看 [LICENSE](LICENSE) 文件。

## 支持

如有问题或建议，请：

1. 查看 [Issues](../../issues) 页面
2. 创建新的 Issue
3. 联系开发团队

---

**注意**: 这是一个测试平台，主要用于验证 RCoder Agent 的功能。在生产环境中使用前，请确保进行充分的安全评估。