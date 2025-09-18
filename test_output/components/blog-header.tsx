"use client"

import { useState } from "react"
import { Search, Menu, X, Sparkles } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"

export function BlogHeader() {
  const [isMenuOpen, setIsMenuOpen] = useState(false)
  const [searchQuery, setSearchQuery] = useState("")

  const categories = ["技术", "生活", "思考", "教程", "随笔"]



  return (
    <header className="bg-white/80 backdrop-blur-md border-b border-slate-200/60 sticky top-0 z-50 shadow-sm">
      <div className="container mx-auto px-4">
        <div className="flex items-center justify-between h-18">
          {/* Logo */}
          <div className="flex items-center space-x-2">
            <div className="p-2 bg-gradient-to-br from-blue-500 to-purple-600 rounded-xl shadow-lg">
              <Sparkles className="h-6 w-6 text-white" />
            </div>
            <h1 className="text-2xl font-bold bg-gradient-to-r from-slate-800 to-slate-600 bg-clip-text text-transparent">
              我的博客是
            </h1>
          </div>

          {/* Desktop Navigation */}
          <nav className="hidden md:flex items-center space-x-1">
            <a
              href="/"
              className="px-4 py-2 text-slate-700 hover:text-blue-600 hover:bg-blue-50 rounded-lg transition-all duration-200 font-medium"
            >
              首页4
            </a>
            {categories.map((category) => (
              <a
                key={category}
                href={`/category/${category}`}
                className="px-4 py-2 text-slate-600 hover:text-blue-600 hover:bg-blue-50 rounded-lg transition-all duration-200"
              >
                {category}
              </a>
            ))}
          </nav>

          {/* Search Bar */}
          <div className="hidden md:flex items-center space-x-4">
            <div className="relative">
              <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 text-slate-400 h-4 w-4" />
              <Input
                type="search"
                placeholder="搜索文章..."
                value={searchQuery}
                onChange={(e) => setSearchQuery(e.target.value)}
                className="pl-10 w-72 bg-slate-50/80 border-slate-200 focus:bg-white focus:border-blue-300 focus:ring-blue-200 transition-all duration-200"
              />
            </div>
          </div>

          {/* Mobile Menu Button */}
          <Button
            variant="ghost"
            size="icon"
            className="md:hidden hover:bg-slate-100"
            onClick={() => setIsMenuOpen(!isMenuOpen)}
          >
            {isMenuOpen ? <X className="h-5 w-5" /> : <Menu className="h-5 w-5" />}
          </Button>
        </div>

        {/* Mobile Menu */}
        {isMenuOpen && (
          <div className="md:hidden py-6 border-t border-slate-200/60 bg-white/90 backdrop-blur-sm">
            <div className="flex flex-col space-y-4">
              <div className="relative">
                <Search className="absolute left-3 top-1/2 transform -translate-y-1/2 text-slate-400 h-4 w-4" />
                <Input
                  type="search"
                  placeholder="搜索文章..."
                  value={searchQuery}
                  onChange={(e) => setSearchQuery(e.target.value)}
                  className="pl-10 bg-slate-50 border-slate-200"
                />
              </div>
              <nav className="flex flex-col space-y-1">
                <a
                  href="/"
                  className="text-slate-700 hover:text-blue-600 hover:bg-blue-50 rounded-lg py-3 px-2 transition-all duration-200 font-medium"
                >
                  首页
                </a>
                {categories.map((category) => (
                  <a
                    key={category}
                    href={`/category/${category}`}
                    className="text-slate-600 hover:text-blue-600 hover:bg-blue-50 rounded-lg py-3 px-2 transition-all duration-200"
                  >
                    {category}
                  </a>
                ))}
              </nav>
            </div>
          </div>
        )}
      </div>
    </header>
  )
}