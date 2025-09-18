import { BlogHeader } from "@/components/blog-header"
import { BlogGrid } from "@/components/blog-grid"
import { BlogFooter } from "@/components/blog-footer"

export default function HomePage() {
  return (
    <div className="min-h-screen bg-gradient-to-br from-slate-50 via-white to-blue-50/30">
      <BlogHeader />
      <main className="container mx-auto px-4 py-12">
        <BlogGrid />
      </main>
      <BlogFooter />
    </div>
  )
}