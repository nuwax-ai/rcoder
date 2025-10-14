'use client';

import { useState, useRef, useEffect } from 'react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { Textarea } from '@/components/ui/textarea';
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select';
import { Badge } from '@/components/ui/badge';
import { ScrollArea } from '@/components/ui/scroll-area';
import { Separator } from '@/components/ui/separator';
import { Send, Bot, User, Loader2, Copy, Check } from 'lucide-react';
import { useRCoderAPI } from '@/hooks/use-rcoder-api';
import { ChatMessage, ProgressEvent } from '@/lib/rcoder-api';

interface ChatInterfaceProps {
  userId?: string;
  projectId?: string;
  onSessionChange?: (sessionId: string) => void;
}

export function ChatInterface({ userId, projectId, onSessionChange }: ChatInterfaceProps) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [input, setInput] = useState('');
  const [agentType, setAgentType] = useState<'codex' | 'claude' | 'proxy'>('codex');
  const [sessionId, setSessionId] = useState<string>('');
  const [copiedMessageId, setCopiedMessageId] = useState<string | null>(null);

  const messagesEndRef = useRef<HTMLDivElement>(null);
  const {
    loading,
    error,
    sendMessage,
    subscribeToProgress,
    unsubscribeFromProgress,
  } = useRCoderAPI();

  // 自动滚动到底部
  const scrollToBottom = () => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' });
  };

  useEffect(() => {
    scrollToBottom();
  }, [messages]);

  // 订阅进度事件
  useEffect(() => {
    if (sessionId) {
      const unsubscribe = subscribeToProgress(sessionId, (event: ProgressEvent) => {
        if (event.type === 'message') {
          const assistantMessage: ChatMessage = {
            id: `msg-${Date.now()}`,
            role: 'assistant',
            content: event.data.message || '',
            timestamp: new Date(),
          };
          setMessages(prev => [...prev, assistantMessage]);
        } else if (event.type === 'progress') {
          // 可以在这里添加进度显示逻辑
          console.log('Progress:', event.data.progress);
        } else if (event.type === 'error') {
          console.error('Progress error:', event.data.error);
        }
      });

      return unsubscribe;
    }
  }, [sessionId, subscribeToProgress]);

  const handleSendMessage = async () => {
    if (!input.trim() || loading) return;

    const userMessage: ChatMessage = {
      id: `msg-${Date.now()}`,
      role: 'user',
      content: input.trim(),
      timestamp: new Date(),
    };

    setMessages(prev => [...prev, userMessage]);
    setInput('');

    try {
      const response = await sendMessage({
        message: input.trim(),
        agent_type: agentType,
        session_id: sessionId || undefined,
        user_id: userId,
        project_id: projectId,
      });

      if (response.success && response.data) {
        // 更新会话ID
        if (response.data.session_id !== sessionId) {
          setSessionId(response.data.session_id);
          onSessionChange?.(response.data.session_id);
        }

        // 如果响应包含完整的回复，直接显示
        if (response.data.response) {
          const assistantMessage: ChatMessage = {
            id: `msg-${Date.now()}`,
            role: 'assistant',
            content: response.data.response,
            timestamp: new Date(),
          };
          setMessages(prev => [...prev, assistantMessage]);
        }
      } else {
        // 显示错误消息
        const errorMessage: ChatMessage = {
          id: `msg-${Date.now()}`,
          role: 'assistant',
          content: `错误: ${response.error?.message || 'Unknown error'}`,
          timestamp: new Date(),
        };
        setMessages(prev => [...prev, errorMessage]);
      }
    } catch (error) {
      console.error('Send message error:', error);
    }
  };

  const handleKeyPress = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      handleSendMessage();
    }
  };

  const copyMessage = async (content: string, messageId: string) => {
    try {
      await navigator.clipboard.writeText(content);
      setCopiedMessageId(messageId);
      setTimeout(() => setCopiedMessageId(null), 2000);
    } catch (error) {
      console.error('Failed to copy message:', error);
    }
  };

  const clearChat = () => {
    setMessages([]);
    setSessionId('');
    onSessionChange?.('');
  };

  return (
    <div className="flex flex-col h-full max-h-[800px]">
      {/* 头部配置 */}
      <Card className="p-4 mb-4">
        <div className="flex items-center justify-between">
          <div className="flex items-center space-x-4">
            <div className="flex items-center space-x-2">
              <label htmlFor="agent-type" className="text-sm font-medium">
                Agent 类型:
              </label>
              <Select value={agentType} onValueChange={(value: 'codex' | 'claude' | 'proxy') => setAgentType(value)}>
                <SelectTrigger className="w-32">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="codex">Codex</SelectItem>
                  <SelectItem value="claude">Claude</SelectItem>
                  <SelectItem value="proxy">Proxy</SelectItem>
                </SelectContent>
              </Select>
            </div>

            {sessionId && (
              <Badge variant="secondary" className="text-xs">
                会话: {sessionId.slice(0, 8)}...
              </Badge>
            )}
          </div>

          <Button variant="outline" size="sm" onClick={clearChat}>
            清空对话
          </Button>
        </div>
      </Card>

      {/* 错误显示 */}
      {error && (
        <Card className="p-3 mb-4 border-red-200 bg-red-50">
          <p className="text-sm text-red-600">错误: {error}</p>
        </Card>
      )}

      {/* 消息列表 */}
      <Card className="flex-1 p-4">
        <ScrollArea className="h-full">
          <div className="space-y-4">
            {messages.length === 0 ? (
              <div className="text-center text-gray-500 py-8">
                <Bot className="h-12 w-12 mx-auto mb-4 text-gray-300" />
                <p>开始对话吧！选择一个 Agent 类型并发送消息。</p>
              </div>
            ) : (
              messages.map((message) => (
                <div key={message.id} className="flex space-x-3">
                  <div className="flex-shrink-0">
                    {message.role === 'user' ? (
                      <div className="w-8 h-8 bg-blue-100 rounded-full flex items-center justify-center">
                        <User className="h-4 w-4 text-blue-600" />
                      </div>
                    ) : (
                      <div className="w-8 h-8 bg-green-100 rounded-full flex items-center justify-center">
                        <Bot className="h-4 w-4 text-green-600" />
                      </div>
                    )}
                  </div>

                  <div className="flex-1 space-y-2">
                    <div className="flex items-center justify-between">
                      <span className="text-sm font-medium text-gray-900">
                        {message.role === 'user' ? '你' : 'Assistant'}
                      </span>
                      <div className="flex items-center space-x-2">
                        <span className="text-xs text-gray-500">
                          {message.timestamp.toLocaleTimeString()}
                        </span>
                        <Button
                          variant="ghost"
                          size="sm"
                          className="h-6 w-6 p-0"
                          onClick={() => copyMessage(message.content, message.id)}
                        >
                          {copiedMessageId === message.id ? (
                            <Check className="h-3 w-3" />
                          ) : (
                            <Copy className="h-3 w-3" />
                          )}
                        </Button>
                      </div>
                    </div>

                    <div className="bg-gray-50 rounded-lg p-3">
                      <p className="text-sm whitespace-pre-wrap">{message.content}</p>
                    </div>
                  </div>
                </div>
              ))
            )}

            {loading && (
              <div className="flex space-x-3">
                <div className="flex-shrink-0">
                  <div className="w-8 h-8 bg-green-100 rounded-full flex items-center justify-center">
                    <Bot className="h-4 w-4 text-green-600" />
                  </div>
                </div>
                <div className="flex-1">
                  <div className="flex items-center space-x-2">
                    <Loader2 className="h-4 w-4 animate-spin" />
                    <span className="text-sm text-gray-500">Agent 正在思考...</span>
                  </div>
                </div>
              </div>
            )}

            <div ref={messagesEndRef} />
          </div>
        </ScrollArea>
      </Card>

      {/* 输入区域 */}
      <Card className="p-4 mt-4">
        <div className="flex space-x-4">
          <Textarea
            value={input}
            onChange={(e) => setInput(e.target.value)}
            onKeyPress={handleKeyPress}
            placeholder="输入你的消息... (Shift+Enter 换行, Enter 发送)"
            className="flex-1 resize-none"
            rows={3}
            disabled={loading}
          />
          <div className="flex flex-col space-y-2">
            <Button
              onClick={handleSendMessage}
              disabled={!input.trim() || loading}
              className="px-4"
            >
              {loading ? (
                <Loader2 className="h-4 w-4 animate-spin" />
              ) : (
                <Send className="h-4 w-4" />
              )}
              <span className="ml-2">发送</span>
            </Button>
          </div>
        </div>
      </Card>
    </div>
  );
}