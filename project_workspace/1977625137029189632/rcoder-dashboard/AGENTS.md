# XAGI AI Agent Development Guide

## Overview
This template is optimized for AI-powered development with support for Claude, Cursor, and other AI coding assistants.

## AI Agent Configuration

### Supported AI Agents
- **Claude Code** - Full integration with automated workflows
- **Cursor** - AI-powered IDE integration
- **GitHub Copilot** - Code completion and suggestions
- **Windsurf** - Advanced AI coding assistant

### Key Features for AI Development

#### 1. Smart Component Generation
- Radix UI + Tailwind CSS component patterns
- Consistent TypeScript typing
- Accessibility-first design
- Responsive design patterns

#### 2. Automated Testing
- Vitest testing framework
- React Testing Library integration
- Coverage reporting
- Accessibility testing

#### 3. Code Quality Assurance
- ESLint + Prettier configuration
- TypeScript strict mode
- Automated linting and formatting
- Performance optimization

## Development Workflow

### For AI Agents
1. **Component Generation**: Use `src/components/ui/` patterns
2. **Page Creation**: Follow App Router structure in `src/app/`
3. **API Integration**: Use `src/lib/api.ts` for HTTP client
4. **State Management**: Implement TanStack Query patterns

### For Human Developers
1. **Setup**: `pnpm install`
2. **Development**: `pnpm dev`
3. **Testing**: `pnpm test`
4. **Build**: `pnpm build`

## Project Structure

```
src/
├── app/                 # Next.js App Router
├── components/          # React components
│   ├── ui/             # Radix UI components
│   └── common/         # Common components
├── lib/                # Utilities and helpers
└── hooks/              # Custom React hooks
```

## AI-Specific Guidelines

### Component Patterns
- Use class-variance-authority for styling variants
- Implement proper TypeScript interfaces
- Include accessibility attributes
- Follow semantic HTML structure

### API Integration
- Use Axios HTTP client with interceptors
- Implement proper error handling
- Use TypeScript interfaces for API responses
- Implement loading states

### State Management
- Use TanStack Query for server state
- React Hook Form + Zod for form state
- Local state with React hooks
- Context for global state

## Best Practices

1. **Type Safety**: Strict TypeScript configuration
2. **Performance**: Code splitting and lazy loading
3. **Accessibility**: WCAG 2.1 AA compliance
4. **Testing**: Comprehensive test coverage
5. **Documentation**: JSDoc comments and type definitions

## AI Prompts

### Component Generation Prompt
```
Create a [component type] component using Radix UI and Tailwind CSS. Follow the existing patterns in src/components/ui/. Include proper TypeScript types, accessibility attributes, and responsive design.
```

### Page Creation Prompt
```
Create a Next.js page using App Router. Include proper layout, metadata, and follow the existing patterns in src/app/. Use the UI components from src/components/ui/.
```

## Troubleshooting

### Common Issues
1. **TypeScript Errors**: Check types in `src/lib/`
2. **Styling Issues**: Verify Tailwind CSS configuration
3. **Build Errors**: Run `pnpm clean` before building
4. **Test Failures**: Check test configuration in `vitest.config.ts`

### AI Agent Compatibility
This template is designed to work seamlessly with major AI coding assistants and provides optimal patterns for AI-generated code.