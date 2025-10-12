import * as React from 'react';
import { cn } from '@/lib/utils';

// 文本域属性接口
export interface TextareaProps extends React.TextareaHTMLAttributes<HTMLTextAreaElement> {}

/**
 * 文本域组件
 * 提供样式统一的文本域组件
 */
const Textarea = React.forwardRef<HTMLTextAreaElement, TextareaProps>(
  ({ className, ...props }, ref) => {
    return (
      <textarea
        className={cn(
          // 基础样式
          'flex min-h-[80px] w-full rounded-md border border-gray-300 bg-white px-3 py-2',
          'text-sm ring-offset-white placeholder:text-gray-500',
          // 焦点状态
          'focus:outline-none focus:ring-2 focus:ring-primary-500 focus:ring-offset-2',
          // 禁用状态
          'disabled:cursor-not-allowed disabled:opacity-50',
          // 错误状态
          'aria-invalid:border-red-500 aria-invalid:ring-red-500',
          // 调整大小
          'resize-vertical',
          className
        )}
        ref={ref}
        {...props}
      />
    );
  }
);
Textarea.displayName = 'Textarea';

export { Textarea };
