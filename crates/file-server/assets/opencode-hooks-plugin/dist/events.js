export function buildToolHookInput(eventName, event, cwd) {
    return {
        session_id: event.sessionId,
        cwd,
        hook_event_name: eventName,
        tool_name: event.tool,
        tool_input: event.args,
    };
}
export function buildSessionHookInput(eventName, event, cwd) {
    return {
        session_id: event.sessionId,
        cwd,
        hook_event_name: eventName,
    };
}
/**
 * Detect if a tool execution resulted in failure (best-effort).
 * Used to decide between PostToolUse and PostToolUseFailure.
 */
export function detectToolFailure(event) {
    // Explicit error flag
    if (event.error === true || (typeof event.error === "string" && event.error.length > 0)) {
        return true;
    }
    const output = typeof event.output === "string" ? event.output : JSON.stringify(event.output ?? "");
    if (event.tool === "bash") {
        // Check for bash error patterns
        const errorPatterns = [
            /exit code [1-9]/i,
            /exited with code [1-9]/i,
            /command failed/i,
            /error:/i,
            /\bfailed\b/i,
            /\bFAILED\b/,
        ];
        return errorPatterns.some((p) => p.test(output));
    }
    // For other tools, check output for generic error indicators
    const genericErrorPatterns = [/error/i, /failed/i, /exception/i];
    return genericErrorPatterns.some((p) => p.test(output));
}
/**
 * Map OpenCode tool.execute.after event to Claude hook event name.
 * Returns PostToolUseFailure if failure detected, else PostToolUse.
 */
export function mapToolAfterEvent(event) {
    return detectToolFailure(event) ? "PostToolUseFailure" : "PostToolUse";
}
//# sourceMappingURL=events.js.map