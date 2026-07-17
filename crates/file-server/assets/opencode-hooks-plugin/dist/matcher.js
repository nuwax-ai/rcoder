/**
 * Tests whether a hook should fire based on its matcher pattern.
 *
 * @param matcher - The regex pattern string from hook config (optional)
 * @param value - The value to test against (e.g. tool name). If undefined, always matches.
 * @returns true if the hook should fire
 */
export function matchHook(matcher, value) {
    // If no value to test against, always match (events with no matcher concept)
    if (value === undefined)
        return true;
    // Empty string, "*", or undefined matcher = match everything
    if (!matcher || matcher === "*")
        return true;
    try {
        const regex = new RegExp(matcher, "i");
        return regex.test(value);
    }
    catch {
        // Invalid regex: treat as no match to avoid crashing
        return false;
    }
}
//# sourceMappingURL=matcher.js.map