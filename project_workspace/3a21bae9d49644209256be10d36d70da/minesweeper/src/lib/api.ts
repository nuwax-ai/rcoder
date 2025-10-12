import axios, { AxiosInstance, AxiosRequestConfig, AxiosResponse, AxiosError } from 'axios';

interface ApiResponse<T = any> {
  code: number;
  data: T;
  message: string;
}

interface ApiError {
  code: number;
  message: string;
}

class ApiClient {
  private instance: AxiosInstance;

  constructor(baseURL: string = '/api') {
    this.instance = axios.create({
      baseURL,
      timeout: 10000,
      headers: {
        'Content-Type': 'application/json',
      },
    });

    this.setupInterceptors();
  }

  private setupInterceptors() {
    // 请求拦截器
    this.instance.interceptors.request.use(
      (config) => {
        // 可以在这里添加token等认证信息
        const token = localStorage.getItem('token');
        if (token) {
          config.headers.Authorization = `Bearer ${token}`;
        }
        return config;
      },
      (error) => {
        return Promise.reject(error);
      }
    );

    // 响应拦截器
    this.instance.interceptors.response.use(
      (response: AxiosResponse<ApiResponse>) => {
        const { code, data, message } = response.data;
        
        // 处理业务错误
        if (code !== 200) {
          const error: ApiError = { code, message };
          return Promise.reject(error);
        }
        
        return data;
      },
      (error: AxiosError<ApiError>) => {
        if (error.response) {
          // 服务器返回错误状态码
          const { status, data } = error.response;
          console.error('API Error:', status, data?.message || error.message);
        } else if (error.request) {
          // 请求已经发出，但没有收到响应
          console.error('Network Error:', error.message);
        } else {
          // 在设置请求时发生错误
          console.error('Request Error:', error.message);
        }
        
        return Promise.reject(error);
      }
    );
  }

  async get<T>(url: string, config?: AxiosRequestConfig): Promise<T> {
    return this.instance.get(url, config);
  }

  async post<T>(url: string, data?: any, config?: AxiosRequestConfig): Promise<T> {
    return this.instance.post(url, data, config);
  }

  async put<T>(url: string, data?: any, config?: AxiosRequestConfig): Promise<T> {
    return this.instance.put(url, data, config);
  }

  async delete<T>(url: string, config?: AxiosRequestConfig): Promise<T> {
    return this.instance.delete(url, config);
  }

  async patch<T>(url: string, data?: any, config?: AxiosRequestConfig): Promise<T> {
    return this.instance.patch(url, data, config);
  }
}

// 便捷方法
export const api = {
  get: <T>(url: string, config?: AxiosRequestConfig) => new ApiClient().get<T>(url, config),
  post: <T>(url: string, data?: any, config?: AxiosRequestConfig) => new ApiClient().post<T>(url, data, config),
  put: <T>(url: string, data?: any, config?: AxiosRequestConfig) => new ApiClient().put<T>(url, data, config),
  delete: <T>(url: string, config?: AxiosRequestConfig) => new ApiClient().delete<T>(url, config),
  patch: <T>(url: string, data?: any, config?: AxiosRequestConfig) => new ApiClient().patch<T>(url, data, config),
};

export default ApiClient;