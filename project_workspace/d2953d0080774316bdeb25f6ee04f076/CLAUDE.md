# React Vite Template - Claude 开发指南

## 项目概述
这是一个现代化的 React 18 + Vite + TypeScript 项目模板，针对 Claude AI 助手进行了优化配置。

## 核心特性
- ⚡️ 快速开发体验 (Vite 热重载)
- 🔒 类型安全 (TypeScript)
- 🎨 现代样式 (Tailwind CSS)
- 🧩 完整 UI 组件库 (Radix UI - 27个组件)
- 📝 表单管理 (React Hook Form + Zod)
- 🌐 HTTP 客户端 (Axios)
- 📦 完整的项目结构

## 开发环境设置
```bash
# 安装依赖
npm install

# 启动开发服务器
npm run dev

# 构建项目
npm run build
```

## Claude 优化配置
- VS Code 设置已优化 GitHub Copilot 集成
- Cursor 配置已包含在 `.cursorrules`
- ESLint 和 Prettier 预配置
- TypeScript 严格模式启用

## 组件开发模式
```typescript
// 创建新组件时的标准模式
interface ComponentProps {
  title: string;
  items: Item[];
  onAction?: (id: string) => void;
}

export const Component: React.FC<ComponentProps> = ({ title, items, onAction }) => {
  return (
    <div className="component">
      <h2>{title}</h2>
      {/* 组件内容 */}
    </div>
  );
};
```

## API 集成模式
```typescript
// 在 services.ts 中定义新 API
export const exampleApi = {
  getData: (params: ListParams) => 
    api.get<ListResult<Data>>('/api/data', { params }),
  
  createItem: (data: CreateData) => 
    api.post<Data>('/api/data', data),
};
```

## Claude 提示词模板

### 功能开发
```
请为这个 React Vite 项目开发一个 [功能名称] 功能：
- 使用 TypeScript 和 Tailwind CSS
- 遵循现有组件结构
- 添加到 src/components/ 目录
- 在 App.tsx 中展示使用
- 包含必要的类型定义
```

### 问题排查
```
项目中遇到 [问题描述]：
- 错误信息：[具体错误]
- 相关代码：[代码片段]
- 预期行为：[期望结果]

请分析问题并提供解决方案。
```

## 项目约定
- 组件放在 `src/components/` 下
- API 定义在 `src/lib/services.ts`
- 工具函数在 `src/lib/utils.ts`
- 样式优先使用 Tailwind CSS
- 所有文件必须使用 TypeScript

## 常用开发模式
```typescript
// 数据获取
const { data, loading, error } = useApi(() => api.getData(params));

// 状态管理
const [state, setState] = useState<DataType>(initialValue);

// 事件处理
const handleClick = (id: string) => {
  onAction?.(id);
};
```

## 性能优化建议
- 使用 React.memo 优化纯组件
- 合理使用 useCallback/useMemo
- 避免不必要的重新渲染
- 代码分割和懒加载

## 调试技巧
- 使用 React Developer Tools
- 在 Chrome DevTools 中查看网络请求
- 使用 console.log 进行调试
- 设置断点进行逐步调试

## 最佳实践
1. **类型优先**: 为所有变量、函数参数、返回值添加类型
2. **组件拆分**: 保持组件单一职责，便于维护
3. **状态管理**: 选择合适的状态管理方案
4. **错误处理**: 添加适当的错误边界和错误处理
5. **性能优化**: 定期检查和优化组件性能

## Form 表单开发

### React Hook Form + Zod 集成
此模板包含完整的表单解决方案，使用 React Hook Form 进行状态管理，Zod 进行模式验证。

#### Form 组件生成模式
```typescript
// Claude: 生成 Zod 模式进行表单验证
import { z } from "zod"

const formSchema = z.object({
  // 定义表单字段和验证规则
  email: z.string().email("无效的邮箱地址"),
  password: z.string().min(8, "密码至少需要8个字符"),
  confirmPassword: z.string(),
}).refine((data) => data.password === data.confirmPassword, {
  message: "密码不匹配",
  path: ["confirmPassword"],
})

type FormData = z.infer<typeof formSchema>
```

