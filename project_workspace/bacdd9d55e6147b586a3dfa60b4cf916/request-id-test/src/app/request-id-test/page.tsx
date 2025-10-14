/**
 * Request ID 传递机制测试页面
 * 用于验证 RCoder 系统中 request_id 的正确传递
 */
'use client';

import { useState } from 'react';
import axios from 'axios';

interface ChatRequest {
  prompt: string;
  project_id?: string;
  session_id?: string;
  request_id?: string;
  model_provider?: {
    id: string;
    name: string;
    base_url: string;
    api_key: string;
    requires_openai_auth: boolean;
    default_model: string;
    api_protocol: string;
  };
}

interface ChatResponse {
  project_id: string;
  session_id: string;
  error?: string;
  request_id?: string;
}

interface TestResult {
  timestamp: string;
  requestId: string;
  response: ChatResponse | null;
  error: string | null;
  duration: number;
}

export default function RequestIdTestPage() {
  const [isLoading, setIsLoading] = useState(false);
  const [testResults, setTestResults] = useState<TestResult[]>([]);
  const [currentProjectId, setCurrentProjectId] = useState('');
  const [currentSessionId, setCurrentSessionId] = useState('');

  // 生成测试用的 request_id
  const generateTestRequestId = () => {
    return `test_${Date.now()}_${Math.random().toString(36).substr(2, 9)}`;
  };

  // 执行单次测试
  const runSingleTest = async (customRequestId?: string) => {
    if (!currentProjectId) {
      alert('请先设置项目 ID');
      return;
    }

    setIsLoading(true);
    const startTime = Date.now();
    const requestId = customRequestId || generateTestRequestId();

    const testRequest: ChatRequest = {
      prompt: `测试 request_id 传递机制 - 时间戳: ${Date.now()}`,
      project_id: currentProjectId,
      session_id: currentSessionId || undefined,
      request_id: requestId,
      model_provider: {
        id: "openai_gpt4",
        name: "openai",
        base_url: "https://api.openai.com/v1",
        api_key: "sk-test-key",
        requires_openai_auth: true,
        default_model: "gpt-4",
        api_protocol: "openai"
      }
    };

    try {
      const response = await axios.post('http://localhost:3000/chat', testRequest, {
        headers: {
          'Content-Type': 'application/json',
        },
        timeout: 30000,
      });

      const endTime = Date.now();
      const testResult: TestResult = {
        timestamp: new Date().toISOString(),
        requestId: requestId,
        response: response.data.data,
        error: null,
        duration: endTime - startTime,
      };

      setTestResults(prev => [testResult, ...prev.slice(0, 9)]); // 保留最新10条

      console.log('✅ 测试成功:', testResult);
    } catch (error) {
      const endTime = Date.now();
      const testResult: TestResult = {
        timestamp: new Date().toISOString(),
        requestId: requestId,
        response: null,
        error: error instanceof Error ? error.message : String(error),
        duration: endTime - startTime,
      };

      setTestResults(prev => [testResult, ...prev.slice(0, 9)]);
      console.error('❌ 测试失败:', testResult);
    } finally {
      setIsLoading(false);
    }
  };

  // 执行连续测试
  const runSequentialTests = async () => {
    if (!currentProjectId) {
      alert('请先设置项目 ID');
      return;
    }

    for (let i = 1; i <= 5; i++) {
      console.log(`🚀 开始第 ${i} 次测试...`);
      await runSingleTest(`sequential_test_${i}_${Date.now()}`);
      // 等待1秒再进行下一次测试
      await new Promise(resolve => setTimeout(resolve, 1000));
    }
  };

  // 清空测试结果
  const clearResults = () => {
    setTestResults([]);
  };

  return (
    <div className="min-h-screen bg-gray-50 p-8">
      <div className="max-w-4xl mx-auto">
        <h1 className="text-3xl font-bold text-gray-900 mb-8">
          Request ID 传递机制测试
        </h1>

        {/* 配置区域 */}
        <div className="bg-white rounded-lg shadow-md p-6 mb-8">
          <h2 className="text-xl font-semibold mb-4">测试配置</h2>
          <div className="grid grid-cols-2 gap-4 mb-4">
            <div>
              <label className="block text-sm font-medium text-gray-700 mb-1">
                项目 ID
              </label>
              <input
                type="text"
                value={currentProjectId}
                onChange={(e) => setCurrentProjectId(e.target.value)}
                placeholder="输入项目 ID"
                className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              />
            </div>
            <div>
              <label className="block text-sm font-medium text-gray-700 mb-1">
                会话 ID (可选)
              </label>
              <input
                type="text"
                value={currentSessionId}
                onChange={(e) => setCurrentSessionId(e.target.value)}
                placeholder="输入会话 ID 或留空自动生成"
                className="w-full px-3 py-2 border border-gray-300 rounded-md focus:outline-none focus:ring-2 focus:ring-blue-500"
              />
            </div>
          </div>

          <div className="flex space-x-4">
            <button
              onClick={() => runSingleTest()}
              disabled={isLoading}
              className="px-4 py-2 bg-blue-600 text-white rounded-md hover:bg-blue-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {isLoading ? '执行中...' : '单次测试'}
            </button>
            <button
              onClick={runSequentialTests}
              disabled={isLoading}
              className="px-4 py-2 bg-green-600 text-white rounded-md hover:bg-green-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              {isLoading ? '执行中...' : '连续测试 (5次)'}
            </button>
            <button
              onClick={clearResults}
              disabled={isLoading}
              className="px-4 py-2 bg-gray-600 text-white rounded-md hover:bg-gray-700 disabled:opacity-50 disabled:cursor-not-allowed"
            >
              清空结果
            </button>
          </div>
        </div>

        {/* 测试结果 */}
        <div className="bg-white rounded-lg shadow-md p-6">
          <h2 className="text-xl font-semibold mb-4">
            测试结果 {testResults.length > 0 && `(${testResults.length} 条记录)`}
          </h2>

          {testResults.length === 0 ? (
            <p className="text-gray-500">暂无测试结果</p>
          ) : (
            <div className="space-y-4">
              {testResults.map((result, index) => (
                <div
                  key={result.timestamp}
                  className={`p-4 rounded-lg border ${
                    result.error
                      ? 'border-red-300 bg-red-50'
                      : 'border-green-300 bg-green-50'
                  }`}
                >
                  <div className="flex justify-between items-start mb-2">
                    <div>
                      <span className="font-semibold">#{testResults.length - index} </span>
                      <span className="text-sm text-gray-600">
                        {result.timestamp}
                      </span>
                    </div>
                    <div className="text-right">
                      <span className={`inline-block px-2 py-1 text-xs font-semibold rounded ${
                        result.error
                          ? 'bg-red-100 text-red-800'
                          : 'bg-green-100 text-green-800'
                      }`}>
                        {result.error ? '失败' : '成功'}
                      </span>
                      <div className="text-sm text-gray-600 mt-1">
                        {result.duration}ms
                      </div>
                    </div>
                  </div>

                  <div className="text-sm space-y-1">
                    <div>
                      <strong>请求 request_id:</strong> {result.requestId}
                    </div>
                    {result.response && (
                      <>
                        <div>
                          <strong>响应 request_id:</strong> {result.response.request_id || '未返回'}
                        </div>
                        <div>
                          <strong>项目 ID:</strong> {result.response.project_id}
                        </div>
                        <div>
                          <strong>会话 ID:</strong> {result.response.session_id}
                        </div>
                        {result.response.error && (
                          <div className="text-red-600">
                            <strong>响应错误:</strong> {result.response.error}
                          </div>
                        )}
                        <div className={`mt-2 p-2 rounded text-xs font-mono ${
                          result.response.request_id === result.requestId
                            ? 'bg-green-100 text-green-800'
                            : 'bg-yellow-100 text-yellow-800'
                        }`}>
                          {result.response.request_id === result.requestId
                            ? '✅ request_id 匹配'
                            : '⚠️ request_id 不匹配'}
                        </div>
                      </>
                    )}
                    {result.error && (
                      <div className="text-red-600">
                        <strong>错误:</strong> {result.error}
                      </div>
                    )}
                  </div>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* 说明文档 */}
        <div className="bg-blue-50 border border-blue-200 rounded-lg p-6 mt-8">
          <h3 className="text-lg font-semibold text-blue-900 mb-3">
            📋 测试说明
          </h3>
          <div className="text-sm text-blue-800 space-y-2">
            <p>
              <strong>测试目标:</strong> 验证 RCoder 系统中 request_id 的正确传递机制
            </p>
            <p>
              <strong>验证流程:</strong>
            </p>
            <ol className="list-decimal list-inside ml-4 space-y-1">
              <li>客户端发送带有自定义 request_id 的请求</li>
              <li>系统处理请求并返回响应</li>
              <li>验证响应中的 request_id 与请求中的是否一致</li>
            </ol>
            <p>
              <strong>预期结果:</strong> 响应中的 request_id 应与请求中的完全一致
            </p>
            <p>
              <strong>技术实现:</strong> 系统通过以下组件传递 request_id:
            </p>
            <ul className="list-disc list-inside ml-4 space-y-1">
              <li><code>ChatHandler</code>: 接收并处理 request_id</li>
              <li><code>AcpAgent</code>: 将 request_id 放入 PromptRequest.meta</li>
              <li><code>ChannelUtils</code>: 从 meta 提取并存储到 SESSION_REQUEST_CONTEXT</li>
              <li><code>SessionNotification</code>: 发送带有 request_id 的通知</li>
            </ul>
          </div>
        </div>
      </div>
    </div>
  );
}