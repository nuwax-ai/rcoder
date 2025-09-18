import { Github, Twitter, Mail, Rss } from "lucide-react"
import { Button } from "@/components/ui/button"

export function BlogFooter() {
  return (
    <footer className="bg-muted mt-16">
      <div className="container mx-auto px-4 py-12">
        <div className="grid grid-cols-1 md:grid-cols-3 gap-8">
          {/* About Section */}
          <div className="space-y-4">
            <h3 className="text-lg font-semibold text-foreground">关于博客</h3>
            <p className="text-muted-foreground text-sm leading-relaxed">
              这里是我分享技术见解、生活感悟和学习心得的地方。 希望我的文章能够对你有所帮助和启发。
            </p>
          </div>

          {/* Quick Links */}
          <div className="space-y-4">
            <h3 className="text-lg font-semibold text-foreground">快速链接</h3>
            <div className="flex flex-col space-y-2">
              <a href="/" className="text-muted-foreground hover:text-primary transition-colors text-sm">
                首页
              </a>
              <a href="/about" className="text-muted-foreground hover:text-primary transition-colors text-sm">
                关于我
              </a>
              <a href="/contact" className="text-muted-foreground hover:text-primary transition-colors text-sm">
                联系方式
              </a>
              <a href="/rss" className="text-muted-foreground hover:text-primary transition-colors text-sm">
                RSS 订阅
              </a>
            </div>
          </div>

          {/* Social Links */}
          <div className="space-y-4">
            <h3 className="text-lg font-semibold text-foreground">关注我</h3>
            <div className="flex space-x-2">
              <Button variant="outline" size="icon" className="h-8 w-8 bg-transparent">
                <Github className="h-4 w-4" />
              </Button>
              <Button variant="outline" size="icon" className="h-8 w-8 bg-transparent">
                <Twitter className="h-4 w-4" />
              </Button>
              <Button variant="outline" size="icon" className="h-8 w-8 bg-transparent">
                <Mail className="h-4 w-4" />
              </Button>
              <Button variant="outline" size="icon" className="h-8 w-8 bg-transparent">
                <Rss className="h-4 w-4" />
              </Button>
            </div>
          </div>
        </div>

        <div className="border-t border-border mt-8 pt-8 text-center">
          <p className="text-muted-foreground text-sm">© 2024 我的博客. 保留所有权利.</p>
        </div>
      </div>
    </footer>
  )
}