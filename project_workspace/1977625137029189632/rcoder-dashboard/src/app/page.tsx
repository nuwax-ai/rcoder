'use client';

import Link from 'next/link';
import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs';
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle, DialogTrigger } from '@/components/ui/dialog';
import { DropdownMenu, DropdownMenuContent, DropdownMenuItem, DropdownMenuTrigger } from '@/components/ui/dropdown-menu';
import { Tooltip, TooltipContent, TooltipProvider, TooltipTrigger } from '@/components/ui/tooltip';
import { MoreHorizontal, ExternalLink, Github, Settings } from 'lucide-react';

export default function HomePage() {
  const [loading, setLoading] = useState(false);

  return (
    <div className="min-h-screen bg-gradient-to-br from-gray-50 to-gray-100">
      {/* 导航栏 */}
      <nav className="border-b bg-white/80 backdrop-blur-sm">
        <div className="mx-auto max-w-7xl px-4 sm:px-6 lg:px-8">
          <div className="flex h-16 items-center justify-between">
            <div className="flex items-center">
              <Link href="/" className="text-xl font-bold text-gray-900">
                Next.js Template
              </Link>
            </div>
            <div className="flex items-center space-x-4">
              <Button variant="ghost">关于</Button>
              <Button>联系我们</Button>
            </div>
          </div>
        </div>
      </nav>

      {/* 主要内容 */}
      <main className="mx-auto max-w-7xl px-4 py-12 sm:px-6 lg:px-8">
        {/* 英雄区域 */}
        <div className="text-center">
          <h1 className="text-4xl font-bold tracking-tight text-gray-900 sm:text-6xl">
            欢迎使用 Next.js 模板
          </h1>
          <p className="mt-6 text-lg leading-8 text-gray-600">
            基于 Next.js 14 和 Tailwind CSS 构建的现代化应用
          </p>
          <div className="mt-10 flex items-center justify-center gap-x-6">
            <Button size="lg" className="px-8">
              开始使用
            </Button>
            <Button variant="outline" size="lg" className="px-8">
              了解更多
            </Button>
          </div>
        </div>

        {/* 特性卡片 */}
        <div className="mt-24">
          <div className="text-center">
            <h2 className="text-3xl font-bold tracking-tight text-gray-900">
              特性介绍
            </h2>
            <p className="mt-4 text-lg text-gray-600">
              基于 Next.js 14 和 Tailwind CSS 构建的现代化应用
            </p>
          </div>
          <div className="mt-12 grid grid-cols-1 gap-8 sm:grid-cols-2 lg:grid-cols-3">
            <Card className="p-6">
              <div className="flex h-12 w-12 items-center justify-center rounded-lg bg-blue-100">
                <svg
                  className="h-6 w-6 text-blue-600"
                  fill="none"
                  viewBox="0 0 24 24"
                  strokeWidth="1.5"
                  stroke="currentColor"
                >
                  <path
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    d="M13.5 10.5V6.75a4.5 4.5 0 119 0v3.75M3.75 21.75h16.5a1.5 1.5 0 001.5-1.5v-6a1.5 1.5 0 00-1.5-1.5H3.75a1.5 1.5 0 00-1.5 1.5v6a1.5 1.5 0 001.5 1.5z"
                  />
                </svg>
              </div>
              <h3 className="mt-4 text-lg font-semibold text-gray-900">
                快速开发
              </h3>
              <p className="mt-2 text-gray-600">
                基于 Next.js 14 App Router，提供最佳开发体验
              </p>
            </Card>

            <Card className="p-6">
              <div className="flex h-12 w-12 items-center justify-center rounded-lg bg-blue-100">
                <svg
                  className="h-6 w-6 text-blue-600"
                  fill="none"
                  viewBox="0 0 24 24"
                  strokeWidth="1.5"
                  stroke="currentColor"
                >
                  <path
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    d="M9.813 15.904L9 18.75l-.813-2.846a4.5 4.5 0 00-3.09-3.09L2.25 12l2.846-.813a4.5 4.5 0 003.09-3.09L9 5.25l.813 2.846a4.5 4.5 0 003.09 3.09L15.75 12l-2.846.813a4.5 4.5 0 00-3.09 3.09zM18.259 8.715L18 9.75l-.259-1.035a3.375 3.375 0 00-2.455-2.456L14.25 6l1.036-.259a3.375 3.375 0 002.455-2.456L18 2.25l.259 1.035a3.375 3.375 0 002.456 2.456L21.75 6l-1.035.259a3.375 3.375 0 00-2.456 2.456zM16.894 20.567L16.5 21.75l-.394-1.183a2.25 2.25 0 00-1.423-1.423L13.5 18.75l1.183-.394a2.25 2.25 0 001.423-1.423l.394-1.183.394 1.183a2.25 2.25 0 001.423 1.423l1.183.394-1.183.394a2.25 2.25 0 00-1.423 1.423z"
                  />
                </svg>
              </div>
              <h3 className="mt-4 text-lg font-semibold text-gray-900">
                现代化设计
              </h3>
              <p className="mt-2 text-gray-600">
                使用 Tailwind CSS 构建响应式、美观的用户界面
              </p>
            </Card>

            <Card className="p-6">
              <div className="flex h-12 w-12 items-center justify-center rounded-lg bg-blue-100">
                <svg
                  className="h-6 w-6 text-blue-600"
                  fill="none"
                  viewBox="0 0 24 24"
                  strokeWidth="1.5"
                  stroke="currentColor"
                >
                  <path
                    strokeLinecap="round"
                    strokeLinejoin="round"
                    d="M9 12.75L11.25 15 15 9.75M21 12a9 9 0 11-18 0 9 9 0 0118 0z"
                  />
                </svg>
              </div>
              <h3 className="mt-4 text-lg font-semibold text-gray-900">
                TypeScript 支持
              </h3>
              <p className="mt-2 text-gray-600">
                完整的 TypeScript 支持，提供更好的开发体验
              </p>
            </Card>
          </div>
        </div>

        {/* 组件演示 */}
        <div className="mt-24">
          <div className="text-center">
            <h2 className="text-3xl font-bold tracking-tight text-gray-900">
              组件演示
            </h2>
            <p className="mt-4 text-lg text-gray-600">
              内置 Radix UI 组件库，提供丰富的交互组件
            </p>
          </div>
          
          <Card className="mx-auto mt-12 max-w-2xl p-6">
            <div className="space-y-4">
              <h3 className="text-lg font-semibold text-gray-900">组件演示</h3>
              
              {/* Radix UI 组件演示 */}
              <div className="mt-6 space-y-4">
                <div className="flex items-center justify-between">
                  <h4 className="text-sm font-medium">Radix UI 组件演示</h4>
                  
                  {/* Dropdown Menu */}
                  <DropdownMenu>
                    <DropdownMenuTrigger asChild>
                      <Button variant="ghost" className="h-8 w-8 p-0">
                        <MoreHorizontal className="h-4 w-4" />
                      </Button>
                    </DropdownMenuTrigger>
                    <DropdownMenuContent align="end">
                      <DropdownMenuItem>查看文档</DropdownMenuItem>
                      <DropdownMenuItem>复制代码</DropdownMenuItem>
                      <DropdownMenuItem>自定义样式</DropdownMenuItem>
                    </DropdownMenuContent>
                  </DropdownMenu>
                </div>

                {/* Tabs */}
                <Tabs defaultValue="components" className="w-full">
                  <TabsList className="grid w-full grid-cols-3">
                    <TabsTrigger value="components">组件</TabsTrigger>
                    <TabsTrigger value="api">API</TabsTrigger>
                    <TabsTrigger value="settings">设置</TabsTrigger>
                  </TabsList>
                  <TabsContent value="components" className="space-y-4">
                    <div className="flex flex-wrap gap-2">
                      <Button size="sm">默认</Button>
                      <Button variant="secondary" size="sm">次要</Button>
                      <Button variant="destructive" size="sm">危险</Button>
                      <Button variant="outline" size="sm">边框</Button>
                      <Button variant="ghost" size="sm">幽灵</Button>
                      <Button variant="link" size="sm">链接</Button>
                    </div>
                  </TabsContent>
                  <TabsContent value="api" className="space-y-4">
                    <p className="text-sm text-gray-600">
                      查看 API 调用结果和响应数据
                    </p>
                  </TabsContent>
                  <TabsContent value="settings" className="space-y-4">
                    <p className="text-sm text-gray-600">
                      配置项目设置和偏好选项
                    </p>
                  </TabsContent>
                </Tabs>

                {/* Dialog */}
                <div className="flex gap-2">
                  <Dialog>
                    <DialogTrigger asChild>
                      <Button variant="outline" size="sm">打开对话框</Button>
                    </DialogTrigger>
                    <DialogContent className="sm:max-w-[425px]">
                      <DialogHeader>
                        <DialogTitle>组件库配置</DialogTitle>
                        <DialogDescription>
                          配置 Radix UI 组件的默认样式和行为
                        </DialogDescription>
                      </DialogHeader>
                      <div className="grid gap-4 py-4">
                        <p className="text-sm text-gray-600">
                          这里可以添加配置选项，如主题、颜色、尺寸等。
                        </p>
                      </div>
                    </DialogContent>
                  </Dialog>

                  {/* Tooltip */}
                  <TooltipProvider>
                    <Tooltip>
                      <TooltipTrigger asChild>
                        <Button variant="outline" size="sm">
                          <Settings className="h-4 w-4 mr-2" />
                          设置
                        </Button>
                      </TooltipTrigger>
                      <TooltipContent>
                        <p>打开项目设置面板</p>
                      </TooltipContent>
                    </Tooltip>
                  </TooltipProvider>
                </div>
              </div>
              
              <div className="rounded-lg bg-gray-50 p-4">
                <p className="text-sm text-gray-600">
                  查看 <code className="bg-gray-200 px-1 rounded">src/components/ui/</code> 目录了解可用的组件
                </p>
              </div>
            </div>
          </Card>
        </div>
      </main>

      {/* 页脚 */}
      <footer className="border-t bg-white">
        <div className="mx-auto max-w-7xl px-4 py-8 sm:px-6 lg:px-8">
          <div className="text-center text-sm text-gray-500">
            <p>&copy; 2024 Next.js Template. 保留所有权利。</p>
          </div>
        </div>
      </footer>
    </div>
  );
}