import * as React from "react"
import * as AspectRatioPrimitive from "@radix-ui/react-aspect-ratio"

import { cn } from "@/lib/utils"

const AspectRatio = React.forwardRef<
  React.ElementRef<typeof AspectRatioPrimitive.Root>,
  React.ComponentPropsWithoutRef<typeof AspectRatioPrimitive.Root>
>(({ className, ...props }, ref) => (
  <AspectRatioPrimitive.Root
    ref={ref}
    className={cn("relative overflow-hidden rounded-md bg-gray-100", className)}
    {...props}
  />
))
AspectRatio.displayName = AspectRatioPrimitive.Root.displayName

export { AspectRatio }
