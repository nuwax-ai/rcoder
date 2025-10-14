'use client';

import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { ChatInterface } from '@/components/chat/ChatInterface';
import { Bot, Settings, Info } from 'lucide-react';

export default function HomePage() {
  const [currentSession, setCurrentSession] = useState<string>('');

  return (
    <div className="min-h-screen bg-gradient-to-br from-gray-50 to-gray-100">
      {/* 导航栏 */}
      <nav className="border-b bg-white/80 backdrop-blur-sm">
        <div className="mx-auto max-w-7xl px-4 sm:px-6 lg:px-8">
          <div className="flex h-16 items-center justify-between">
            <div className="flex items-center space-x-3">
              <div className="flex h-10 w-10 items-center justify-center rounded-lg bg-blue-600">
                <Bot className="h-6 w-6 text-white" />
              </div>
              <div>
                <h1 className="text-xl font-bold text-gray-900">RCoder Agent 测试平台</h1>
                <p className="text-xs text-gray-500">测试和验证 AI Agent 功能</p>
              </div>
            </div>
            <div className="flex items-center space-x-4">
              <Button variant="ghost" size="sm">
                <Info className="h-4 w-4 mr-2" />
                关于
              </Button>
              <Button variant="outline" size="sm">
                <Settings className="h-4 w-4 mr-2" />
                设置
              </Button>
            </div>
          </div>
        </div>
      </nav>

      {/* 主要内容 */}
      <main className="mx-auto max-w-7xl px-4 py-8 sm:px-6 lg:px-8">
        {/* 项目信息 */}
        <div className="mb-8">
          <Card className="p-6">
            <div className="flex items-start justify-between">
              <div>
                <h2 className="text-2xl font-bold text-gray-900 mb-2">
                  欢迎使用 RCoder Agent 测试界面
                </h2>
                <p className="text-gray-600 mb-4">
                  这是一个专门用于测试 RCoder 平台 AI Agent 功能的界面。您可以通过这个界面与不同类型的 Agent 进行交互，
                  包括 Codex、Claude 和 Proxy Agent。
                </p>
                <div className="flex flex-wrap gap-2">
                  <div className="px-3 py-1 bg-blue-100 text-blue-800 rounded-full text-sm">
                    支持 ACP 协议
                  </div>
                  <div className="px-3 py-1 bg-green-100 text-green-800 rounded-full text-sm">
                    实时进度监控
                  </div>
                  <div className="px-3 py-1 bg-purple-100 text-purple-800 rounded-full text-sm">
                    多 Agent 类型
                  </div>
                  <div className="px-3 py-1 bg-orange-100 text-orange-800 rounded-full text-sm">
                    会话管理
                  </div>
                </div>
              </div>
            </div>
          </Card>
        </div>

        {/* Agent 特性说明 */}
        <div className="mb-8 grid grid-cols-1 md:grid-cols-3 gap-6">
          <Card className="p-6">
            <div className="flex items-center space-x-3 mb-3">
              <div className="p-2 bg-blue-100 rounded-lg">
                <Bot className="h-5 w-5 text-blue-600" />
              </div>
              <h3 className="font-semibold text-gray-900">Codex Agent</h3>
            </div>
            <p className="text-sm text-gray-600">
              使用 ACP 协议和 MPMC 架构的高性能 Agent，适合复杂的代码生成和任务处理。
            </p>
          </Card>

          <Card className="p-6">
            <div className="flex items-center space-x-3 mb-3">
              <div className="p-2 bg-green-100 rounded-lg">
                <Bot className="h-5 w-5 text-green-600" />
              </div>
              <h3 className="font-semibold text-gray-900">Claude Agent</h3>
            </div>
            <p className="text-sm text-gray-600">
              基于 Claude Code CLI 的 Agent，提供强大的代码理解和生成能力。
            </p>
          </Card>

          <Card className="p-6">
            <div className="flex items-center space-x-3 mb-3">
              <div className="p-2 bg-purple-100 rounded-lg">
                <Bot className="h-5 w-5 text-purple-600" />
              </div>
              <h3 className="font-semibold text-gray-900">Proxy Agent</h3>
            </div>
            <p className="text-sm text-gray-600">
              通过 ACP 代理管理器进行路由的 Agent，支持灵活的负载均衡和故障转移。
            </p>
          </Card>
        </div>

        {/* 聊天界面 */}
        <div className="h-[600px]">
          <ChatInterface
            onSessionChange={setCurrentSession}
            userId="test-user"
            projectId="test-project"
          />
        </div>

        {/* 会话信息 */}
        {currentSession && (
          <Card className="mt-6 p-4">
            <div className="flex items-center justify-between">
              <div className="flex items-center space-x-2">
                <div className="w-2 h-2 bg-green-500 rounded-full animate-pulse"></div>
                <span className="text-sm text-gray-600">当前会话活跃</span>
                <code className="px-2 py-1 bg-gray-100 rounded text-xs font-mono">
                  {currentSession}
                </code>
              </div>
              <div className="text-xs text-gray-500">
                会话 ID 已复制到剪贴板
              </div>
            </div>
          </Card>
        )}
      </main>

      {/* 页脚 */}
      <footer className="border-t bg-white mt-12">
        <div className="mx-auto max-w-7xl px-4 py-8 sm:px-6 lg:px-8">
          <div className="text-center text-sm text-gray-500">
            <p>&copy; 2024 RCoder Agent 测试平台. 基于 ACP 协议构建.</p>
            <p className="mt-2">
              支持 Agent 类型: Codex | Claude | Proxy
            </p>
          </div>
        </div>
      </footer>
    </div>
  );
}