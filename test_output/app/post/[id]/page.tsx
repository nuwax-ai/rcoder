import { BlogHeader } from "@/components/blog-header"
import { BlogFooter } from "@/components/blog-footer"
import { BlogPost } from "@/components/blog-post"

interface PageProps {
  params: {
    id: string
  }
}

export default function PostPage({ params }: PageProps) {
  return (
    <div className="min-h-screen bg-background">
      <BlogHeader />
      <main className="container mx-auto px-4 py-8">
        <BlogPost postId={params.id} />
      </main>
      <BlogFooter />
    </div>
  )
}