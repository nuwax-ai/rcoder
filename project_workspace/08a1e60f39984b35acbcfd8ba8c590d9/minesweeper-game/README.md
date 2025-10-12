# 扫雷游戏 (Minesweeper Game)

一个基于 React + Next.js + TypeScript 构建的现代化扫雷游戏。

## 🎮 游戏特性

- **多种难度级别**：初级、中级、高级和自定义
- **经典扫雷玩法**：左键揭开格子，右键插旗标记
- **智能首次点击**：确保第一次点击及周围不会有地雷
- **实时游戏统计**：显示剩余地雷数、已揭开格子数和游戏进度
- **响应式设计**：适配不同屏幕尺寸
- **现代化UI**：使用 Tailwind CSS 和 Radix UI 组件

## 🚀 快速开始

### 环境要求

- Node.js 18.0.0 或更高版本
- npm、yarn 或 pnpm 包管理器

### 安装依赖

```bash
# 使用 npm
npm install

# 使用 yarn
yarn install

# 使用 pnpm
pnpm install
```

### 启动开发服务器

```bash
# 使用 npm
npm run dev

# 使用 yarn
yarn dev

# 使用 pnpm
pnpm dev
```

打开浏览器访问 [http://localhost:3000](http://localhost:3000) 开始游戏。

## 🎯 游戏规则

1. **点击格子**：左键点击揭开格子
2. **标记地雷**：右键点击插旗标记疑似地雷位置
3. **数字提示**：数字表示该格子周围8个格子中地雷的数量
4. **游戏胜利**：揭开所有非地雷格子即为胜利
5. **游戏失败**：点击到地雷即为失败

## 🎮 难度级别

| 级别 | 网格大小 | 地雷数量 |
|------|----------|----------|
| 初级 | 9×9 | 10个 |
| 中级 | 16×16 | 40个 |
| 高级 | 16×30 | 99个 |
| 自定义 | 10×10 | 15个 |

## 🛠️ 技术栈

- **框架**：Next.js 14 (App Router)
- **语言**：TypeScript
- **样式**：Tailwind CSS
- **组件库**：Radix UI
- **图标**：Lucide React
- **构建工具**：Next.js 内置

## 📁 项目结构

```
src/
├── app/                    # Next.js App Router
│   ├── page.tsx           # 主页面
│   └── layout.tsx         # 布局组件
├── components/            # React 组件
│   ├── minesweeper/      # 扫雷游戏组件
│   │   ├── MinesweeperGame.tsx    # 主游戏组件
│   │   ├── GameBoard.tsx          # 游戏板组件
│   │   ├── Cell.tsx               # 单个格子组件
│   │   ├── GameControls.tsx       # 游戏控制面板
│   │   └── DifficultySelector.tsx # 难度选择器
│   └── ui/                # UI 基础组件
├── types/                # TypeScript 类型定义
│   └── minesweeper.ts    # 扫雷游戏类型
├── utils/                # 工具函数
│   └── minesweeper.ts    # 扫雷游戏逻辑
└── lib/                  # 库文件
    └── utils.ts          # 通用工具函数
```

## 🎯 核心功能

### 游戏逻辑
- 地雷随机分布算法
- 相邻地雷数量计算
- 递归揭开空白区域
- 游戏胜负判定

### 用户交互
- 左键点击揭开格子
- 右键插旗/取消插旗
- 键盘支持（Enter/空格键）
- 防止意外点击已揭开的格子

### 视觉反馈
- 不同状态的格子样式
- 数字颜色编码（1-8不同颜色）
- 游戏状态实时显示
- 进度条可视化

## 🔧 开发命令

```bash
# 开发模式
npm run dev

# 构建生产版本
npm run build

# 启动生产服务器
npm run start

# 代码检查
npm run lint

# 类型检查
npm run type-check

# 代码格式化
npm run format

# 运行测试
npm run test
```

## 📱 响应式设计

- 桌面端：完整游戏体验
- 平板端：适配中等屏幕
- 移动端：触摸优化，自动调整格子大小

## 🎨 自定义样式

游戏使用 Tailwind CSS，可以通过修改 `tailwind.config.js` 来自定义主题：

```javascript
// tailwind.config.js
module.exports = {
  theme: {
    extend: {
      colors: {
        mine: '#ef4444',
        flag: '#3b82f6',
        revealed: '#f3f4f6',
      }
    }
  }
}
```

## 🤝 贡献

欢迎提交 Issue 和 Pull Request！

## 📄 许可证

MIT License