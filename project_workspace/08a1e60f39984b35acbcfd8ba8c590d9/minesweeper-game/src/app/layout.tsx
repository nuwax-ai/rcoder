import type { Metadata } from 'next';
import { Inter } from 'next/font/google';
import './globals.css';

// 配置字体
const inter = Inter({ subsets: ['latin'] });

// 元数据配置
export const metadata: Metadata = {
  title: {
    default: '{{projectName}}',
    template: '%s | {{projectName}}',
  },
  description: '{{description}}',
  keywords: ['Next.js', 'React', 'TypeScript', 'Tailwind CSS'],
  authors: [{ name: '{{author}}' }],
  creator: '{{author}}',
  publisher: '{{author}}',
  formatDetection: {
    email: false,
    address: false,
    telephone: false,
  },
  metadataBase: new URL('https://example.com'),
  alternates: {
    canonical: '/',
  },
  openGraph: {
    type: 'website',
    locale: 'zh_CN',
    url: 'https://example.com',
    title: '{{projectName}}',
    description: '{{description}}',
    siteName: '{{projectName}}',
  },
  twitter: {
    card: 'summary_large_image',
    title: '{{projectName}}',
    description: '{{description}}',
    creator: '@username',
  },
  robots: {
    index: true,
    follow: true,
    googleBot: {
      index: true,
      follow: true,
      'max-video-preview': -1,
      'max-image-preview': 'large',
      'max-snippet': -1,
    },
  },
};

export default function RootLayout({
  children,
}: {
  children: React.ReactNode;
}) {
  return (
    <html lang="zh-CN" className="h-full">
      <body className={`${inter.className} h-full antialiased`}>
        <div className="min-h-full">
          {children}
        </div>
      </body>
    </html>
  );
}
