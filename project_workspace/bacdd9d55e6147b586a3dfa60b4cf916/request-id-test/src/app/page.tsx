/**
 * 主页面 - 重定向到 request_id 测试页面
 */
'use client';

import { useEffect } from 'react';

export default function HomePage() {
  useEffect(() => {
    // 自动重定向到测试页面
    window.location.href = '/request-id-test';
  }, []);

  return (
    <div className="min-h-screen flex items-center justify-center bg-gray-50">
      <div className="text-center">
        <h1 className="text-2xl font-bold text-gray-900 mb-4">
          RCoder Request ID 测试
        </h1>
        <p className="text-gray-600">
          正在重定向到测试页面...
        </p>
        <p className="text-sm text-gray-500 mt-2">
          如果没有自动跳转，请{' '}
          <a href="/request-id-test" className="text-blue-600 hover:underline">
            点击这里
          </a>
        </p>
      </div>
    </div>
  );
}