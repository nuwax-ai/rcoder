/** @type {import('next').NextConfig} */
const nextConfig = {
  // 启用严格模式
  reactStrictMode: true,

  // 启用 SWC 最小化（默认启用）
  swcMinify: true,

  
  // 图片优化配置
  images: {
    // 允许的图片域名
    domains: [],
    // 图片格式
    formats: ['image/webp', 'image/avif'],
  },

  // 环境变量
  env: {
    CUSTOM_KEY: process.env.CUSTOM_KEY,
  },

  // 重定向配置
  async redirects() {
    return [
      // 示例重定向
      // {
      //   source: '/old-page',
      //   destination: '/new-page',
      //   permanent: true,
      // },
    ];
  },

  // 重写配置
  async rewrites() {
    return [
      // 示例重写
      // {
      //   source: '/api/:path*',
      //   destination: 'http://localhost:3001/api/:path*',
      // },
    ];
  },

  // 头部配置
  async headers() {
    return [
      {
        source: '/(.*)',
        headers: [
          {
            key: 'X-Frame-Options',
            value: 'DENY',
          },
          {
            key: 'X-Content-Type-Options',
            value: 'nosniff',
          },
          {
            key: 'Referrer-Policy',
            value: 'origin-when-cross-origin',
          },
        ],
      },
    ];
  },
};

module.exports = nextConfig;
