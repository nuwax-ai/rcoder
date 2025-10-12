# Claude Code Development Guide

## Claude Code Integration

This template is optimized for Claude Code with specific patterns and configurations.

### Claude-Specific Patterns

#### Component Generation
```typescript
// Claude: Generate a button component with variants
import { cva, type VariantProps } from "class-variance-authority"
import { forwardRef } from "react"

const buttonVariants = cva(
  "inline-flex items-center justify-center rounded-md text-sm font-medium",
  {
    variants: {
      variant: {
        default: "bg-primary text-primary-foreground hover:bg-primary/90",
        destructive: "bg-destructive text-destructive-foreground hover:bg-destructive/90",
        outline: "border border-input bg-background hover:bg-accent hover:text-accent-foreground",
        secondary: "bg-secondary text-secondary-foreground hover:bg-secondary/80",
        ghost: "hover:bg-accent hover:text-accent-foreground",
        link: "text-primary underline-offset-4 hover:underline",
      },
      size: {
        default: "h-10 px-4 py-2",
        sm: "h-9 rounded-md px-3",
        lg: "h-11 rounded-md px-8",
        icon: "h-10 w-10",
      },
    },
    defaultVariants: {
      variant: "default",
      size: "default",
    },
  }
)

export interface ButtonProps
  extends React.ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof buttonVariants> {
  asChild?: boolean
}

const Button = forwardRef<HTMLButtonElement, ButtonProps>(
  ({ className, variant, size, asChild = false, ...props }, ref) => {
    return (
      <button
        className={cn(buttonVariants({ variant, size, className }))}
        ref={ref}
        {...props}
      />
    )
  }
)
Button.displayName = "Button"

export { Button, buttonVariants }
```

#### API Service Pattern
```typescript
// Claude: Generate API service with TanStack Query
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { api } from '@/lib/api'

export const useUserData = (userId: string) => {
  return useQuery({
    queryKey: ['user', userId],
    queryFn: () => api.get(`/users/${userId}`).then(res => res.data),
    enabled: !!userId,
  })
}

export const useUpdateUser = () => {
  const queryClient = useQueryClient()
  
  return useMutation({
    mutationFn: (userData: UpdateUserData) => 
      api.put(`/users/${userData.id}`, userData),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['users'] })
    },
  })
}
```

### Claude Code Prompts

#### For Component Development
```
Claude, create a [component type] component using Radix UI and Tailwind CSS. Follow the existing patterns in src/components/ui/. Include:

1. Proper TypeScript interfaces
2. Accessibility attributes
3. Responsive design variants
4. ForwardRef implementation
5. Class variance authority for styling
6. Proper exports and displayName
```

#### For Page Development
```
Claude, create a Next.js page using App Router with the following requirements:

1. Use the layout.tsx pattern
2. Include proper metadata
3. Implement loading states with TanStack Query
4. Use UI components from src/components/ui/
5. Follow the existing project structure
6. Include error handling
```

#### For API Integration
```
Claude, create an API service layer that:

1. Uses the existing Axios configuration in src/lib/api.ts
2. Implements TanStack Query hooks
3. Includes proper TypeScript interfaces
4. Handles loading and error states
5. Follows RESTful conventions
```

### Project Structure for Claude

```
src/
├── app/                    # Next.js App Router
│   ├── layout.tsx         # Root layout
│   ├── page.tsx           # Home page
│   └── [route]/           # Dynamic routes
├── components/            # React components
│   ├── ui/               # Radix UI components
│   │   ├── button.tsx
│   │   ├── input.tsx
│   │   └── ...
│   └── common/           # Shared components
├── lib/                  # Utilities
│   ├── api.ts           # HTTP client
│   ├── utils.ts         # Helper functions
│   └── ...
└── hooks/               # Custom React hooks
```

### Best Practices for Claude

1. **Type Safety**: Always use TypeScript interfaces
2. **Accessibility**: Include ARIA attributes and semantic HTML
3. **Performance**: Use React.memo and useCallback appropriately
4. **Testing**: Write tests for components and hooks
5. **Documentation**: Add JSDoc comments for complex logic

### Claude Configuration Files

- `.cursorrules` - Cursor AI development rules
- `AGENTS.md` - General AI agent guidelines
- `CLAUDE.md` - Claude-specific patterns

### Common Claude Commands

```bash
# Generate a new component
claude component Button

# Create a new page
claude page dashboard

# Optimize existing code
claude optimize src/components/Button.tsx

# Generate tests
claude test src/components/Button.tsx
```

### Troubleshooting Claude Issues

1. **Import Errors**: Check file paths and exports
2. **Type Errors**: Verify TypeScript interfaces
3. **Styling Issues**: Confirm Tailwind CSS classes
4. **Build Errors**: Run `pnpm clean` first
5. **Test Failures**: Check test configuration