#### Form 组件实现
```typescript
// Claude: 生成完整的表单组件
import { useForm } from "react-hook-form"
import { zodResolver } from "@hookform/resolvers/zod"
import { Form, FormControl, FormField, FormItem, FormLabel, FormMessage } from "@/components/ui/form"
import { Input } from "@/components/ui/input"
import { Button } from "@/components/ui/button"

export function UserForm() {
  const form = useForm<FormData>({
    resolver: zodResolver(formSchema),
    defaultValues: {
      email: "",
      password: "",
      confirmPassword: "",
    },
  })

  const onSubmit = (data: FormData) => {
    console.log(data)
  }

  return (
    <Form {...form}>
      <form onSubmit={form.handleSubmit(onSubmit)} className="space-y-4">
        <FormField
          control={form.control}
          name="email"
          render={({ field }) => (
            <FormItem>
              <FormLabel>邮箱</FormLabel>
              <FormControl>
                <Input placeholder="请输入邮箱" {...field} />
              </FormControl>
              <FormMessage />
            </FormItem>
          )}
        />
        <Button type="submit">提交</Button>
      </form>
    </Form>
  )
}
```

### 可用的 Radix UI 组件 (27个)

#### 布局和导航
- **Accordion**: 可折叠的内容部分
- **Collapsible**: 显示/隐藏内容区域
- **Navigation Menu**: 复杂的导航结构
- **Menubar**: 应用程序菜单栏
- **Tabs**: 标签页内容组织
- **Separator**: 视觉内容分隔符

#### 数据显示
- **Avatar**: 用户头像
- **Card**: 内容容器
- **Progress**: 进度指示器
- **Scroll Area**: 自定义可滚动区域
- **Aspect Ratio**: 保持宽高比

#### 表单控件
- **Button**: 交互式按钮，支持多种变体
- **Checkbox**: 布尔输入控件
- **Input**: 文本输入字段
- **Label**: 表单字段标签
- **Radio Group**: 单选选择
- **Select**: 下拉选择
- **Slider**: 范围输入控件
- **Switch**: 切换控件
- **Textarea**: 多行文本输入
- **Toggle**: 切换按钮
- **Toggle Group**: 分组切换按钮

#### 覆盖层和对话框
- **Alert Dialog**: 确认对话框
- **Dialog**: 模态对话框
- **Dropdown Menu**: 上下文菜单
- **Popover**: 浮动内容面板
- **Tooltip**: 悬停信息

#### 表单系统
- **Form**: 完整的表单管理
- **FormField**: 带验证的字段包装器
- **FormItem**: 表单元素容器
- **FormLabel**: 可访问的表单标签
- **FormControl**: 输入控件包装器
- **FormDescription**: 帮助文本
- **FormMessage**: 错误消息

### 组件使用示例

#### 按钮变体
```typescript
// Claude: 生成不同变体的按钮
<Button variant="default">主要按钮</Button>
<Button variant="outline">轮廓按钮</Button>
<Button variant="ghost">幽灵按钮</Button>
<Button variant="destructive">删除按钮</Button>
```

#### 表单集成
```typescript
// Claude: 生成包含多种输入类型的表单
<FormField
  control={form.control}
  name="category"
  render={({ field }) => (
    <FormItem>
      <FormLabel>分类</FormLabel>
      <Select onValueChange={field.onChange} defaultValue={field.value}>
        <FormControl>
          <SelectTrigger>
            <SelectValue placeholder="选择一个分类" />
          </SelectTrigger>
        </FormControl>
        <SelectContent>
          <SelectItem value="option1">选项 1</SelectItem>
          <SelectItem value="option2">选项 2</SelectItem>
        </SelectContent>
      </Select>
      <FormMessage />
    </FormItem>
  )}
/>
```
