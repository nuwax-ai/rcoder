import * as React from 'react';
import { cn } from '@/lib/utils';

// 卡片属性接口
export interface CardProps extends React.HTMLAttributes<HTMLDivElement> {}

// 卡片标题属性接口
export interface CardHeaderProps extends React.HTMLAttributes<HTMLDivElement> {}

// 卡片内容属性接口
export interface CardContentProps extends React.HTMLAttributes<HTMLDivElement> {}

// 卡片页脚属性接口
export interface CardFooterProps extends React.HTMLAttributes<HTMLDivElement> {}

// 卡片标题属性接口
export interface CardTitleProps extends React.HTMLAttributes<HTMLHeadingElement> {}

// 卡片描述属性接口
export interface CardDescriptionProps extends React.HTMLAttributes<HTMLParagraphElement> {}

/**
 * 卡片组件
 * 用于展示相关内容和操作
 */
const Card = React.forwardRef<HTMLDivElement, CardProps>(
  ({ className, ...props }, ref) => (
    <div
      ref={ref}
      className={cn(
        'rounded-lg border border-gray-200 bg-white text-gray-950 shadow-sm',
        className
      )}
      {...props}
    />
  )
);
Card.displayName = 'Card';

/**
 * 卡片头部组件
 */
const CardHeader = React.forwardRef<HTMLDivElement, CardHeaderProps>(
  ({ className, ...props }, ref) => (
    <div
      ref={ref}
      className={cn('flex flex-col space-y-1.5 p-6', className)}
      {...props}
    />
  )
);
CardHeader.displayName = 'CardHeader';

/**
 * 卡片标题组件
 */
const CardTitle = React.forwardRef<HTMLParagraphElement, CardTitleProps>(
  ({ className, ...props }, ref) => (
    <h3
      ref={ref}
      className={cn(
        'text-2xl font-semibold leading-none tracking-tight',
        className
      )}
      {...props}
    />
  )
);
CardTitle.displayName = 'CardTitle';

/**
 * 卡片描述组件
 */
const CardDescription = React.forwardRef<HTMLParagraphElement, CardDescriptionProps>(
  ({ className, ...props }, ref) => (
    <p
      ref={ref}
      className={cn('text-sm text-gray-500', className)}
      {...props}
    />
  )
);
CardDescription.displayName = 'CardDescription';

/**
 * 卡片内容组件
 */
const CardContent = React.forwardRef<HTMLDivElement, CardContentProps>(
  ({ className, ...props }, ref) => (
    <div ref={ref} className={cn('p-6 pt-0', className)} {...props} />
  )
);
CardContent.displayName = 'CardContent';

/**
 * 卡片页脚组件
 */
const CardFooter = React.forwardRef<HTMLDivElement, CardFooterProps>(
  ({ className, ...props }, ref) => (
    <div
      ref={ref}
      className={cn('flex items-center p-6 pt-0', className)}
      {...props}
    />
  )
);
CardFooter.displayName = 'CardFooter';

export { Card, CardHeader, CardFooter, CardTitle, CardDescription, CardContent };
