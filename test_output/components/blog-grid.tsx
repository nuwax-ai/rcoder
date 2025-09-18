import { BlogCard } from "@/components/blog-card"

// 示例博客数据
const blogPosts = [
  {
    id: 1,
    title: "Next.js 15 新特性详解",
    excerpt: "探索 Next.js 15 带来的革命性变化，包括新的编译器、改进的性能和开发者体验。",
    category: "技术",
    date: "2024-01-15",
    readTime: "5 分钟",
    image: "/nextjs-development-coding.jpg",
    author: "张三",
  },
  {
    id: 2,
    title: "现代前端开发的最佳实践",
    excerpt: "分享在现代前端开发中应该遵循的最佳实践，提高代码质量和开发效率。",
    category: "教程",
    date: "2024-01-12",
    readTime: "8 分钟",
    image: "/frontend-development-best-practices.jpg",
    author: "李四",
  },
  {
    id: 3,
    title: "我的编程学习之路",
    excerpt: "回顾我从零基础到成为全栈开发者的学习历程，分享一些心得体会。",
    category: "生活",
    date: "2024-01-10",
    readTime: "6 分钟",
    image: "/programming-learning-journey.jpg",
    author: "王五",
  },
  {
    id: 4,
    title: "TypeScript 高级类型系统",
    excerpt: "深入了解 TypeScript 的高级类型系统，掌握泛型、条件类型和映射类型。",
    category: "技术",
    date: "2024-01-08",
    readTime: "10 分钟",
    image: "/typescript-advanced-types.jpg",
    author: "赵六",
  },
  {
    id: 5,
    title: "关于工作与生活平衡的思考",
    excerpt: "在快节奏的现代生活中，如何找到工作与生活的平衡点，保持身心健康。",
    category: "思考",
    date: "2024-01-05",
    readTime: "4 分钟",
    image: "/work-life-balance-reflection.jpg",
    author: "孙七",
  },
  {
    id: 6,
    title: "React 性能优化实战指南",
    excerpt: "通过实际案例学习 React 性能优化技巧，包括 memo、useMemo 和 useCallback 的使用。",
    category: "教程",
    date: "2024-01-03",
    readTime: "12 分钟",
    image: "/react-performance-optimization.png",
    author: "周八",
  },
]

export function BlogGrid() {
  return (
    <div className="space-y-12">
      <div className="text-center relative">
        <div className="absolute inset-0 flex items-center justify-center">
          <div className="w-32 h-px bg-gradient-to-r from-transparent via-blue-200 to-transparent"></div>
        </div>
        <div className="relative bg-gradient-to-br from-slate-50 via-white to-blue-50/30 px-8 py-2 inline-block">
          <h2 className="text-4xl font-bold bg-gradient-to-r from-slate-800 via-slate-700 to-slate-600 bg-clip-text text-transparent mb-4">
            最新文章
          </h2>
          <p className="text-slate-600 text-lg font-medium">分享技术见解、生活感悟和学习心得</p>
        </div>
      </div>

      <div className="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-8">
        {blogPosts.map((post) => (
          <BlogCard key={post.id} post={post} />
        ))}
      </div>
    </div>
  )
}