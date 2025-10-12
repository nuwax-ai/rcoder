import * as React from 'react';
import { cn } from '@/lib/utils';

// 输入框属性接口
export interface InputProps extends React.InputHTMLAttributes<HTMLInputElement> {}

/**
 * 输入框组件
 * 提供样式统一的输入框组件
 */
const Input = React.forwardRef<HTMLInputElement, InputProps>(
  ({ className, type, ...props }, ref) => {
    return (
      <input
        type={type}
        className={cn(
          // 基础样式
          'flex h-10 w-full rounded-md border border-gray-300 bg-white px-3 py-2',
          'text-sm ring-offset-white file:border-0 file:bg-transparent file:text-sm file:font-medium',
          'placeholder:text-gray-500',
          // 焦点状态
          'focus:outline-none focus:ring-2 focus:ring-primary-500 focus:ring-offset-2',
          // 禁用状态
          'disabled:cursor-not-allowed disabled:opacity-50',
          // 错误状态
          'aria-invalid:border-red-500 aria-invalid:ring-red-500',
          className
        )}
        ref={ref}
        {...props}
      />
    );
  }
);
Input.displayName = 'Input';

export { Input };
