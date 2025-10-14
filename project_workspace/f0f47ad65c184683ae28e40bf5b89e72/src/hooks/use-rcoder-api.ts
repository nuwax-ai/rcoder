'use client';

import { useState, useCallback, useRef, useEffect } from 'react';
import {
  apiClient,
  ChatRequest,
  ChatResponse,
  ProgressEvent,
  SessionInfo
} from '@/lib/rcoder-api';

export function useRCoderAPI() {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const eventSourceRef = useRef<EventSource | null>(null);

  const sendMessage = useCallback(async (request: ChatRequest): Promise<ChatResponse> => {
    setLoading(true);
    setError(null);

    try {
      const response = await apiClient.chat(request);

      if (!response.success) {
        setError(response.error?.message || 'Unknown error occurred');
      }

      return response;
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error';
      setError(errorMessage);
      return {
        success: false,
        error: {
          code: 'HOOK_ERROR',
          message: errorMessage,
        },
      };
    } finally {
      setLoading(false);
    }
  }, []);

  const sendMessageProxy = useCallback(async (request: ChatRequest): Promise<ChatResponse> => {
    setLoading(true);
    setError(null);

    try {
      const response = await apiClient.chatProxy(request);

      if (!response.success) {
        setError(response.error?.message || 'Unknown error occurred');
      }

      return response;
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Unknown error';
      setError(errorMessage);
      return {
        success: false,
        error: {
          code: 'HOOK_ERROR',
          message: errorMessage,
        },
      };
    } finally {
      setLoading(false);
    }
  }, []);

  const subscribeToProgress = useCallback((
    sessionId: string,
    onProgress: (event: ProgressEvent) => void
  ) => {
    // 清理之前的连接
    if (eventSourceRef.current) {
      eventSourceRef.current.close();
    }

    eventSourceRef.current = apiClient.createProgressStream(sessionId);

    eventSourceRef.current.onmessage = (event) => {
      try {
        const progressEvent: ProgressEvent = JSON.parse(event.data);
        onProgress(progressEvent);
      } catch (error) {
        console.error('Error parsing progress event:', error);
      }
    };

    eventSourceRef.current.onerror = (error) => {
      console.error('EventSource error:', error);
      setError('Connection to progress stream lost');
    };

    return () => {
      if (eventSourceRef.current) {
        eventSourceRef.current.close();
        eventSourceRef.current = null;
      }
    };
  }, []);

  const unsubscribeFromProgress = useCallback(() => {
    if (eventSourceRef.current) {
      eventSourceRef.current.close();
      eventSourceRef.current = null;
    }
  }, []);

  const getSession = useCallback(async (sessionId: string) => {
    return await apiClient.getSession(sessionId);
  }, []);

  const uploadFile = useCallback(async (file: File, sessionId?: string) => {
    setLoading(true);
    setError(null);

    try {
      const response = await apiClient.uploadFile(file, sessionId);

      if (!response.success) {
        setError(response.error?.message || 'Upload failed');
      }

      return response;
    } catch (error) {
      const errorMessage = error instanceof Error ? error.message : 'Upload failed';
      setError(errorMessage);
      return {
        success: false,
        error: {
          code: 'UPLOAD_ERROR',
          message: errorMessage,
        },
      };
    } finally {
      setLoading(false);
    }
  }, []);

  // 清理函数
  useEffect(() => {
    return () => {
      if (eventSourceRef.current) {
        eventSourceRef.current.close();
      }
    };
  }, []);

  return {
    loading,
    error,
    sendMessage,
    sendMessageProxy,
    getSession,
    uploadFile,
    subscribeToProgress,
    unsubscribeFromProgress,
  };
}