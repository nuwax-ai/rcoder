import { api } from './api';

// 接口返回数据类型定义
export interface User {
  id: number;
  username: string;
  email: string;
  avatar?: string;
  createdAt: string;
}

export interface LoginParams {
  username: string;
  password: string;
}

export interface LoginResult {
  token: string;
  user: User;
}

export interface ListParams {
  page: number;
  pageSize: number;
  keyword?: string;
}

export interface ListResult<T> {
  list: T[];
  total: number;
  page: number;
  pageSize: number;
}

// 用户相关API
export const userApi = {
  // 登录
  login: (params: LoginParams) => api.post<LoginResult>('/auth/login', params),
  
  // 获取用户信息
  getUserInfo: () => api.get<User>('/user/info'),
  
  // 更新用户信息
  updateUserInfo: (data: Partial<User>) => api.put<User>('/user/info', data),
};

// 示例数据列表API
export const exampleApi = {
  // 获取列表数据
  getList: (params: ListParams) => api.get<ListResult<any>>('/example/list', { params }),
  
  // 创建项目
  create: (data: any) => api.post<any>('/example/create', data),
  
  // 更新项目
  update: (id: number, data: any) => api.put<any>(`/example/update/${id}`, data),
  
  // 删除项目
  delete: (id: number) => api.delete<any>(`/example/delete/${id}`),
  
  // 获取详情
  getDetail: (id: number) => api.get<any>(`/example/detail/${id}`),
};

// React Hook 封装示例
import { useState, useEffect } from 'react';

export function useApi<T>(
  apiCall: () => Promise<T>,
  deps: any[] = []
) {
  const [data, setData] = useState<T | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<Error | null>(null);

  const fetchData = async () => {
    setLoading(true);
    setError(null);
    
    try {
      const result = await apiCall();
      setData(result);
    } catch (err) {
      setError(err as Error);
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    fetchData();
  }, deps);

  return { data, loading, error, refetch: fetchData };
}

// 使用示例：
// const { data: userInfo, loading, error } = useApi(() => userApi.getUserInfo());