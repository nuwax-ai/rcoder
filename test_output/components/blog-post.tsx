import { Calendar, Clock, User, ArrowLeft } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Badge } from "@/components/ui/badge"
import Image from "next/image"
import Link from "next/link"

// 示例文章数据
const samplePost = {
  id: 1,
  title: "Next.js 15 新特性详解",
  content: `
    <p>Next.js 15 带来了许多令人兴奋的新特性和改进，让我们一起来探索这些变化如何提升我们的开发体验。</p>
    
    <h2>主要新特性</h2>
    
    <h3>1. 改进的编译器</h3>
    <p>新版本的编译器在性能上有了显著提升，构建速度比之前版本快了 30%。这得益于对 Rust 编译器的进一步优化和新的缓存策略。</p>
    
    <h3>2. 增强的开发者体验</h3>
    <p>开发服务器的启动时间大幅缩短，热重载功能也更加稳定。新的错误页面提供了更详细的调试信息，帮助开发者快速定位问题。</p>
    
    <h3>3. 新的 API 路由功能</h3>
    <p>API 路由现在支持更多的 HTTP 方法，并且提供了更好的类型安全性。中间件系统也得到了增强，支持更复杂的请求处理逻辑。</p>
    
    <h2>升级指南</h2>
    
    <p>要升级到 Next.js 15，你需要确保你的项目满足以下要求：</p>
    
    <ul>
      <li>Node.js 版本 >= 18.17</li>
      <li>React 版本 >= 18.2</li>
      <li>TypeScript 版本 >= 5.0（如果使用 TypeScript）</li>
    </ul>
    
    <p>升级过程相对简单，大多数项目可以无缝升级，但建议在升级前仔细阅读迁移指南。</p>
    
    <h2>总结</h2>
    
    <p>Next.js 15 是一个重要的版本更新，它不仅提升了性能，还改善了开发者体验。如果你正在使用 Next.js 开发项目，我强烈推荐升级到这个版本。</p>
  `,
  category: "技术",
  date: "2024-01-15",
  readTime: "5 分钟",
  image: "/nextjs-development-coding.jpg",
  author: "张三",
}

interface BlogPostProps {
  postId: string
}

export function BlogPost({ postId }: BlogPostProps) {
  // 在实际应用中，这里会根据 postId 从 API 或数据库获取文章数据
  const post = samplePost

  return (
    <article className="max-w-4xl mx-auto">
      {/* Back Button */}
      <div className="mb-6">
        <Link href="/">
          <Button variant="ghost" className="text-muted-foreground hover:text-foreground">
            <ArrowLeft className="h-4 w-4 mr-2" />
            返回首页
          </Button>
        </Link>
      </div>

      {/* Article Header */}
      <header className="mb-8">
        <div className="mb-4">
          <Badge variant="secondary" className="bg-primary text-primary-foreground">
            {post.category}
          </Badge>
        </div>

        <h1 className="text-4xl font-bold text-foreground mb-6 text-balance">{post.title}</h1>

        <div className="flex items-center space-x-6 text-sm text-muted-foreground mb-6">
          <div className="flex items-center space-x-2">
            <User className="h-4 w-4" />
            <span>{post.author}</span>
          </div>
          <div className="flex items-center space-x-2">
            <Calendar className="h-4 w-4" />
            <span>{post.date}</span>
          </div>
          <div className="flex items-center space-x-2">
            <Clock className="h-4 w-4" />
            <span>{post.readTime}</span>
          </div>
        </div>

        <div className="relative overflow-hidden rounded-lg mb-8">
          <Image
            src={post.image || "/placeholder.svg"}
            alt={post.title}
            width={800}
            height={400}
            className="w-full h-64 md:h-96 object-cover"
          />
        </div>
      </header>

      {/* Article Content */}
      <div
        className="prose prose-lg max-w-none prose-headings:text-foreground prose-p:text-foreground prose-p:leading-relaxed prose-li:text-foreground prose-strong:text-foreground"
        dangerouslySetInnerHTML={{ __html: post.content }}
      />

      {/* Article Footer */}
      <footer className="mt-12 pt-8 border-t border-border">
        <div className="flex items-center justify-between">
          <div className="text-sm text-muted-foreground">感谢阅读！如果你觉得这篇文章有帮助，欢迎分享给更多人。</div>
          <div className="flex space-x-2">
            <Button variant="outline" size="sm">
              分享
            </Button>
            <Button variant="outline" size="sm">
              收藏
            </Button>
          </div>
        </div>
      </footer>
    </article>
  )
}