import { Calendar, Clock, User, ArrowUpRight } from "lucide-react"
import { Card, CardContent, CardFooter, CardHeader } from "@/components/ui/card"
import { Badge } from "@/components/ui/badge"
import Image from "next/image"

interface BlogPost {
  id: number
  title: string
  excerpt: string
  category: string
  date: string
  
  readTime: string
  image: string
  author: string
}

interface BlogCardProps {
  post: BlogPost
}

export function BlogCard({ post }: BlogCardProps) {
  return (
    <Card className="group hover:shadow-2xl hover:shadow-blue-500/10 transition-all duration-500 hover:-translate-y-2 cursor-pointer bg-white/80 backdrop-blur-sm border-slate-200/60 overflow-hidden">
      <CardHeader className="p-0">
        <div className="relative overflow-hidden">
          <Image
            src={post.image || "/placeholder.svg"}
            alt={post.title}
            width={400}
            height={200}
            className="w-full h-52 object-cover group-hover:scale-110 transition-transform duration-700"
          />
          <div className="absolute inset-0 bg-gradient-to-t from-black/20 via-transparent to-transparent opacity-0 group-hover:opacity-100 transition-opacity duration-300"></div>
          <div className="absolute top-4 left-4">
            <Badge className="bg-white/90 text-slate-700 hover:bg-white shadow-lg backdrop-blur-sm border-0 font-medium">
              {post.category}
            </Badge>
          </div>
          <div className="absolute top-4 right-4 opacity-0 group-hover:opacity-100 transition-all duration-300 transform translate-x-2 group-hover:translate-x-0">
            <div className="p-2 bg-white/90 rounded-full shadow-lg backdrop-blur-sm">
              <ArrowUpRight className="h-4 w-4 text-slate-700" />
            </div>
          </div>
        </div>
      </CardHeader>

      <CardContent className="p-6 pb-4">
        <h3 className="text-xl font-bold text-slate-800 mb-3 group-hover:text-blue-600 transition-colors duration-300 line-clamp-2 leading-tight">
          {post.title}
        </h3>
        <p className="text-slate-600 text-sm leading-relaxed line-clamp-3">{post.excerpt}</p>
      </CardContent>

      <CardFooter className="px-6 pb-6 pt-2">
        <div className="flex items-center justify-between w-full text-xs text-slate-500">
          <div className="flex items-center space-x-4">
            <div className="flex items-center space-x-1.5">
              <User className="h-3.5 w-3.5 text-slate-400" />
              <span className="font-medium">{post.author}</span>
            </div>
            <div className="flex items-center space-x-1.5">
              <Calendar className="h-3.5 w-3.5 text-slate-400" />
              <span>{post.date}</span>
            </div>
          </div>
          <div className="flex items-center space-x-1.5 text-blue-600 font-medium">
            <Clock className="h-3.5 w-3.5" />
            <span>{post.readTime}</span>
          </div>
        </div>
      </CardFooter>
    </Card>
  )
}