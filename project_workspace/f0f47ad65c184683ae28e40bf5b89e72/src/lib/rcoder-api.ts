// RCoder API 接口类型定义和通信函数

export interface ChatMessage {
  id: string;
  role: 'user' | 'assistant' | 'system';
  content: string;
  timestamp: Date;
}

export interface ChatRequest {
  message: string;
  agent_type?: 'codex' | 'claude' | 'proxy';
  session_id?: string;
  user_id?: string;
  project_id?: string;
}

export interface ChatResponse {
  success: boolean;
  data?: {
    response: string;
    session_id: string;
    request_id: string;
    agent_type: string;
  };
  error?: {
    code: string;
    message: string;
  };
}

export interface ProgressEvent {
  type: 'message' | 'progress' | 'error' | 'complete';
  data: {
    session_id: string;
    message?: string;
    progress?: number;
    error?: string;
    timestamp: Date;
  };
}

export interface SessionInfo {
  session_id: string;
  user_id?: string;
  project_id?: string;
  agent_type: string;
  created_at: Date;
  last_activity: Date;
}

// API 基础配置
const API_BASE_URL = process.env.NEXT_PUBLIC_API_BASE_URL || 'http://localhost:3000';

// API 客户端类
export class RCoderAPIClient {
  private baseUrl: string;

  constructor(baseUrl: string = API_BASE_URL) {
    this.baseUrl = baseUrl;
  }

  // 发送聊天消息
  async chat(request: ChatRequest): Promise<ChatResponse> {
    try {
      const response = await fetch(`${this.baseUrl}/chat`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify(request),
      });

      if (!response.ok) {
        throw new Error(`HTTP error! status: ${response.status}`);
      }

      return await response.json();
    } catch (error) {
      console.error('Chat API error:', error);
      return {
        success: false,
        error: {
          code: 'NETWORK_ERROR',
          message: error instanceof Error ? error.message : 'Unknown error',
        },
      };
    }
  }

  // 通过代理发送聊天消息
  async chatProxy(request: ChatRequest): Promise<ChatResponse> {
    try {
      const response = await fetch(`${this.baseUrl}/chat/proxy`, {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: JSON.stringify(request),
      });

      if (!response.ok) {
        throw new Error(`HTTP error! status: ${response.status}`);
      }

      return await response.json();
    } catch (error) {
      console.error('Chat Proxy API error:', error);
      return {
        success: false,
        error: {
          code: 'NETWORK_ERROR',
          message: error instanceof Error ? error.message : 'Unknown error',
        },
      };
    }
  }

  // 获取会话信息
  async getSession(sessionId: string): Promise<{ success: boolean; data?: SessionInfo; error?: any }> {
    try {
      const response = await fetch(`${this.baseUrl}/sessions/${sessionId}`);

      if (!response.ok) {
        throw new Error(`HTTP error! status: ${response.status}`);
      }

      return await response.json();
    } catch (error) {
      console.error('Get session error:', error);
      return {
        success: false,
        error: {
          code: 'NETWORK_ERROR',
          message: error instanceof Error ? error.message : 'Unknown error',
        },
      };
    }
  }

  // 创建 SSE 连接监听进度
  createProgressStream(sessionId: string): EventSource {
    const url = `${this.baseUrl}/progress/${sessionId}`;
    return new EventSource(url);
  }

  // 上传文件
  async uploadFile(file: File, sessionId?: string): Promise<{ success: boolean; data?: any; error?: any }> {
    const formData = new FormData();
    formData.append('file', file);
    if (sessionId) {
      formData.append('session_id', sessionId);
    }

    try {
      const response = await fetch(`${this.baseUrl}/chat/multipart`, {
        method: 'POST',
        body: formData,
      });

      if (!response.ok) {
        throw new Error(`HTTP error! status: ${response.status}`);
      }

      return await response.json();
    } catch (error) {
      console.error('Upload file error:', error);
      return {
        success: false,
        error: {
          code: 'NETWORK_ERROR',
          message: error instanceof Error ? error.message : 'Unknown error',
        },
      };
    }
  }
}

// 创建默认的 API 客户端实例
export const apiClient = new RCoderAPIClient();